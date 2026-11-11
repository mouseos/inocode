//! Persistent log and live feed of stall detections and advisor consultations.
//!
//! One JSON object per line under `<INOCODE_HOME>/advisor/events.jsonl`. Every event is also
//! sent on a process-wide broadcast channel so a server can forward it to clients.

use crate::Trigger;
use serde::Deserialize;
use serde::Serialize;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AdvisorEventKind {
    /// A stall trigger fired.
    Stall,
    /// The advisor's answer arrived.
    Advice,
    /// The consultation failed.
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdvisorEvent {
    pub ts_ms: u64,
    pub thread_id: String,
    pub kind: AdvisorEventKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<Trigger>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recent_actions: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Advice text (`advice`) or error text (`error`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

impl AdvisorEvent {
    fn base(thread_id: &str, kind: AdvisorEventKind) -> Self {
        Self {
            ts_ms: now_ms(),
            thread_id: thread_id.to_string(),
            kind,
            trigger: None,
            signature: None,
            recent_actions: None,
            model: None,
            text: None,
        }
    }

    pub fn stall(
        thread_id: &str,
        trigger: Trigger,
        signature: &str,
        recent_actions: Vec<String>,
    ) -> Self {
        Self {
            trigger: Some(trigger),
            signature: Some(signature.to_string()),
            recent_actions: Some(recent_actions),
            ..Self::base(thread_id, AdvisorEventKind::Stall)
        }
    }

    pub fn advice(thread_id: &str, trigger: Trigger, model: Option<&str>, text: &str) -> Self {
        Self {
            trigger: Some(trigger),
            model: model.map(str::to_string),
            text: Some(text.to_string()),
            ..Self::base(thread_id, AdvisorEventKind::Advice)
        }
    }

    pub fn error(thread_id: &str, trigger: Trigger, model: Option<&str>, text: &str) -> Self {
        Self {
            trigger: Some(trigger),
            model: model.map(str::to_string),
            text: Some(text.to_string()),
            ..Self::base(thread_id, AdvisorEventKind::Error)
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn log_path(inocode_home: &Path) -> PathBuf {
    inocode_home.join("advisor").join("events.jsonl")
}

fn write_lock() -> &'static Mutex<()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(Default::default)
}

/// Appends one event to the log under `inocode_home`.
pub fn append_event(inocode_home: &Path, event: &AdvisorEvent) -> std::io::Result<()> {
    let path = log_path(inocode_home);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut line = serde_json::to_string(event).map_err(std::io::Error::other)?;
    line.push('\n');
    let _guard = write_lock().lock().unwrap_or_else(|e| e.into_inner());
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(line.as_bytes())
}

/// All logged events, oldest first. Unparseable lines are skipped.
pub fn read_events(inocode_home: &Path) -> Vec<AdvisorEvent> {
    let Ok(text) = std::fs::read_to_string(log_path(inocode_home)) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// One page of events, newest first. `cursor` is the number of matching events already
/// returned; the returned cursor is `Some` when more remain.
pub fn page_events(
    inocode_home: &Path,
    thread_id: Option<&str>,
    limit: usize,
    cursor: Option<&str>,
) -> (Vec<AdvisorEvent>, Option<String>) {
    let mut events = read_events(inocode_home);
    if let Some(id) = thread_id {
        events.retain(|e| e.thread_id == id);
    }
    events.reverse();
    let offset = cursor.and_then(|c| c.parse::<usize>().ok()).unwrap_or(0);
    let limit = limit.max(1);
    let page: Vec<AdvisorEvent> = events.iter().skip(offset).take(limit).cloned().collect();
    let next = (offset + page.len() < events.len()).then(|| (offset + page.len()).to_string());
    (page, next)
}

fn feed() -> &'static tokio::sync::broadcast::Sender<AdvisorEvent> {
    static S: OnceLock<tokio::sync::broadcast::Sender<AdvisorEvent>> = OnceLock::new();
    S.get_or_init(|| tokio::sync::broadcast::channel(64).0)
}

/// Live feed of every event recorded with [`record_global`].
pub fn subscribe() -> tokio::sync::broadcast::Receiver<AdvisorEvent> {
    feed().subscribe()
}

/// Appends to the default log (`INOCODE_HOME`) and broadcasts; log errors are not propagated.
pub fn record_global(home: &Path, event: AdvisorEvent) {
    if let Err(err) = append_event(home, &event) {
        eprintln!("inocode-advisor: failed to record event: {err}");
    }
    let _ = feed().send(event);
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn events_round_trip_and_page_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        append_event(
            dir.path(),
            &AdvisorEvent::stall("t1", Trigger::SameFailure, "cargo test exit=101", vec!["a".into()]),
        )
        .unwrap();
        append_event(
            dir.path(),
            &AdvisorEvent::advice("t1", Trigger::SameFailure, Some("m"), "try X"),
        )
        .unwrap();
        append_event(
            dir.path(),
            &AdvisorEvent::error("t2", Trigger::NoProgress, None, "advisor turn aborted"),
        )
        .unwrap();
        let (page, next) = page_events(dir.path(), None, 2, None);
        assert_eq!(page.len(), 2);
        assert_eq!(page[0].kind, AdvisorEventKind::Error);
        assert_eq!(next.as_deref(), Some("2"));
        let (rest, next) = page_events(dir.path(), None, 2, next.as_deref());
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].kind, AdvisorEventKind::Stall);
        assert_eq!(next, None);
        let (t1, _) = page_events(dir.path(), Some("t1"), 10, None);
        assert_eq!(t1.len(), 2);
        assert_eq!(t1[0].text.as_deref(), Some("try X"));
        assert_eq!(t1[1].recent_actions.as_deref(), Some(&["a".to_string()][..]));
    }

    #[tokio::test]
    async fn record_global_broadcasts() {
        let dir = tempfile::tempdir().unwrap();
        let mut rx = subscribe();
        record_global(
            dir.path(),
            AdvisorEvent::error("t9", Trigger::Alternation, None, "boom"),
        );
        let got = loop {
            let e = rx.recv().await.unwrap();
            if e.thread_id == "t9" {
                break e;
            }
        };
        assert_eq!(got.text.as_deref(), Some("boom"));
        assert_eq!(read_events(dir.path()).len(), 1);
    }
}
