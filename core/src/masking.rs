//! Observation masking: in what is sent, the output of tool calls made
//! before the most recent turns is replaced by a one-line note. What is
//! stored stays whole. Design and sources: docs/evidence/observation-masking.md.

use crate::conversation::{Body, Message};

/// Assistant turns whose tool outputs are sent whole (The Complexity Trap
/// kept the last 10 turns).
const KEEP_TURNS: usize = 10;

/// `messages` with the results of tool calls from assistant turns before the
/// last [`KEEP_TURNS`] replaced by a note of how many lines they had.
pub(crate) fn mask_old_results(messages: &[Message]) -> Vec<Message> {
    let assistants = messages
        .iter()
        .filter(|message| matches!(message.body, Body::Assistant { .. }))
        .count();
    let masked = assistants.saturating_sub(KEEP_TURNS);
    let mut out = Vec::with_capacity(messages.len());
    let mut seen = 0usize;
    for message in messages {
        let mut message = message.clone();
        match &mut message.body {
            Body::Assistant { .. } => seen += 1,
            Body::ToolResult { content, .. } if seen <= masked && seen > 0 => {
                let lines = content.lines().count();
                *content = format!("Old tool output omitted ({lines} lines).");
            }
            _ => {}
        }
        out.push(message);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation::{AssistantPart, Origin, Speaker, StopReason};
    use crate::ProviderKind;

    fn call(n: usize) -> Vec<Message> {
        vec![
            Message {
                id: format!("a{n}"),
                body: Body::Assistant {
                    origin: Origin {
                        format: ProviderKind::Anthropic,
                        model: "m".into(),
                    },
                    stop: StopReason::ToolUse,
                    parts: vec![AssistantPart::ToolCall {
                        call_id: format!("c{n}"),
                        name: "read".into(),
                        arguments: "{}".into(),
                        replay: None,
                    }],
                },
            },
            Message {
                id: format!("r{n}"),
                body: Body::ToolResult {
                    call_id: format!("c{n}"),
                    name: "read".into(),
                    content: format!("line one {n}\nline two"),
                    is_error: false,
                },
            },
        ]
    }

    fn result(message: &Message) -> &str {
        match &message.body {
            Body::ToolResult { content, .. } => content,
            _ => panic!("not a result"),
        }
    }

    #[test]
    fn only_results_before_the_last_ten_turns_are_replaced() {
        let mut messages = vec![Message {
            id: "p".into(),
            body: Body::User {
                from: Speaker::Person,
                parts: vec!["task".into()],
            },
        }];
        for n in 0..10 {
            messages.extend(call(n));
        }
        assert_eq!(mask_old_results(&messages), messages);

        messages.extend(call(10));
        messages.extend(call(11));
        let sent = mask_old_results(&messages);
        assert_eq!(result(&sent[2]), "Old tool output omitted (2 lines).");
        assert_eq!(result(&sent[4]), "Old tool output omitted (2 lines).");
        assert_eq!(result(&sent[6]), "line one 2\nline two");
        assert_eq!(sent[0], messages[0]);
        assert_eq!(sent.len(), messages.len());
    }
}
