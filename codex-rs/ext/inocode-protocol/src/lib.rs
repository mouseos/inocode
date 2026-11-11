//! Telegraphic message format for agent-to-agent communication.
//!
//! A message is a kind line followed by `key: value` lines:
//!
//! ```text
//! BLOCKED
//! reason: tests/foo.rs fails to compile, missing `Bar::new` (not in src/bar.rs)
//! need: owner of src/bar.rs to add `Bar::new`
//! conf: high
//! ```
//!
//! Only the message handed to another agent is structured; an agent's own reasoning stays free-form.

use serde::Deserialize;
use serde::Serialize;
use std::fmt;
use std::io::Write;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Kind {
    Ask,
    Answer,
    Blocked,
    Result,
    Handoff,
}

impl Kind {
    pub const ALL: [Kind; 5] = [
        Kind::Ask,
        Kind::Answer,
        Kind::Blocked,
        Kind::Result,
        Kind::Handoff,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Ask => "ASK",
            Kind::Answer => "ANSWER",
            Kind::Blocked => "BLOCKED",
            Kind::Result => "RESULT",
            Kind::Handoff => "HANDOFF",
        }
    }

    pub fn parse(s: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.as_str() == s)
    }

    /// Keys that must be present (besides `conf`, which every kind requires).
    pub fn required_keys(self) -> &'static [&'static str] {
        match self {
            Kind::Ask => &["q"],
            Kind::Answer => &["a"],
            Kind::Blocked => &["reason", "need"],
            Kind::Result => &["status", "summary"],
            Kind::Handoff => &["task", "state", "next"],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    High,
    Med,
    Low,
}

impl Confidence {
    pub fn as_str(self) -> &'static str {
        match self {
            Confidence::High => "high",
            Confidence::Med => "med",
            Confidence::Low => "low",
        }
    }

    pub fn parse(s: &str) -> Option<Confidence> {
        match s {
            "high" => Some(Confidence::High),
            "med" => Some(Confidence::Med),
            "low" => Some(Confidence::Low),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolError {
    Empty,
    UnknownKind(String),
    MalformedLine(String),
    MissingKey(&'static str),
    MissingField(String),
    BadConfidence(String),
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProtocolError::Empty => write!(f, "empty message"),
            ProtocolError::UnknownKind(k) => {
                write!(f, "unknown kind `{k}` (use ASK, ANSWER, BLOCKED, RESULT, HANDOFF)")
            }
            ProtocolError::MalformedLine(l) => write!(f, "line is not `key: value`: {l}"),
            ProtocolError::MissingKey(_) => write!(f, "missing conf"),
            ProtocolError::MissingField(k) => write!(f, "missing required key `{k}`"),
            ProtocolError::BadConfidence(c) => {
                write!(f, "conf must be high, med or low, got `{c}`")
            }
        }
    }
}

impl std::error::Error for ProtocolError {}

/// A parsed message. `fields` keeps insertion order; keys are lowercase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub kind: Kind,
    pub conf: Confidence,
    pub fields: Vec<(String, String)>,
}

impl Message {
    pub fn new(kind: Kind, conf: Confidence) -> Self {
        Self {
            kind,
            conf,
            fields: Vec::new(),
        }
    }

    pub fn with(mut self, key: &str, value: &str) -> Self {
        self.fields.push((key.to_ascii_lowercase(), value.to_string()));
        self
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// Checks that every required key for the kind is present and non-empty.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        for key in self.kind.required_keys() {
            if self.get(key).is_none_or(|v| v.trim().is_empty()) {
                return Err(ProtocolError::MissingField((*key).to_string()));
            }
        }
        Ok(())
    }

    /// Renders the message. Values are kept verbatim; newlines in a value continue on
    /// following lines indented by two spaces.
    pub fn render(&self) -> String {
        let mut out = String::from(self.kind.as_str());
        for (k, v) in &self.fields {
            push_field(&mut out, k, v);
        }
        push_field(&mut out, "conf", self.conf.as_str());
        out
    }

    /// Parses and validates a message.
    pub fn parse(text: &str) -> Result<Message, ProtocolError> {
        let mut lines = text.lines().filter(|l| !l.trim().is_empty());
        let head = lines.next().ok_or(ProtocolError::Empty)?.trim();
        let kind = Kind::parse(head).ok_or_else(|| ProtocolError::UnknownKind(head.to_string()))?;
        let mut fields: Vec<(String, String)> = Vec::new();
        for line in lines {
            if let Some(cont) = line.strip_prefix("  ") {
                match fields.last_mut() {
                    Some((_, v)) => {
                        v.push('\n');
                        v.push_str(cont);
                        continue;
                    }
                    None => return Err(ProtocolError::MalformedLine(line.to_string())),
                }
            }
            let Some((k, v)) = line.split_once(':') else {
                return Err(ProtocolError::MalformedLine(line.to_string()));
            };
            fields.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
        let conf_pos = fields
            .iter()
            .position(|(k, _)| k == "conf")
            .ok_or(ProtocolError::MissingKey("conf"))?;
        let (_, conf_raw) = fields.remove(conf_pos);
        let conf =
            Confidence::parse(&conf_raw).ok_or(ProtocolError::BadConfidence(conf_raw))?;
        let message = Message { kind, conf, fields };
        message.validate()?;
        Ok(message)
    }
}

fn push_field(out: &mut String, key: &str, value: &str) {
    out.push('\n');
    out.push_str(key);
    out.push_str(": ");
    let mut parts = value.split('\n');
    out.push_str(parts.next().unwrap_or(""));
    for part in parts {
        out.push_str("\n  ");
        out.push_str(part);
    }
}

/// Text appended to sub-agent instructions when `protocol.enabled` is true.
pub const SYSTEM_PROMPT_SECTION: &str = "\
Agent-to-agent messages (send_message, followup_task, final results) use telegraphic English:
first line is the kind: ASK | ANSWER | BLOCKED | RESULT | HANDOFF. Then `key: value` lines.
ASK needs q. ANSWER needs a. BLOCKED needs reason, need. RESULT needs status, summary.
HANDOFF needs task, state, next. Optional: to, re, files.
Every message ends with `conf: high|med|low`. Never omit it; use low when guessing.
Keep negations, numbers, versions and file paths exact; do not abbreviate them.
Drop filler words, not facts. Think freely in your own reasoning; only the message is structured.
Example:
RESULT
status: done
summary: fixed off-by-one in src/lib.rs:42; cargo test -p foo passes (12 passed, 0 failed)
conf: high";

/// Appends [`SYSTEM_PROMPT_SECTION`] to existing instructions.
pub fn append_to_instructions(existing: Option<&str>) -> String {
    match existing {
        Some(text) if !text.trim().is_empty() => format!("{text}\n\n{SYSTEM_PROMPT_SECTION}"),
        _ => SYSTEM_PROMPT_SECTION.to_string(),
    }
}

/// Verbatim message log: one JSON line per message under
/// `<INOCODE_HOME>/messages/<thread_id>.jsonl`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoggedMessage {
    pub ts_ms: u64,
    pub from: String,
    pub to: String,
    /// `send_message` (no wake) or `followup_task` (wakes the target).
    pub delivery: String,
    pub text: String,
}

