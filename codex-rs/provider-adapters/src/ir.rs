//! Provider-neutral request model and event model.
//!
//! `history::turns_from_items` turns Responses input items into [`Turn`]s; the wire modules
//! (`chat`, later `messages`) turn turns into a provider body. Streams go the other way:
//! a wire parser produces [`AdapterEvent`]s and `events::Assembler` turns them into [`Out`].

use codex_protocol::models::ResponseItem;

#[derive(Clone, Debug, PartialEq)]
pub enum Part {
    Text(String),
    /// A URL or `data:` URL.
    Image(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolCall {
    /// The call id as Codex knows it (`call_id`).
    pub id: String,
    pub namespace: Option<String>,
    pub name: String,
    /// JSON text.
    pub arguments: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolResult {
    pub call_id: String,
    pub name: Option<String>,
    pub content: Vec<Part>,
    pub is_error: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Thinking {
    pub text: String,
    /// Anthropic thinking signature, recovered from the reasoning envelope.
    pub signature: Option<String>,
    /// Anthropic `redacted_thinking` payloads, in order.
    pub redacted: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum AssistantPart {
    Text(String),
    Thinking(Thinking),
    Call(ToolCall),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Turn {
    /// `developer` / `system` messages.
    System(String),
    User(Vec<Part>),
    Assistant(Vec<AssistantPart>),
    ToolResult(ToolResult),
}

/// A function tool as declared to the wire.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolDecl {
    pub wire_name: String,
    pub namespace: Option<String>,
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
    pub strict: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Usage {
    /// Includes cached and cache-written input.
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cached_input_tokens: i64,
    pub cache_write_input_tokens: i64,
    pub reasoning_output_tokens: i64,
}

/// What a wire parser yields.
#[derive(Clone, Debug, PartialEq)]
pub enum AdapterEvent {
    TextDelta(String),
    /// Reasoning shown as a summary (Anthropic thinking).
    ThinkingDelta(String),
    /// Reasoning that is raw content (chat `reasoning_content`).
    ReasoningRawDelta(String),
    ThinkingSignature(String),
    RedactedThinking(String),
    ToolCallStart { id: String, name: String },
    ToolCallDelta(String),
    ToolCallEnd,
    Heartbeat,
    Done {
        usage: Option<Usage>,
        stop_reason: Option<String>,
    },
    Error(AdapterFailure),
}

#[derive(Clone, Debug, PartialEq)]
pub struct AdapterFailure {
    pub message: String,
    pub status: Option<u16>,
    pub error_type: Option<String>,
    pub code: Option<String>,
}

impl AdapterFailure {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            status: None,
            error_type: None,
            code: None,
        }
    }
}

/// What the assembler yields; the API crate maps these onto its own event type.
#[derive(Clone, Debug, PartialEq)]
pub enum Out {
    Created,
    ItemAdded(ResponseItem),
    ItemDone(ResponseItem),
    TextDelta(String),
    ToolArgsDelta {
        item_id: String,
        call_id: String,
        delta: String,
    },
    ReasoningSummaryDelta {
        delta: String,
        summary_index: i64,
    },
    ReasoningContentDelta {
        delta: String,
        content_index: i64,
    },
    Completed {
        response_id: String,
        usage: Option<Usage>,
    },
    /// The turn was cut short: `reason` is a Responses `incomplete_details.reason`
    /// (`max_output_tokens`, `content_filter`, `context_window`).
    Incomplete {
        response_id: String,
        reason: String,
        usage: Option<Usage>,
    },
    Failed(AdapterFailure),
}
