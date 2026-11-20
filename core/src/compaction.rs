//! Compaction: when a thread nears its model's context window, the older
//! part of its history is replaced in what is sent — never in what is
//! stored. The person's messages stay word for word, a record of the calls
//! made is written by the harness, the most recent messages stay as they
//! are, and only the rest is summarized by the model. The system prompt
//! (the project's rules) and the plan are not part of the history, so they
//! are untouched. Design and sources: docs/evidence/agent-tools.md; cost,
//! oversized histories and the compaction model:
//! docs/evidence/compaction-cost-and-overflow.md; the person's large
//! messages: docs/evidence/large-person-messages.md.

use crate::conversation::{AssistantPart, Body, Conversation, Message, Speaker, ToolSpec};
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

/// Share of the window above which a person's message from an earlier turn,
/// kept before a compaction, is sent shortened with its whole text saved.
const PERSON_MAX_SHARE: f64 = 0.05;
/// Characters of a shortened person's message kept from its start and end.
const PERSON_HEAD_CHARS: usize = 2_000;
const PERSON_TAIL_CHARS: usize = 1_000;

/// Bytes counted as one token when the model's own ratio has not been
/// measured yet: low enough that Japanese text, about three bytes a token,
/// is not underestimated.
pub(crate) const FALLBACK_BYTES_PER_TOKEN: f64 = 3.0;
/// Share of the window the turn's own request may fill when it is sent
/// again to be summarized: compaction starts at [`COMPACT_AT`], and the
/// rest leaves room for the instruction and the summary.
const RESEND_UP_TO: f64 = 0.9;

const INSTRUCTIONS: &str = "The conversation above will be replaced by your summary so that \
the work can go on in a shorter context. ";
const FULL_INSTRUCTIONS: &str = "The earlier part of the conversation above will be replaced \
by your summary so that the work can go on in a shorter context; the most recent messages \
stay as they are. Do not call tools. ";
const SUMMARY_CONTENT: &str = "Write the summary for yourself, to continue exactly where the \
work stands: what the person asked for and the constraints they set; what has been done and \
how it was verified; what remains, in order; decisions and their reasons; and the exact file \
paths, identifiers, commands, values, and error messages that the remaining work needs. If \
the person asked for replies in a language, name it. Say plainly which steps are finished and which are not. Do not restate the project rules or the \
plan; they are kept separately. Answer with the summary only.";

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