fn log_path(inocode_home: &Path, thread_id: &str) -> std::path::PathBuf {
    let safe: String = thread_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    inocode_home.join("messages").join(format!("{safe}.jsonl"))
}

/// Appends a message to the log of the root `thread_id` it belongs to.
pub fn log_message(
    inocode_home: &Path,
    thread_id: &str,
    message: &LoggedMessage,
) -> std::io::Result<()> {
    let path = log_path(inocode_home, thread_id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut line = serde_json::to_string(message).map_err(std::io::Error::other)?;
    line.push('\n');
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(line.as_bytes())
}

/// Reads back all logged messages for a thread, oldest first.
pub fn read_log(inocode_home: &Path, thread_id: &str) -> Vec<LoggedMessage> {
    let Ok(text) = std::fs::read_to_string(log_path(inocode_home, thread_id)) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// Convenience using `INOCODE_HOME`. `thread_id` is resolved to the root of its agent tree
/// (see [`link_thread`]) so every message of one conversation lands in one log; the message
/// is also sent to [`subscribe_messages`] listeners.
pub fn log_message_global(thread_id: &str, message: &LoggedMessage) {
    let root = root_of(thread_id);
    let _ = log_message(&codex_inocode_metrics::inocode_home(), &root, message);
    let _ = message_feed().send((root, message.clone()));
}

fn parents() -> &'static std::sync::Mutex<std::collections::HashMap<String, String>> {
    static M: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, String>>> =
        std::sync::OnceLock::new();
    M.get_or_init(Default::default)
}

/// Records that `child` was spawned by `parent` (both thread ids).
pub fn link_thread(child: &str, parent: &str) {
    if child == parent {
        return;
    }
    parents()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(child.to_string(), parent.to_string());
}

/// The root of the agent tree `thread_id` belongs to (itself when it has no recorded parent).
pub fn root_of(thread_id: &str) -> String {
    let map = parents().lock().unwrap_or_else(|e| e.into_inner());
    let mut current = thread_id.to_string();
    // Bounded walk: a cycle cannot loop forever.
    for _ in 0..64 {
        match map.get(&current) {
            Some(parent) => current = parent.clone(),
            None => break,
        }
    }
    current
}

fn message_feed() -> &'static tokio::sync::broadcast::Sender<(String, LoggedMessage)> {
    static S: std::sync::OnceLock<tokio::sync::broadcast::Sender<(String, LoggedMessage)>> =
        std::sync::OnceLock::new();
    S.get_or_init(|| tokio::sync::broadcast::channel(128).0)
}

/// Live feed of `(root_thread_id, message)` for every message logged with [`log_message_global`].
pub fn subscribe_messages() -> tokio::sync::broadcast::Receiver<(String, LoggedMessage)> {
    message_feed().subscribe()
}

