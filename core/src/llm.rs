//! Streaming clients for OpenAI-compatible and Anthropic-compatible chat APIs.
//!
//! The transport is plain HTTP with server-sent events. The format is chosen by
//! configuration: Chat Completions (`/chat/completions`) or Messages
//! (`/messages`) against the same base URL, so any endpoint speaking either
//! format works. OAuth-based connection modes are not implemented.

use std::fmt;
use std::time::Duration;

use serde_json::{json, Value};

use crate::sse::SseParser;
use crate::ErrorInfo;

/// Default endpoint base used when `INOCODE_LLM_BASE_URL` is not set.
pub const DEFAULT_BASE_URL: &str = "https://logfare.ai/v1";

/// Default model used when `INOCODE_LLM_MODEL` is not set.
pub const DEFAULT_MODEL: &str = "logfare/auto";

/// `max_tokens` for Anthropic-style requests, which require it. The value is
/// the one used by the provider's documented samples.
pub const DEFAULT_MAX_TOKENS: u32 = 1024;

const API_KEY_ENV: &str = "LOGFARE_API_KEY";
const BASE_URL_ENV: &str = "INOCODE_LLM_BASE_URL";
const MODEL_ENV: &str = "INOCODE_LLM_MODEL";
const PROVIDER_ENV: &str = "INOCODE_LLM_PROVIDER";
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Which API format a [`Llm`] speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProviderKind {
    /// Chat Completions format (`POST {base}/chat/completions`).
    #[default]
    OpenAi,
    /// Anthropic Messages format (`POST {base}/messages`).
    Anthropic,
}

impl ProviderKind {
    fn from_env() -> Result<Self, ErrorInfo> {
        match std::env::var(PROVIDER_ENV) {
            Err(_) => Ok(Self::default()),
            Ok(value) if value.eq_ignore_ascii_case("openai") => Ok(Self::OpenAi),
            Ok(value) if value.eq_ignore_ascii_case("anthropic") => Ok(Self::Anthropic),
            Ok(value) if value.trim().is_empty() => Ok(Self::default()),
            Ok(value) => Err(ErrorInfo::new(
                "provider_config",
                format!("{PROVIDER_ENV} must be \"openai\" or \"anthropic\", got \"{value}\""),
            )),
        }
    }
}

/// Endpoint base, model, credential, and API format for one provider.
///
/// The manual `Debug` implementation keeps the API key out of any derived
/// output, so a mislogged config cannot leak the credential.
#[derive(Clone, PartialEq, Eq)]
pub struct LlmConfig {
    pub provider: ProviderKind,
    pub base_url: String,
    pub model: String,
    pub api_key: String,
}

impl fmt::Debug for LlmConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LlmConfig")
            .field("provider", &self.provider)
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("api_key", &"[redacted]")
            .finish()
    }
}

impl LlmConfig {
    /// Reads the key from `LOGFARE_API_KEY`; the API format, base URL, and
    /// model can be overridden with `INOCODE_LLM_PROVIDER`,
    /// `INOCODE_LLM_BASE_URL`, and `INOCODE_LLM_MODEL`. The default format is
    /// OpenAI-compatible, so existing configurations behave as before.
    pub fn from_env() -> Result<Self, ErrorInfo> {
        let api_key = std::env::var(API_KEY_ENV).map_err(|_| {
            ErrorInfo::new("provider_unconfigured", format!("{API_KEY_ENV} is not set"))
        })?;
        if api_key.trim().is_empty() {
            return Err(ErrorInfo::new(
                "provider_unconfigured",
                format!("{API_KEY_ENV} is empty"),
            ));
        }
        Ok(Self {
            provider: ProviderKind::from_env()?,
            base_url: std::env::var(BASE_URL_ENV).unwrap_or_else(|_| DEFAULT_BASE_URL.to_string()),
            model: std::env::var(MODEL_ENV).unwrap_or_else(|_| DEFAULT_MODEL.to_string()),
            api_key,
        })
    }
}

/// Chat client for one API format. Holds only the HTTP client and configuration.
pub struct Llm {
    client: reqwest::Client,
    config: LlmConfig,
}

