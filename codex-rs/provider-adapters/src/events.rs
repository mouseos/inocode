//! [`AdapterEvent`]s → Responses-shaped output items (ported from the ordering rules of OpenCodex
//! `bridge/sse.ts` `bridgeToResponsesSSE`; typed events replace the SSE text it writes).
//!
//! Rules kept from the original:
//! * one open item at a time — a text delta after reasoning closes the reasoning, a tool call
//!   closes any open message or reasoning, and so on;
//! * a reasoning item is closed only by the next meaningful event, so a late signature lands on
//!   the thinking block it belongs to;
//! * the provider's signature and redacted blocks leave inside the reasoning envelope;
//! * a tool the caller never declared fails the turn instead of reaching the tool runner;
//! * a turn cut short (`max_tokens`, content filter, refusal) is `Incomplete`, and the half-written
//!   tool call is discarded rather than executed.

use crate::envelope::Envelope;
use crate::ir::AdapterEvent;
use crate::ir::AdapterFailure;
use crate::ir::Out;
use crate::tool_names::ToolRegistry;
use codex_protocol::models::ResponseItem;
use serde_json::json;

struct Reasoning {
    summary: String,
    raw: String,
    signature: Option<String>,
    redacted: Vec<String>,
}

struct Call {
    call_id: String,
    namespace: Option<String>,
    name: String,
    args: String,
}

pub struct Assembler {
    registry: ToolRegistry,
    response_id: String,
    created: bool,
    finished: bool,
    message: Option<String>,
    reasoning: Option<Reasoning>,
    call: Option<Call>,
}

fn item(value: serde_json::Value) -> Option<ResponseItem> {
    serde_json::from_value(value).ok()
}

impl Assembler {
    pub fn new(registry: ToolRegistry, response_id: impl Into<String>) -> Self {
        Self {
            registry,
            response_id: response_id.into(),
            created: false,
            finished: false,
            message: None,
            reasoning: None,
            call: None,
        }
    }

    pub fn is_finished(&self) -> bool {
        self.finished
    }

    fn start(&mut self, out: &mut Vec<Out>) {
        if !self.created {
            self.created = true;
            out.push(Out::Created);
        }
    }

    fn close_message(&mut self, out: &mut Vec<Out>, phase: &str) {
        let Some(text) = self.message.take() else { return };
        if let Some(done) = item(json!({
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": text}],
            "phase": phase,
        })) {
            out.push(Out::ItemDone(done));
        }
    }

    fn close_reasoning(&mut self, out: &mut Vec<Out>) {
        let Some(reasoning) = self.reasoning.take() else { return };
        let envelope = Envelope {
            sig: reasoning.signature,
            red: reasoning.redacted,
            txt: None,
        };
        let summary = if reasoning.summary.is_empty() {
            json!([])
        } else {
            json!([{"type": "summary_text", "text": reasoning.summary}])
        };
        let content = if reasoning.raw.is_empty() {
            serde_json::Value::Null
        } else {
            json!([{"type": "reasoning_text", "text": reasoning.raw}])
        };
        let encrypted = if envelope.is_empty() {
            serde_json::Value::Null
        } else {
            json!(envelope.encode())
        };
        if let Some(done) = item(json!({
            "type": "reasoning",
            "summary": summary,
            "content": content,
            "encrypted_content": encrypted,
        })) {
            out.push(Out::ItemDone(done));
        }
    }

    fn call_item(call: &Call, arguments: &str) -> Option<ResponseItem> {
        item(json!({
            "type": "function_call",
            "name": call.name,
            "namespace": call.namespace,
            "arguments": arguments,
            "call_id": call.call_id,
        }))
    }

    fn close_call(&mut self, out: &mut Vec<Out>) {
        let Some(call) = self.call.take() else { return };
        let args = if call.args.trim().is_empty() {
            "{}".to_string()
        } else {
            call.args.clone()
        };
        if let Some(done) = Self::call_item(&call, &args) {
            out.push(Out::ItemDone(done));
        }
    }

    fn close_all(&mut self, out: &mut Vec<Out>, message_phase: &str) {
        self.close_message(out, message_phase);
        self.close_reasoning(out);
        self.close_call(out);
    }

    fn open_reasoning(&mut self, out: &mut Vec<Out>) {
        self.close_message(out, "commentary");
        if self.call.is_some() {
            self.close_call(out);
        }
        if self.reasoning.is_none() {
            self.reasoning = Some(Reasoning {
                summary: String::new(),
                raw: String::new(),
                signature: None,
                redacted: Vec::new(),
            });
            if let Some(added) = item(json!({
                "type": "reasoning", "summary": [], "content": null, "encrypted_content": null,
            })) {
                out.push(Out::ItemAdded(added));
            }
        }
    }