/// A parsed telegraphic message, for display: kind, confidence, and the key/value fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedView {
    pub kind: Kind,
    pub conf: Confidence,
    pub fields: Vec<(String, String)>,
}

/// Breaks a verbatim message into kind, confidence and fields; `None` when it is not a valid
/// telegraphic message.
pub fn parse_view(text: &str) -> Option<ParsedView> {
    let m = Message::parse(text).ok()?;
    Some(ParsedView {
        kind: m.kind,
        conf: m.conf,
        fields: m.fields,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn round_trips_each_kind() {
        let cases = [
            Message::new(Kind::Ask, Confidence::Med).with("q", "which crate owns Foo?"),
            Message::new(Kind::Answer, Confidence::High).with("a", "codex-core, src/foo.rs"),
            Message::new(Kind::Blocked, Confidence::Low)
                .with("reason", "no write access to /etc/hosts")
                .with("need", "path change"),
            Message::new(Kind::Result, Confidence::High)
                .with("status", "done")
                .with("summary", "0 failures, 12 passed"),
            Message::new(Kind::Handoff, Confidence::Med)
                .with("task", "port wait.rs")
                .with("state", "half done")
                .with("next", "write tests"),
        ];
        for m in cases {
            let parsed = Message::parse(&m.render()).unwrap();
            assert_eq!(parsed, m);
        }
    }

    #[test]
    fn rejects_missing_conf_and_fields() {
        assert_eq!(
            Message::parse("ASK\nq: hi"),
            Err(ProtocolError::MissingKey("conf"))
        );
        assert_eq!(
            Message::parse("ASK\nconf: high"),
            Err(ProtocolError::MissingField("q".into()))
        );
        assert_eq!(
            Message::parse("ASK\nq: x\nconf: sure"),
            Err(ProtocolError::BadConfidence("sure".into()))
        );
        assert_eq!(
            Message::parse("YELL\nq: x\nconf: low"),
            Err(ProtocolError::UnknownKind("YELL".into()))
        );
        assert_eq!(Message::parse("  \n"), Err(ProtocolError::Empty));
    }

    #[test]
    fn keeps_negations_numbers_and_paths_verbatim() {
        let text = "RESULT\nstatus: failed\nsummary: did NOT fix C:\\a\\b.rs:42; 3 of 17 tests fail\nconf: med";
        let m = Message::parse(text).unwrap();
        assert_eq!(
            m.get("summary"),
            Some("did NOT fix C:\\a\\b.rs:42; 3 of 17 tests fail")
        );
        assert_eq!(m.render(), text);
    }

    #[test]
    fn multiline_values_round_trip() {
        let m = Message::new(Kind::Ask, Confidence::Low).with("q", "line1\nline2");
        assert_eq!(Message::parse(&m.render()).unwrap(), m);
    }

    #[test]
    fn prompt_section_is_short_and_mentions_conf() {
        assert!(SYSTEM_PROMPT_SECTION.contains("conf: high|med|low"));
        assert!(SYSTEM_PROMPT_SECTION.lines().count() <= 16);
        assert!(append_to_instructions(Some("base")).starts_with("base\n\n"));
    }

    #[test]
    fn root_of_follows_links_and_parse_view_decomposes() {
        link_thread("c1", "root-x");
        link_thread("c2", "c1");
        assert_eq!(root_of("c2"), "root-x");
        assert_eq!(root_of("lonely"), "lonely");
        let v = parse_view("ASK
q: which crate?
conf: med").unwrap();
        assert_eq!(v.kind, Kind::Ask);
        assert_eq!(v.conf, Confidence::Med);
        assert_eq!(v.fields, vec![("q".to_string(), "which crate?".to_string())]);
        assert_eq!(parse_view("just text"), None);
    }

    #[tokio::test]
    async fn global_log_uses_the_root_and_broadcasts() {
        let mut rx = subscribe_messages();
        link_thread("kid-g", "root-g");
        let msg = LoggedMessage {
            ts_ms: 5,
            from: "/root/a".into(),
            to: "/root".into(),
            delivery: "final".into(),
            text: "done".into(),
        };
        log_message_global("kid-g", &msg);
        let (root, got) = loop {
            let (r, m) = rx.recv().await.unwrap();
            if r == "root-g" {
                break (r, m);
            }
        };
        assert_eq!(root, "root-g");
        assert_eq!(got, msg);
    }

    #[test]
    fn log_is_verbatim_and_readable() {
        let dir = tempfile::tempdir().unwrap();
        let msg = LoggedMessage {
            ts_ms: 1,
            from: "/root/a".into(),
            to: "/root/b".into(),
            delivery: "send_message".into(),
            text: "ASK\nq: x\nconf: low".into(),
        };
        log_message(dir.path(), "thread/1", &msg).unwrap();
        log_message(dir.path(), "thread/1", &msg).unwrap();
        assert_eq!(read_log(dir.path(), "thread/1"), vec![msg.clone(), msg]);
    }
}
