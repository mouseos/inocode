//! Compaction: when a thread nears its model's context window, the older
//! part of its history is replaced in what is sent — never in what is
//! stored. The person's messages stay word for word, a record of the calls
//! made is written by the harness, the most recent messages stay as they
//! are, and only the rest is summarized by the model. The system prompt
//! (the project's rules) and the plan are not part of the history, so they
//! are untouched. Design and sources: docs/evidence/agent-tools.md.

use crate::conversation::{AssistantPart, Body, Conversation, Message, Speaker};
use crate::store::StoredMessage;
use crate::{ErrorInfo, Llm, TokenUsage};

/// Share of the context window at which a thread is compacted (DSH).
pub(crate) const COMPACT_AT: f64 = 0.8;
/// Share of the context window kept word for word at the end (DSH).
const KEEP_RECENT: f64 = 0.16;
/// Characters of the call record; the oldest lines go first.
const RECORD_MAX_CHARS: usize = 8_000;
/// Characters of one call's arguments or result in the record.
const RECORD_FIELD_CHARS: usize = 160;
/// Speaker kind of a compaction message: `compaction:<last replaced seq>`.
const KIND: &str = "compaction:";

const INSTRUCTIONS: &str = "The conversation above will be replaced by your summary so that \
the work can go on in a shorter context. Write the summary for yourself, to continue exactly \
where the work stands: what the person asked for and the constraints they set; what has been \
done and how it was verified; what remains, in order; decisions and their reasons; and the \
exact file paths, identifiers, commands, values, and error messages that the remaining work \
needs. Say plainly which steps are finished and which are not. Do not restate the project \
rules or the plan; they are kept separately. Answer with the summary only.";

/// The seq up to which a compaction message replaced the history.
fn replaced_up_to(message: &Message) -> Option<i64> {
    match &message.body {
        Body::User {
            from: Speaker::Harness(kind),
            ..
        } => kind.strip_prefix(KIND)?.parse().ok(),
        _ => None,
    }
}

fn is_person(message: &Message) -> bool {
    matches!(
        message.body,
        Body::User {
            from: Speaker::Person,
            ..
        }
    )
}

/// What is sent for a stored history: everything, or — after a
/// compaction — the person's earlier messages, the compaction message, and
/// the messages after the replaced part. Each message keeps its seq.
pub(crate) fn assemble(stored: Vec<StoredMessage>) -> Vec<(i64, Message)> {
    let latest = stored
        .iter()
        .rposition(|stored| replaced_up_to(&stored.message).is_some());
    let Some(latest) = latest else {
        return stored
            .into_iter()
            .map(|stored| (stored.seq, stored.message))
            .collect();
    };
    let up_to = replaced_up_to(&stored[latest].message).unwrap_or(0);
    let summary_seq = stored[latest].seq;
    let mut head = Vec::new();
    let mut tail = Vec::new();
    let mut summary = None;
    for item in stored {
        if item.seq == summary_seq {
            summary = Some((item.seq, item.message));
        } else if replaced_up_to(&item.message).is_some() {
            // An earlier compaction, itself replaced.
        } else if item.seq > up_to {
            tail.push((item.seq, item.message));
        } else if is_person(&item.message) {
            head.push((item.seq, item.message));
        }
    }
    head.extend(summary);
    head.extend(tail);
    head
}

/// Whether a request that used `input` tokens calls for compaction.
pub(crate) fn due(input: u64, window: u64) -> bool {
    input as f64 >= COMPACT_AT * window as f64
}

/// Where the kept tail starts: the earliest message such that the messages
/// from it on fit the recent budget, estimated by sharing the measured
/// input tokens out by characters. Never starts at a tool result, whose
/// call must come with it. `None` when nothing before it could be replaced.
fn cut(messages: &[(i64, Message)], input: u64, window: u64) -> Option<usize> {
    let sizes: Vec<usize> = messages
        .iter()
        .map(|(_, message)| serde_json::to_string(&message.body).map_or(0, |text| text.len()))
        .collect();
    let total: usize = sizes.iter().sum::<usize>().max(1);
    let budget = KEEP_RECENT * window as f64;
    let mut used = 0.0;
    let mut start = messages.len();
    while start > 0 {
        let tokens = sizes[start - 1] as f64 * input as f64 / total as f64;
        if used + tokens > budget {
            break;
        }
        used += tokens;
        start -= 1;
    }
    while start > 0
        && start < messages.len()
        && matches!(messages[start].1.body, Body::ToolResult { .. })
    {
        start -= 1;
    }
    let previous = messages
        .iter()
        .rposition(|(_, message)| replaced_up_to(message).is_some());
    let replaceable = messages[..start]
        .iter()
        .enumerate()
        .any(|(index, (_, message))| {
            !is_person(message) && previous.is_none_or(|previous| index > previous)
        });
    replaceable.then_some(start)
}

