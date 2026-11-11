//! Compaction events: what the last compaction of a thread did (files re-attached, pinned rules
//! re-injected) and the state of the automatic-compaction circuit breaker.
//!
//! The compaction code emits events here; a server turns them into `inocode/compaction/event`
//! notifications and answers `inocode/compaction/state` from the last event.

use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::OnceLock;

/// Consecutive automatic-compaction failures after which the thread stops auto-compacting
/// (`compaction.max_consecutive_failures`, read at the point of use).
pub fn max_consecutive_failures() -> u32 {
    u32::try_from(codex_inocode_flags::int_param("compaction.max_consecutive_failures"))
        .unwrap_or(u32::MAX)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CompactionEventKind {
    Completed,
    Failed,
    BreakerTripped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionEvent {
    pub thread_id: String,
    pub kind: CompactionEventKind,
    pub auto: bool,
    pub reason: String,
    pub reattached_files: Vec<String>,
    pub pinned_rule_ids: Vec<String>,
    pub consecutive_failures: u32,
}

#[derive(Default)]
struct Pending {
    reattached_files: Vec<String>,
    pinned_rule_ids: Vec<String>,
}

fn pending() -> &'static Mutex<HashMap<String, Pending>> {
    static M: OnceLock<Mutex<HashMap<String, Pending>>> = OnceLock::new();
    M.get_or_init(Default::default)
}

fn last() -> &'static Mutex<HashMap<String, CompactionEvent>> {
    static M: OnceLock<Mutex<HashMap<String, CompactionEvent>>> = OnceLock::new();
    M.get_or_init(Default::default)
}

fn feed() -> &'static tokio::sync::broadcast::Sender<CompactionEvent> {
    static S: OnceLock<tokio::sync::broadcast::Sender<CompactionEvent>> = OnceLock::new();
    S.get_or_init(|| tokio::sync::broadcast::channel(64).0)
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Notes the files a compaction of `thread_id` re-attached; reported by the next [`emit`].
pub fn note_reattached(thread_id: &str, files: Vec<String>) {
    lock(pending()).entry(thread_id.to_string()).or_default().reattached_files = files;
}

/// Notes the pinned rules a compaction of `thread_id` re-injected; reported by the next [`emit`].
pub fn note_pinned(thread_id: &str, rule_ids: Vec<String>) {
    lock(pending()).entry(thread_id.to_string()).or_default().pinned_rule_ids = rule_ids;
}

/// Ids in a pinned-rules text whose lines look like `- [id] title ...`.
pub fn rule_ids_in_text(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| {
            let rest = line.trim_start().strip_prefix("- [")?;
            let (id, _) = rest.split_once(']')?;
            Some(id.to_string())
        })
        .collect()
}

/// Records and broadcasts one event, attaching (and clearing) the notes of this compaction.
pub fn emit(
    thread_id: &str,
    kind: CompactionEventKind,
    auto: bool,
    reason: &str,
    consecutive_failures: u32,
) -> CompactionEvent {
    let notes = lock(pending()).remove(thread_id).unwrap_or_default();
    let event = CompactionEvent {
        thread_id: thread_id.to_string(),
        kind,
        auto,
        reason: reason.to_string(),
        reattached_files: notes.reattached_files,
        pinned_rule_ids: notes.pinned_rule_ids,
        consecutive_failures,
    };
    lock(last()).insert(thread_id.to_string(), event.clone());
    let _ = feed().send(event.clone());
    event
}

/// The most recent event of `thread_id`.
pub fn last_event(thread_id: &str) -> Option<CompactionEvent> {
    lock(last()).get(thread_id).cloned()
}

/// Live feed of every emitted event.
pub fn subscribe() -> tokio::sync::broadcast::Receiver<CompactionEvent> {
    feed().subscribe()
}

/// Breaker state of `thread_id` from its last event.
pub fn breaker_state(thread_id: &str) -> (u32, bool) {
    match last_event(thread_id) {
        Some(e) => (
            e.consecutive_failures,
            e.kind == CompactionEventKind::BreakerTripped
                || e.consecutive_failures >= max_consecutive_failures(),
        ),
        None => (0, false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn emit_attaches_notes_once_and_tracks_the_breaker() {
        note_reattached("t-a", vec!["src/a.rs".into()]);
        note_pinned("t-a", vec!["r1".into()]);
        let done = emit("t-a", CompactionEventKind::Completed, true, "ContextLimit", 0);
        assert_eq!(done.reattached_files, vec!["src/a.rs".to_string()]);
        assert_eq!(done.pinned_rule_ids, vec!["r1".to_string()]);
        assert_eq!(breaker_state("t-a"), (0, false));
        let failed = emit("t-a", CompactionEventKind::Failed, true, "ContextLimit", 1);
        assert!(failed.reattached_files.is_empty());
        emit("t-a", CompactionEventKind::Failed, true, "x", 3);
        assert_eq!(breaker_state("t-a"), (3, true));
        emit("t-a", CompactionEventKind::BreakerTripped, true, "x", 3);
        assert_eq!(last_event("t-a").unwrap().kind, CompactionEventKind::BreakerTripped);
        assert_eq!(breaker_state("never"), (0, false));
    }

    #[test]
    fn rule_ids_are_read_from_the_pinned_text() {
        let text = "禁止事項(常に守る):\n- [no-force-push] force push 禁止 (理由: x)\n- [r2] other";
        assert_eq!(rule_ids_in_text(text), vec!["no-force-push", "r2"]);
    }

    #[tokio::test]
    async fn events_are_broadcast() {
        let mut rx = subscribe();
        emit("t-b", CompactionEventKind::Failed, false, "UserRequested", 0);
        let got = loop {
            let e = rx.recv().await.unwrap();
            if e.thread_id == "t-b" {
                break e;
            }
        };
        assert!(!got.auto);
    }
}
