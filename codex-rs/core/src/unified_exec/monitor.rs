//! Program-side health assessment for background terminal sessions.
//!
//! Everything here is pure logic over a snapshot of observable facts, so the
//! model is only woken when something is actually worth looking at, and the
//! rules can be tested without real clocks or real processes.

use std::collections::HashSet;
use std::sync::OnceLock;
use std::time::Duration;

use regex_lite::Regex;

/// How often the background monitor re-evaluates a live terminal.
pub(crate) const MONITOR_INTERVAL: Duration = Duration::from_secs(30);
/// A prompt-looking last line plus this much silence means the command waits for input.
pub(crate) const PROMPT_IDLE: Duration = Duration::from_secs(15);
/// Default silence after which a finite command is reported as possibly stalled.
pub(crate) const STALL_IDLE: Duration = Duration::from_secs(10 * 60);
/// This many consecutive byte-identical lines are reported as a possible loop.
pub(crate) const LOOP_MIN_REPEATS: usize = 20;
/// Anomaly notifications to the model per process.
pub(crate) const MAX_ANOMALY_NOTIFICATIONS: u32 = 3;
/// Bytes of output kept for classification and for the status tail.
pub(crate) const TAIL_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TerminalKind {
    /// Long running by design; silence and repetition are normal.
    Server,
    Watch,
    Build,
    Test,
    OneShot,
    Unknown,
}

impl TerminalKind {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "server" => Some(Self::Server),
            "watch" => Some(Self::Watch),
            "build" => Some(Self::Build),
            "test" => Some(Self::Test),
            "one-shot" | "one_shot" | "oneshot" => Some(Self::OneShot),
            _ => None,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Server => "server",
            Self::Watch => "watch",
            Self::Build => "build",
            Self::Test => "test",
            Self::OneShot => "one-shot",
            Self::Unknown => "unknown",
        }
    }

    /// Guess the kind from the command line; the model can override it.
    pub(crate) fn infer(command: &str) -> Self {
        let lower = command.to_ascii_lowercase();
        let has = |needle: &str| lower.contains(needle);
        if has("tail -f")
            || has("--watch")
            || has("cargo watch")
            || has("nodemon")
            || (has("ping ") && !has(" -n ") && !has(" -c "))
        {
            return Self::Watch;
        }
        if has("npm run dev")
            || has("npm start")
            || has("yarn dev")
            || has("pnpm dev")
            || has("vite")
            || has("next dev")
            || has("http.server")
            || has("runserver")
            || has("uvicorn")
            || has("flask run")
        {
            return Self::Server;
        }
        if has("cargo test") || has("pytest") || has("npm test") || has("go test") || has("jest") {
            return Self::Test;
        }
        if has("cargo build")
            || has("npm run build")
            || has("make ")
            || has("cmake")
            || has("gradle")
        {
            return Self::Build;
        }
        Self::Unknown
    }

    fn tolerates_silence(self) -> bool {
        matches!(self, Self::Server | Self::Watch)
    }

    fn stall_idle(self) -> Duration {
        match self {
            Self::Build | Self::Test => STALL_IDLE * 2,
            _ => STALL_IDLE,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Health {
    Running,
    WaitingForInput,
    Stalled,
    Looping,
    Exited,
}

impl Health {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::WaitingForInput => "waiting_for_input_suspected",
            Self::Stalled => "stalled_suspected",
            Self::Looping => "loop_suspected",
            Self::Exited => "exited",
        }
    }

    pub(crate) fn is_anomaly(self) -> bool {
        matches!(self, Self::WaitingForInput | Self::Stalled | Self::Looping)
    }
}

/// Facts observed about one terminal at one moment.
#[derive(Clone, Debug)]
pub(crate) struct Observation {
    pub exited: bool,
    pub elapsed: Duration,
    /// Time since the last output byte, or since start when nothing was printed.
    pub idle: Duration,
    pub total_bytes: u64,
    pub tail: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Assessment {
    pub health: Health,
    pub reason: String,
    /// Identifies this particular anomaly so the same one is not reported twice.
    pub fingerprint: String,
}

fn ansi_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new("\u{1b}\\[[0-9;?]*[ -/]*[@-~]|\u{1b}\\][^\u{7}]*\u{7}").expect("ansi regex")
    })
}

fn prompt_regexes() -> &'static [Regex] {
    static RE: OnceLock<Vec<Regex>> = OnceLock::new();
    RE.get_or_init(|| {
        [
            r"(?i)\[(y/n|yes/no)\]\s*[:?]?\s*$",
            r"(?i)\((y/n|yes/no)\)\s*[:?]?\s*$",
            r"(?i)(password|passphrase|passcode)[^\n]{0,40}:\s*$",
            r"(?i)press (any key|enter)[^\n]{0,40}$",
            r"(?i)(continue|proceed|overwrite|replace)\?\s*$",
            r"^(>>>|\.\.\.|In \[\d+\]:|\(Pdb\)|\(gdb\)|mysql>|sqlite>) ?$",
        ]
        .iter()
        .map(|pattern| Regex::new(pattern).expect("prompt regex"))
        .collect()
    })
}