    pub fn push(&mut self, event: AdapterEvent) -> Vec<Out> {
        let mut out = Vec::new();
        if self.finished {
            return out;
        }
        match event {
            AdapterEvent::Heartbeat => {}
            AdapterEvent::TextDelta(text) => {
                if text.is_empty() {
                    return out;
                }
                self.start(&mut out);
                self.close_reasoning(&mut out);
                self.close_call(&mut out);
                match self.message.as_mut() {
                    Some(current) => current.push_str(&text),
                    None => {
                        if let Some(added) = item(json!({
                            "type": "message", "role": "assistant", "content": [],
                        })) {
                            out.push(Out::ItemAdded(added));
                        }
                        self.message = Some(text.clone());
                    }
                }
                out.push(Out::TextDelta(text));
            }
            AdapterEvent::ThinkingDelta(text) => {
                self.start(&mut out);
                self.open_reasoning(&mut out);
                if let Some(reasoning) = self.reasoning.as_mut() {
                    reasoning.summary.push_str(&text);
                }
                out.push(Out::ReasoningSummaryDelta {
                    delta: text,
                    summary_index: 0,
                });
            }
            AdapterEvent::ReasoningRawDelta(text) => {
                self.start(&mut out);
                self.open_reasoning(&mut out);
                if let Some(reasoning) = self.reasoning.as_mut() {
                    reasoning.raw.push_str(&text);
                }
                out.push(Out::ReasoningContentDelta {
                    delta: text,
                    content_index: 0,
                });
            }
            AdapterEvent::ThinkingSignature(signature) => {
                self.start(&mut out);
                self.open_reasoning(&mut out);
                if let Some(reasoning) = self.reasoning.as_mut() {
                    reasoning.signature = Some(signature);
                }
            }
            AdapterEvent::RedactedThinking(data) => {
                self.start(&mut out);
                self.open_reasoning(&mut out);
                if let Some(reasoning) = self.reasoning.as_mut() {
                    reasoning.redacted.push(data);
                }
            }
            AdapterEvent::ToolCallStart { id, name } => {
                self.start(&mut out);
                self.close_message(&mut out, "commentary");
                self.close_reasoning(&mut out);
                self.close_call(&mut out);
                let Some((namespace, real_name)) = self.registry.resolve(&name) else {
                    self.finished = true;
                    out.push(Out::Failed(AdapterFailure {
                        message: format!("the model called a tool that was not declared: {name}"),
                        status: Some(502),
                        error_type: Some("upstream_error".to_string()),
                        code: Some("undeclared_tool_call".to_string()),
                    }));
                    return out;
                };
                let call = Call {
                    call_id: id,
                    namespace,
                    name: real_name,
                    args: String::new(),
                };
                if let Some(added) = Self::call_item(&call, "") {
                    out.push(Out::ItemAdded(added));
                }
                self.call = Some(call);
            }
            AdapterEvent::ToolCallDelta(delta) => {
                if let Some(call) = self.call.as_mut() {
                    call.args.push_str(&delta);
                    out.push(Out::ToolArgsDelta {
                        item_id: call.call_id.clone(),
                        call_id: call.call_id.clone(),
                        delta,
                    });
                }
            }
            AdapterEvent::ToolCallEnd => self.close_call(&mut out),
            AdapterEvent::Done { usage, stop_reason } => {
                self.start(&mut out);
                self.finished = true;
                let truncation = truncation_reason(stop_reason.as_deref());
                if truncation.is_some() {
                    // A half-written call must never run.
                    self.call = None;
                }
                self.close_all(&mut out, if truncation.is_some() { "commentary" } else { "final_answer" });
                out.push(match truncation {
                    Some(reason) => Out::Incomplete {
                        response_id: self.response_id.clone(),
                        reason: reason.to_string(),
                        usage,
                    },
                    None => Out::Completed {
                        response_id: self.response_id.clone(),
                        usage,
                    },
                });
            }
            AdapterEvent::Error(failure) => {
                self.finished = true;
                out.push(Out::Failed(failure));
            }
        }
        out
    }
}

