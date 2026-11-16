//! A provider-neutral conversation and its conversion to the three wire
//! formats. Design and sources: docs/evidence/conversation-model.md.

use std::collections::HashSet;

use serde_json::{json, Map, Value};

use crate::{ErrorInfo, ProviderKind};

/// System instructions plus the turns of one conversation.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Conversation {
    pub system: Option<String>,
    pub messages: Vec<Message>,
}

/// One turn. `id` is unique within its conversation; it is never sent.
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub id: String,
    pub body: Body,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Body {
    User {
        from: Speaker,
        parts: Vec<String>,
    },
    Assistant {
        origin: Origin,
        stop: StopReason,
        /// In the order the provider produced them.
        parts: Vec<AssistantPart>,
    },
    /// The answer to one tool call.
    ToolResult {
        call_id: String,
        name: String,
        content: String,
        is_error: bool,
    },
}

/// Who wrote a user turn: the person, or the harness (continuation nudges,
/// re-checks, loop warnings), named by kind so the harness can count them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Speaker {
    Person,
    Harness(String),
}

/// The format and model that produced an assistant turn. Replay data is only
/// valid for this pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    pub format: ProviderKind,
    pub model: String,
}

/// Why an assistant turn ended. `Error` and `Aborted` turns are never sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    Completed,
    Length,
    ToolUse,
    Error,
    Aborted,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AssistantPart {
    Text {
        text: String,
    },
    /// Readable reasoning (possibly empty) and the opaque value needed to
    /// return it to the model that produced it.
    Reasoning {
        text: String,
        replay: Option<Replay>,
    },
    ToolCall {
        call_id: String,
        name: String,
        /// The JSON text the model produced, unparsed.
        arguments: String,
        replay: Option<Replay>,
    },
}

/// A provider value returned untouched to the origin format and model:
/// - Anthropic: the `thinking` / `redacted_thinking` content block;
/// - Responses: the `reasoning` (or `function_call`) input item;
/// - Chat Completions: for reasoning, an object whose fields are added to the
///   assistant message (e.g. `{"reasoning_content": …}`); for a tool call,
///   the `tool_calls` entry.
#[derive(Debug, Clone, PartialEq)]
pub struct Replay(pub Value);

/// `messages` (with the system turn first) for a Chat Completions body.
pub fn to_chat_completions(
    conversation: &Conversation,
    model: &str,
) -> Result<Map<String, Value>, ErrorInfo> {
    let target = Target::new(ProviderKind::OpenAi, model);
    let turns = prepare(conversation)?;
    let mut messages = Vec::new();
    if let Some(system) = &conversation.system {
        messages.push(json!({ "role": "system", "content": system }));
    }
    for body in turns {
        match body {
            Body::User { parts, .. } => {
                messages.push(json!({ "role": "user", "content": parts.join("\n\n") }));
            }
            Body::Assistant { origin, parts, .. } => {
                let replays = target.matches(origin);
                let mut message = Map::new();
                message.insert("role".into(), json!("assistant"));
                let mut texts = Vec::new();
                let mut calls = Vec::new();
                for part in parts {
                    match part {
                        AssistantPart::Text { text } if !text.is_empty() => {
                            texts.push(text.as_str())
                        }
                        AssistantPart::Text { .. } => {}
                        AssistantPart::Reasoning { replay, .. } => {
                            if let (true, Some(Replay(Value::Object(fields)))) = (replays, replay) {
                                message.extend(fields.clone());
                            }
                        }
                        AssistantPart::ToolCall {
                            call_id,
                            name,
                            arguments,
                            replay,
                        } => calls.push(match replay {
                            Some(Replay(value)) if replays => value.clone(),
                            _ => json!({
                                "id": call_id,
                                "type": "function",
                                "function": { "name": name, "arguments": arguments },
                            }),
                        }),
                    }
                }
                if texts.is_empty() && calls.is_empty() {
                    continue;
                }
                if !texts.is_empty() {
                    message.insert("content".into(), json!(texts.join("\n\n")));
                }
                if !calls.is_empty() {
                    message.insert("tool_calls".into(), Value::Array(calls));
                }
                messages.push(Value::Object(message));
            }
            Body::ToolResult {
                call_id, content, ..
            } => messages.push(json!({
                "role": "tool",
                "tool_call_id": call_id,
                "content": content,
            })),
        }
    }
    let mut fields = Map::new();
    fields.insert("messages".into(), Value::Array(messages));
    Ok(fields)
}

