//! `inocode/advisor/events` and the `inocode/advisor/event` notification: stall detections and
//! advisor consultations recorded by `codex-inocode-advisor` (`<INOCODE_HOME>/advisor/events.jsonl`).

use super::InocodeResult;
use super::unknown_method;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::InocodeAdvisorEvent;
use codex_app_server_protocol::InocodeAdvisorEventKind;
use codex_app_server_protocol::InocodeAdvisorEventNotification;
use codex_app_server_protocol::InocodeAdvisorEventsParams;
use codex_app_server_protocol::InocodeAdvisorEventsResponse;
use codex_app_server_protocol::InocodeAdvisorTrigger;
use codex_inocode_advisor::AdvisorEvent;
use codex_inocode_advisor::AdvisorEventKind;
use codex_inocode_advisor::Trigger;
use std::path::Path;

const DEFAULT_LIMIT: usize = 100;

fn kind(k: AdvisorEventKind) -> InocodeAdvisorEventKind {
    match k {
        AdvisorEventKind::Stall => InocodeAdvisorEventKind::Stall,
        AdvisorEventKind::Advice => InocodeAdvisorEventKind::Advice,
        AdvisorEventKind::Error => InocodeAdvisorEventKind::Error,
    }
}

fn trigger(t: Trigger) -> InocodeAdvisorTrigger {
    match t {
        Trigger::SameFailure => InocodeAdvisorTrigger::SameFailure,
        Trigger::RepeatedAction => InocodeAdvisorTrigger::RepeatedAction,
        Trigger::Alternation => InocodeAdvisorTrigger::Alternation,
        Trigger::NoProgress => InocodeAdvisorTrigger::NoProgress,
    }
}

fn ts(ms: u64) -> i64 {
    i64::try_from(ms).unwrap_or(i64::MAX)
}

pub(crate) fn to_event(e: AdvisorEvent) -> InocodeAdvisorEvent {
    InocodeAdvisorEvent {
        ts_ms: ts(e.ts_ms),
        thread_id: e.thread_id,
        kind: kind(e.kind),
        trigger: e.trigger.map(trigger),
        signature: e.signature,
        recent_actions: e.recent_actions,
        model: e.model,
        text: e.text,
    }
}

pub(crate) fn to_notification(e: AdvisorEvent) -> InocodeAdvisorEventNotification {
    let e = to_event(e);
    InocodeAdvisorEventNotification {
        ts_ms: e.ts_ms,
        thread_id: e.thread_id,
        kind: e.kind,
        trigger: e.trigger,
        signature: e.signature,
        recent_actions: e.recent_actions,
        model: e.model,
        text: e.text,
    }
}

pub(crate) fn events_in(
    home: &Path,
    params: &InocodeAdvisorEventsParams,
) -> InocodeAdvisorEventsResponse {
    let limit = if params.limit == 0 {
        DEFAULT_LIMIT
    } else {
        params.limit as usize
    };
    let (events, next_cursor) = codex_inocode_advisor::page_events(
        home,
        params.thread_id.as_deref(),
        limit,
        params.cursor.as_deref(),
    );
    InocodeAdvisorEventsResponse {
        events: events.into_iter().map(to_event).collect(),
        next_cursor,
    }
}

/// Forwards advisor events to clients as `inocode/advisor/event`. Call once at startup.
pub(crate) fn spawn_notifier(
    outgoing: std::sync::Arc<crate::outgoing_message::OutgoingMessageSender>,
) {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let mut rx = codex_inocode_advisor::subscribe();
    handle.spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    outgoing
                        .send_server_notification(
                            codex_app_server_protocol::ServerNotification::InocodeAdvisorEvent(
                                to_notification(event),
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
        ClientRequest::InocodeAdvisorEvents { params, .. } => Ok(Some(
            events_in(&codex_inocode_flags::inocode_home(), &params).into(),
        )),
        other => Err(unknown_method(other.method_name())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_stall_advice_and_error_newest_first() {
        let dir = tempfile::tempdir().expect("tempdir");
        for e in [
            AdvisorEvent::stall(
                "t",
                Trigger::SameFailure,
                "sig",
                vec!["shell cargo test -> failed".into()],
            ),
            AdvisorEvent::advice("t", Trigger::SameFailure, Some("m"), "check the lockfile"),
            AdvisorEvent::error("t", Trigger::NoProgress, None, "advisor turn aborted"),
        ] {
            codex_inocode_advisor::append_event(dir.path(), &e).expect("append");
        }
        let res = events_in(
            dir.path(),
            &InocodeAdvisorEventsParams {
                thread_id: Some("t".into()),
                limit: 2,
                cursor: None,
            },
        );
        assert_eq!(res.events.len(), 2);
        assert_eq!(res.events[0].kind, InocodeAdvisorEventKind::Error);
        assert_eq!(res.events[1].text.as_deref(), Some("check the lockfile"));
        assert_eq!(res.next_cursor.as_deref(), Some("2"));
        let rest = events_in(
            dir.path(),
            &InocodeAdvisorEventsParams {
                thread_id: None,
                limit: 2,
                cursor: res.next_cursor,
            },
        );
        assert_eq!(rest.events.len(), 1);
        assert_eq!(
            rest.events[0].trigger,
            Some(InocodeAdvisorTrigger::SameFailure)
        );
        assert_eq!(rest.events[0].recent_actions.as_ref().map(Vec::len), Some(1));
    }
}