/// Provider stop reasons that mean the turn did not finish (ported from OpenCodex
/// `responses/truncated-stop-reason.ts`), as a Responses `incomplete_details.reason`.
pub fn truncation_reason(stop_reason: Option<&str>) -> Option<&'static str> {
    match stop_reason? {
        "max_tokens" | "length" | "max_output_tokens" => Some("max_output_tokens"),
        "content_filter" | "content-filter" | "refusal" => Some("content_filter"),
        "model_context_window_exceeded" => Some("context_window"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::ToolDecl;
    use pretty_assertions::assert_eq;

    fn registry() -> ToolRegistry {
        ToolRegistry::new(&[ToolDecl {
            wire_name: "mcp__files__read".into(),
            namespace: Some("mcp__files__".into()),
            name: "read".into(),
            description: String::new(),
            parameters: json!({}),
            strict: false,
        }])
    }

    fn kinds(out: &[Out]) -> Vec<&'static str> {
        out.iter()
            .map(|o| match o {
                Out::Created => "created",
                Out::ItemAdded(_) => "added",
                Out::ItemDone(_) => "done",
                Out::TextDelta(_) => "text",
                Out::ToolArgsDelta { .. } => "args",
                Out::ReasoningSummaryDelta { .. } => "summary",
                Out::ReasoningContentDelta { .. } => "content",
                Out::Completed { .. } => "completed",
                Out::Incomplete { .. } => "incomplete",
                Out::Failed(_) => "failed",
            })
            .collect()
    }

    fn run(events: Vec<AdapterEvent>) -> Vec<Out> {
        let mut assembler = Assembler::new(registry(), "resp_1");
        events.into_iter().flat_map(|e| assembler.push(e)).collect()
    }

    #[test]
    fn reasoning_then_text_then_a_namespaced_call() {
        let out = run(vec![
            AdapterEvent::ThinkingDelta("hm".into()),
            AdapterEvent::ThinkingSignature("sig".into()),
            AdapterEvent::TextDelta("ok".into()),
            AdapterEvent::ToolCallStart { id: "c1".into(), name: "mcp__files__read".into() },
            AdapterEvent::ToolCallDelta("{\"p\":1}".into()),
            AdapterEvent::ToolCallEnd,
            AdapterEvent::Done { usage: None, stop_reason: Some("tool_use".into()) },
        ]);
        assert_eq!(
            kinds(&out),
            vec![
                "created", "added", "summary", "done", "added", "text", "done", "added", "args",
                "done", "completed"
            ]
        );
        // The signature left inside the reasoning envelope.
        let Out::ItemDone(ResponseItem::Reasoning { encrypted_content, .. }) = &out[3] else {
            panic!("expected reasoning");
        };
        let envelope = Envelope::decode(encrypted_content.as_deref().unwrap_or_default());
        assert_eq!(envelope.and_then(|e| e.sig), Some("sig".to_string()));
        // The namespace was restored from the declared set.
        let Out::ItemDone(ResponseItem::FunctionCall { name, namespace, arguments, .. }) = &out[9]
        else {
            panic!("expected call");
        };
        assert_eq!(name, "read");
        assert_eq!(namespace.as_deref(), Some("mcp__files__"));
        assert_eq!(arguments, "{\"p\":1}");
    }

    #[test]
    fn an_undeclared_tool_fails_the_turn() {
        let out = run(vec![
            AdapterEvent::ToolCallStart { id: "c1".into(), name: "rm_rf".into() },
            AdapterEvent::Done { usage: None, stop_reason: None },
        ]);
        assert_eq!(kinds(&out), vec!["created", "failed"]);
    }

    #[test]
    fn a_truncated_turn_is_incomplete_and_drops_the_open_call() {
        let out = run(vec![
            AdapterEvent::TextDelta("part".into()),
            AdapterEvent::ToolCallStart { id: "c1".into(), name: "mcp__files__read".into() },
            AdapterEvent::ToolCallDelta("{\"p\"".into()),
            AdapterEvent::Done { usage: None, stop_reason: Some("max_tokens".into()) },
        ]);
        assert!(matches!(out.last(), Some(Out::Incomplete { reason, .. }) if reason == "max_output_tokens"));
        assert!(!out.iter().any(|o| matches!(o, Out::ItemDone(ResponseItem::FunctionCall { .. }))));
    }

    #[test]
    fn errors_end_the_stream_and_nothing_follows() {
        let mut assembler = Assembler::new(registry(), "resp_1");
        let failed = assembler.push(AdapterEvent::Error(AdapterFailure::new("boom")));
        assert_eq!(kinds(&failed), vec!["failed"]);
        assert!(assembler.push(AdapterEvent::TextDelta("late".into())).is_empty());
    }
}