/// `system` and `messages` for an Anthropic Messages body. Tool results and
/// the user text after them share one user message, results first.
pub fn to_anthropic(
    conversation: &Conversation,
    model: &str,
) -> Result<Map<String, Value>, ErrorInfo> {
    let target = Target::new(ProviderKind::Anthropic, model);
    let turns = prepare(conversation)?;
    let mut messages: Vec<(&'static str, Vec<Value>)> = Vec::new();
    for body in turns {
        let (role, blocks) = match body {
            Body::User { parts, .. } => (
                "user",
                parts
                    .iter()
                    .map(|text| json!({ "type": "text", "text": text }))
                    .collect(),
            ),
            Body::Assistant { origin, parts, .. } => {
                let replays = target.matches(origin);
                let mut blocks = Vec::new();
                for part in parts {
                    match part {
                        AssistantPart::Text { text } if !text.is_empty() => {
                            blocks.push(json!({ "type": "text", "text": text }));
                        }
                        AssistantPart::Text { .. } => {}
                        AssistantPart::Reasoning { replay, .. } => {
                            if let (true, Some(Replay(value))) = (replays, replay) {
                                blocks.push(value.clone());
                            }
                        }
                        AssistantPart::ToolCall {
                            call_id,
                            name,
                            arguments,
                            ..
                        } => blocks.push(json!({
                            "type": "tool_use",
                            "id": call_id,
                            "name": name,
                            "input": arguments_object(call_id, arguments)?,
                        })),
                    }
                }
                ("assistant", blocks)
            }
            Body::ToolResult {
                call_id,
                content,
                is_error,
                ..
            } => {
                let mut block = json!({
                    "type": "tool_result",
                    "tool_use_id": call_id,
                    "content": content,
                });
                if *is_error {
                    block["is_error"] = json!(true);
                }
                ("user", vec![block])
            }
        };
        if blocks.is_empty() {
            continue;
        }
        match messages.last_mut() {
            Some((last, content)) if *last == role => content.extend(blocks),
            _ => messages.push((role, blocks)),
        }
    }
    let mut fields = Map::new();
    if let Some(system) = &conversation.system {
        fields.insert("system".into(), json!(system));
    }
    fields.insert(
        "messages".into(),
        Value::Array(
            messages
                .into_iter()
                .map(|(role, content)| json!({ "role": role, "content": content }))
                .collect(),
        ),
    );
    Ok(fields)
}

/// `instructions` and `input` for a Responses body.
pub fn to_responses(
    conversation: &Conversation,
    model: &str,
) -> Result<Map<String, Value>, ErrorInfo> {
    let target = Target::new(ProviderKind::Responses, model);
    let turns = prepare(conversation)?;
    let mut input = Vec::new();
    for body in turns {
        match body {
            Body::User { parts, .. } => input.push(json!({
                "type": "message",
                "role": "user",
                "content": parts
                    .iter()
                    .map(|text| json!({ "type": "input_text", "text": text }))
                    .collect::<Vec<_>>(),
            })),
            Body::Assistant { origin, parts, .. } => {
                let replays = target.matches(origin);
                for part in parts {
                    match part {
                        AssistantPart::Text { text } if !text.is_empty() => input.push(json!({
                            "type": "message",
                            "role": "assistant",
                            "content": [{ "type": "output_text", "text": text }],
                        })),
                        AssistantPart::Text { .. } => {}
                        AssistantPart::Reasoning { replay, .. } => {
                            if let (true, Some(Replay(value))) = (replays, replay) {
                                input.push(value.clone());
                            }
                        }
                        AssistantPart::ToolCall {
                            call_id,
                            name,
                            arguments,
                            replay,
                        } => input.push(match replay {
                            Some(Replay(value)) if replays => value.clone(),
                            _ => json!({
                                "type": "function_call",
                                "call_id": call_id,
                                "name": name,
                                "arguments": arguments,
                            }),
                        }),
                    }
                }
            }
            Body::ToolResult {
                call_id, content, ..
            } => input.push(json!({
                "type": "function_call_output",
                "call_id": call_id,
                "output": content,
            })),
        }
    }
    let mut fields = Map::new();
    if let Some(system) = &conversation.system {
        fields.insert("instructions".into(), json!(system));
    }
    fields.insert("input".into(), Value::Array(input));
    Ok(fields)
}

/// The format and model a conversion sends to.
struct Target<'a> {
    format: ProviderKind,
    model: &'a str,
}

