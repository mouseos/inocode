//! `inocode/messages/list` and the `inocode/messages/appended` notification: the verbatim
//! agent-to-agent log of a conversation (`<INOCODE_HOME>/messages/<root thread>.jsonl`), with
//! telegraphic messages broken into kind, confidence and fields.

use super::InocodeResult;
use super::unknown_method;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::InocodeMessage;
use codex_app_server_protocol::InocodeMessageConfidence;
use codex_app_server_protocol::InocodeMessageField;
use codex_app_server_protocol::InocodeMessageKind;
use codex_app_server_protocol::InocodeMessagesAppendedNotification;
use codex_app_server_protocol::InocodeMessagesListParams;
use codex_app_server_protocol::InocodeMessagesListResponse;
use codex_app_server_protocol::InocodeParsedMessage;
use codex_inocode_protocol::Confidence;
use codex_inocode_protocol::Kind;
use codex_inocode_protocol::LoggedMessage;
use std::path::Path;

const DEFAULT_LIMIT: usize = 200;

pub(crate) fn to_message(root_thread_id: &str, m: &LoggedMessage) -> InocodeMessage {
    let parsed = codex_inocode_protocol::parse_view(&m.text).map(|v| InocodeParsedMessage {
        kind: match v.kind {
            Kind::Ask => InocodeMessageKind::Ask,
            Kind::Answer => InocodeMessageKind::Answer,
            Kind::Blocked => InocodeMessageKind::Blocked,
            Kind::Result => InocodeMessageKind::Result,
            Kind::Handoff => InocodeMessageKind::Handoff,
        },
        conf: match v.conf {
            Confidence::High => InocodeMessageConfidence::High,
            Confidence::Med => InocodeMessageConfidence::Med,
            Confidence::Low => InocodeMessageConfidence::Low,
        },
        fields: v
            .fields
            .into_iter()
            .map(|(key, value)| InocodeMessageField { key, value })
            .collect(),
    });
    InocodeMessage {
        ts_ms: i64::try_from(m.ts_ms).unwrap_or(i64::MAX),
        thread_id: root_thread_id.to_string(),
        from: m.from.clone(),
        to: m.to.clone(),
        delivery: m.delivery.clone(),
        text: m.text.clone(),
        parsed,
    }
}

pub(crate) fn list_in(
    home: &Path,
    params: &InocodeMessagesListParams,
) -> InocodeMessagesListResponse {
    let root = codex_inocode_protocol::root_of(&params.thread_id);
    let mut logged = codex_inocode_protocol::read_log(home, &root);
    if root != params.thread_id {
        // A child id was passed: its own log (older versions keyed by sender) is included too.
        logged.extend(codex_inocode_protocol::read_log(home, &params.thread_id));
    }
    if let Some(since) = params.since_ts_ms {
        let since = u64::try_from(since).unwrap_or(0);
        logged.retain(|m| m.ts_ms >= since);
    }
    logged.sort_by_key(|m| m.ts_ms);
    let limit = if params.limit == 0 {
        DEFAULT_LIMIT
    } else {
        params.limit as usize
    };
    let offset = params
        .cursor
        .as_deref()
        .and_then(|c| c.parse::<usize>().ok())
        .unwrap_or(0);
    let messages: Vec<InocodeMessage> = logged
        .iter()
        .skip(offset)
        .take(limit)
        .map(|m| to_message(&params.thread_id, m))
        .collect();
    let next_cursor =
        (offset + messages.len() < logged.len()).then(|| (offset + messages.len()).to_string());
    InocodeMessagesListResponse {
        messages,
        next_cursor,
    }
}

/// Forwards newly logged messages as `inocode/messages/appended`. Call once at startup.
pub(crate) fn spawn_notifier(
    outgoing: std::sync::Arc<crate::outgoing_message::OutgoingMessageSender>,
) {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let mut rx = codex_inocode_protocol::subscribe_messages();
    handle.spawn(async move {
        loop {
            match rx.recv().await {
                Ok((root, message)) => {
                    outgoing
                        .send_server_notification(
                            codex_app_server_protocol::ServerNotification::InocodeMessagesAppended(
                                InocodeMessagesAppendedNotification {
                                    message: to_message(&root, &message),
                                    thread_id: root,
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
        ClientRequest::InocodeMessagesList { params, .. } => Ok(Some(
            list_in(&codex_inocode_flags::inocode_home(), &params).into(),
        )),
        other => Err(unknown_method(other.method_name())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn logged(ts: u64, delivery: &str, text: &str) -> LoggedMessage {
        LoggedMessage {
            ts_ms: ts,
            from: "/root/a".into(),
            to: "/root/b".into(),
            delivery: delivery.into(),
            text: text.into(),
        }
    }

    #[test]
    fn list_keeps_text_verbatim_and_parses_telegraphic_messages() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tree_root = "msg-root-1";
        codex_inocode_protocol::link_thread("msg-child-1", tree_root);
        let ask = "ASK\nq: which crate owns Foo?\nconf: med";
        for (ts, d, t) in [
            (30, "final", "child finished"),
            (10, "spawn", "Port wait.rs"),
            (20, "send_message", ask),
        ] {
            codex_inocode_protocol::log_message(dir.path(), tree_root, &logged(ts, d, t))
                .expect("log");
        }
        let res = list_in(
            dir.path(),
            &InocodeMessagesListParams {
                thread_id: tree_root.into(),
                since_ts_ms: None,
                limit: 10,
                cursor: None,
            },
        );
        let deliveries: Vec<&str> = res.messages.iter().map(|m| m.delivery.as_str()).collect();
        assert_eq!(deliveries, ["spawn", "send_message", "final"]);
        let ask_msg = &res.messages[1];
        assert_eq!(ask_msg.text, ask);
        let parsed = ask_msg.parsed.as_ref().expect("parsed");
        assert_eq!(parsed.kind, InocodeMessageKind::Ask);
        assert_eq!(parsed.conf, InocodeMessageConfidence::Med);
        assert_eq!(parsed.fields[0].key, "q");
        assert!(res.messages[0].parsed.is_none());
        // Asking with a child id reads the root's conversation.
        let via_child = list_in(
            dir.path(),
            &InocodeMessagesListParams {
                thread_id: "msg-child-1".into(),
                since_ts_ms: Some(20),
                limit: 1,
                cursor: None,
            },
        );
        assert_eq!(via_child.messages.len(), 1);
        assert_eq!(via_child.next_cursor.as_deref(), Some("1"));
    }
}