impl Llm {
    pub fn new(config: LlmConfig) -> Result<Self, ErrorInfo> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|error| {
                ErrorInfo::new(
                    "provider_config",
                    format!("cannot build the HTTP client: {error}"),
                )
            })?;
        Ok(Self { client, config })
    }

    pub fn from_env() -> Result<Self, ErrorInfo> {
        Self::new(LlmConfig::from_env()?)
    }

    /// Starts a streaming completion and returns the deltas as they arrive.
    pub async fn chat(&self, prompt: &str) -> Result<ChatStream, ErrorInfo> {
        match self.config.provider {
            ProviderKind::OpenAi => self.openai_chat(prompt).await,
            ProviderKind::Anthropic => self.anthropic_chat(prompt).await,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{}", self.config.base_url.trim_end_matches('/'), path)
    }

    async fn openai_chat(&self, prompt: &str) -> Result<ChatStream, ErrorInfo> {
        let body = json!({
            "model": self.config.model,
            "stream": true,
            "messages": [{ "role": "user", "content": prompt }],
        });
        let response = self
            .client
            .post(self.url("chat/completions"))
            .bearer_auth(&self.config.api_key)
            .header("accept", "text/event-stream")
            .json(&body)
            .send()
            .await
            .map_err(|error| ErrorInfo::new("provider_http", format!("request failed: {error}")))?;
        let response = require_success(response).await?;
        Ok(ChatStream {
            inner: ChatStreamInner::OpenAi(OpenAiStream {
                response,
                parser: SseParser::new(),
                body_ended: false,
                done: false,
                signalled: false,
            }),
        })
    }

    async fn anthropic_chat(&self, prompt: &str) -> Result<ChatStream, ErrorInfo> {
        let body = json!({
            "model": self.config.model,
            "max_tokens": DEFAULT_MAX_TOKENS,
            "messages": [{ "role": "user", "content": prompt }],
            "stream": true,
        });
        let response = self
            .client
            .post(self.url("messages"))
            .header("x-api-key", &self.config.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("accept", "text/event-stream")
            .json(&body)
            .send()
            .await
            .map_err(|error| ErrorInfo::new("provider_http", format!("request failed: {error}")))?;
        let response = require_success(response).await?;
        Ok(ChatStream {
            inner: ChatStreamInner::Anthropic(AnthropicStream {
                response,
                parser: SseParser::new(),
                body_ended: false,
                done: false,
            }),
        })
    }
}

async fn require_success(response: reqwest::Response) -> Result<reqwest::Response, ErrorInfo> {
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(ErrorInfo::new(
            "provider_http",
            format!(
                "provider returned HTTP {}: {}",
                status.as_u16(),
                snippet(&body)
            ),
        ));
    }
    Ok(response)
}

fn snippet(body: &str) -> String {
    const LIMIT: usize = 300;
    let trimmed = body.trim();
    if trimmed.chars().count() <= LIMIT {
        trimmed.to_string()
    } else {
        format!("{}…", trimmed.chars().take(LIMIT).collect::<String>())
    }
}

/// Text deltas of one completion, in generation order.
pub struct ChatStream {
    inner: ChatStreamInner,
}

impl ChatStream {
    /// Returns the next text delta, or `None` once the provider finished.
    ///
    /// A body that ends before the provider signalled completion is an error,
    /// so a truncated answer is never reported as complete.
    pub async fn next(&mut self) -> Result<Option<String>, ErrorInfo> {
        match &mut self.inner {
            ChatStreamInner::OpenAi(stream) => stream.next().await,
            ChatStreamInner::Anthropic(stream) => stream.next().await,
        }
    }
}

enum ChatStreamInner {
    OpenAi(OpenAiStream),
    Anthropic(AnthropicStream),
}

struct OpenAiStream {
    response: reqwest::Response,
    parser: SseParser,
    body_ended: bool,
    done: bool,
    signalled: bool,
}

impl OpenAiStream {
    async fn next(&mut self) -> Result<Option<String>, ErrorInfo> {
        if self.done {
            return Ok(None);
        }
        loop {
            match self.parser.next_item() {
                Some(Ok(data)) => match openai_delta(&data, &mut self.done, &mut self.signalled)? {
                    Some(text) => return Ok(Some(text)),
                    None => continue,
                },
                Some(Err(error)) => {
                    self.done = true;
                    return Err(error);
                }
                None => {}
            }
            if self.body_ended {
                // The body is over and the parser has nothing left: only a
                // provider that signalled completion finished successfully.
                self.done = true;
                return if self.signalled {
                    Ok(None)
                } else {
                    Err(ErrorInfo::new(
                        "provider_stream",
                        "the provider stream ended before it signalled completion",
                    ))
                };
            }
            match self.response.chunk().await {
                Ok(Some(chunk)) => self.parser.push(&chunk),
                Ok(None) => {
                    self.parser.end();
                    self.body_ended = true;
                }
                Err(error) => {
                    self.done = true;
                    return Err(ErrorInfo::new(
                        "provider_stream",
                        format!("stream failed: {error}"),
                    ));
                }
            }
        }
    }
}

