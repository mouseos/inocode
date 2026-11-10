//! Mechanical stall detection and the advisor consultation procedure.
//!
//! The detector watches tool-call observations and fires on:
//! 1. the same failure signature K times in a row,
//! 2. the same action repeated, or an A,B,A,B alternation,
//! 3. N turns with neither a file change nor a changed test result.
//!
//! When it fires, a fixed-shape state is sent to an advisor model, which is asked for other
//! hypotheses, suspicious assumptions and distinguishing experiments (never for an
//! implementation). The answer is fed back to the main agent as hypotheses.

use serde::Deserialize;
use serde::Serialize;
use std::collections::VecDeque;
use std::future::Future;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StallConfig {
    /// Same failure signature this many times in a row fires.
    pub same_failure_k: usize,
    /// Same action (tool + args) this many times in a row fires.
    pub repeat_n: usize,
    /// A,B,A,B alternation length (number of observations) that fires.
    pub alternation_len: usize,
    /// Turns without a file change or test-result change that fire.
    pub no_progress_turns: usize,
}

impl Default for StallConfig {
    fn default() -> Self {
        Self {
            same_failure_k: 3,
            repeat_n: 3,
            alternation_len: 4,
            no_progress_turns: 8,
        }
    }
}

/// What happened in one tool call.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Observation {
    pub tool: String,
    /// Tool arguments, any stable textual form (e.g. the JSON string).
    pub args: String,
    pub exit_code: Option<i32>,
    pub error: Option<String>,
    /// The call modified at least one file.
    pub changed_files: bool,
    /// Normalized test outcome (e.g. "12 passed 0 failed"), if the call ran tests.
    pub test_result: Option<String>,
}

impl Observation {
    pub fn failed(&self) -> bool {
        self.error.is_some() || self.exit_code.is_some_and(|c| c != 0)
    }

    /// Same command + same exit code, or the normalized error message.
    pub fn failure_signature(&self) -> Option<String> {
        if !self.failed() {
            return None;
        }
        let mut sig = String::new();
        if let Some(code) = self.exit_code
            && code != 0
        {
            sig.push_str(&format!("{} {} exit={code}", self.tool, self.args));
        }
        if let Some(err) = &self.error {
            if !sig.is_empty() {
                sig.push_str(" | ");
            }
            sig.push_str(&normalize_error(err));
        }
        Some(sig)
    }

    fn action_key(&self) -> String {
        format!("{}\u{1}{}", self.tool, self.args)
    }
}

/// Lowercases, replaces digit runs and hex addresses with `N`, collapses whitespace, truncates.
pub fn normalize_error(text: &str) -> String {
    let mut out = String::new();
    let mut in_digits = false;
    let mut last_space = true;
    for ch in text.chars() {
        if ch.is_ascii_digit() {
            if !in_digits {
                out.push('N');
                in_digits = true;
            }
            last_space = false;
            continue;
        }
        in_digits = false;
        if ch.is_whitespace() {
            if !last_space {
                out.push(' ');
            }
            last_space = true;
        } else {
            out.extend(ch.to_lowercase());
            last_space = false;
        }
    }
    let trimmed = out.trim();
    trimmed.chars().take(200).collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    SameFailure,
    RepeatedAction,
    Alternation,
    NoProgress,
}

