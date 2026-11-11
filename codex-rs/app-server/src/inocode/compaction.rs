//! `inocode/compaction/state` and the `inocode/compaction/event` notification, backed by
//! `codex-inocode-compaction`.

use super::InocodeResult;
use super::unknown_method;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::InocodeCompactionEvent;
use codex_app_server_protocol::InocodeCompactionEventKind;
use codex_app_server_protocol::InocodeCompactionEventNotification;
use codex_app_server_protocol::InocodeCompactionStateResponse;
use codex_inocode_compaction::CompactionEvent;
use codex_inocode_compaction::CompactionEventKind;

fn kind(k: CompactionEventKind) -> InocodeCompactionEventKind {
    match k {
        CompactionEventKind::Completed => InocodeCompactionEventKind::Completed,
        CompactionEventKind::Failed => InocodeCompactionEventKind::Failed,
        CompactionEventKind::BreakerTripped => InocodeCompactionEventKind::BreakerTripped,
    }
}

pub(crate) fn to_event(e: CompactionEvent) -> InocodeCompactionEvent {
    InocodeCompactionEvent {
        thread_id: e.thread_id,
        kind: kind(e.kind),
        auto: e.auto,
        reason: e.reason,
        reattached_files: e.reattached_files,
        pinned_rule_ids: e.pinned_rule_ids,
        consecutive_failures: e.consecutive_failures,
    }
}

pub(crate) fn state_of(thread_id: &str) -> InocodeCompactionStateResponse {
    let (consecutive_failures, breaker_tripped) =
        codex_inocode_compaction::breaker_state(thread_id);
    InocodeCompactionStateResponse {
        consecutive_failures,
        max_failures: codex_inocode_compaction::max_consecutive_failures(),
        breaker_tripped,
        last_event: codex_inocode_compaction::last_event(thread_id).map(to_event),
    }
}

/// Forwards compaction events as `inocode/compaction/event`. Call once at startup.
pub(crate) fn spawn_notifier(
    outgoing: std::sync::Arc<crate::outgoing_message::OutgoingMessageSender>,
) {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let mut rx = codex_inocode_compaction::subscribe();
    handle.spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let e = to_event(event);
                    outgoing
                        .send_server_notification(
                            codex_app_server_protocol::ServerNotification::InocodeCompactionEvent(
                                InocodeCompactionEventNotification {
                                    thread_id: e.thread_id,
                                    kind: e.kind,
                                    auto: e.auto,
                                    reason: e.reason,
                                    reattached_files: e.reattached_files,
                                    pinned_rule_ids: e.pinned_rule_ids,
                                    consecutive_failures: e.consecutive_failures,
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
        ClientRequest::InocodeCompactionState { params, .. } => {
            Ok(Some(state_of(&params.thread_id).into()))
        }
        other => Err(unknown_method(other.method_name())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_reports_the_breaker_and_the_last_event() {
        let idle = state_of("cmp-state-none");
        assert!(!idle.breaker_tripped && idle.last_event.is_none());
        assert_eq!(idle.max_failures, 3);
        codex_inocode_compaction::note_reattached("cmp-state-1", vec!["a.rs".into()]);
        codex_inocode_compaction::emit(
            "cmp-state-1",
            CompactionEventKind::Failed,
            true,
            "ContextLimit",
            3,
        );
        codex_inocode_compaction::emit(
            "cmp-state-1",
            CompactionEventKind::BreakerTripped,
            true,
            "ContextLimit",
            3,
        );
        let s = state_of("cmp-state-1");
        assert!(s.breaker_tripped);
        assert_eq!(s.consecutive_failures, 3);
        assert_eq!(
            s.last_event.expect("event").kind,
            InocodeCompactionEventKind::BreakerTripped
        );
    }
}
