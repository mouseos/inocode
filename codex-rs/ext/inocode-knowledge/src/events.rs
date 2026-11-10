//! ガードの発動の記録(JSONL)と購読、失敗例の記録の通知。

use crate::guard::GuardVerdict;
use crate::record::{Kind, Scope, Severity};
use crate::store::Store;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GuardOutcome {
    Blocked,
    Warned,
}

/// どのツール呼び出しで発動したか。
#[derive(Debug, Clone, Default)]
pub struct GuardTarget {
    pub thread_id: String,
    pub turn_id: Option<String>,
    pub tool_use_id: Option<String>,
    pub tool: String,
    /// コマンド、またはパス。
    pub target: String,
    pub cwd: String,
}

/// ガードの発動 1 件。1 件の規則に合うごとに 1 件。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GuardEvent {
    pub ts_ms: u64,
    pub thread_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,
    pub tool: String,
    pub target: String,
    pub severity: Severity,
    pub result: GuardOutcome,
    pub rule_id: String,
    pub rule_title: String,
    pub reason: String,
    pub scope: Scope,
    #[serde(default)]
    pub cwd: String,
}

fn guard_sender() -> &'static tokio::sync::broadcast::Sender<GuardEvent> {
    static S: OnceLock<tokio::sync::broadcast::Sender<GuardEvent>> = OnceLock::new();
    S.get_or_init(|| tokio::sync::broadcast::channel(64).0)
}

/// ガードの発動を購読する。
pub fn subscribe_guard() -> tokio::sync::broadcast::Receiver<GuardEvent> {
    guard_sender().subscribe()
}

pub fn guard_events_path() -> PathBuf {
    crate::inocode_home().join("guard").join("events.jsonl")
}

fn now_ms() -> u64 {
    u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or(0)
}

/// 判定の結果を、規則ごとに記録して購読者へ送る。記録の失敗は無視する。
pub fn record_guard_hits(verdict: &GuardVerdict, t: &GuardTarget) -> Vec<GuardEvent> {
    let mut out = Vec::new();
    let hits = verdict
        .blocked
        .iter()
        .map(|h| (h, GuardOutcome::Blocked))
        .chain(verdict.warnings.iter().map(|h| (h, GuardOutcome::Warned)));
    for (h, result) in hits {
        let ev = GuardEvent {
            ts_ms: now_ms(),
            thread_id: t.thread_id.clone(),
            turn_id: t.turn_id.clone(),
            tool_use_id: t.tool_use_id.clone(),
            tool: t.tool.clone(),
            target: t.target.clone(),
            severity: h.severity,
            result,
            rule_id: h.id.clone(),
            rule_title: h.title.clone(),
            reason: h.reason.clone(),
            scope: h.scope,
            cwd: t.cwd.clone(),
        };
        append_guard(&ev);
        let _ = guard_sender().send(ev.clone());
        out.push(ev);
    }
    out
}

fn append_guard(ev: &GuardEvent) {
    let path = guard_events_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let Ok(line) = serde_json::to_string(ev) else {
        return;
    };
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{line}");
    }
}

/// 履歴を新しい順に返す。`cursor` は読み飛ばす件数。
pub fn list_guard_events(
    cwd: Option<&str>,
    thread_id: Option<&str>,
    limit: usize,
    cursor: usize,
) -> (Vec<GuardEvent>, Option<usize>) {
    let Ok(text) = std::fs::read_to_string(guard_events_path()) else {
        return (Vec::new(), None);
    };
    let mut all: Vec<GuardEvent> = text
        .lines()
        .filter_map(|l| serde_json::from_str::<GuardEvent>(l).ok())
        .filter(|e| cwd.is_none_or(|c| e.cwd == c))
        .filter(|e| thread_id.is_none_or(|t| e.thread_id == t))
        .collect();
    all.reverse();
    let limit = if limit == 0 { 50 } else { limit };
    let total = all.len();
    let page: Vec<GuardEvent> = all.into_iter().skip(cursor).take(limit).collect();
    let next = (cursor + page.len() < total).then_some(cursor + page.len());
    (page, next)
}

/// 失敗例などが記録されたことの通知。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnowledgeRecorded {
    pub id: String,
    pub kind: Kind,
    pub scope: Scope,
    pub title: String,
}

fn recorded_sender() -> &'static tokio::sync::broadcast::Sender<KnowledgeRecorded> {
    static S: OnceLock<tokio::sync::broadcast::Sender<KnowledgeRecorded>> = OnceLock::new();
    S.get_or_init(|| tokio::sync::broadcast::channel(64).0)
}

pub fn subscribe_recorded() -> tokio::sync::broadcast::Receiver<KnowledgeRecorded> {
    recorded_sender().subscribe()
}

pub(crate) fn notify_recorded(store: &Store, id: &str) {
    if let Some(rec) = store.read(Scope::Project, id) {
        let _ = recorded_sender().send(KnowledgeRecorded {
            id: rec.id,
            kind: rec.kind,
            scope: rec.scope,
            title: rec.title,
        });
    }
}