/// Interprets one Chat Completions `data` payload, updating the completion
/// signals on the stream.
fn openai_delta(
    data: &str,
    done: &mut bool,
    signalled: &mut bool,
) -> Result<Option<String>, ErrorInfo> {
    if data.trim() == "[DONE]" {
        *done = true;
        *signalled = true;
        return Ok(None);
    }
    let value: Value = serde_json::from_str(data).map_err(|error| {
        ErrorInfo::new("provider_protocol", format!("invalid SSE payload: {error}"))
    })?;
    if let Some(error) = value.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("provider reported an error in the stream");
        return Err(ErrorInfo::new("provider_stream", message.to_string()));
    }
    let choice = value.get("choices").and_then(|choices| choices.get(0));
    if choice
        .and_then(|choice| choice.get("finish_reason"))
        .is_some_and(|reason| !reason.is_null())
    {
        *signalled = true;
    }
    let content = choice
        .and_then(|choice| choice.get("delta"))
        .and_then(|delta| delta.get("content"))
        .and_then(Value::as_str)
        .unwrap_or("");
    Ok(if content.is_empty() {
        None
    } else {
        Some(content.to_string())
    })
}

struct AnthropicStream {
    response: reqwest::Response,
    parser: SseParser,
    body_ended: bool,
    done: bool,
}

impl AnthropicStream {
    async fn next(&mut self) -> Result<Option<String>, ErrorInfo> {
        if self.done {
            return Ok(None);
        }
        loop {
            match self.parser.next_item() {
                Some(Ok(data)) => match anthropic_event(&data)? {
                    AnthropicEvent::Text(text) => return Ok(Some(text)),
                    AnthropicEvent::Stop => {
                        self.done = true;
                        return Ok(None);
                    }
                    AnthropicEvent::Ignored => continue,
                },
                Some(Err(error)) => {
                    self.done = true;
                    return Err(error);
                }
                None => {}
            }
            if self.body_ended {
                // `message_stop` is the only completion signal; a body that
                // ends without it is a truncated answer, never a success.
                self.done = true;
                return Err(ErrorInfo::new(
                    "provider_stream",
                    "the provider stream ended before message_stop",
                ));
            }
            match self.response.chunk().await {
                Ok(Some(chunk)) => self.parser.push(&chunk),
                Ok(None) => {
                    self.parser.end();
                    self.body_ended = true;
                }
                Err(error) => {
                    self.done = true;
                    return Err(ErrorInfo::new(
                        "provider_stream",
                        format!("stream failed: {error}"),
                    ));
                }
            }
        }
    }
}

/// One Anthropic Messages SSE event reduced to what this protocol needs.
#[derive(Debug)]
enum AnthropicEvent {
    Text(String),
    Stop,
    Ignored,
}

