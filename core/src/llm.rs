//! Streaming client for OpenAI-compatible chat completions.
//!
//! The transport is plain HTTP with `stream: true`, so the provider is chosen by
//! configuration rather than by code: any endpoint that speaks the Chat
//! Completions API works. Higher-level connection modes (Anthropic Messages,
//! OAuth) are not implemented in this stage.

use std::time::Duration;

use serde_json::json;

use crate::sse::{SseItem, SseParser};
use crate::ErrorInfo;

/// Default endpoint used when `INOCODE_LLM_BASE_URL` is not set.
pub const DEFAULT_BASE_URL: &str = "https://logfare.ai/v1";

/// Default model used when `INOCODE_LLM_MODEL` is not set.
pub const DEFAULT_MODEL: &str = "logfare/auto";

const API_KEY_ENV: &str = "LOGFARE_API_KEY";
const BASE_URL_ENV: &str = "INOCODE_LLM_BASE_URL";
const MODEL_ENV: &str = "INOCODE_LLM_MODEL";

/// Endpoint, model, and credential for one OpenAI-compatible provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmConfig {
    pub base_url: String,
    pub model: String,
    pub api_key: String,
}

impl LlmConfig {
    /// Reads the key from `LOGFARE_API_KEY`; base URL and model can be
    /// overridden with `INOCODE_LLM_BASE_URL` and `INOCODE_LLM_MODEL`.
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
            base_url: std::env::var(BASE_URL_ENV).unwrap_or_else(|_| DEFAULT_BASE_URL.to_string()),
            model: std::env::var(MODEL_ENV).unwrap_or_else(|_| DEFAULT_MODEL.to_string()),
            api_key,
        })
    }
}

/// Chat Completions client. Holds only the HTTP client and its configuration.
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
        let url = format!(
            "{}/chat/completions",
            self.config.base_url.trim_end_matches('/')
        );
        let body = json!({
            "model": self.config.model,
            "stream": true,
            "messages": [{ "role": "user", "content": prompt }],
        });
        let response = self
            .client
            .post(&url)
            .bearer_auth(&self.config.api_key)
            .header("accept", "text/event-stream")
            .json(&body)
            .send()
            .await
            .map_err(|error| ErrorInfo::new("provider_http", format!("request failed: {error}")))?;
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
        Ok(ChatStream {
            response,
            parser: SseParser::new(),
            body_ended: false,
            done: false,
        })
    }
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
    response: reqwest::Response,
    parser: SseParser,
    body_ended: bool,
    done: bool,
}

impl ChatStream {
    /// Returns the next text delta, or `None` once the provider finished.
    ///
    /// A body that ends before the provider signalled completion is an error,
    /// so a truncated answer is never reported as complete.
    pub async fn next(&mut self) -> Result<Option<String>, ErrorInfo> {
        if self.done {
            return Ok(None);
        }
        loop {
            match self.parser.next_item() {
                Some(Ok(SseItem::Delta(text))) => return Ok(Some(text)),
                Some(Ok(SseItem::Finished)) => {
                    self.done = true;
                    return Ok(None);
                }
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
                return if self.parser.is_finished() {
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
