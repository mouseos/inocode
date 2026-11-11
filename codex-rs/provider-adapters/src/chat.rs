//! OpenAI-compatible `chat/completions` wire (ported from OpenCodex `adapters/openai-chat.ts`,
//! `openai-chat/messages.ts`, `openai-chat/response-events.ts`; standard path only —
//! per-destination quirks are added as `ChatCompat` switches when a destination needs them).

use crate::ids::IdAllocator;
use crate::ir::AdapterEvent;
use crate::ir::AdapterFailure;
use crate::ir::AssistantPart;
use crate::ir::Part;
use crate::ir::ToolCall;
use crate::ir::ToolDecl;
use crate::ir::ToolResult;
use crate::ir::Turn;
use crate::ir::Usage;
use crate::tool_names::wire_name;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;

/// Switches for destinations that differ from the plain OpenAI shape.
#[derive(Clone, Debug)]
pub struct ChatCompat {
    /// Role for in-conversation developer messages (`developer` for OpenAI, `system` elsewhere).
    pub developer_role: &'static str,
    /// Send earlier reasoning back as `reasoning_content` (DeepSeek-style).
    pub replay_reasoning: bool,
    /// `max_tokens` or `max_completion_tokens`.
    pub max_tokens_field: &'static str,
    pub send_parallel_tool_calls: bool,
    pub send_reasoning_effort: bool,
}

impl Default for ChatCompat {
    fn default() -> Self {
        Self {
            developer_role: "system",
            replay_reasoning: false,
            max_tokens_field: "max_tokens",
            send_parallel_tool_calls: true,
            send_reasoning_effort: true,
        }
    }
}

pub struct ChatRequest<'a> {
    pub model: &'a str,
    pub turns: &'a [Turn],
    pub tools: &'a [ToolDecl],
    pub tool_choice: &'a str,
    pub parallel_tool_calls: bool,
    pub reasoning_effort: Option<&'a str>,
    pub max_output_tokens: Option<u32>,
    /// `{"name", "schema", "strict"}` of a JSON-schema output format.
    pub json_schema: Option<&'a Value>,
}

const IMAGE_NOTE: &str = "[inocode] image output from the preceding tool result(s):";

pub fn build_body(req: &ChatRequest<'_>, compat: &ChatCompat) -> Value {
    let mut body = Map::new();
    body.insert("model".into(), json!(req.model));
    body.insert("messages".into(), Value::Array(build_messages(req, compat)));
    body.insert("stream".into(), json!(true));
    body.insert("stream_options".into(), json!({ "include_usage": true }));
    if !req.tools.is_empty() {
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                let mut function = Map::new();
                function.insert("name".into(), json!(t.wire_name));
                function.insert("description".into(), json!(t.description));
                function.insert("parameters".into(), t.parameters.clone());
                if t.strict {
                    function.insert("strict".into(), json!(true));
                }
                json!({ "type": "function", "function": Value::Object(function) })
            })
            .collect();
        body.insert("tools".into(), Value::Array(tools));
        if matches!(req.tool_choice, "auto" | "none" | "required") {
            body.insert("tool_choice".into(), json!(req.tool_choice));
        }
        if compat.send_parallel_tool_calls {
            body.insert("parallel_tool_calls".into(), json!(req.parallel_tool_calls));
        }
    }
    if let Some(effort) = req.reasoning_effort
        && compat.send_reasoning_effort
        && effort != "none"
    {
        body.insert("reasoning_effort".into(), json!(effort));
    }
    if let Some(max) = req.max_output_tokens {
        body.insert(compat.max_tokens_field.into(), json!(max));
    }
    if let Some(format) = req.json_schema {
        body.insert(
            "response_format".into(),
            json!({
                "type": "json_schema",
                "json_schema": {
                    "name": format.get("name").cloned().unwrap_or(json!("response")),
                    "schema": format.get("schema").cloned().unwrap_or(json!({})),
                    "strict": format.get("strict").cloned().unwrap_or(json!(false)),
                }
            }),
        );
    }
    Value::Object(body)
}

fn text_of(parts: &[Part]) -> String {
    parts
        .iter()
        .filter_map(|p| match p {
            Part::Text(t) => Some(t.as_str()),
            Part::Image(_) => None,
        })
        .collect()
}

fn tool_result_text(parts: &[Part]) -> String {
    let text = text_of(parts);
    if text.is_empty() && parts.iter().any(|p| matches!(p, Part::Image(_))) {
        "[image]".to_string()
    } else {
        text
    }
}

