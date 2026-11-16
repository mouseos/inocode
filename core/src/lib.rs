//! Request handling for InoCode.
//!
//! A [`Request`] is decoded from a JSON frame payload and answered either with
//! a [`Response`] (immediate methods such as `ping` and `echo`) or, for `chat`,
//! with a stream of [`EventPayload`] events. The core knows nothing about TCP,
//! framing, or connections, so the server owns all transport concerns.

pub mod conversation;
pub mod deepai;
mod llm;
pub mod oauth;
pub mod reasoning;
mod sse;
mod usage;

pub use usage::{Usage, UsageWindow};

pub use llm::{
    ChatStream, Llm, LlmConfig, ModelInfo, ProviderKind, DEFAULT_BASE_URL, DEFAULT_MAX_TOKENS,
    DEFAULT_MODEL,
};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// One request decoded from a JSON frame payload.
///
/// Requests are only received, so the type deserializes but does not need to
/// serialize.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Request {
    /// Correlation id that responses and events must echo.
    pub id: u64,
    /// Method to run.
    pub method: String,
    /// Method arguments. `None` when the client omitted `params`.
    #[serde(default)]
    pub params: Option<Value>,
}

impl Request {
    /// Prompt of a `chat` request.
    pub fn prompt(&self) -> Result<&str, ErrorInfo> {
        self.params
            .as_ref()
            .and_then(|params| params.get("prompt"))
            .and_then(Value::as_str)
            .filter(|prompt| !prompt.trim().is_empty())
            .ok_or_else(|| {
                ErrorInfo::new(
                    "invalid_params",
                    "chat requires a non-empty \"prompt\" string",
                )
            })
    }

    /// Request id that a `cancel` request refers to.
    pub fn cancel_target(&self) -> Result<u64, ErrorInfo> {
        self.params
            .as_ref()
            .and_then(|params| params.get("request_id"))
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                ErrorInfo::new("invalid_params", "cancel requires a \"request_id\" number")
            })
    }
}

/// Response for an immediate request.
#[derive(Debug, Serialize)]
pub struct Response {
    /// Echoed request id, or `null` when the payload was not a valid request.
    pub id: Option<u64>,
    /// Serialized next to `id`, so the payload is either `result` or `error`.
    #[serde(flatten)]
    pub body: ResponseBody,
}

/// The outcome of an immediate request.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum ResponseBody {
    Result { result: Value },
    Error { error: ErrorInfo },
}

/// Failure reported to the client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ErrorInfo {
    pub code: &'static str,
    pub message: String,
}

impl ErrorInfo {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// One event of a `chat` request.
///
/// `event` is `started`, `delta`, `completed`, `failed`, or `cancelled`;
/// `text` carries a delta and `error` carries a failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EventPayload {
    pub id: u64,
    pub event: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorInfo>,
}

impl EventPayload {
    pub fn started(id: u64) -> Self {
        Self::new(id, "started")
    }

    pub fn delta(id: u64, text: impl Into<String>) -> Self {
        Self {
            id,
            event: "delta",
            text: Some(text.into()),
            error: None,
        }
    }

    pub fn completed(id: u64) -> Self {
        Self::new(id, "completed")
    }

    pub fn cancelled(id: u64) -> Self {
        Self::new(id, "cancelled")
    }

    pub fn failed(id: u64, error: ErrorInfo) -> Self {
        Self {
            id,
            event: "failed",
            text: None,
            error: Some(error),
        }
    }

    fn new(id: u64, event: &'static str) -> Self {
        Self {
            id,
            event,
            text: None,
            error: None,
        }
    }
}

/// The provider a [`Core`] answers `chat` with.
enum Provider {
    /// No provider configured: `chat` reports `provider_unconfigured`.
    Unconfigured,
    /// Configuration was attempted and failed; the error is reported as is.
    Unavailable(ErrorInfo),
    Ready(Llm),
}

/// Request processor.
///
/// Holds no per-connection state, so one instance serves every connection. The
/// immediate methods (`ping`, `echo`) exercise the request path; `chat` streams
/// a completion from the configured provider.
pub struct Core {
    provider: Provider,
}

impl Core {
    /// Core without a provider: `chat` reports `provider_unconfigured`.
    pub fn without_llm() -> Self {
        Self {
            provider: Provider::Unconfigured,
        }
    }

    pub fn with_llm(llm: Llm) -> Self {
        Self {
            provider: Provider::Ready(llm),
        }
    }

    /// Core configured from the environment, or from the recorded failure.
    pub fn from_env() -> Self {
        match Llm::from_env() {
            Ok(llm) => Self::with_llm(llm),
            Err(error) => Self {
                provider: Provider::Unavailable(error),
            },
        }
    }