/// Shortens, in what is sent (`messages`, with their `seqs`), the person's
/// messages from turns before the one starting at `turn_start` that stand
/// before the latest compaction and exceed [`PERSON_MAX_SHARE`] of
/// `window`: kept word for word, they would hold the history above the
/// compaction threshold however often it is compacted. The start and end
/// stay; `save` stores the whole text and returns where, and a message it
/// cannot save stays whole.
pub(crate) fn shorten_people(
    messages: &mut [Message],
    seqs: &[i64],
    turn_start: i64,
    window: u64,
    mut save: impl FnMut(i64, &str) -> Option<std::path::PathBuf>,
) {
    let Some(summary) = messages
        .iter()
        .rposition(|message| replaced_up_to(message).is_some())
    else {
        return;
    };
    let limit = (PERSON_MAX_SHARE * window as f64 * FALLBACK_BYTES_PER_TOKEN) as usize;
    for (message, &seq) in messages[..summary].iter_mut().zip(seqs) {
        if seq >= turn_start || !is_person(message) {
            continue;
        }
        let Body::User { parts, .. } = &mut message.body else {
            continue;
        };
        let text = parts.join("\n");
        let chars = text.chars().count();
        if text.len() <= limit || chars <= PERSON_HEAD_CHARS + PERSON_TAIL_CHARS {
            continue;
        }
        let Some(path) = save(seq, &text) else {
            continue;
        };
        let head: String = text.chars().take(PERSON_HEAD_CHARS).collect();
        let tail: String = text.chars().skip(chars - PERSON_TAIL_CHARS).collect();
        let omitted = chars - PERSON_HEAD_CHARS - PERSON_TAIL_CHARS;
        *parts = vec![format!(
            "{head}\n…({omitted} characters of this message are left out here. The whole \
             message is saved in {}; read it with the read tool by that path. find does not \
             search there.)…\n{tail}",
            path.display()
        )];
    }
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

/// The size of a request with `system`, `messages` and `tools`, in bytes
/// of what is sent; tokens are estimated from it.
/// Bytes of text an image token is counted as, about what a token of
/// English text takes.
const IMAGE_BYTES_PER_TOKEN: u64 = 4;

pub(crate) fn request_bytes(system: Option<&str>, messages: &[Message], tools: &[ToolSpec]) -> u64 {
    let bodies: usize = messages
        .iter()
        .map(|message| serde_json::to_string(&message.body).map_or(0, |text| text.len()))
        .sum();
    // An image is sent as base64 but costs tokens by its size; it is
    // counted as that many tokens of text.
    let images: u64 = messages
        .iter()
        .filter_map(|message| match &message.body {
            Body::ToolResult { images, .. } => Some(images),
            _ => None,
        })
        .flatten()
        .map(|image| crate::picture::estimated_tokens(image) * IMAGE_BYTES_PER_TOKEN)
        .sum();
    let tools: usize = tools
        .iter()
        .map(|tool| tool.name.len() + tool.description.len() + tool.parameters.to_string().len())
        .sum();
    (system.map_or(0, str::len) + bodies + tools) as u64 + images
}

/// Tokens of `bytes` at `bytes_per_token`.
pub(crate) fn tokens(bytes: u64, bytes_per_token: f64) -> u64 {
    (bytes as f64 / bytes_per_token) as u64
}

/// The older messages as plain text, in parts of at most `budget` bytes,
/// each ending before a message that is not a tool result so a call and
/// its result stay together. A message is cut as `recall` cuts it, except
/// an earlier compaction, whose record and summary stand for everything
/// before it and are kept whole.
fn parts(messages: &[(i64, Message)], budget: u64) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    for (seq, message) in messages {
        let text = match &message.body {
            Body::User { parts, .. } if replaced_up_to(message).is_some() => parts.join("\n"),
            _ => crate::agent_tools::render(message),
        };
        let line = format!("#{seq} {text}\n");
        let splits = !current.is_empty()
            && !matches!(message.body, Body::ToolResult { .. })
            && (current.len() + line.len()) as u64 > budget;
        if splits {
            parts.push(std::mem::take(&mut current));
        }
        current.push_str(&line);
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}

/// The summary text of one request, and its usage.
async fn summarize(
    llm: &Llm,
    conversation: &Conversation,
    tools: &[ToolSpec],
) -> Result<(String, Option<TokenUsage>), ErrorInfo> {
    let mut stream = llm.converse(conversation, tools).await?;
    let mut summary = String::new();
    while let Some(text) = stream.next().await? {
        summary.push_str(&text);
    }
    Ok((summary.trim().to_string(), stream.usage()))
}

fn instruction(text: String) -> Message {
    Message {
        id: "compaction-request".to_string(),
        body: Body::User {
            from: Speaker::Harness("compaction-request".to_string()),
            parts: vec![text],
        },
    }
}

/// The summary of `replaced`, by `summarizer` (the turn's own model `llm`
/// when it is `None`), with every request's usage. With the turn's model
/// and a history that fits, the request repeats the turn's own (system,
/// tools, every message) so its cached prefix is read; else only the
/// replaced part goes, in parts when it exceeds the model's window.
#[allow(clippy::too_many_arguments)]
async fn summary(
    llm: &Llm,
    summarizer: Option<&Llm>,
    system: Option<String>,
    messages: &[(i64, Message)],
    replaced: &[(i64, Message)],
    tools: &[ToolSpec],
    input: u64,
    window: u64,
) -> (Result<String, ErrorInfo>, Vec<TokenUsage>) {
    let mut usages = Vec::new();
    let model = summarizer.unwrap_or(llm);
    let model_window = match summarizer {
        Some(summarizer) => summarizer.context_window().await.unwrap_or(window),
        None => window,
    };
    let budget = (COMPACT_AT * model_window as f64) as u64;
    let replaced_messages: Vec<Message> = replaced
        .iter()
        .map(|(_, message)| message.clone())
        .collect();
    // The request's tokens shared out by bytes, as for the kept tail.
    let all: Vec<Message> = messages
        .iter()
        .map(|(_, message)| message.clone())
        .collect();
    let bytes_per_token =
        request_bytes(system.as_deref(), &all, tools).max(1) as f64 / input.max(1) as f64;
    let replaced_tokens = tokens(
        request_bytes(system.as_deref(), &replaced_messages, &[]),
        bytes_per_token,
    );
    let resend = summarizer.is_none() && input as f64 <= RESEND_UP_TO * window as f64;
    let single = if resend {
        let mut request: Vec<Message> = messages
            .iter()
            .map(|(_, message)| message.clone())
            .collect();
        request.push(instruction(format!("{FULL_INSTRUCTIONS}{SUMMARY_CONTENT}")));
        Some((request, tools))
    } else if replaced_tokens <= budget {
        let mut request = replaced_messages;
        request.push(instruction(format!("{INSTRUCTIONS}{SUMMARY_CONTENT}")));
        Some((request, &[][..]))
    } else {
        None
    };
    if let Some((request, tools)) = single {
        let conversation = Conversation {
            system,
            messages: request,
        };
        let answer = summarize(model, &conversation, tools).await;
        return match answer {
            Ok((text, usage)) => {
                usages.extend(usage);
                (Ok(text), usages)
            }
            Err(error) => (Err(error), usages),
        };
    }
    let fixed = tokens(request_bytes(system.as_deref(), &[], &[]), bytes_per_token);
    let part_budget = (budget.saturating_sub(fixed) / 2).max(1_000);
    let parts = parts(replaced, (part_budget as f64 * bytes_per_token) as u64);
    let count = parts.len();
    let mut summary = String::from("(none yet)");
    for (index, part) in parts.into_iter().enumerate() {
        let text = format!(
            "The conversation so far is too long to read at once, so it comes in {count} \
             parts and you keep one summary up to date. Summary of the parts before this \
             one:\n{summary}\n\nPart {} of {count}:\n{part}\nRewrite the summary so that it \
             also covers this part, keeping every detail from the earlier summary that the \
             remaining work needs. {SUMMARY_CONTENT}",
            index + 1
        );
        let conversation = Conversation {
            system: system.clone(),
            messages: vec![instruction(text)],
        };
        match summarize(model, &conversation, &[]).await {
            Ok((text, usage)) => {
                usages.extend(usage);
                summary = text;
            }
            Err(error) => return (Err(error), usages),
        }
    }
    (Ok(summary), usages)
}

/// A compaction of `messages` (what is sent now, in order) after a request
/// with `tools` that used `input` of `window` tokens: the message to store,
/// and the usage of each summary request. `None` when nothing can be
/// replaced. When no summary can be written, the call record stands in for
/// it and the work goes on.
pub(crate) async fn compact(
    llm: &Llm,
    summarizer: Option<&Llm>,
    system: Option<String>,
    messages: &[(i64, Message)],
    tools: &[ToolSpec],
    input: u64,
    window: u64,
) -> Option<(Body, Vec<TokenUsage>)> {
    let start = cut(messages, input, window)?;
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
    let (written, usages) = summary(
        llm, summarizer, system, messages, replaced, tools, input, window,
    )
    .await;
    let summary = match written {
        Ok(text) if !text.is_empty() => format!(
            "Summary of earlier work (messages up to #{up_to} are summarized here; recall \
             with from_seq reads them word for word):\n{text}"
        ),
        Ok(_) => format!(
            "No summary of earlier work could be written (the answer was empty); the calls \
             above are recorded, and recall with from_seq reads messages up to #{up_to} word \
             for word."
        ),
        Err(error) => format!(
            "No summary of earlier work could be written ({}); the calls above are \
             recorded, and recall with from_seq reads messages up to #{up_to} word for word.",
            error.message
        ),
    };
    Some((
        Body::User {
            from: Speaker::Harness(format!("{KIND}{up_to}")),
            parts: vec![record, summary],
        },
        usages,
    ))
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
                images: Vec::new(),
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

    #[test]
    fn an_unmeasured_request_is_estimated_from_its_size() {
        let messages = vec![person(&"あ".repeat(300))];
        let bytes = serde_json::to_string(&messages[0].body).unwrap().len() as u64;
        assert_eq!(request_bytes(Some("abc"), &messages, &[]), bytes + 3);
        // Japanese is about three bytes a token: not underestimated.
        let fallback = tokens(
            request_bytes(None, &messages, &[]),
            FALLBACK_BYTES_PER_TOKEN,
        );
        assert!(fallback >= 300);
        assert_eq!(tokens(4_000, 4.0), 1_000);
    }

    #[test]
    fn parts_keep_a_call_with_its_result() {
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
        // A budget smaller than any message: every part still holds whole calls.
        let parts = parts(&messages, 1);
        assert_eq!(parts.len(), 3);
        assert!(
            parts[1].starts_with("#2 ") && parts[1].contains("#3 "),
            "{}",
            parts[1]
        );
        assert!(
            parts[2].starts_with("#4 ") && parts[2].contains("#5 "),
            "{}",
            parts[2]
        );
    }

    #[test]
    fn parts_keep_an_earlier_summary_whole() {
        let long = "s".repeat(5_000);
        let earlier = Message {
            id: String::new(),
            body: Body::User {
                from: Speaker::Harness(format!("{KIND}1")),
                parts: vec!["record".into(), long.clone()],
            },
        };
        let messages = vec![(2, earlier), (3, person(&"p".repeat(5_000)))];
        let parts = parts(&messages, u64::MAX);
        assert!(parts[0].contains(&long));
        // Other messages are cut as recall cuts them.
        assert!(!parts[0].contains(&"p".repeat(5_000)));
    }

    #[test]
    fn large_earlier_person_messages_are_sent_shortened_and_saved() {
        let summary = Message {
            id: String::new(),
            body: Body::User {
                from: Speaker::Harness(format!("{KIND}4")),
                parts: vec!["record".into(), "summary".into()],
            },
        };
        let big = format!("START{}END", "x".repeat(20_000));
        let mut messages = vec![
            person(&big),
            person("small"),
            person(&big),
            summary,
            person(&big),
        ];
        let seqs = [1, 2, 6, 7, 8];
        let mut saved = Vec::new();
        // Window 100,000: the limit is 5% of it at three bytes a token.
        shorten_people(&mut messages, &seqs, 6, 100_000, |seq, text| {
            saved.push((seq, text.to_string()));
            Some(std::path::PathBuf::from(format!("saved-{seq}.txt")))
        });
        assert_eq!(saved, [(1, big.clone())]);
        let Body::User { parts, .. } = &messages[0].body else {
            panic!("a person's message");
        };
        assert!(parts[0].starts_with("STARTxx"));
        assert!(parts[0].ends_with("xxEND"));
        assert!(parts[0].contains("saved in saved-1.txt"));
        assert!(parts[0].chars().count() < 3_300);
        // Small messages, this turn's messages and those after the summary stay.
        for index in [1, 2, 4] {
            let Body::User { parts, .. } = &messages[index].body else {
                panic!("a person's message");
            };
            assert!(parts[0] == "small" || parts[0] == big);
        }

        // A message that cannot be saved stays whole.
        let mut messages = vec![
            person(&big),
            Message {
                id: String::new(),
                body: Body::User {
                    from: Speaker::Harness(format!("{KIND}1")),
                    parts: vec![],
                },
            },
        ];
        shorten_people(&mut messages, &[1, 2], 3, 100_000, |_, _| None);
        let Body::User { parts, .. } = &messages[0].body else {
            panic!("a person's message");
        };
        assert_eq!(parts[0], big);
    }
}