impl<'a> Target<'a> {
    fn new(format: ProviderKind, model: &'a str) -> Self {
        Self { format, model }
    }

    /// Replay data and reasoning go only back to the model that made them.
    fn matches(&self, origin: &Origin) -> bool {
        origin.format == self.format && origin.model == self.model
    }
}

/// The turns to send: assistant turns that ended in an error or abort are
/// dropped with any results answering their calls, then every call must be
/// answered by the results directly after it, and every result must answer
/// such a call.
fn prepare(conversation: &Conversation) -> Result<Vec<&Body>, ErrorInfo> {
    let mut dropped_calls = HashSet::new();
    let mut turns = Vec::new();
    for message in &conversation.messages {
        match &message.body {
            Body::Assistant {
                stop: StopReason::Error | StopReason::Aborted,
                parts,
                ..
            } => dropped_calls.extend(parts.iter().filter_map(call_id)),
            Body::ToolResult { call_id, .. } if dropped_calls.contains(call_id.as_str()) => {}
            body => turns.push(body),
        }
    }

    let mut seen = HashSet::new();
    let mut open: Vec<&str> = Vec::new();
    for body in &turns {
        match body {
            Body::ToolResult { call_id, .. } => {
                let Some(index) = open.iter().position(|open| open == call_id) else {
                    return Err(invalid(format!(
                        "tool result {call_id} does not directly follow its call"
                    )));
                };
                open.swap_remove(index);
            }
            _ if !open.is_empty() => {
                return Err(invalid(format!("tool call {} has no result", open[0])));
            }
            Body::Assistant { parts, .. } => {
                for part in parts {
                    if let AssistantPart::ToolCall { call_id, name, .. } = part {
                        if !valid_tool_name(name) {
                            return Err(invalid(format!(
                                "tool name {name:?} must match ^[A-Za-z0-9_-]{{1,64}}$"
                            )));
                        }
                        if !seen.insert(call_id.as_str()) {
                            return Err(invalid(format!("tool call id {call_id} is repeated")));
                        }
                        open.push(call_id);
                    }
                }
            }
            Body::User { .. } => {}
        }
    }
    if let Some(call_id) = open.first() {
        return Err(invalid(format!("tool call {call_id} has no result")));
    }
    Ok(turns)
}

fn call_id(part: &AssistantPart) -> Option<&str> {
    match part {
        AssistantPart::ToolCall { call_id, .. } => Some(call_id),
        _ => None,
    }
}

/// The name rule every one of the three formats accepts.
fn valid_tool_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

/// Anthropic takes the arguments as an object; a model that streamed none
/// sent the empty string.
fn arguments_object(call_id: &str, arguments: &str) -> Result<Value, ErrorInfo> {
    if arguments.trim().is_empty() {
        return Ok(json!({}));
    }
    match serde_json::from_str::<Value>(arguments) {
        Ok(value @ Value::Object(_)) => Ok(value),
        _ => Err(invalid(format!(
            "the arguments of tool call {call_id} are not a JSON object"
        ))),
    }
}

