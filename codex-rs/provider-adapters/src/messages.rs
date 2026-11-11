//! Claude Messages API wire (ported from OpenCodex `adapters/anthropic.ts`,
//! `adapters/anthropic-model-contract.ts` and the cache handling in `anthropic.ts`).
//!
//! Not ported (see `ref/docs/20-providers-experiment.md`, chapter 10): server tools
//! (`web_search` / `web_fetch`), fast mode (`speed`), `display: updates`, image re-encoding.

use crate::envelope::Envelope;
use crate::ids::IdAllocator;
use crate::ir::AdapterEvent;
use crate::ir::AdapterFailure;
use crate::ir::AssistantPart;
use crate::ir::Part;
use crate::ir::ToolDecl;
use crate::ir::ToolResult;
use crate::ir::Turn;
use crate::ir::Usage;
use crate::tool_names::wire_name;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;
use std::collections::HashSet;

pub const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Beta header the OAuth credential needs.
pub const OAUTH_BETA: &str = "claude-code-20250219,oauth-2025-04-20";
/// First system block required for OAuth credentials.
pub const OAUTH_SYSTEM_INSTRUCTION: &str =
    "You are a Claude agent, built on Anthropic's Claude Agent SDK.";
/// OAuth credentials reject arbitrary tool names; custom tools carry this prefix on the wire.
pub const OAUTH_TOOL_PREFIX: &str = "custom_";

const DEFAULT_MAX_TOKENS: u32 = 8192;
const REASONING_MAX_TOKENS_CEILING: u32 = 32_000;
const ADAPTIVE_THINKING_CEILING: u32 = 40_192;
const MIN_THINKING_BUDGET: u32 = 1024;
const OUTPUT_HEADROOM: u32 = 8192;
const OUTPUT_FLOOR: u32 = 4096;
const MAX_CACHE_BREAKPOINTS: usize = 4;
const BUILTIN_TOOLS: [&str; 4] = ["web_search", "code_execution", "text_editor", "computer"];

// ------------------------------------------------------------ model contract

/// `(family, major, minor)` of a Claude model id, tolerant of a routing prefix or suffix.
/// A date-pinned id (`claude-opus-4-20250514`) parses as minor 0.
pub fn family_version(model_id: &str) -> Option<(String, u32, u32)> {
    let mut search = 0;
    while let Some(found) = model_id[search..].find("claude-") {
        let start = search + found;
        let boundary = start == 0 || model_id.as_bytes()[start - 1] == b'/';
        search = start + "claude-".len();
        if !boundary {
            continue;
        }
        let rest = &model_id[search..];
        let family: String = rest
            .chars()
            .take_while(char::is_ascii_alphabetic)
            .collect::<String>()
            .to_ascii_lowercase();
        if family.is_empty() {
            continue;
        }
        let after = &rest[family.len()..];
        let Some(after) = after.strip_prefix('-') else {
            continue;
        };
        let major_digits: String = after.chars().take_while(char::is_ascii_digit).collect();
        let Ok(major) = major_digits.parse::<u32>() else {
            continue;
        };
        let tail = &after[major_digits.len()..];
        let minor = tail
            .strip_prefix(['.', '-'])
            .map(|t| t.chars().take_while(char::is_ascii_digit).collect::<String>())
            .filter(|d| (1..=2).contains(&d.len()))
            .and_then(|d| d.parse::<u32>().ok())
            .unwrap_or(0);
        return Some((family, major, minor));
    }
    None
}

fn at_least(version: &(String, u32, u32), minimum: (u32, u32)) -> bool {
    version.1 > minimum.0 || (version.1 == minimum.0 && version.2 >= minimum.1)
}

fn meets(model_id: &str, minimums: &[(&str, (u32, u32))]) -> bool {
    let Some(version) = family_version(model_id) else {
        return false;
    };
    minimums
        .iter()
        .find(|(family, _)| *family == version.0)
        .is_some_and(|(_, minimum)| at_least(&version, *minimum))
}

const ADAPTIVE_MINIMUMS: [(&str, (u32, u32)); 4] =
    [("sonnet", (5, 0)), ("opus", (4, 7)), ("fable", (0, 0)), ("haiku", (5, 5))];

/// Families that moved to adaptive thinking (`thinking.type: "adaptive"` + `output_config.effort`).
pub fn uses_adaptive_thinking(model_id: &str) -> bool {
    meets(model_id, &ADAPTIVE_MINIMUMS)
}

/// Sonnet 5.5+: `disabled` is rejected; `between_tools` is the lowest setting.
pub fn uses_between_tools_floor(model_id: &str) -> bool {
    family_version(model_id).is_some_and(|v| v.0 == "sonnet" && at_least(&v, (5, 5)))
}

/// Families that think by default and accept an explicit `thinking: {type: "disabled"}`.
pub fn supports_explicit_thinking_disable(model_id: &str) -> bool {
    family_version(model_id).is_some_and(|v| {
        (v.0 == "sonnet" && at_least(&v, (5, 0)) && !at_least(&v, (5, 5)))
            || (v.0 == "haiku" && at_least(&v, (5, 5)))
    })
}

/// Forced `tool_choice` (`any` / `tool`) is rejected by Opus 5.5, Fable 5.1+, Sonnet 5.5+.
pub fn rejects_forced_tool_choice(model_id: &str) -> bool {
    let Some(v) = family_version(model_id) else {
        return false;
    };
    match v.0.as_str() {
        "opus" => v.1 == 5 && v.2 == 5,
        "fable" => at_least(&v, (5, 1)),
        "sonnet" => at_least(&v, (5, 5)),
        _ => false,
    }
}

/// Families that reject any non-default `temperature` / `top_p` / `top_k`.
pub fn rejects_sampling_parameters(model_id: &str) -> bool {
    meets(model_id, &ADAPTIVE_MINIMUMS)
}

fn reasoning_budget(effort: &str) -> u32 {
    match effort {
        "minimal" => 1024,
        "low" => 4096,
        "high" => 16_384,
        "xhigh" => 24_576,
        "max" => 32_000,
        _ => 8192,
    }
}