    /// Runs an immediate request and returns the response to send back.
    pub fn handle(&self, request: Request) -> Response {
        let Request { id, method, params } = request;
        let body = match method.as_str() {
            "ping" => ResponseBody::Result {
                result: json!({ "pong": true }),
            },
            "echo" => match params {
                Some(value) => ResponseBody::Result { result: value },
                None => ResponseBody::Error {
                    error: ErrorInfo::new("invalid_params", "echo requires params"),
                },
            },
            _ => ResponseBody::Error {
                error: ErrorInfo::new("unknown_method", "Unknown method"),
            },
        };
        Response { id: Some(id), body }
    }

    /// Starts a streaming completion for `prompt`.
    pub async fn chat(&self, prompt: &str) -> Result<ChatStream, ErrorInfo> {
        match &self.provider {
            Provider::Ready(llm) => llm.chat(prompt).await,
            Provider::Unavailable(error) => Err(error.clone()),
            Provider::Unconfigured => Err(ErrorInfo::new(
                "provider_unconfigured",
                "no LLM provider is configured",
            )),
        }
    }

    /// Lists the models the configured provider advertises.
    pub async fn models(&self) -> Result<Vec<ModelInfo>, ErrorInfo> {
        match &self.provider {
            Provider::Ready(llm) => llm.models().await,
            Provider::Unavailable(error) => Err(error.clone()),
            Provider::Unconfigured => Err(ErrorInfo::new(
                "provider_unconfigured",
                "no LLM provider is configured",
            )),
        }
    }

    /// The configured account's usage.
    pub async fn usage(&self) -> Result<Usage, ErrorInfo> {
        match &self.provider {
            Provider::Ready(llm) => llm.usage().await,
            Provider::Unavailable(error) => Err(error.clone()),
            Provider::Unconfigured => Err(ErrorInfo::new(
                "provider_unconfigured",
                "no LLM provider is configured",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(id: u64, method: &str, params: Option<Value>) -> Request {
        Request {
            id,
            method: method.to_string(),
            params,
        }
    }

    #[test]
    fn ping_answers_with_pong_and_echoes_the_id() {
        let response = Core::without_llm().handle(request(7, "ping", None));
        assert_eq!(response.id, Some(7));
        assert!(matches!(
            response.body,
            ResponseBody::Result { result } if result == json!({ "pong": true })
        ));
    }

    #[test]
    fn echo_returns_the_sent_value() {
        let params = json!({ "text": "日本語", "numbers": [1, 2, 3] });
        let response = Core::without_llm().handle(request(1, "echo", Some(params.clone())));
        assert!(matches!(
            response.body,
            ResponseBody::Result { result } if result == params
        ));
    }

    #[test]
    fn echo_without_params_is_an_error() {
        let response = Core::without_llm().handle(request(3, "echo", None));
        assert_eq!(response.id, Some(3));
        assert!(matches!(
            response.body,
            ResponseBody::Error { error } if error.code == "invalid_params"
        ));
    }

    #[test]
    fn unknown_method_is_rejected() {
        let response = Core::without_llm().handle(request(9, "delete_everything", None));
        assert_eq!(response.id, Some(9));
        assert!(matches!(
            response.body,
            ResponseBody::Error { error }
                if error.code == "unknown_method" && error.message == "Unknown method"
        ));
    }

    #[test]
    fn chat_requires_a_prompt() {
        let missing = request(1, "chat", None);
        assert_eq!(missing.prompt().unwrap_err().code, "invalid_params");

        let empty = request(1, "chat", Some(json!({ "prompt": "   " })));
        assert_eq!(empty.prompt().unwrap_err().code, "invalid_params");

        let present = request(1, "chat", Some(json!({ "prompt": "hello" })));
        assert_eq!(present.prompt().unwrap(), "hello");
    }

    #[test]
    fn cancel_requires_a_request_id() {
        let missing = request(1, "cancel", None);
        assert_eq!(missing.cancel_target().unwrap_err().code, "invalid_params");

        let present = request(1, "cancel", Some(json!({ "request_id": 5 })));
        assert_eq!(present.cancel_target().unwrap(), 5);
    }

    #[test]
    fn events_serialize_with_their_id_and_name() {
        let started = serde_json::to_value(EventPayload::started(4)).unwrap();
        assert_eq!(started, json!({ "id": 4, "event": "started" }));

        let delta = serde_json::to_value(EventPayload::delta(4, "hi")).unwrap();
        assert_eq!(delta, json!({ "id": 4, "event": "delta", "text": "hi" }));

        let failed = serde_json::to_value(EventPayload::failed(
            4,
            ErrorInfo::new("provider_http", "boom"),
        ))
        .unwrap();
        assert_eq!(
            failed,
            json!({ "id": 4, "event": "failed", "error": { "code": "provider_http", "message": "boom" } })
        );
    }
}