impl Trigger {
    pub fn as_str(self) -> &'static str {
        match self {
            Trigger::SameFailure => "same_failure",
            Trigger::RepeatedAction => "repeated_action",
            Trigger::Alternation => "alternation",
            Trigger::NoProgress => "no_progress",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stall {
    pub trigger: Trigger,
    /// Failure signature (for `SameFailure`) or a description of the repetition.
    pub signature: String,
}

/// Feed every tool call to [`StallDetector::observe`]; it returns a [`Stall`] when one fires.
/// After firing, its history is cleared so the same stall is not reported every call.
#[derive(Debug, Clone)]
pub struct StallDetector {
    config: StallConfig,
    history: VecDeque<Observation>,
    turns_without_progress: usize,
    last_test_result: Option<String>,
}

impl StallDetector {
    pub fn new(config: StallConfig) -> Self {
        Self {
            config,
            history: VecDeque::new(),
            turns_without_progress: 0,
            last_test_result: None,
        }
    }

    pub fn observe(&mut self, obs: Observation) -> Option<Stall> {
        let test_changed = obs
            .test_result
            .as_ref()
            .is_some_and(|t| self.last_test_result.as_ref() != Some(t));
        if let Some(t) = &obs.test_result {
            self.last_test_result = Some(t.clone());
        }
        if obs.changed_files || test_changed {
            self.turns_without_progress = 0;
        } else {
            self.turns_without_progress += 1;
        }
        self.history.push_back(obs);
        let cap = self
            .config
            .same_failure_k
            .max(self.config.repeat_n)
            .max(self.config.alternation_len)
            .max(2)
            + 1;
        while self.history.len() > cap {
            self.history.pop_front();
        }

        let stall = self
            .check_same_failure()
            .or_else(|| self.check_repeat())
            .or_else(|| self.check_alternation())
            .or_else(|| self.check_no_progress());
        if stall.is_some() {
            self.history.clear();
            self.turns_without_progress = 0;
        }
        stall
    }

    fn tail(&self, n: usize) -> Option<Vec<&Observation>> {
        (n > 0 && self.history.len() >= n).then(|| self.history.iter().rev().take(n).collect())
    }

    fn check_same_failure(&self) -> Option<Stall> {
        let tail = self.tail(self.config.same_failure_k)?;
        let first = tail[0].failure_signature()?;
        tail.iter()
            .all(|o| o.failure_signature().as_deref() == Some(first.as_str()))
            .then_some(Stall {
                trigger: Trigger::SameFailure,
                signature: first,
            })
    }

    fn check_repeat(&self) -> Option<Stall> {
        let tail = self.tail(self.config.repeat_n)?;
        let key = tail[0].action_key();
        tail.iter()
            .all(|o| o.action_key() == key)
            .then(|| Stall {
                trigger: Trigger::RepeatedAction,
                signature: format!("{} {}", tail[0].tool, tail[0].args),
            })
    }

    fn check_alternation(&self) -> Option<Stall> {
        let n = self.config.alternation_len;
        if n < 4 {
            return None;
        }
        let tail = self.tail(n)?;
        let a = tail[0].action_key();
        let b = tail[1].action_key();
        if a == b {
            return None;
        }
        tail.iter()
            .enumerate()
            .all(|(i, o)| o.action_key() == *if i % 2 == 0 { &a } else { &b })
            .then(|| Stall {
                trigger: Trigger::Alternation,
                signature: format!("{} {} <-> {} {}", tail[0].tool, tail[0].args, tail[1].tool, tail[1].args),
            })
    }

    fn check_no_progress(&self) -> Option<Stall> {
        (self.config.no_progress_turns > 0
            && self.turns_without_progress >= self.config.no_progress_turns)
            .then(|| Stall {
                trigger: Trigger::NoProgress,
                signature: format!(
                    "{} tool calls without a file change or test-result change",
                    self.turns_without_progress
                ),
            })
    }
}

/// Fixed-shape state sent to the advisor.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsultState {
    pub goal: String,
    pub current_work: String,
    pub tried: Vec<String>,
    pub failure_signature: String,
    pub evidence: Vec<String>,
}

impl ConsultState {
    /// The prompt for the advisor: state plus a question that excludes implementation.
    pub fn render_prompt(&self, stall: &Stall) -> String {
        let mut out = String::new();
        out.push_str("An engineering agent is stuck. Do not write the implementation.\n");
        out.push_str("Give: (1) other hypotheses for the cause, (2) assumptions that look suspicious, (3) experiments that would distinguish between the hypotheses. Be concrete and brief.\n\n");
        out.push_str(&format!("Trigger: {}\n", stall.trigger.as_str()));
        out.push_str(&format!("Goal: {}\n", self.goal));
        out.push_str(&format!("Current work: {}\n", self.current_work));
        out.push_str("Tried:\n");
        for t in &self.tried {
            out.push_str(&format!("- {t}\n"));
        }
        out.push_str(&format!("Failure signature: {}\n", self.failure_signature));
        out.push_str("Evidence:\n");
        for e in &self.evidence {
            out.push_str(&format!("- {e}\n"));
        }
        out
    }
}

/// Wraps the advisor's answer as the next-turn input for the main agent, as hypotheses.
pub fn advice_as_input(stall: &Stall, advice: &str) -> String {
    format!(
        "[advisor] Stall detected ({}: {}). The advisor suggests the following hypotheses; verify them against evidence before relying on them.\n\n{}",
        stall.trigger.as_str(),
        stall.signature,
        advice.trim()
    )
}

/// Runs a consultation. `ask` receives the prompt and returns the advisor's text; production
/// code passes a closure that calls the configured model, tests pass a mock.
pub async fn consult<F, Fut, E>(
    state: &ConsultState,
    stall: &Stall,
    ask: F,
) -> Result<String, E>
where
    F: FnOnce(String) -> Fut,
    Fut: Future<Output = Result<String, E>>,
{
    let advice = ask(state.render_prompt(stall)).await?;
    Ok(advice_as_input(stall, &advice))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn run(cmd: &str, code: i32) -> Observation {
        Observation {
            tool: "shell".into(),
            args: cmd.into(),
            exit_code: Some(code),
            ..Default::default()
        }
    }

    fn edit(path: &str) -> Observation {
        Observation {
            tool: "apply_patch".into(),
            args: path.into(),
            changed_files: true,
            ..Default::default()
        }
    }

    #[test]
    fn same_failure_signature_fires_at_k() {
        let mut d = StallDetector::new(StallConfig::default());
        let err = |n: u32| Observation {
            tool: "shell".into(),
            args: format!("cargo build {n}"),
            error: Some(format!("error[E0425] at line {n}: cannot find value `x`")),
            ..Default::default()
        };
        assert_eq!(d.observe(err(1)), None);
        assert_eq!(d.observe(err(2)), None);
        let stall = d.observe(err(3)).unwrap();
        assert_eq!(stall.trigger, Trigger::SameFailure);
        // history cleared: does not refire immediately
        assert_eq!(d.observe(err(4)), None);
    }

    #[test]
    fn same_command_same_exit_code_is_same_failure() {
        let mut d = StallDetector::new(StallConfig::default());
        d.observe(run("pytest", 1));
        d.observe(run("pytest", 1));
        assert_eq!(d.observe(run("pytest", 1)).unwrap().trigger, Trigger::SameFailure);
    }

    #[test]
    fn different_failures_do_not_fire() {
        let mut d = StallDetector::new(StallConfig::default());
        assert_eq!(d.observe(run("a", 1)), None);
        assert_eq!(d.observe(run("b", 1)), None);
        assert_eq!(d.observe(run("c", 1)), None);
    }

    #[test]
    fn repeated_successful_action_fires() {
        let mut d = StallDetector::new(StallConfig::default());
        d.observe(run("ls", 0));
        d.observe(run("ls", 0));
        assert_eq!(d.observe(run("ls", 0)).unwrap().trigger, Trigger::RepeatedAction);
    }

    #[test]
    fn alternation_fires() {
        let mut d = StallDetector::new(StallConfig::default());
        d.observe(run("a", 0));
        d.observe(run("b", 0));
        d.observe(run("a", 0));
        assert_eq!(d.observe(run("b", 0)).unwrap().trigger, Trigger::Alternation);
    }

    #[test]
    fn no_progress_fires_and_edits_reset() {
        let mut d = StallDetector::new(StallConfig {
            no_progress_turns: 4,
            ..Default::default()
        });
        for i in 0..3 {
            assert_eq!(d.observe(run(&format!("read {i}"), 0)), None);
        }
        d.observe(edit("a.rs")); // progress resets the counter
        for i in 0..3 {
            assert_eq!(d.observe(run(&format!("look {i}"), 0)), None);
        }
        assert_eq!(d.observe(run("look 9", 0)).unwrap().trigger, Trigger::NoProgress);
    }

    #[test]
    fn changed_test_result_counts_as_progress() {
        let mut d = StallDetector::new(StallConfig {
            no_progress_turns: 3,
            ..Default::default()
        });
        let t = |i: usize, r: &str| Observation {
            tool: "shell".into(),
            args: format!("test {i}"),
            test_result: Some(r.into()),
            ..Default::default()
        };
        assert_eq!(d.observe(t(0, "5 failed")), None);
        assert_eq!(d.observe(t(1, "5 failed")), None);
        assert_eq!(d.observe(t(2, "4 failed")), None); // changed -> reset
        assert_eq!(d.observe(t(3, "4 failed")), None);
        assert_eq!(d.observe(t(4, "4 failed")), None);
        assert_eq!(d.observe(t(5, "4 failed")).unwrap().trigger, Trigger::NoProgress);
    }

    #[test]
    fn normalizes_numbers_and_whitespace() {
        assert_eq!(
            normalize_error("Error  at line 42:\n  Timeout after 30s"),
            "error at line N: timeout after Ns"
        );
    }

    #[tokio::test]
    async fn consult_uses_mock_and_frames_advice_as_hypotheses() {
        let stall = Stall {
            trigger: Trigger::SameFailure,
            signature: "cargo build exit=101".into(),
        };
        let state = ConsultState {
            goal: "make build pass".into(),
            current_work: "fixing lib.rs".into(),
            tried: vec!["added import".into()],
            failure_signature: stall.signature.clone(),
            evidence: vec!["E0432 unresolved import".into()],
        };
        let mut seen = String::new();
        let out: Result<String, ()> = consult(&state, &stall, |prompt| {
            seen = prompt;
            async { Ok("Maybe the crate is not a dependency.".to_string()) }
        })
        .await;
        let out = out.unwrap();
        assert!(seen.contains("Do not write the implementation"));
        assert!(seen.contains("E0432 unresolved import"));
        assert!(out.starts_with("[advisor] Stall detected (same_failure"));
        assert!(out.contains("Maybe the crate is not a dependency."));
    }
}