fn adaptive_effort(effort: &str) -> &str {
    if effort == "minimal" { "low" } else { effort }
}

// -------------------------------------------------------------------- request

#[derive(Clone, Debug)]
pub struct MessagesCompat {
    /// OAuth credential: system identity block, `custom_` tool prefix.
    pub oauth: bool,
    /// Official endpoint: automatic top-level prompt caching.
    pub native_endpoint: bool,
    /// Cache lifetime: `none`, `short` (5 min) or `long` (1 h).
    pub cache: CacheRetention,
    /// `max_tokens` when the request names none.
    pub default_max_tokens: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheRetention {
    None,
    Short,
    Long,
}

impl Default for MessagesCompat {
    fn default() -> Self {
        Self {
            oauth: false,
            native_endpoint: true,
            cache: CacheRetention::Long,
            default_max_tokens: DEFAULT_MAX_TOKENS,
        }
    }
}

pub struct MessagesRequest<'a> {
    pub model: &'a str,
    pub turns: &'a [Turn],
    pub tools: &'a [ToolDecl],
    pub tool_choice: &'a str,
    pub parallel_tool_calls: bool,
    pub reasoning_effort: Option<&'a str>,
    pub max_output_tokens: Option<u32>,
    pub json_schema: Option<&'a Value>,
}

fn tool_wire_name(compat: &MessagesCompat, flat: &str) -> String {
    if compat.oauth {
        let lower = flat.to_ascii_lowercase();
        if BUILTIN_TOOLS.contains(&lower.as_str()) || lower.starts_with(OAUTH_TOOL_PREFIX) {
            return flat.to_string();
        }
        return format!("{OAUTH_TOOL_PREFIX}{flat}");
    }
    flat.to_string()
}

/// The name a tool call comes back under, undone.
pub fn tool_name_from_wire(compat: &MessagesCompat, wire: &str) -> String {
    if compat.oauth {
        return wire.strip_prefix(OAUTH_TOOL_PREFIX).unwrap_or(wire).to_string();
    }
    wire.to_string()
}

fn parse_data_url(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(',')?;
    let media = meta.strip_suffix(";base64")?;
    Some((media.to_string(), data.to_string()))
}

fn content_block(part: &Part) -> Value {
    match part {
        Part::Text(text) => json!({ "type": "text", "text": text }),
        Part::Image(url) => match parse_data_url(url) {
            Some((media_type, data)) => json!({
                "type": "image",
                "source": { "type": "base64", "media_type": media_type, "data": data },
            }),
            None => json!({ "type": "image", "source": { "type": "url", "url": url } }),
        },
    }
}

fn non_empty_blocks(parts: &[Part]) -> Vec<Value> {
    parts
        .iter()
        .filter(|p| !matches!(p, Part::Text(t) if t.is_empty()))
        .map(content_block)
        .collect()
}

fn tool_result_block(result: &ToolResult, wire_id: &str) -> Value {
    let blocks = non_empty_blocks(&result.content);
    let content = if blocks.is_empty() {
        json!("(empty tool output)")
    } else if blocks.iter().all(|b| b["type"] == "text") && blocks.len() == 1 {
        blocks[0]["text"].clone()
    } else {
        Value::Array(blocks)
    };
    let mut block = Map::new();
    block.insert("type".into(), json!("tool_result"));
    block.insert("tool_use_id".into(), json!(wire_id));
    block.insert("content".into(), content);
    if result.is_error {
        block.insert("is_error".into(), json!(true));
    }
    Value::Object(block)
}

fn orphan_result_blocks(result: &ToolResult) -> Vec<Value> {
    let label = match &result.name {
        Some(name) => format!("{name} ({})", result.call_id),
        None => result.call_id.clone(),
    };
    let has_image = result.content.iter().any(|p| matches!(p, Part::Image(_)));
    if !has_image {
        let text: String = result
            .content
            .iter()
            .filter_map(|p| match p {
                Part::Text(t) => Some(t.as_str()),
                Part::Image(_) => None,
            })
            .collect();
        return vec![json!({
            "type": "text",
            "text": format!("[tool_result without adjacent tool_use: {label}]\n{text}"),
        })];
    }
    let mut blocks = vec![json!({
        "type": "text",
        "text": format!("[tool_result without adjacent tool_use: {label}]"),
    })];
    blocks.extend(non_empty_blocks(&result.content));
    blocks
}

fn tool_input(arguments: &str) -> Value {
    match serde_json::from_str::<Value>(arguments) {
        Ok(value @ Value::Object(_)) => value,
        _ => json!({}),
    }
}