fn clip(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default();
    match line.char_indices().nth(RECORD_FIELD_CHARS) {
        Some((end, _)) => format!("{}…", &line[..end]),
        None => line.to_string(),
    }
}

/// One line per call: what was called, with what, and how it ended.
fn record_lines(messages: &[(i64, Message)]) -> Vec<String> {
    let mut lines = Vec::new();
    for (index, (seq, message)) in messages.iter().enumerate() {
        let Body::Assistant { parts, .. } = &message.body else {
            continue;
        };
        for part in parts {
            let AssistantPart::ToolCall {
                call_id,
                name,
                arguments,
                ..
            } = part
            else {
                continue;
            };
            let outcome = messages[index + 1..]
                .iter()
                .find_map(|(_, later)| match &later.body {
                    Body::ToolResult {
                        call_id: answered,
                        content,
                        is_error,
                        ..
                    } if answered == call_id => Some(format!(
                        "{}: {}",
                        if *is_error { "error" } else { "ok" },
                        clip(content)
                    )),
                    _ => None,
                })
                .unwrap_or_else(|| "no result".to_string());
            lines.push(format!("#{seq} {name} {} -> {outcome}", clip(arguments)));
        }
    }
    lines
}

/// The record so far: the previous compaction's record plus the new
/// lines, the oldest dropped first when it grows too long.
fn record(previous: Option<&str>, new: Vec<String>) -> String {
    let mut lines: Vec<String> = previous
        .map(|text| text.lines().skip(1).map(str::to_string).collect())
        .unwrap_or_default();
    lines.retain(|line| !line.starts_with('('));
    lines.extend(new);
    let mut kept = Vec::new();
    let mut size = 0;
    for line in lines.iter().rev() {
        if size + line.len() + 1 > RECORD_MAX_CHARS {
            kept.push("(older calls omitted; recall reads them)".to_string());
            break;
        }
        size += line.len() + 1;
        kept.push(line.clone());
    }
    kept.reverse();
    format!(
        "Calls made so far (recorded by the harness):\n{}",
        kept.join("\n")
    )
}

