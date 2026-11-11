//! `inocode/guard/list` and the `inocode/guard/fired` / `inocode/knowledge/recorded`
//! notifications, backed by the guard log in `codex-inocode-knowledge`.

use super::InocodeResult;
use super::unknown_method;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::InocodeGuardEvent;
use codex_app_server_protocol::InocodeGuardFiredNotification;
use codex_app_server_protocol::InocodeGuardListResponse;
use codex_app_server_protocol::InocodeGuardResult;
use codex_app_server_protocol::InocodeKnowledgeKind;
use codex_app_server_protocol::InocodeKnowledgeRecordedNotification;
use codex_app_server_protocol::InocodeKnowledgeScope;
use codex_app_server_protocol::InocodeRuleSeverity;
use codex_inocode_knowledge as kb;

fn severity(s: kb::Severity) -> InocodeRuleSeverity {
    match s {
        kb::Severity::Block => InocodeRuleSeverity::Block,
        kb::Severity::Warn => InocodeRuleSeverity::Warn,
    }
}

fn result(r: kb::GuardOutcome) -> InocodeGuardResult {
    match r {
        kb::GuardOutcome::Blocked => InocodeGuardResult::Blocked,
        kb::GuardOutcome::Warned => InocodeGuardResult::Warned,
    }
}

fn scope(s: kb::Scope) -> InocodeKnowledgeScope {
    match s {
        kb::Scope::Project => InocodeKnowledgeScope::Project,
        kb::Scope::Global => InocodeKnowledgeScope::Global,
    }
}

fn ts(ms: u64) -> i64 {
    i64::try_from(ms).unwrap_or(i64::MAX)
}

pub(crate) fn to_event(e: kb::GuardEvent) -> InocodeGuardEvent {
    InocodeGuardEvent {
        ts_ms: ts(e.ts_ms),
        thread_id: e.thread_id,
        turn_id: e.turn_id,
        tool_use_id: e.tool_use_id,
        tool: e.tool,
        target: e.target,
        severity: severity(e.severity),
        result: result(e.result),
        rule_id: e.rule_id,
        rule_title: e.rule_title,
        reason: e.reason,
        scope: scope(e.scope),
    }
}

pub(crate) fn to_fired(e: kb::GuardEvent) -> InocodeGuardFiredNotification {
    let e = to_event(e);
    InocodeGuardFiredNotification {
        ts_ms: e.ts_ms,
        thread_id: e.thread_id,
        turn_id: e.turn_id,
        tool_use_id: e.tool_use_id,
        tool: e.tool,
        target: e.target,
        severity: e.severity,
        result: e.result,
        rule_id: e.rule_id,
        rule_title: e.rule_title,
        reason: e.reason,
        scope: e.scope,
    }
}

fn kind(k: kb::Kind) -> InocodeKnowledgeKind {
    match k {
        kb::Kind::Rule => InocodeKnowledgeKind::Rule,
        kb::Kind::FailureCase => InocodeKnowledgeKind::FailureCase,
        kb::Kind::Issue => InocodeKnowledgeKind::Issue,
        kb::Kind::Decision => InocodeKnowledgeKind::Decision,
        kb::Kind::Fact => InocodeKnowledgeKind::Fact,
        kb::Kind::Contract => InocodeKnowledgeKind::Contract,
        kb::Kind::Procedure => InocodeKnowledgeKind::Procedure,
    }
}

/// Forwards guard firings and automatic knowledge recordings to clients. Call once at startup.
pub(crate) fn spawn_notifier(
    outgoing: std::sync::Arc<crate::outgoing_message::OutgoingMessageSender>,
) {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let mut guard_rx = kb::subscribe_guard();
    let guard_out = std::sync::Arc::clone(&outgoing);
    handle.spawn(async move {
        loop {
            match guard_rx.recv().await {
                Ok(event) => {
                    guard_out
                        .send_server_notification(
                            codex_app_server_protocol::ServerNotification::InocodeGuardFired(
                                to_fired(event),
                            ),
                        )
                        .await;
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
    let mut rec_rx = kb::subscribe_recorded();
    handle.spawn(async move {
        loop {
            match rec_rx.recv().await {
                Ok(r) => {
                    outgoing
                        .send_server_notification(
                            codex_app_server_protocol::ServerNotification::InocodeKnowledgeRecorded(
                                InocodeKnowledgeRecordedNotification {
                                    id: r.id,
                                    kind: kind(r.kind),
                                    scope: scope(r.scope),
                                    title: r.title,
                                },
                            ),
                        )
                        .await;
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

pub(crate) async fn handle(request: ClientRequest) -> InocodeResult {
    match request {
        ClientRequest::InocodeGuardList { params, .. } => {
            let cursor = params
                .cursor
                .and_then(|c| c.parse::<usize>().ok())
                .unwrap_or(0);
            let (events, next) = kb::list_guard_events(
                params.cwd.as_deref(),
                params.thread_id.as_deref(),
                params.limit as usize,
                cursor,
            );
            Ok(Some(
                InocodeGuardListResponse {
                    events: events.into_iter().map(to_event).collect(),
                    next_cursor: next.map(|n| n.to_string()),
                }
                .into(),
            ))
        }
        other => Err(unknown_method(other.method_name())),
    }
}