/// System blocks and messages. Leading `System` turns become `system`; later ones stay in place
/// as user messages (OpenCodex maps developer → user).
fn build_messages(req: &MessagesRequest<'_>, compat: &MessagesCompat) -> (Vec<String>, Vec<Value>) {
    let mut ids = IdAllocator::default();
    for turn in req.turns {
        match turn {
            Turn::Assistant(parts) => ids.reserve(parts.iter().filter_map(|p| match p {
                AssistantPart::Call(c) => Some(c.id.as_str()),
                _ => None,
            })),
            Turn::ToolResult(r) => ids.reserve([r.call_id.as_str()]),
            _ => {}
        }
    }
    let leading = req
        .turns
        .iter()
        .take_while(|t| matches!(t, Turn::System(_)))
        .count();
    let system: Vec<String> = {
        let joined = req.turns[..leading]
            .iter()
            .filter_map(|t| match t {
                Turn::System(s) => Some(s.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        if joined.is_empty() { Vec::new() } else { vec![joined] }
    };

    let turns = &req.turns[leading..];
    let mut messages: Vec<Value> = Vec::new();
    let mut i = 0;
    while i < turns.len() {
        match &turns[i] {
            Turn::System(text) => {
                let content = if text.is_empty() { "(empty)" } else { text.as_str() };
                messages.push(json!({ "role": "user", "content": content }));
            }
            Turn::User(parts) => {
                let blocks = non_empty_blocks(parts);
                let content = if blocks.is_empty() {
                    json!("(empty)")
                } else if blocks.len() == 1 && blocks[0]["type"] == "text" {
                    blocks[0]["text"].clone()
                } else {
                    Value::Array(blocks)
                };
                messages.push(json!({ "role": "user", "content": content }));
            }
            Turn::ToolResult(result) => {
                messages.push(json!({ "role": "user", "content": orphan_result_blocks(result) }));
            }
            Turn::Assistant(parts) => {
                let mut preface: Vec<Value> = Vec::new();
                let mut tool_uses: Vec<Value> = Vec::new();
                let mut original_order: Vec<Value> = Vec::new();
                let mut tool_use_ids: Vec<String> = Vec::new();
                let mut signed = false;
                for part in parts {
                    match part {
                        AssistantPart::Text(text) => {
                            if !text.is_empty() {
                                let block = json!({ "type": "text", "text": text });
                                preface.push(block.clone());
                                original_order.push(block);
                            }
                        }
                        AssistantPart::Thinking(thinking) => {
                            for data in &thinking.redacted {
                                let block = json!({ "type": "redacted_thinking", "data": data });
                                preface.push(block.clone());
                                original_order.push(block);
                            }
                            if let Some(signature) = thinking.signature.as_deref().filter(|s| !s.is_empty()) {
                                signed = true;
                                let block = json!({
                                    "type": "thinking",
                                    "thinking": thinking.text,
                                    "signature": signature,
                                });
                                preface.push(block.clone());
                                original_order.push(block);
                            }
                        }
                        AssistantPart::Call(call) => {
                            let wire_id = ids.allocate(&call.id);
                            tool_use_ids.push(wire_id.clone());
                            let flat = wire_name(call.namespace.as_deref(), &call.name);
                            let block = json!({
                                "type": "tool_use",
                                "id": wire_id,
                                "name": tool_wire_name(compat, &flat),
                                "input": tool_input(&call.arguments),
                            });
                            tool_uses.push(block.clone());
                            original_order.push(block);
                        }
                    }
                }
                // Text or thinking after a tool_use ends the tool turn; keep the original order
                // only when a signed thinking block pins it.
                let content = if signed {
                    original_order
                } else {
                    preface.into_iter().chain(tool_uses).collect()
                };
                i += 1;
                if content.is_empty() {
                    continue;
                }
                messages.push(json!({ "role": "assistant", "content": content }));
                if !tool_use_ids.is_empty() {
                    let required: HashSet<&str> = tool_use_ids.iter().map(String::as_str).collect();
                    let mut results: Vec<Value> = Vec::new();
                    let mut orphans: Vec<Value> = Vec::new();
                    let mut seen: HashSet<String> = HashSet::new();
                    while let Some(Turn::ToolResult(result)) = turns.get(i) {
                        match ids.lookup(&result.call_id) {
                            Some(id) if required.contains(id.as_str()) && !seen.contains(&id) => {
                                results.push(tool_result_block(result, &id));
                                seen.insert(id);
                            }
                            _ => orphans.extend(orphan_result_blocks(result)),
                        }
                        i += 1;
                    }
                    for id in &tool_use_ids {
                        if !seen.contains(id) {
                            results.push(json!({
                                "type": "tool_result",
                                "tool_use_id": id,
                                "content": "[missing tool_result for this tool_use in history]",
                                "is_error": true,
                            }));
                        }
                    }
                    results.extend(orphans);
                    messages.push(json!({ "role": "user", "content": results }));
                }
                continue;
            }
        }
        i += 1;
    }
    // Newer models reject an assistant-tail history as prefill.
    if messages.last().is_none_or(|m| m["role"] == "assistant") {
        messages.push(json!({ "role": "user", "content": "(continue)" }));
    }
    (system, messages)
}

/// Anthropic rejects a missing root type and root-level composition in `input_schema`.
fn normalize_input_schema(schema: &Value) -> Value {
    let mut object = match schema {
        Value::Object(map) => map.clone(),
        _ => Map::new(),
    };
    let composition = ["oneOf", "anyOf", "allOf"];
    let has_composition = composition
        .iter()
        .any(|key| object.get(*key).is_some_and(Value::is_array));
    if !has_composition {
        object.insert("type".into(), json!("object"));
        if object.get("properties").is_none_or(Value::is_null) {
            object.insert("properties".into(), json!({}));
        }
        return Value::Object(object);
    }
    let mut properties = match object.get("properties") {
        Some(Value::Object(map)) => map.clone(),
        _ => Map::new(),
    };
    let mut required: Vec<String> = match object.get("required") {
        Some(Value::Array(list)) => list.iter().filter_map(|v| v.as_str().map(str::to_string)).collect(),
        _ => Vec::new(),
    };
    for key in composition {
        let Some(Value::Array(variants)) = object.get(key) else {
            continue;
        };
        for variant in variants {
            if let Some(Value::Object(props)) = variant.get("properties") {
                for (name, schema) in props {
                    properties.insert(name.clone(), schema.clone());
                }
            }
            if key == "allOf"
                && let Some(Value::Array(list)) = variant.get("required")
            {
                for item in list.iter().filter_map(Value::as_str) {
                    if !required.iter().any(|r| r == item) {
                        required.push(item.to_string());
                    }
                }
            }
        }
    }
    for key in composition.iter().chain(["type", "properties", "required"].iter()) {
        object.remove(*key);
    }
    object.insert("type".into(), json!("object"));
    object.insert("properties".into(), Value::Object(properties));
    if !required.is_empty() {
        object.insert("required".into(), json!(required));
    }
    Value::Object(object)
}

fn cache_control(retention: CacheRetention) -> Option<Value> {
    match retention {
        CacheRetention::None => None,
        CacheRetention::Short => Some(json!({ "type": "ephemeral" })),
        CacheRetention::Long => Some(json!({ "type": "ephemeral", "ttl": "1h" })),
    }
}

fn blocks_mut<'a>(body: &'a mut Value, key: &str) -> Option<&'a mut Vec<Value>> {
    body.get_mut(key).and_then(Value::as_array_mut)
}

fn mark_last(blocks: &mut [Value], cc: &Value) {
    if let Some(Value::Object(block)) = blocks.last_mut() {
        block.insert("cache_control".into(), cc.clone());
    }
}

fn mark_last_text(blocks: &mut [Value], cc: &Value) {
    for block in blocks.iter_mut().rev() {
        if block["type"] == "text"
            && let Value::Object(map) = block
        {
            map.insert("cache_control".into(), cc.clone());
            return;
        }
    }
    mark_last(blocks, cc);
}

fn mark_message(message: &mut Value, cc: &Value) {
    match message.get("content").cloned() {
        Some(Value::String(text)) => {
            message["content"] = json!([{ "type": "text", "text": text, "cache_control": cc }]);
        }
        Some(Value::Array(_)) => {
            if let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) {
                mark_last_text(blocks, cc);
            }
        }
        _ => {}
    }
}