fn anthropic_event(data: &str) -> Result<AnthropicEvent, ErrorInfo> {
    let value: Value = serde_json::from_str(data).map_err(|error| {
        ErrorInfo::new("provider_protocol", format!("invalid SSE payload: {error}"))
    })?;
    match value.get("type").and_then(Value::as_str) {
        Some("content_block_delta") => {
            let delta = value.get("delta");
            // Only `text_delta` carries answer text; `input_json_delta` and
            // other delta types must not be concatenated into the answer.
            if delta
                .and_then(|delta| delta.get("type"))
                .and_then(Value::as_str)
                == Some("text_delta")
            {
                Ok(
                    match delta
                        .and_then(|delta| delta.get("text"))
                        .and_then(Value::as_str)
                    {
                        Some(text) if !text.is_empty() => AnthropicEvent::Text(text.to_string()),
                        _ => AnthropicEvent::Ignored,
                    },
                )
            } else {
                Ok(AnthropicEvent::Ignored)
            }
        }
        Some("message_stop") => Ok(AnthropicEvent::Stop),
        Some("error") => {
            let message = value
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("provider reported an error in the stream");
            Err(ErrorInfo::new("provider_stream", message.to_string()))
        }
        // `message_start`, `content_block_start`/`_stop`, `message_delta`,
        // `ping`, and unknown event types carry no answer text. The streaming
        // spec requires handling unknown event types gracefully.
        Some(_) => Ok(AnthropicEvent::Ignored),
        None => Err(ErrorInfo::new(
            "provider_protocol",
            "SSE payload has no \"type\" field",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_output_does_not_contain_the_api_key() {
        let config = LlmConfig {
            provider: ProviderKind::Anthropic,
            base_url: "https://example.test/v1".to_string(),
            model: "test-model".to_string(),
            api_key: "super-secret-key-material".to_string(),
        };
        let rendered = format!("{config:?}");
        assert!(!rendered.contains("super-secret-key-material"));
        assert!(rendered.contains("[redacted]"));
        assert!(rendered.contains("Anthropic"));

        // Cloning keeps the redaction: the clone must not leak either.
        let rendered = format!("{:?}", config.clone());
        assert!(!rendered.contains("super-secret-key-material"));
    }

    #[test]
    fn provider_selection_reads_the_environment() {
        let key = "provider-selection-test-key";
        std::env::set_var(API_KEY_ENV, key);
        std::env::remove_var(PROVIDER_ENV);
        assert_eq!(
            LlmConfig::from_env().expect("configured").provider,
            ProviderKind::OpenAi,
            "unset provider must default to OpenAI-compatible"
        );

        std::env::set_var(PROVIDER_ENV, "anthropic");
        assert_eq!(
            LlmConfig::from_env().expect("configured").provider,
            ProviderKind::Anthropic
        );
        std::env::set_var(PROVIDER_ENV, "OpenAI");
        assert_eq!(
            LlmConfig::from_env().expect("configured").provider,
            ProviderKind::OpenAi
        );

        std::env::set_var(PROVIDER_ENV, "ollama");
        assert_eq!(
            LlmConfig::from_env().expect_err("must fail").code,
            "provider_config"
        );

        std::env::set_var(PROVIDER_ENV, " ");
        assert_eq!(
            LlmConfig::from_env().expect("configured").provider,
            ProviderKind::OpenAi
        );

        std::env::remove_var(PROVIDER_ENV);
        std::env::remove_var(API_KEY_ENV);
    }

    #[test]
    fn anthropic_events_reduce_to_text_stop_and_ignored() {
        let text = anthropic_event(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"こん"}}"#,
        )
        .expect("text delta");
        assert!(matches!(text, AnthropicEvent::Text(ref t) if t == "こん"));

        // Non-text deltas must not become answer text.
        let thinking = anthropic_event(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hmm"}}"#,
        )
        .expect("ignored delta");
        assert!(matches!(thinking, AnthropicEvent::Ignored));

        let input_json = anthropic_event(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"a\":1}"}}"#,
        )
        .expect("ignored delta");
        assert!(matches!(input_json, AnthropicEvent::Ignored));

        let empty_text = anthropic_event(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":""}}"#,
        )
        .expect("empty text delta");
        assert!(matches!(empty_text, AnthropicEvent::Ignored));

        for event in [
            r#"{"type":"message_start","message":{}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":15}}"#,
            r#"{"type":"ping"}"#,
            r#"{"type":"message_created_someday"}"#,
        ] {
            assert!(
                matches!(
                    anthropic_event(event).expect("ignored event"),
                    AnthropicEvent::Ignored
                ),
                "{event}"
            );
        }

        assert!(matches!(
            anthropic_event(r#"{"type":"message_stop"}"#).expect("stop"),
            AnthropicEvent::Stop
        ));

        let error = anthropic_event(
            r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
        )
        .expect_err("must fail");
        assert_eq!(error.code, "provider_stream");
        assert_eq!(error.message, "Overloaded");

        let no_type = anthropic_event(r#"{"nonsense":true}"#).expect_err("must fail");
        assert_eq!(no_type.code, "provider_protocol");
    }

    #[test]
    fn openai_deltas_extract_content_and_completion_signals() {
        let mut done = false;
        let mut signalled = false;
        let text = openai_delta(
            r#"{"choices":[{"delta":{"role":"assistant"}}]}"#,
            &mut done,
            &mut signalled,
        )
        .expect("no error");
        assert!(text.is_none());
        assert!(!done);

        let text = openai_delta(
            r#"{"choices":[{"delta":{"content":"こん"}}]}"#,
            &mut done,
            &mut signalled,
        )
        .expect("no error");
        assert_eq!(text.as_deref(), Some("こん"));

        openai_delta(
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            &mut done,
            &mut signalled,
        )
        .expect("no error");
        assert!(signalled);
        assert!(!done);

        openai_delta("[DONE]", &mut done, &mut signalled).expect("no error");
        assert!(done);

        let error = openai_delta(
            r#"{"error":{"message":"rate limited"}}"#,
            &mut done,
            &mut signalled,
        )
        .expect_err("must fail");
        assert_eq!(error.code, "provider_stream");
        assert_eq!(error.message, "rate limited");
    }
}
