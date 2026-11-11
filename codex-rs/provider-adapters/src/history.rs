//! Responses input items → [`Turn`]s (replaces OpenCodex `responses/parser.ts`: InoCode receives
//! typed items, so only the grouping rules are needed).
//!
//! * `developer` / `system` messages become [`Turn::System`].
//! * Consecutive assistant output (reasoning, assistant messages, function calls) is one
//!   [`Turn::Assistant`], in order.
//! * A `function_call_output` becomes a [`Turn::ToolResult`].
//! * Items that have no wire form (tool search, additional tools, agent messages) are dropped.

use crate::envelope::Envelope;
use crate::ir::AssistantPart;
use crate::ir::Part;
use crate::ir::Thinking;
use crate::ir::ToolCall;
use crate::ir::ToolResult;
use crate::ir::Turn;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::ImageReference;
use codex_protocol::models::ReasoningItemContent;
use codex_protocol::models::ReasoningItemReasoningSummary;
use codex_protocol::models::ResponseItem;

pub fn turns_from_items(items: &[ResponseItem]) -> Vec<Turn> {
    let mut turns = Vec::new();
    let mut assistant: Vec<AssistantPart> = Vec::new();
    let flush = |turns: &mut Vec<Turn>, assistant: &mut Vec<AssistantPart>| {
        if !assistant.is_empty() {
            turns.push(Turn::Assistant(std::mem::take(assistant)));
        }
    };

    for item in items {
        match item {
            ResponseItem::Message { role, content, .. } => match role.as_str() {
                "assistant" => {
                    let text = join_text(content);
                    if !text.is_empty() {
                        assistant.push(AssistantPart::Text(text));
                    }
                }
                "developer" | "system" => {
                    flush(&mut turns, &mut assistant);
                    let text = join_text(content);
                    if !text.is_empty() {
                        turns.push(Turn::System(text));
                    }
                }
                _ => {
                    flush(&mut turns, &mut assistant);
                    let parts = user_parts(content);
                    if !parts.is_empty() {
                        turns.push(Turn::User(parts));
                    }
                }
            },
            ResponseItem::Reasoning {
                summary,
                content,
                encrypted_content,
                ..
            } => {
                let envelope = encrypted_content
                    .as_deref()
                    .and_then(Envelope::decode)
                    .unwrap_or_default();
                let mut text = String::new();
                for part in content.iter().flatten() {
                    match part {
                        ReasoningItemContent::ReasoningText { text: t }
                        | ReasoningItemContent::Text { text: t } => text.push_str(t),
                    }
                }
                if text.is_empty() {
                    for ReasoningItemReasoningSummary::SummaryText { text: t } in summary {
                        text.push_str(t);
                    }
                }
                if text.is_empty()
                    && let Some(hidden) = &envelope.txt
                {
                    text.clone_from(hidden);
                }
                if !text.is_empty() || envelope.sig.is_some() || !envelope.red.is_empty() {
                    assistant.push(AssistantPart::Thinking(Thinking {
                        text,
                        signature: envelope.sig,
                        redacted: envelope.red,
                    }));
                }
            }
            ResponseItem::FunctionCall {
                name,
                namespace,
                arguments,
                call_id,
                ..
            } => assistant.push(AssistantPart::Call(ToolCall {
                id: call_id.clone(),
                namespace: namespace.clone(),
                name: name.clone(),
                arguments: arguments.clone(),
            })),
            ResponseItem::CustomToolCall {
                call_id,
                name,
                namespace,
                input,
                ..
            } => assistant.push(AssistantPart::Call(ToolCall {
                id: call_id.clone(),
                namespace: namespace.clone(),
                name: name.clone(),
                arguments: serde_json::json!({ "input": input }).to_string(),
            })),
            ResponseItem::FunctionCallOutput {
                call_id,
                name,
                output,
                ..
            } => {
                flush(&mut turns, &mut assistant);
                turns.push(Turn::ToolResult(ToolResult {
                    call_id: call_id.clone().unwrap_or_default(),
                    name: name.clone(),
                    content: output_parts(&output.body),
                    is_error: output.success == Some(false),
                }));
            }
            ResponseItem::CustomToolCallOutput {
                call_id,
                name,
                output,
                ..
            } => {
                flush(&mut turns, &mut assistant);
                turns.push(Turn::ToolResult(ToolResult {
                    call_id: call_id.clone(),
                    name: name.clone(),
                    content: output_parts(&output.body),
                    is_error: output.success == Some(false),
                }));
            }
            _ => {}
        }
    }
    flush(&mut turns, &mut assistant);
    turns
}