/// Explicit `cache_control` breakpoints: tools, system, the second-to-last and last user turn.
fn apply_prompt_caching(body: &mut Value, cc: &Value, limit: usize, skip_last_user: bool, uncached_system_tail: bool) {
    if limit == 0 {
        return;
    }
    let already = body["messages"].as_array().is_some_and(|messages| {
        messages.iter().any(|m| {
            m["content"]
                .as_array()
                .is_some_and(|blocks| blocks.iter().any(|b| !b["cache_control"].is_null()))
        })
    });
    if already {
        return;
    }
    let mut used = 0;
    if let Some(tools) = blocks_mut(body, "tools")
        && !tools.is_empty()
    {
        mark_last(tools, cc);
        used += 1;
    }
    if used >= limit {
        return;
    }
    if let Some(system) = blocks_mut(body, "system")
        && !system.is_empty()
    {
        let index = system.len() - if uncached_system_tail && system.len() > 1 { 2 } else { 1 };
        if let Value::Object(map) = &mut system[index] {
            map.insert("cache_control".into(), cc.clone());
        }
        used += 1;
    }
    if used >= limit {
        return;
    }
    let Some(messages) = blocks_mut(body, "messages") else {
        return;
    };
    let user_indexes: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m["role"] == "user")
        .map(|(i, _)| i)
        .collect();
    if user_indexes.len() >= 2 {
        mark_message(&mut messages[user_indexes[user_indexes.len() - 2]], cc);
        used += 1;
    }
    if used >= limit || skip_last_user {
        return;
    }
    if let Some(&last) = user_indexes.last() {
        mark_message(&mut messages[last], cc);
    }
}

fn count_breakpoints(body: &Value) -> usize {
    let count = |blocks: Option<&Vec<Value>>| {
        blocks.map_or(0, |b| b.iter().filter(|x| !x["cache_control"].is_null()).count())
    };
    let mut total = count(body["tools"].as_array()) + count(body["system"].as_array());
    if let Some(messages) = body["messages"].as_array() {
        for message in messages {
            total += count(message["content"].as_array());
        }
    }
    total
}

fn enforce_cache_limit(body: &mut Value, limit: usize) {
    let mut excess = count_breakpoints(body).saturating_sub(limit);
    if excess == 0 {
        return;
    }
    let strip = |blocks: &mut Vec<Value>, excess: &mut usize| {
        for block in blocks {
            if *excess == 0 {
                break;
            }
            if let Value::Object(map) = block
                && map.remove("cache_control").is_some()
            {
                *excess -= 1;
            }
        }
    };
    if let Some(messages) = blocks_mut(body, "messages") {
        for message in messages {
            if let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) {
                strip(blocks, &mut excess);
            }
        }
    }
    for key in ["system", "tools"] {
        if excess > 0
            && let Some(blocks) = blocks_mut(body, key)
        {
            strip(blocks, &mut excess);
        }
    }
}

/// A 1-hour breakpoint may not follow a 5-minute one.
fn normalize_ttl_ordering(body: &mut Value) {
    let mut seen_short = false;
    let mut visit = |blocks: &mut Vec<Value>| {
        for block in blocks {
            let Some(cc) = block.get_mut("cache_control").and_then(Value::as_object_mut) else {
                continue;
            };
            if cc.get("ttl").and_then(Value::as_str) != Some("1h") {
                seen_short = true;
            } else if seen_short {
                cc.remove("ttl");
            }
        }
    };
    for key in ["tools", "system"] {
        if let Some(blocks) = blocks_mut(body, key) {
            visit(blocks);
        }
    }
    if let Some(messages) = blocks_mut(body, "messages") {
        for message in messages {
            if let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) {
                visit(blocks);
            }
        }
    }
}