/// A compaction of `messages` (what is sent now, in order) after a request
/// that used `input` of `window` tokens: the message to store, and the
/// tokens the summary request used. `None` when nothing can be replaced.
pub(crate) async fn compact(
    llm: &Llm,
    system: Option<String>,
    messages: &[(i64, Message)],
    input: u64,
    window: u64,
) -> Result<Option<(Body, Option<TokenUsage>)>, ErrorInfo> {
    let Some(start) = cut(messages, input, window) else {
        return Ok(None);
    };
    let replaced = &messages[..start];
    let up_to = replaced
        .iter()
        .filter(|(_, message)| !is_person(message))
        .map(|(seq, _)| *seq)
        .max()
        .unwrap_or(0);
    let previous_record = replaced
        .iter()
        .find_map(|(_, message)| match &message.body {
            Body::User {
                from: Speaker::Harness(kind),
                parts,
            } if kind.starts_with(KIND) => parts.first().map(String::as_str),
            _ => None,
        });
    let previous = replaced
        .iter()
        .rposition(|(_, message)| replaced_up_to(message).is_some());
    let fresh = &replaced[previous.map_or(0, |index| index + 1)..];
    let record = record(previous_record, record_lines(fresh));

    let mut request: Vec<Message> = replaced
        .iter()
        .map(|(_, message)| message.clone())
        .collect();
    request.push(Message {
        id: "compaction-request".to_string(),
        body: Body::User {
            from: Speaker::Harness("compaction-request".to_string()),
            parts: vec![INSTRUCTIONS.to_string()],
        },
    });
    let mut stream = llm
        .converse(
            &Conversation {
                system,
                messages: request,
            },
            &[],
        )
        .await?;
    let mut summary = String::new();
    while let Some(text) = stream.next().await? {
        summary.push_str(&text);
    }
    let summary = format!(
        "Summary of earlier work (messages up to #{up_to} are summarized here; recall with \
         from_seq reads them word for word):\n{}",
        summary.trim()
    );
    Ok(Some((
        Body::User {
            from: Speaker::Harness(format!("{KIND}{up_to}")),
            parts: vec![record, summary],
        },
        stream.usage(),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation::{Origin, StopReason};
    use crate::ProviderKind;

    fn person(text: &str) -> Message {
        Message {
            id: String::new(),
            body: Body::User {
                from: Speaker::Person,
                parts: vec![text.to_string()],
            },
        }
    }

    fn call(id: &str) -> Message {
        Message {
            id: String::new(),
            body: Body::Assistant {
                origin: Origin {
                    format: ProviderKind::OpenAi,
                    model: "m".into(),
                },
                stop: StopReason::ToolUse,
                parts: vec![AssistantPart::ToolCall {
                    call_id: id.into(),
                    name: "plan".into(),
                    arguments: "{\"items\":[]}".into(),
                    replay: None,
                }],
            },
        }
    }

    fn result(id: &str) -> Message {
        Message {
            id: String::new(),
            body: Body::ToolResult {
                call_id: id.into(),
                name: "plan".into(),
                content: "Plan updated.".into(),
                is_error: false,
            },
        }
    }

    fn stored(messages: Vec<Message>) -> Vec<StoredMessage> {
        messages
            .into_iter()
            .enumerate()
            .map(|(index, message)| StoredMessage {
                seq: index as i64 + 1,
                created: 0,
                message,
            })
            .collect()
    }

    #[test]
    fn after_a_compaction_the_person_summary_and_tail_are_sent() {
        let summary = Message {
            id: String::new(),
            body: Body::User {
                from: Speaker::Harness("compaction:3".into()),
                parts: vec!["record".into(), "summary".into()],
            },
        };
        let sent = assemble(stored(vec![
            person("first"),
            call("a"),
            result("a"),
            person("second"),
            call("b"),
            result("b"),
            summary,
        ]));
        let seqs: Vec<i64> = sent.iter().map(|(seq, _)| *seq).collect();
        assert_eq!(seqs, [1, 7, 4, 5, 6]);
    }

    #[test]
    fn the_cut_never_separates_a_call_from_its_result() {
        let messages: Vec<(i64, Message)> = stored(vec![
            person("go"),
            call("a"),
            result("a"),
            call("b"),
            result("b"),
        ])
        .into_iter()
        .map(|stored| (stored.seq, stored.message))
        .collect();
        // A budget that fits only the last message moves back to its call.
        let start = cut(&messages, 1000, 1000).expect("something to replace");
        assert!(matches!(messages[start].1.body, Body::Assistant { .. }));
        assert_eq!(start, 3);
        // Only the person's message before the cut: nothing to replace.
        assert_eq!(cut(&messages[..1], 10, 1_000_000), None);
    }

    #[test]
    fn the_record_keeps_the_newest_calls() {
        let messages: Vec<(i64, Message)> = stored(vec![call("a"), result("a")])
            .into_iter()
            .map(|stored| (stored.seq, stored.message))
            .collect();
        let lines = record_lines(&messages);
        assert_eq!(lines, ["#1 plan {\"items\":[]} -> ok: Plan updated."]);
        let text = record(
            Some("Calls made so far (recorded by the harness):\n#0 old"),
            lines,
        );
        assert!(text.ends_with("#0 old\n#1 plan {\"items\":[]} -> ok: Plan updated."));
        let many: Vec<String> = (0..2000).map(|index| format!("#{index} call")).collect();
        let text = record(None, many);
        assert!(text.len() <= RECORD_MAX_CHARS + 100);
        assert!(text.contains("(older calls omitted"));
        assert!(text.ends_with("#1999 call"));
    }
}