fn join_text(content: &[ContentItem]) -> String {
    let mut text = String::new();
    for item in content {
        match item {
            ContentItem::InputText { text: t } | ContentItem::OutputText { text: t, .. } => {
                text.push_str(t);
            }
            _ => {}
        }
    }
    text
}

fn user_parts(content: &[ContentItem]) -> Vec<Part> {
    let mut parts = Vec::new();
    for item in content {
        match item {
            ContentItem::InputText { text } | ContentItem::OutputText { text, .. } => {
                if !text.is_empty() {
                    parts.push(Part::Text(text.clone()));
                }
            }
            ContentItem::InputImage { image, .. } => match image {
                ImageReference::Inline { image_url } => parts.push(Part::Image(image_url.clone())),
                ImageReference::File { .. } => parts.push(Part::Text("[image file]".to_string())),
            },
            ContentItem::InputAudio { .. } => parts.push(Part::Text("[audio]".to_string())),
        }
    }
    parts
}

fn output_parts(body: &FunctionCallOutputBody) -> Vec<Part> {
    match body {
        FunctionCallOutputBody::Text(text) => vec![Part::Text(text.clone())],
        FunctionCallOutputBody::ContentItems(items) => items
            .iter()
            .filter_map(|item| match item {
                FunctionCallOutputContentItem::InputText { text } => Some(Part::Text(text.clone())),
                FunctionCallOutputContentItem::InputImage {
                    image: ImageReference::Inline { image_url },
                    ..
                } => Some(Part::Image(image_url.clone())),
                FunctionCallOutputContentItem::InputImage { .. } => {
                    Some(Part::Text("[image file]".to_string()))
                }
                FunctionCallOutputContentItem::InputAudio { .. } => {
                    Some(Part::Text("[audio]".to_string()))
                }
                FunctionCallOutputContentItem::EncryptedContent { .. } => None,
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    fn items(value: serde_json::Value) -> Vec<ResponseItem> {
        serde_json::from_value(value).expect("items")
    }

    #[test]
    fn groups_assistant_output_and_pairs_results() {
        let turns = turns_from_items(&items(json!([
            {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "rules"}]},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]},
            {"type": "reasoning", "summary": [{"type": "summary_text", "text": "thinking"}], "encrypted_content": null},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "calling"}]},
            {"type": "function_call", "name": "shell", "arguments": "{\"cmd\":\"ls\"}", "call_id": "call_a"},
            {"type": "function_call_output", "call_id": "call_a", "output": "files"},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "thanks"}]}
        ])));
        assert_eq!(turns.len(), 5);
        assert_eq!(turns[0], Turn::System("rules".to_string()));
        assert_eq!(turns[1], Turn::User(vec![Part::Text("hi".to_string())]));
        let Turn::Assistant(parts) = &turns[2] else {
            panic!("expected assistant");
        };
        assert_eq!(parts.len(), 3);
        assert!(matches!(&parts[2], AssistantPart::Call(c) if c.id == "call_a"));
        let Turn::ToolResult(result) = &turns[3] else {
            panic!("expected tool result");
        };
        assert_eq!(result.content, vec![Part::Text("files".to_string())]);
    }

    #[test]
    fn a_signed_reasoning_item_keeps_its_signature() {
        let encrypted = Envelope {
            sig: Some("sig-1".to_string()),
            ..Envelope::default()
        }
        .encode();
        let turns = turns_from_items(&items(json!([
            {"type": "reasoning", "summary": [], "encrypted_content": encrypted}
        ])));
        let Turn::Assistant(parts) = &turns[0] else {
            panic!("expected assistant");
        };
        assert!(matches!(&parts[0],
            AssistantPart::Thinking(t) if t.signature.as_deref() == Some("sig-1")));
    }
}