pub fn build_body(req: &MessagesRequest<'_>, compat: &MessagesCompat) -> Value {
    let (system, messages) = build_messages(req, compat);
    let omitted_max = compat.default_max_tokens;
    let mut body = Map::new();
    body.insert("model".into(), json!(req.model));
    body.insert("messages".into(), Value::Array(messages));
    body.insert("stream".into(), json!(true));
    body.insert("max_tokens".into(), json!(req.max_output_tokens.unwrap_or(omitted_max)));

    let mut system_blocks: Vec<Value> = Vec::new();
    if compat.oauth {
        system_blocks.push(json!({ "type": "text", "text": OAUTH_SYSTEM_INSTRUCTION }));
    }
    system_blocks.extend(system.iter().map(|text| json!({ "type": "text", "text": text })));
    if !system_blocks.is_empty() {
        body.insert("system".into(), Value::Array(system_blocks));
    }

    let tools: Vec<Value> = req
        .tools
        .iter()
        .map(|t| {
            let mut tool = Map::new();
            tool.insert("name".into(), json!(tool_wire_name(compat, &t.wire_name)));
            tool.insert("description".into(), json!(t.description));
            tool.insert("input_schema".into(), normalize_input_schema(&t.parameters));
            if t.strict {
                tool.insert("strict".into(), json!(true));
            }
            Value::Object(tool)
        })
        .collect();
    let has_tools = !tools.is_empty();
    if has_tools {
        body.insert("tools".into(), Value::Array(tools));
    }

    // Thinking. Codex sends an effort string; "none" means off.
    let family = family_version(req.model);
    let native_contract = family
        .as_ref()
        .is_some_and(|v| matches!(v.0.as_str(), "haiku" | "sonnet" | "opus" | "fable"))
        || uses_adaptive_thinking(req.model);
    let effort = req.reasoning_effort;
    if effort == Some("none") && supports_explicit_thinking_disable(req.model) {
        body.insert("thinking".into(), json!({ "type": "disabled" }));
    } else if effort == Some("none") && uses_between_tools_floor(req.model) {
        body.insert("thinking".into(), json!({ "type": "between_tools" }));
    } else if let Some(effort) = effort.filter(|e| *e != "none")
        && (native_contract || family.is_none())
    {
        if uses_adaptive_thinking(req.model) {
            body.insert(
                "thinking".into(),
                json!({ "type": "adaptive", "display": "summarized" }),
            );
            let effort = adaptive_effort(effort);
            body.insert("output_config".into(), json!({ "effort": effort }));
            let want = reasoning_budget(effort);
            let floor = want + OUTPUT_HEADROOM;
            let max_tokens = match req.max_output_tokens {
                Some(explicit) => explicit,
                None => omitted_max
                    .max(ADAPTIVE_THINKING_CEILING.min(DEFAULT_MAX_TOKENS.max(floor))),
            };
            body.insert("max_tokens".into(), json!(max_tokens));
        } else {
            // max_tokens must exceed budget_tokens (>= 1024); size both inside a model-safe ceiling.
            let max_out = req.max_output_tokens.unwrap_or(omitted_max);
            let want = reasoning_budget(effort);
            let ceiling = omitted_max.max(REASONING_MAX_TOKENS_CEILING);
            let max_tokens = ceiling.min(max_out.max(want + OUTPUT_HEADROOM));
            let budget = MIN_THINKING_BUDGET.max(want.min(max_tokens.saturating_sub(OUTPUT_FLOOR)));
            body.insert("max_tokens".into(), json!(max_tokens));
            body.insert("thinking".into(), json!({ "type": "enabled", "budget_tokens": budget }));
        }
    }

    if let Some(format) = req.json_schema
        && let Some(schema) = format.get("schema")
    {
        let mut output_config = match body.remove("output_config") {
            Some(Value::Object(map)) => map,
            _ => Map::new(),
        };
        output_config.insert("format".into(), json!({ "type": "json_schema", "schema": schema }));
        body.insert("output_config".into(), Value::Object(output_config));
    }

    // tool_choice
    let mut tool_choice: Option<Value> = None;
    if has_tools || req.tool_choice == "none" {
        tool_choice = match req.tool_choice {
            "auto" => Some(json!({ "type": "auto" })),
            "none" => Some(json!({ "type": "none" })),
            "required" => Some(json!({ "type": "any" })),
            _ => None,
        };
    }
    if tool_choice.is_none() && has_tools && !req.parallel_tool_calls {
        tool_choice = Some(json!({ "type": "auto" }));
    }
    if let Some(choice) = tool_choice.as_mut()
        && rejects_forced_tool_choice(req.model)
        && matches!(choice["type"].as_str(), Some("any" | "tool"))
    {
        *choice = json!({ "type": "auto" });
    }
    if let Some(choice) = tool_choice.as_mut()
        && !req.parallel_tool_calls
        && choice["type"] != "none"
    {
        choice["disable_parallel_tool_use"] = json!(true);
    }
    if let Some(choice) = tool_choice {
        body.insert("tool_choice".into(), choice);
    }

    let mut body = Value::Object(body);
    if let Some(cc) = cache_control(compat.cache) {
        let automatic = compat.native_endpoint;
        if automatic {
            body["cache_control"] = cc.clone();
        }
        let limit = if automatic { MAX_CACHE_BREAKPOINTS - 1 } else { MAX_CACHE_BREAKPOINTS };
        let tail = system.len() > 1;
        apply_prompt_caching(&mut body, &cc, limit, automatic, tail);
        enforce_cache_limit(&mut body, limit);
        normalize_ttl_ordering(&mut body);
    }
    body
}

// --------------------------------------------------------------------- stream

fn tokens(usage: &Value, key: &str) -> Option<i64> {
    match usage.get(key) {
        None | Some(Value::Null) => Some(0),
        Some(v) => v.as_i64().filter(|n| *n >= 0),
    }
}

/// `input_tokens` counts only uncached input; the pool-wide convention is inclusive.
fn usage_from(usage: &Value) -> Option<Usage> {
    if !usage.is_object() {
        return None;
    }
    let input = tokens(usage, "input_tokens")?;
    let output = tokens(usage, "output_tokens")?;
    let read = tokens(usage, "cache_read_input_tokens")?;
    let write = tokens(usage, "cache_creation_input_tokens")?;
    let thinking = usage
        .get("output_tokens_details")
        .and_then(|d| d.get("thinking_tokens"))
        .and_then(Value::as_i64)
        .filter(|t| *t >= 0 && *t <= output)
        .unwrap_or(0);
    Some(Usage {
        input_tokens: input + read + write,
        output_tokens: output,
        cached_input_tokens: read,
        cache_write_input_tokens: write,
        reasoning_output_tokens: thinking,
    })
}

/// `error.type` → HTTP status, for errors that arrive inside the stream.
fn status_for_error_type(error_type: &str) -> Option<u16> {
    match error_type {
        "invalid_request_error" => Some(400),
        "authentication_error" => Some(401),
        "permission_error" => Some(403),
        "not_found_error" => Some(404),
        "request_too_large" => Some(413),
        "rate_limit_error" => Some(429),
        "api_error" => Some(500),
        "overloaded_error" => Some(529),
        _ => None,
    }
}

/// Parses Messages SSE payloads into [`AdapterEvent`]s.
pub struct MessagesStream {
    compat: MessagesCompat,
    current_block: String,
    tool_args: String,
    usage: Value,
    usage_invalid: bool,
    stop_reason: Option<String>,
    done: bool,
    minted: u32,
}

impl MessagesStream {
    pub fn new(compat: MessagesCompat) -> Self {
        Self {
            compat,
            current_block: String::new(),
            tool_args: String::new(),
            usage: Value::Null,
            usage_invalid: false,
            stop_reason: None,
            done: false,
            minted: 0,
        }
    }

    pub fn is_done(&self) -> bool {
        self.done
    }