/// Remove escape sequences and carriage-return overwrites (progress bars, spinners).
pub(crate) fn clean_lines(text: &str) -> Vec<String> {
    let stripped = ansi_regex().replace_all(text, "");
    stripped
        .split('\n')
        .map(|line| {
            let line = line.trim_end_matches('\r');
            line.rsplit('\r')
                .next()
                .unwrap_or("")
                .trim_end()
                .to_string()
        })
        .collect()
}

/// Collapse digit runs so lines that differ only by counters compare equal.
pub(crate) fn normalize_line(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut in_digits = false;
    for ch in line.chars() {
        if ch.is_ascii_digit() {
            if !in_digits {
                out.push('#');
            }
            in_digits = true;
        } else {
            in_digits = false;
            out.push(ch);
        }
    }
    out
}

/// The last non-empty line when the output ends without a newline, i.e. a pending prompt.
fn pending_prompt_line(tail: &str) -> Option<String> {
    if tail.ends_with('\n') {
        return None;
    }
    let lines = clean_lines(tail);
    let last = lines.last()?.trim_start().to_string();
    (!last.is_empty()).then_some(last)
}

fn is_prompt(line: &str) -> bool {
    prompt_regexes().iter().any(|re| re.is_match(line))
}

/// Count trailing byte-identical non-empty lines. Lines that differ only by numbers
/// are progress, so they are not counted as repetition.
pub(crate) fn trailing_identical_lines(tail: &str) -> Option<(String, usize)> {
    let lines: Vec<String> = clean_lines(tail)
        .into_iter()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let last = lines.last()?;
    let count = lines.iter().rev().take_while(|line| *line == last).count();
    Some((last.clone(), count))
}

/// Classify a terminal. A larger scale (after the model chose to wait) raises the thresholds.
pub(crate) fn assess(kind: TerminalKind, observation: &Observation, scale: u32) -> Assessment {
    if observation.exited {
        return Assessment {
            health: Health::Exited,
            reason: "process has exited".to_string(),
            fingerprint: String::new(),
        };
    }

    let scale = scale.max(1);
    if observation.idle >= PROMPT_IDLE * scale
        && let Some(line) = pending_prompt_line(&observation.tail)
        && is_prompt(&line)
    {
        return Assessment {
            health: Health::WaitingForInput,
            reason: format!(
                "output ends with a prompt ({line:?}) and nothing was printed for {}s",
                observation.idle.as_secs()
            ),
            fingerprint: format!("prompt:{}", normalize_line(&line)),
        };
    }

    if !kind.tolerates_silence() {
        if let Some((line, count)) = trailing_identical_lines(&observation.tail)
            && count >= LOOP_MIN_REPEATS
        {
            return Assessment {
                health: Health::Looping,
                reason: format!("the same line was printed {count} times in a row: {line:?}"),
                fingerprint: format!("loop:{line}"),
            };
        }
        if observation.idle >= kind.stall_idle() * scale {
            return Assessment {
                health: Health::Stalled,
                reason: format!(
                    "no output for {}s (CPU and child process activity are not measured)",
                    observation.idle.as_secs()
                ),
                fingerprint: format!("stalled:{}", observation.total_bytes),
            };
        }
    }
    Assessment {
        health: Health::Running,
        reason: "producing output or within the expected quiet period".to_string(),
        fingerprint: String::new(),
    }
}

/// Per-process notification bookkeeping.
#[derive(Debug, Default)]
pub(crate) struct MonitorState {
    pub command: String,
    pub kind_override: Option<TerminalKind>,
    notified: u32,
    last_notified: Option<String>,
    acknowledged: HashSet<String>,
    scale_shift: u32,
    /// The model already received the exit through a tool result.
    pub exit_reported: bool,
}

impl MonitorState {
    pub(crate) fn kind(&self) -> TerminalKind {
        self.kind_override
            .unwrap_or_else(|| TerminalKind::infer(&self.command))
    }

    pub(crate) fn scale(&self) -> u32 {
        1u32 << self.scale_shift.min(6)
    }

    /// Whether this assessment should wake the model. Records the notification.
    pub(crate) fn should_notify(&mut self, assessment: &Assessment) -> bool {
        if !assessment.health.is_anomaly()
            || self.notified >= MAX_ANOMALY_NOTIFICATIONS
            || self.acknowledged.contains(&assessment.fingerprint)
            || self.last_notified.as_deref() == Some(assessment.fingerprint.as_str())
        {
            return false;
        }
        self.notified += 1;
        self.last_notified = Some(assessment.fingerprint.clone());
        true
    }

    /// The model looked at an anomaly and chose to keep waiting: stay quiet about
    /// this one and double the thresholds for the next.
    pub(crate) fn acknowledge(&mut self, assessment: &Assessment) {
        if assessment.health.is_anomaly()
            && self.acknowledged.insert(assessment.fingerprint.clone())
        {
            self.scale_shift += 1;
        }
    }

    pub(crate) fn notifications_sent(&self) -> u32 {
        self.notified
    }
}

/// Last `max_lines` cleaned lines of output.
pub(crate) fn tail_lines(tail: &str, max_lines: usize) -> String {
    let lines = clean_lines(tail);
    let start = lines.len().saturating_sub(max_lines);
    lines[start..].join("\n").trim_matches('\n').to_string()
}

#[cfg(test)]
#[path = "monitor_tests.rs"]
mod tests;