fn invalid(message: String) -> ErrorInfo {
    ErrorInfo::new("invalid_conversation", message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(id: &str, body: Body) -> Message {
        Message {
            id: id.to_string(),
            body,
        }
    }

    fn user(id: &str, text: &str) -> Message {
        message(
            id,
            Body::User {
                from: Speaker::Person,
                parts: vec![text.to_string()],
            },
        )
    }

    fn assistant(
        id: &str,
        format: ProviderKind,
        stop: StopReason,
        parts: Vec<AssistantPart>,
    ) -> Message {
        message(
            id,
            Body::Assistant {
                origin: Origin {
                    format,
                    model: "m1".to_string(),
                },
                stop,
                parts,
            },
        )
    }

    fn text(text: &str) -> AssistantPart {
        AssistantPart::Text {
            text: text.to_string(),
        }
    }

    fn call(call_id: &str, arguments: &str) -> AssistantPart {
        AssistantPart::ToolCall {
            call_id: call_id.to_string(),
            name: "read_file".to_string(),
            arguments: arguments.to_string(),
            replay: None,
        }
    }

    fn result(id: &str, call_id: &str, content: &str, is_error: bool) -> Message {
        message(
            id,
            Body::ToolResult {
                call_id: call_id.to_string(),
                name: "read_file".to_string(),
                content: content.to_string(),
                is_error,
            },
        )
    }

    fn reasoning(replay: Value) -> AssistantPart {
        AssistantPart::Reasoning {
            text: "think".to_string(),
            replay: Some(Replay(replay)),
        }
    }

    /// A user turn, an assistant turn with reasoning, text and two calls,
    /// their results (one failed, answered out of order), and a follow-up
    /// user turn.
    fn tool_round(format: ProviderKind, reasoning_replay: Value) -> Conversation {
        Conversation {
            system: Some("be brief".to_string()),
            messages: vec![
                user("1", "hi"),
                assistant(
                    "2",
                    format,
                    StopReason::ToolUse,
                    vec![
                        reasoning(reasoning_replay),
                        text("reading"),
                        call("c1", r#"{"path":"a"}"#),
                        call("c2", ""),
                    ],
                ),
                result("3", "c2", "missing", true),
                result("4", "c1", "A", false),
                user("5", "next"),
            ],
        }
    }

    #[test]
    fn chat_completions_shape_and_same_model_reasoning() {
        let conversation = tool_round(ProviderKind::OpenAi, json!({"reasoning_content": "think"}));
        let fields = to_chat_completions(&conversation, "m1").expect("converts");
        assert_eq!(
            fields["messages"],
            json!([
                {"role": "system", "content": "be brief"},
                {"role": "user", "content": "hi"},
                {"role": "assistant", "reasoning_content": "think", "content": "reading", "tool_calls": [
                    {"id": "c1", "type": "function", "function": {"name": "read_file", "arguments": "{\"path\":\"a\"}"}},
                    {"id": "c2", "type": "function", "function": {"name": "read_file", "arguments": ""}}
                ]},
                {"role": "tool", "tool_call_id": "c2", "content": "missing"},
                {"role": "tool", "tool_call_id": "c1", "content": "A"},
                {"role": "user", "content": "next"}
            ])
        );
        let other = to_chat_completions(&conversation, "m2").expect("converts");
        assert!(other["messages"][2].get("reasoning_content").is_none());
    }

    #[test]
    fn anthropic_merges_results_with_the_next_user_turn() {
        let thinking = json!({"type": "thinking", "thinking": "think", "signature": "sig"});
        let conversation = tool_round(ProviderKind::Anthropic, thinking.clone());
        let fields = to_anthropic(&conversation, "m1").expect("converts");
        assert_eq!(fields["system"], json!("be brief"));
        assert_eq!(
            fields["messages"],
            json!([
                {"role": "user", "content": [{"type": "text", "text": "hi"}]},
                {"role": "assistant", "content": [
                    thinking,
                    {"type": "text", "text": "reading"},
                    {"type": "tool_use", "id": "c1", "name": "read_file", "input": {"path": "a"}},
                    {"type": "tool_use", "id": "c2", "name": "read_file", "input": {}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "c2", "content": "missing", "is_error": true},
                    {"type": "tool_result", "tool_use_id": "c1", "content": "A"},
                    {"type": "text", "text": "next"}
                ]}
            ])
        );
        let other = to_anthropic(&conversation, "m2").expect("converts");
        assert_eq!(other["messages"][1]["content"][0]["type"], json!("text"));
    }

    #[test]
    fn responses_items_and_replayed_reasoning() {
        let item =
            json!({"type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "e"});
        let conversation = tool_round(ProviderKind::Responses, item.clone());
        let fields = to_responses(&conversation, "m1").expect("converts");
        assert_eq!(fields["instructions"], json!("be brief"));
        assert_eq!(
            fields["input"],
            json!([
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]},
                item,
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "reading"}]},
                {"type": "function_call", "call_id": "c1", "name": "read_file", "arguments": "{\"path\":\"a\"}"},
                {"type": "function_call", "call_id": "c2", "name": "read_file", "arguments": ""},
                {"type": "function_call_output", "call_id": "c2", "output": "missing"},
                {"type": "function_call_output", "call_id": "c1", "output": "A"},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "next"}]}
            ])
        );
        let from_anthropic = tool_round(ProviderKind::Anthropic, json!({"type": "thinking"}));
        let fields = to_responses(&from_anthropic, "m1").expect("converts");
        assert_eq!(
            fields["input"][1]["type"],
            json!("message"),
            "foreign reasoning is dropped"
        );
    }

    #[test]
    fn errored_and_aborted_turns_are_dropped_with_their_results() {
        for stop in [StopReason::Error, StopReason::Aborted] {
            let conversation = Conversation {
                system: None,
                messages: vec![
                    user("1", "hi"),
                    assistant(
                        "2",
                        ProviderKind::OpenAi,
                        stop,
                        vec![text("half"), call("c1", "{}")],
                    ),
                    result("3", "c1", "A", false),
                    user("4", "again"),
                ],
            };
            let fields = to_chat_completions(&conversation, "m1").expect("converts");
            assert_eq!(
                fields["messages"],
                json!([{"role": "user", "content": "hi"}, {"role": "user", "content": "again"}])
            );
            let fields = to_anthropic(&conversation, "m1").expect("converts");
            assert_eq!(
                fields["messages"],
                json!([{"role": "user", "content": [
                    {"type": "text", "text": "hi"},
                    {"type": "text", "text": "again"}
                ]}])
            );
        }
    }

    #[test]
    fn calls_and_results_must_pair_up() {
        let calling = || {
            assistant(
                "2",
                ProviderKind::OpenAi,
                StopReason::ToolUse,
                vec![call("c1", "{}")],
            )
        };
        let cases = [
            ("no result", vec![user("1", "hi"), calling()]),
            (
                "result after a user turn",
                vec![calling(), user("3", "x"), result("4", "c1", "A", false)],
            ),
            (
                "result without a call",
                vec![user("1", "hi"), result("2", "c9", "A", false)],
            ),
            (
                "repeated call id",
                vec![
                    calling(),
                    result("3", "c1", "A", false),
                    calling(),
                    result("5", "c1", "A", false),
                ],
            ),
            (
                "bad tool name",
                vec![assistant(
                    "2",
                    ProviderKind::OpenAi,
                    StopReason::ToolUse,
                    vec![AssistantPart::ToolCall {
                        call_id: "c1".to_string(),
                        name: "read file".to_string(),
                        arguments: "{}".to_string(),
                        replay: None,
                    }],
                )],
            ),
        ];
        for (case, messages) in cases {
            let conversation = Conversation {
                system: None,
                messages,
            };
            let error = to_responses(&conversation, "m1").expect_err(case);
            assert_eq!(error.code, "invalid_conversation", "{case}");
        }
    }

    #[test]
    fn anthropic_needs_object_arguments() {
        let conversation = Conversation {
            system: None,
            messages: vec![
                assistant(
                    "1",
                    ProviderKind::OpenAi,
                    StopReason::ToolUse,
                    vec![call("c1", "[1]")],
                ),
                result("2", "c1", "A", false),
            ],
        };
        assert!(to_anthropic(&conversation, "m1").is_err());
        assert!(to_chat_completions(&conversation, "m1").is_ok());
    }
}