    /// `message_delta.usage` values are cumulative: later frames win per key.
    fn merge_usage(&mut self, next: Option<&Value>) {
        let Some(next) = next else { return };
        if self.usage_invalid {
            return;
        }
        let Some(next) = next.as_object() else {
            self.usage_invalid = true;
            return;
        };
        let mut merged = match &self.usage {
            Value::Object(map) => map.clone(),
            _ => Map::new(),
        };
        for (key, value) in next {
            merged.insert(key.clone(), value.clone());
        }
        let merged = Value::Object(merged);
        if usage_from(&merged).is_none() {
            self.usage_invalid = true;
        } else {
            self.usage = merged;
        }
    }

    fn done_event(&mut self) -> Vec<AdapterEvent> {
        self.done = true;
        if self.stop_reason.as_deref() == Some("error") {
            return vec![AdapterEvent::Error(AdapterFailure {
                message: "upstream ended the turn with stop_reason \"error\"".to_string(),
                status: Some(502),
                error_type: Some("upstream_error".to_string()),
                code: None,
            })];
        }
        let usage = if self.usage_invalid { None } else { usage_from(&self.usage) };
        vec![AdapterEvent::Done {
            usage,
            stop_reason: self.stop_reason.take(),
        }]
    }

    /// One SSE payload (`data:` field).
    pub fn feed(&mut self, data: &str) -> Vec<AdapterEvent> {
        if self.done {
            return Vec::new();
        }
        let payload = data.trim();
        if payload.is_empty() {
            return Vec::new();
        }
        let Ok(value) = serde_json::from_str::<Value>(payload) else {
            return Vec::new();
        };
        let Some(kind) = value.get("type").and_then(Value::as_str) else {
            return Vec::new();
        };
        match kind {
            "ping" => vec![AdapterEvent::Heartbeat],
            "message_start" => {
                self.merge_usage(value.get("message").and_then(|m| m.get("usage")));
                Vec::new()
            }
            "content_block_start" => {
                let Some(block) = value.get("content_block") else {
                    return Vec::new();
                };
                let block_type = block.get("type").and_then(Value::as_str).unwrap_or("");
                self.current_block = block_type.to_string();
                match block_type {
                    "thinking" => {
                        let text = block.get("thinking").and_then(Value::as_str).unwrap_or("");
                        vec![AdapterEvent::ThinkingDelta(text.to_string())]
                    }
                    "redacted_thinking" => block
                        .get("data")
                        .and_then(Value::as_str)
                        .map(|d| vec![AdapterEvent::RedactedThinking(d.to_string())])
                        .unwrap_or_default(),
                    "tool_use" => {
                        self.tool_args.clear();
                        self.minted += 1;
                        let id = block
                            .get("id")
                            .and_then(Value::as_str)
                            .filter(|id| crate::ids::is_valid(id))
                            .map(str::to_string)
                            .unwrap_or_else(|| format!("toolu_minted_{}", self.minted));
                        let name = block.get("name").and_then(Value::as_str).unwrap_or("");
                        vec![AdapterEvent::ToolCallStart {
                            id,
                            name: tool_name_from_wire(&self.compat, name),
                        }]
                    }
                    _ => Vec::new(),
                }
            }
            "content_block_delta" => {
                let Some(delta) = value.get("delta") else {
                    return Vec::new();
                };
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => delta
                        .get("text")
                        .and_then(Value::as_str)
                        .filter(|t| !t.is_empty())
                        .map(|t| vec![AdapterEvent::TextDelta(t.to_string())])
                        .unwrap_or_default(),
                    Some("thinking_delta") => delta
                        .get("thinking")
                        .and_then(Value::as_str)
                        .map(|t| vec![AdapterEvent::ThinkingDelta(t.to_string())])
                        .unwrap_or_default(),
                    Some("signature_delta") if self.current_block == "thinking" => delta
                        .get("signature")
                        .and_then(Value::as_str)
                        .map(|s| vec![AdapterEvent::ThinkingSignature(s.to_string())])
                        .unwrap_or_default(),
                    Some("input_json_delta") if self.current_block == "tool_use" => delta
                        .get("partial_json")
                        .and_then(Value::as_str)
                        .map(|p| {
                            self.tool_args.push_str(p);
                            vec![AdapterEvent::ToolCallDelta(p.to_string())]
                        })
                        .unwrap_or_default(),
                    _ => Vec::new(),
                }
            }
            "content_block_stop" => {
                let was_tool = self.current_block == "tool_use";
                self.current_block.clear();
                if !was_tool {
                    return Vec::new();
                }
                let args = std::mem::take(&mut self.tool_args);
                let mut events = Vec::new();
                if args.trim().is_empty() {
                    events.push(AdapterEvent::ToolCallDelta("{}".to_string()));
                } else if serde_json::from_str::<Value>(&args).is_err() {
                    self.done = true;
                    return vec![AdapterEvent::Error(AdapterFailure::new(
                        "Anthropic stream sent malformed tool_use arguments (invalid JSON)",
                    ))];
                }
                events.push(AdapterEvent::ToolCallEnd);
                events
            }
            "message_delta" => {
                self.merge_usage(value.get("usage"));
                if let Some(reason) = value
                    .get("delta")
                    .and_then(|d| d.get("stop_reason"))
                    .and_then(Value::as_str)
                {
                    self.stop_reason = Some(reason.to_string());
                }
                Vec::new()
            }
            "message_stop" => self.done_event(),
            "error" => {
                self.done = true;
                let error = value.get("error").unwrap_or(&Value::Null);
                let error_type = error.get("type").and_then(Value::as_str).map(str::to_string);
                let status = error
                    .get("status")
                    .and_then(Value::as_u64)
                    .and_then(|n| u16::try_from(n).ok())
                    .filter(|n| (400..=599).contains(n))
                    .or_else(|| error_type.as_deref().and_then(status_for_error_type));
                vec![AdapterEvent::Error(AdapterFailure {
                    message: error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("Anthropic error")
                        .to_string(),
                    status,
                    error_type,
                    code: error.get("code").and_then(Value::as_str).map(str::to_string),
                })]
            }
            _ => Vec::new(),
        }
    }

    /// The byte stream ended without `message_stop`.
    pub fn eof(&mut self) -> Vec<AdapterEvent> {
        if self.done {
            return Vec::new();
        }
        if self.stop_reason.is_some() {
            return self.done_event();
        }
        self.done = true;
        vec![AdapterEvent::Error(AdapterFailure::new(
            "upstream stream ended before message_stop — possible truncation",
        ))]
    }
}