fn user_message(parts: &[Part]) -> Value {
    if parts.iter().all(|p| matches!(p, Part::Text(_))) {
        return json!({ "role": "user", "content": text_of(parts) });
    }
    let content: Vec<Value> = parts
        .iter()
        .map(|p| match p {
            Part::Text(t) => json!({ "type": "text", "text": t }),
            Part::Image(url) => json!({ "type": "image_url", "image_url": { "url": url } }),
        })
        .collect();
    json!({ "role": "user", "content": content })
}

fn safe_tool_name(name: Option<&str>) -> String {
    let raw = name.filter(|n| !n.trim().is_empty()).unwrap_or("tool_result");
    raw.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect()
}

struct Builder<'a> {
    compat: &'a ChatCompat,
    out: Vec<Value>,
    /// (wire id, tool name) of calls still waiting for their result.
    pending: Vec<(String, String)>,
    /// Messages that arrived while results were owed; they follow the results.
    deferred: Vec<Value>,
    pending_images: Vec<Value>,
    /// Wire id → index in `out` of the tool message that answered it.
    answered: BTreeMap<String, usize>,
    ids: IdAllocator,
}

impl Builder<'_> {
    fn push_or_defer(&mut self, message: Value) {
        if self.pending.is_empty() {
            self.out.push(message);
        } else {
            self.deferred.push(message);
        }
    }

    fn release_deferred(&mut self) {
        self.out.append(&mut self.deferred);
    }

    fn flush_images(&mut self) {
        if self.pending_images.is_empty() {
            return;
        }
        let mut content = vec![json!({ "type": "text", "text": IMAGE_NOTE })];
        content.append(&mut self.pending_images);
        self.out.push(json!({ "role": "user", "content": content }));
    }

    fn flush_pending_calls(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        for (id, name) in std::mem::take(&mut self.pending) {
            self.out.push(json!({
                "role": "tool",
                "tool_call_id": id,
                "content": format!(
                    "[inocode] no tool result was recorded for \"{name}\"; execution status unknown — do not treat this as success, failure, or user-provided input."
                ),
            }));
        }
        self.flush_images();
        self.release_deferred();
    }

    fn assistant(&mut self, parts: &[AssistantPart]) {
        let text: String = parts
            .iter()
            .filter_map(|p| match p {
                AssistantPart::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        let thinking: String = parts
            .iter()
            .filter_map(|p| match p {
                AssistantPart::Thinking(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect();
        let calls: Vec<&ToolCall> = parts
            .iter()
            .filter_map(|p| match p {
                AssistantPart::Call(c) => Some(c),
                _ => None,
            })
            .collect();

        let mut message = Map::new();
        message.insert("role".into(), json!("assistant"));
        if !text.is_empty() {
            message.insert("content".into(), json!(text));
        }
        let replay = self.compat.replay_reasoning && !thinking.is_empty();
        if replay {
            message.insert("reasoning_content".into(), json!(thinking));
        }
        let carries_nothing = text.is_empty() && calls.is_empty() && !replay;
        if carries_nothing {
            return;
        }
        self.flush_pending_calls();
        if !calls.is_empty() {
            let wire_calls: Vec<(String, &ToolCall)> = calls
                .iter()
                .map(|c| (self.ids.allocate(&c.id), *c))
                .collect();
            let tool_calls: Vec<Value> = wire_calls
                .iter()
                .map(|(id, c)| {
                    let arguments = if c.arguments.trim().is_empty() {
                        "{}".to_string()
                    } else {
                        c.arguments.clone()
                    };
                    json!({
                        "id": id,
                        "type": "function",
                        "function": {
                            "name": wire_name(c.namespace.as_deref(), &c.name),
                            "arguments": arguments,
                        }
                    })
                })
                .collect();
            message.insert("tool_calls".into(), Value::Array(tool_calls));
            message
                .entry("content")
                .or_insert_with(|| json!(""));
            self.pending = wire_calls
                .iter()
                .map(|(id, c)| (id.clone(), wire_name(c.namespace.as_deref(), &c.name)))
                .collect();
        } else {
            message.entry("content").or_insert_with(|| json!(""));
        }
        self.out.push(Value::Object(message));
    }

    fn tool_result(&mut self, result: &ToolResult) {
        let wire_id = self.ids.lookup(&result.call_id);
        let text = tool_result_text(&result.content);
        let images: Vec<Value> = result
            .content
            .iter()
            .filter_map(|p| match p {
                Part::Image(url) => Some(json!({ "type": "image_url", "image_url": { "url": url } })),
                Part::Text(_) => None,
            })
            .collect();

        if let Some(id) = wire_id.clone()
            && let Some(index) = self.pending.iter().position(|(p, _)| *p == id)
        {
            self.out.push(json!({ "role": "tool", "tool_call_id": id, "content": text }));
            self.answered.insert(id, self.out.len() - 1);
            self.pending_images.extend(images);
            self.pending.remove(index);
            if self.pending.is_empty() {
                self.flush_images();
                self.release_deferred();
            }
            return;
        }
        if let Some(id) = wire_id
            && let Some(&index) = self.answered.get(&id)
        {
            // A repeated result for an already-answered call.
            if index + 1 == self.out.len() && self.deferred.is_empty() {
                if !text.is_empty()
                    && let Some(Value::String(current)) = self.out[index].get_mut("content")
                {
                    if current.is_empty() {
                        *current = text;
                    } else {
                        current.push_str("\n\n");
                        current.push_str(&text);
                    }
                }
                self.pending_images.extend(images);
                if self.pending.is_empty() {
                    self.flush_images();
                }
            } else {
                let mut content =
                    vec![json!({ "type": "text", "text": format!("[inocode] additional output for tool call {id}:\n{text}") })];
                content.extend(images);
                self.push_or_defer(json!({ "role": "user", "content": content }));
            }
            return;
        }
        // A result with no call in view: give it a call so the pair is well formed.
        self.flush_pending_calls();
        let id = self.ids.allocate(&result.call_id);
        let name = safe_tool_name(result.name.as_deref());
        self.out.push(json!({
            "role": "assistant",
            "content": "",
            "tool_calls": [{ "id": id, "type": "function", "function": { "name": name, "arguments": "{}" } }],
        }));
        self.out.push(json!({ "role": "tool", "tool_call_id": id, "content": text }));
        self.answered.insert(id, self.out.len() - 1);
        self.pending_images.extend(images);
        self.flush_images();
    }
}

fn build_messages(req: &ChatRequest<'_>, compat: &ChatCompat) -> Vec<Value> {
    let mut builder = Builder {
        compat,
        out: Vec::new(),
        pending: Vec::new(),
        deferred: Vec::new(),
        pending_images: Vec::new(),
        answered: BTreeMap::new(),
        ids: IdAllocator::default(),
    };
    for turn in req.turns {
        match turn {
            Turn::Assistant(parts) => builder.ids.reserve(parts.iter().filter_map(|p| match p {
                AssistantPart::Call(c) => Some(c.id.as_str()),
                _ => None,
            })),
            Turn::ToolResult(r) => builder.ids.reserve([r.call_id.as_str()]),
            _ => {}
        }
    }

    // The leading system turns are one system message; later ones keep their place.
    let leading = req
        .turns
        .iter()
        .take_while(|t| matches!(t, Turn::System(_)))
        .count();
    let system: Vec<&str> = req.turns[..leading]
        .iter()
        .filter_map(|t| match t {
            Turn::System(s) => Some(s.as_str()),
            _ => None,
        })
        .collect();
    if !system.is_empty() {
        builder
            .out
            .push(json!({ "role": "system", "content": system.join("\n\n") }));
    }

    for turn in &req.turns[leading..] {
        match turn {
            Turn::System(text) => {
                if !text.is_empty() {
                    builder.push_or_defer(json!({ "role": compat.developer_role, "content": text }));
                }
            }
            Turn::User(parts) => builder.push_or_defer(user_message(parts)),
            Turn::Assistant(parts) => builder.assistant(parts),
            Turn::ToolResult(result) => builder.tool_result(result),
        }
    }
    builder.flush_pending_calls();
    builder.release_deferred();
    builder.out
}

// ------------------------------------------------------------------ stream

#[derive(Default)]
struct CallAcc {
    id: String,
    name: String,
    args: String,
}

/// Parses `chat/completions` SSE `data:` payloads into [`AdapterEvent`]s.
#[derive(Default)]
pub struct ChatStream {
    calls: BTreeMap<usize, CallAcc>,
    finish_reason: Option<String>,
    usage: Option<Usage>,
    done: bool,
    minted: u32,
}

pub fn stop_reason_for(finish_reason: Option<&str>) -> Option<String> {
    match finish_reason {
        Some("length") => Some("max_tokens".to_string()),
        Some("content_filter") => Some("content_filter".to_string()),
        _ => None,
    }
}

fn usage_from(usage: &Value) -> Option<Usage> {
    let number = |v: Option<&Value>| v.and_then(Value::as_i64).filter(|n| *n >= 0);
    let input = number(usage.get("prompt_tokens"))?;
    let output = number(usage.get("completion_tokens")).unwrap_or(0);
    Some(Usage {
        input_tokens: input,
        output_tokens: output,
        cached_input_tokens: number(
            usage
                .get("prompt_tokens_details")
                .and_then(|d| d.get("cached_tokens")),
        )
        .unwrap_or(0),
        cache_write_input_tokens: 0,
        reasoning_output_tokens: number(
            usage
                .get("completion_tokens_details")
                .and_then(|d| d.get("reasoning_tokens")),
        )
        .unwrap_or(0),
    })
}

fn error_failure(error: &Value) -> AdapterFailure {
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| error.as_str())
        .unwrap_or("upstream error")
        .to_string();
    AdapterFailure {
        message,
        status: error
            .get("status")
            .or_else(|| error.get("code"))
            .and_then(Value::as_u64)
            .and_then(|n| u16::try_from(n).ok())
            .filter(|n| (400..=599).contains(n)),
        error_type: error.get("type").and_then(Value::as_str).map(str::to_string),
        code: error
            .get("code")
            .and_then(Value::as_str)
            .map(str::to_string),
    }
}

impl ChatStream {
    pub fn is_done(&self) -> bool {
        self.done
    }

    fn flush_calls(&mut self) -> Result<Vec<AdapterEvent>, AdapterFailure> {
        let mut events = Vec::new();
        for (_, call) in std::mem::take(&mut self.calls) {
            if call.name.trim().is_empty() {
                return Err(AdapterFailure::new(
                    "upstream sent a tool call without a function name",
                ));
            }
            let args = if call.args.trim().is_empty() {
                "{}".to_string()
            } else {
                call.args
            };
            if serde_json::from_str::<Value>(&args).is_err() {
                return Err(AdapterFailure::new(
                    "upstream sent tool call arguments that are not valid JSON",
                ));
            }
            let id = if call.id.is_empty() {
                self.minted += 1;
                format!("call_minted_{}", self.minted)
            } else {
                call.id
            };
            events.push(AdapterEvent::ToolCallStart { id, name: call.name });
            events.push(AdapterEvent::ToolCallDelta(args));
            events.push(AdapterEvent::ToolCallEnd);
        }
        Ok(events)
    }

    fn finish(&mut self) -> Vec<AdapterEvent> {
        self.done = true;
        match self.flush_calls() {
            Ok(mut events) => {
                events.push(AdapterEvent::Done {
                    usage: self.usage.take(),
                    stop_reason: stop_reason_for(self.finish_reason.as_deref()),
                });
                events
            }
            Err(failure) => vec![AdapterEvent::Error(failure)],
        }
    }

    /// One SSE `data:` payload.
    pub fn feed(&mut self, data: &str) -> Vec<AdapterEvent> {
        if self.done {
            return Vec::new();
        }
        let payload = data.trim();
        if payload.is_empty() {
            return Vec::new();
        }
        if payload == "[DONE]" {
            return self.finish();
        }
        let Ok(chunk) = serde_json::from_str::<Value>(payload) else {
            self.done = true;
            return vec![AdapterEvent::Error(AdapterFailure::new(
                "malformed upstream SSE data frame",
            ))];
        };
        let Some(chunk) = chunk.as_object() else {
            return Vec::new();
        };
        if let Some(error) = chunk.get("error").filter(|e| !e.is_null()) {
            self.done = true;
            return vec![AdapterEvent::Error(error_failure(error))];
        }
        if let Some(usage) = chunk.get("usage").filter(|u| !u.is_null()) {
            self.usage = usage_from(usage).or(self.usage.take());
        }
        let Some(choice) = chunk
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
        else {
            return Vec::new();
        };
        if choice.get("finish_reason").and_then(Value::as_str) == Some("error") {
            self.done = true;
            return vec![AdapterEvent::Error(error_failure(
                choice.get("error").unwrap_or(&Value::Null),
            ))];
        }
        if let Some(reason) = choice
            .get("finish_reason")
            .and_then(Value::as_str)
            .filter(|r| !r.is_empty())
        {
            self.finish_reason = Some(reason.to_string());
        }

        let mut events = Vec::new();
        let Some(delta) = choice.get("delta").and_then(Value::as_object) else {
            return events;
        };
        for key in ["reasoning_content", "reasoning"] {
            if let Some(text) = delta.get(key).and_then(Value::as_str)
                && !text.is_empty()
            {
                events.push(AdapterEvent::ReasoningRawDelta(text.to_string()));
                break;
            }
        }
        if let Some(text) = delta.get("content").and_then(Value::as_str)
            && !text.is_empty()
        {
            events.push(AdapterEvent::TextDelta(text.to_string()));
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for (position, call) in calls.iter().enumerate() {
                let index = call
                    .get("index")
                    .and_then(Value::as_u64)
                    .and_then(|n| usize::try_from(n).ok())
                    .unwrap_or(position);
                let acc = self.calls.entry(index).or_default();
                if let Some(id) = call.get("id").and_then(Value::as_str)
                    && !id.is_empty()
                {
                    acc.id = id.to_string();
                }
                if let Some(function) = call.get("function") {
                    if let Some(name) = function.get("name").and_then(Value::as_str) {
                        acc.name.push_str(name);
                    }
                    if let Some(args) = function.get("arguments").and_then(Value::as_str) {
                        acc.args.push_str(args);
                    }
                }
            }
        }
        events
    }

    /// The byte stream ended. A stream that carried a `finish_reason` but no `[DONE]` is complete;
    /// anything else was cut off.
    pub fn eof(&mut self) -> Vec<AdapterEvent> {
        if self.done {
            return Vec::new();
        }
        if self.finish_reason.is_some() {
            return self.finish();
        }
        self.done = true;
        vec![AdapterEvent::Error(AdapterFailure::new(
            "upstream stream ended without a terminal signal ([DONE] or finish_reason) — possible truncation",
        ))]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::turns_from_items;
    use pretty_assertions::assert_eq;

    fn turns(value: Value) -> Vec<Turn> {
        let items: Vec<codex_protocol::models::ResponseItem> =
            serde_json::from_value(value).expect("items");
        turns_from_items(&items)
    }

    fn body_for(value: Value, tools: &[ToolDecl]) -> Value {
        let t = turns(value);
        build_body(
            &ChatRequest {
                model: "m",
                turns: &t,
                tools,
                tool_choice: "auto",
                parallel_tool_calls: true,
                reasoning_effort: Some("high"),
                max_output_tokens: Some(1000),
                json_schema: None,
            },
            &ChatCompat::default(),
        )
    }

    fn shell_tool() -> ToolDecl {
        ToolDecl {
            wire_name: "shell".into(),
            namespace: None,
            name: "shell".into(),
            description: "run".into(),
            parameters: json!({"type": "object", "properties": {}}),
            strict: false,
        }
    }

    #[test]
    fn a_tool_round_trip_becomes_assistant_tool_calls_then_tool_message() {
        let body = body_for(
            json!([
                {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "rules"}]},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "list"}]},
                {"type": "function_call", "name": "shell", "arguments": "{\"cmd\":\"ls\"}", "call_id": "call_a"},
                {"type": "function_call_output", "call_id": "call_a", "output": "a.txt"}
            ]),
            &[shell_tool()],
        );
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert_eq!(body["max_tokens"], 1000);
        assert_eq!(body["reasoning_effort"], "high");
        assert_eq!(body["tools"][0]["function"]["name"], "shell");
        let messages = body["messages"].as_array().expect("messages");
        assert_eq!(messages[0], json!({"role": "system", "content": "rules"}));
        assert_eq!(messages[1], json!({"role": "user", "content": "list"}));
        assert_eq!(messages[2]["role"], "assistant");
        assert_eq!(messages[2]["content"], "");
        assert_eq!(messages[2]["tool_calls"][0]["id"], "call_a");
        assert_eq!(messages[2]["tool_calls"][0]["function"]["arguments"], "{\"cmd\":\"ls\"}");
        assert_eq!(
            messages[3],
            json!({"role": "tool", "tool_call_id": "call_a", "content": "a.txt"})
        );
    }

    #[test]
    fn a_user_message_that_arrives_while_a_result_is_owed_waits_for_it() {
        let body = body_for(
            json!([
                {"type": "function_call", "name": "shell", "arguments": "{}", "call_id": "c1"},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "interrupt"}]},
                {"type": "function_call_output", "call_id": "c1", "output": "ok"}
            ]),
            &[shell_tool()],
        );
        let messages = body["messages"].as_array().expect("messages");
        // The interrupting user message is not allowed between the call and its result.
        assert_eq!(messages[0]["role"], "assistant");
        assert_eq!(messages[1]["role"], "tool");
        assert_eq!(messages[2]["role"], "user");
        assert_eq!(messages[2]["content"], "interrupt");
    }

    #[test]
    fn a_missing_result_gets_a_placeholder_and_an_orphan_result_gets_a_call() {
        let body = body_for(
            json!([
                {"type": "function_call", "name": "shell", "arguments": "{}", "call_id": "c1"},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "next"}]},
                {"type": "function_call_output", "call_id": "ghost", "name": "tool x", "output": "late"}
            ]),
            &[shell_tool()],
        );
        let messages = body["messages"].as_array().expect("messages");
        assert_eq!(messages[1]["role"], "tool");
        assert!(messages[1]["content"].as_str().is_some_and(|c| c.contains("no tool result was recorded")));
        assert_eq!(messages[2]["content"], "next");
        assert_eq!(messages[3]["tool_calls"][0]["function"]["name"], "tool_x");
        assert_eq!(messages[4]["content"], "late");
    }

    fn run(chunks: &[&str]) -> Vec<AdapterEvent> {
        let mut stream = ChatStream::default();
        let mut events = Vec::new();
        for chunk in chunks {
            events.extend(stream.feed(chunk));
        }
        events.extend(stream.eof());
        events
    }

    #[test]
    fn streams_text_then_tool_calls_with_usage() {
        let events = run(&[
            r#"{"choices":[{"delta":{"content":"Hel"}}]}"#,
            r#"{"choices":[{"delta":{"content":"lo","tool_calls":[{"index":0,"id":"c1","function":{"name":"shell","arguments":"{\"cmd\""}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":":\"ls\"}"}}]},"finish_reason":"tool_calls"}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":12,"completion_tokens":5,"prompt_tokens_details":{"cached_tokens":4},"completion_tokens_details":{"reasoning_tokens":2}}}"#,
            "[DONE]",
        ]);
        assert_eq!(
            events,
            vec![
                AdapterEvent::TextDelta("Hel".into()),
                AdapterEvent::TextDelta("lo".into()),
                AdapterEvent::ToolCallStart { id: "c1".into(), name: "shell".into() },
                AdapterEvent::ToolCallDelta("{\"cmd\":\"ls\"}".into()),
                AdapterEvent::ToolCallEnd,
                AdapterEvent::Done {
                    usage: Some(Usage {
                        input_tokens: 12,
                        output_tokens: 5,
                        cached_input_tokens: 4,
                        cache_write_input_tokens: 0,
                        reasoning_output_tokens: 2,
                    }),
                    stop_reason: None,
                },
            ]
        );
    }

    #[test]
    fn length_is_a_truncation_and_a_cut_stream_is_an_error() {
        let events = run(&[
            r#"{"choices":[{"delta":{"content":"x"},"finish_reason":"length"}]}"#,
            "[DONE]",
        ]);
        assert!(matches!(events.last(), Some(AdapterEvent::Done { stop_reason: Some(r), .. }) if r == "max_tokens"));
        let cut = run(&[r#"{"choices":[{"delta":{"content":"x"}}]}"#]);
        assert!(matches!(cut.last(), Some(AdapterEvent::Error(f)) if f.message.contains("truncation")));
        let bad = run(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"shell","arguments":"{oops"}}]},"finish_reason":"tool_calls"}]}"#,
            "[DONE]",
        ]);
        assert!(matches!(bad.last(), Some(AdapterEvent::Error(f)) if f.message.contains("not valid JSON")));
    }

    #[test]
    fn error_frames_and_reasoning_content_are_surfaced() {
        let events = run(&[
            r#"{"choices":[{"delta":{"reasoning_content":"hmm"}}]}"#,
            r#"{"error":{"message":"rate limited","type":"rate_limit_error","code":429}}"#,
        ]);
        assert_eq!(events[0], AdapterEvent::ReasoningRawDelta("hmm".into()));
        assert!(matches!(&events[1], AdapterEvent::Error(f) if f.status == Some(429) && f.message == "rate limited"));
    }
}