/// Reasoning envelope helper for callers that need the signature of a finished block.
pub fn envelope_for(signature: Option<&str>, redacted: &[String]) -> Envelope {
    Envelope {
        sig: signature.map(str::to_string),
        red: redacted.to_vec(),
        txt: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::turns_from_items;
    use codex_protocol::models::ResponseItem;
    use pretty_assertions::assert_eq;

    fn turns(value: Value) -> Vec<Turn> {
        let items: Vec<ResponseItem> = serde_json::from_value(value).expect("items");
        turns_from_items(&items)
    }

    fn shell() -> ToolDecl {
        ToolDecl {
            wire_name: "shell".into(),
            namespace: None,
            name: "shell".into(),
            description: "run".into(),
            parameters: json!({"type": "object", "properties": {"cmd": {"type": "string"}}}),
            strict: false,
        }
    }

    fn body(value: Value, model: &str, effort: Option<&str>, compat: &MessagesCompat) -> Value {
        let t = turns(value);
        build_body(
            &MessagesRequest {
                model,
                turns: &t,
                tools: &[shell()],
                tool_choice: "auto",
                parallel_tool_calls: true,
                reasoning_effort: effort,
                max_output_tokens: None,
                json_schema: None,
            },
            compat,
        )
    }

    fn no_cache() -> MessagesCompat {
        MessagesCompat {
            cache: CacheRetention::None,
            ..MessagesCompat::default()
        }
    }

    #[test]
    fn model_ids_parse_like_the_original() {
        assert_eq!(family_version("claude-opus-4-8"), Some(("opus".into(), 4, 8)));
        assert_eq!(family_version("anthropic/claude-sonnet-5"), Some(("sonnet".into(), 5, 0)));
        assert_eq!(family_version("claude-opus-4-20250514"), Some(("opus".into(), 4, 0)));
        assert_eq!(family_version("claude-opus-4-8[1m]"), Some(("opus".into(), 4, 8)));
        assert_eq!(family_version("gpt-5"), None);
        assert!(uses_adaptive_thinking("claude-sonnet-5"));
        assert!(!uses_adaptive_thinking("claude-sonnet-4-6"));
        assert!(uses_adaptive_thinking("claude-opus-4-7"));
        assert!(!uses_adaptive_thinking("claude-haiku-4-5"));
        assert!(uses_between_tools_floor("claude-sonnet-5-5"));
        assert!(rejects_forced_tool_choice("claude-opus-5-5"));
        assert!(!rejects_forced_tool_choice("claude-opus-5"));
        assert!(supports_explicit_thinking_disable("claude-sonnet-5"));
        assert!(!supports_explicit_thinking_disable("claude-sonnet-5-5"));
    }

    #[test]
    fn a_tool_round_trip_pairs_use_and_result_and_keeps_thinking_signatures() {
        let signed = Envelope {
            sig: Some("SIG".into()),
            ..Envelope::default()
        }
        .encode();
        let b = body(
            json!([
                {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "rules"}]},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "go"}]},
                {"type": "reasoning", "summary": [{"type": "summary_text", "text": "plan"}], "encrypted_content": signed},
                {"type": "function_call", "name": "shell", "arguments": "{\"cmd\":\"ls\"}", "call_id": "call_1"},
                {"type": "function_call_output", "call_id": "call_1", "output": "a.txt"}
            ]),
            "claude-sonnet-4-5",
            None,
            &no_cache(),
        );
        assert_eq!(b["system"], json!([{"type": "text", "text": "rules"}]));
        let messages = b["messages"].as_array().expect("messages");
        assert_eq!(messages[0], json!({"role": "user", "content": "go"}));
        // Signed thinking first, then the tool_use, in the original order.
        assert_eq!(messages[1]["content"][0]["type"], "thinking");
        assert_eq!(messages[1]["content"][0]["signature"], "SIG");
        assert_eq!(messages[1]["content"][1]["type"], "tool_use");
        assert_eq!(messages[1]["content"][1]["input"], json!({"cmd": "ls"}));
        assert_eq!(messages[2]["role"], "user");
        assert_eq!(messages[2]["content"][0]["type"], "tool_result");
        assert_eq!(messages[2]["content"][0]["tool_use_id"], "call_1");
        assert_eq!(messages[2]["content"][0]["content"], "a.txt");
        assert_eq!(b["max_tokens"], 8192);
        assert_eq!(b["tools"][0]["input_schema"]["type"], "object");
    }

    #[test]
    fn unsigned_thinking_is_dropped_missing_results_are_marked_and_history_ends_on_user() {
        let b = body(
            json!([
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "go"}]},
                {"type": "reasoning", "summary": [{"type": "summary_text", "text": "plan"}], "encrypted_content": null},
                {"type": "function_call", "name": "shell", "arguments": "{}", "call_id": "call_1"},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "done"}]}
            ]),
            "claude-sonnet-4-5",
            None,
            &no_cache(),
        );
        let messages = b["messages"].as_array().expect("messages");
        assert_eq!(messages.len(), 3);
        // No thinking block without a signature; text first, then the tool_use.
        assert_eq!(messages[1]["content"][0]["type"], "text");
        assert_eq!(messages[1]["content"][1]["type"], "tool_use");
        assert_eq!(messages[2]["content"][0]["is_error"], true);
        assert_eq!(messages[2]["content"][0]["content"], "[missing tool_result for this tool_use in history]");
    }

    #[test]
    fn an_assistant_tail_gets_a_continue_message() {
        let b = body(
            json!([
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "go"}]},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "partial"}]}
            ]),
            "claude-sonnet-4-5",
            None,
            &no_cache(),
        );
        let messages = b["messages"].as_array().expect("messages");
        assert_eq!(messages.last().expect("last"), &json!({"role": "user", "content": "(continue)"}));
    }

    #[test]
    fn thinking_follows_the_model_family_contract() {
        let items = json!([{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}]);
        // Budget thinking: max_tokens > budget_tokens.
        let b = body(items.clone(), "claude-sonnet-4-5", Some("high"), &no_cache());
        assert_eq!(b["thinking"]["type"], "enabled");
        let budget = b["thinking"]["budget_tokens"].as_u64().expect("budget");
        assert!(b["max_tokens"].as_u64().expect("max") > budget && budget >= 1024);
        // Adaptive models: effort knob instead of a budget.
        let b = body(items.clone(), "claude-sonnet-5", Some("high"), &no_cache());
        assert_eq!(b["thinking"]["type"], "adaptive");
        assert_eq!(b["output_config"]["effort"], "high");
        assert!(b["max_tokens"].as_u64().expect("max") >= 16_384 + 8192);
        // "minimal" is not an adaptive effort.
        let b = body(items.clone(), "claude-opus-4-8", Some("minimal"), &no_cache());
        assert_eq!(b["output_config"]["effort"], "low");
        // Off switches differ by family.
        assert_eq!(body(items.clone(), "claude-sonnet-5", Some("none"), &no_cache())["thinking"]["type"], "disabled");
        assert_eq!(body(items.clone(), "claude-sonnet-5-5", Some("none"), &no_cache())["thinking"]["type"], "between_tools");
        assert!(body(items, "claude-opus-4-8", Some("none"), &no_cache())["thinking"].is_null());
    }

    #[test]
    fn oauth_adds_the_identity_block_and_the_tool_prefix_and_tool_choice_is_downgraded() {
        let compat = MessagesCompat {
            oauth: true,
            ..no_cache()
        };
        let t = turns(json!([{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}]));
        let b = build_body(
            &MessagesRequest {
                model: "claude-sonnet-5-5",
                turns: &t,
                tools: &[shell()],
                tool_choice: "required",
                parallel_tool_calls: false,
                reasoning_effort: None,
                max_output_tokens: Some(2000),
                json_schema: None,
            },
            &compat,
        );
        assert_eq!(b["system"][0]["text"], OAUTH_SYSTEM_INSTRUCTION);
        assert_eq!(b["tools"][0]["name"], "custom_shell");
        // Sonnet 5.5 rejects a forced choice: auto, with one tool call at a time.
        assert_eq!(b["tool_choice"], json!({"type": "auto", "disable_parallel_tool_use": true}));
        assert_eq!(b["max_tokens"], 2000);
        assert_eq!(tool_name_from_wire(&compat, "custom_shell"), "shell");
    }

    #[test]
    fn caching_marks_tools_system_and_users_within_the_breakpoint_limit() {
        let b = body(
            json!([
                {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "rules"}]},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "one"}]},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "a"}]},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "two"}]}
            ]),
            "claude-sonnet-4-5",
            None,
            &MessagesCompat::default(),
        );
        assert_eq!(b["cache_control"], json!({"type": "ephemeral", "ttl": "1h"}));
        assert!(!b["tools"][0]["cache_control"].is_null());
        assert!(!b["system"][0]["cache_control"].is_null());
        assert!(count_breakpoints(&b) <= MAX_CACHE_BREAKPOINTS);
    }

    fn run(frames: &[&str]) -> Vec<AdapterEvent> {
        let mut stream = MessagesStream::new(MessagesCompat::default());
        let mut events = Vec::new();
        for frame in frames {
            events.extend(stream.feed(frame));
        }
        events.extend(stream.eof());
        events
    }

    #[test]
    fn streams_thinking_with_signature_text_and_a_tool_call() {
        let events = run(&[
            r#"{"type":"message_start","message":{"usage":{"input_tokens":10,"cache_read_input_tokens":5,"output_tokens":1}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hm"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"SIG"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"ok"}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
            r#"{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_1","name":"shell","input":{}}}"#,
            r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"cmd\":"}}"#,
            r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"\"ls\"}"}}"#,
            r#"{"type":"content_block_stop","index":2}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":9,"output_tokens_details":{"thinking_tokens":4}}}"#,
            r#"{"type":"message_stop"}"#,
        ]);
        assert_eq!(
            events,
            vec![
                AdapterEvent::ThinkingDelta(String::new()),
                AdapterEvent::ThinkingDelta("hm".into()),
                AdapterEvent::ThinkingSignature("SIG".into()),
                AdapterEvent::TextDelta("ok".into()),
                AdapterEvent::ToolCallStart { id: "toolu_1".into(), name: "shell".into() },
                AdapterEvent::ToolCallDelta("{\"cmd\":".into()),
                AdapterEvent::ToolCallDelta("\"ls\"}".into()),
                AdapterEvent::ToolCallEnd,
                AdapterEvent::Done {
                    // Anthropic counts only uncached input; the total includes the cache read.
                    usage: Some(Usage {
                        input_tokens: 15,
                        output_tokens: 9,
                        cached_input_tokens: 5,
                        cache_write_input_tokens: 0,
                        reasoning_output_tokens: 4,
                    }),
                    stop_reason: Some("tool_use".into()),
                },
            ]
        );
    }

    #[test]
    fn empty_tool_arguments_become_an_empty_object_and_errors_carry_a_status() {
        let events = run(&[
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"shell","input":{}}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"error","error":{"type":"overloaded_error","message":"busy"}}"#,
        ]);
        assert!(events.contains(&AdapterEvent::ToolCallDelta("{}".into())));
        assert!(matches!(events.last(), Some(AdapterEvent::Error(f)) if f.status == Some(529) && f.message == "busy"));

        let cut = run(&[r#"{"type":"message_start","message":{"usage":{"input_tokens":1}}}"#]);
        assert!(matches!(cut.last(), Some(AdapterEvent::Error(f)) if f.message.contains("truncation")));

        let bad = run(&[
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t","name":"shell","input":{}}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{oops"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
        ]);
        assert!(matches!(bad.last(), Some(AdapterEvent::Error(f)) if f.message.contains("malformed tool_use")));
    }
}
