//! Streaming client for providers that do not speak the Responses API (chat/completions today).
//!
//! It takes the same `ResponsesApiRequest` as [`super::responses::ResponsesClient`] and returns
//! the same `ResponseStream`, so everything around the call (auth, retries, 401 recovery,
//! telemetry, account switching) stays as it is. The translation itself lives in
//! `codex-provider-adapters`.
//!
//! No `x-codex-*` / `x-openai-*` request headers are forwarded: those carry session and thread
//! identifiers that belong to the first-party backend.

use crate::auth::SharedAuthProvider;
use crate::common::ResponseEvent;
use crate::common::ResponseStream;
use crate::common::ResponsesApiRequest;
use crate::endpoint::responses::ResponsesOptions;
use crate::endpoint::session::EndpointSession;
use crate::error::ApiError;
use crate::provider::Provider;
use codex_client::EncodedJsonBody;
use codex_client::HttpTransport;
use codex_client::RequestTelemetry;
use codex_protocol::error::ResponseIncompleteError;
use codex_protocol::protocol::TokenUsage;
use codex_provider_adapters::chat::ChatCompat;
use codex_provider_adapters::chat::ChatRequest;
use codex_provider_adapters::chat::ChatStream;
use codex_provider_adapters::events::Assembler;
use codex_provider_adapters::history::turns_from_items;
use codex_provider_adapters::messages::CacheRetention;
use codex_provider_adapters::messages::MessagesCompat;
use codex_provider_adapters::messages::MessagesRequest;
use codex_provider_adapters::messages::MessagesStream;
use codex_provider_adapters::ir::AdapterEvent;
use codex_provider_adapters::ir::AdapterFailure;
use codex_provider_adapters::ir::Out;
use codex_provider_adapters::ir::Usage;
use codex_provider_adapters::tool_names::ToolRegistry;
use codex_provider_adapters::tool_names::declared_tools;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use http::HeaderMap;
use http::HeaderValue;
use http::Method;
use http::StatusCode;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tracing::debug;

/// Which non-Responses wire a provider speaks.
#[derive(Clone, Debug)]
pub enum WireKind {
    /// `POST {base_url}/chat/completions`.
    ChatCompletions(ChatCompat),
    /// `POST {base_url}/v1/messages`, the Claude Messages API. The key goes in `x-api-key`
    /// (`bearer: false`) or, for OAuth credentials, as the bearer the auth provider supplies.
    AnthropicMessages { compat: MessagesCompat, bearer: bool },
}

/// The Claude wire for a base URL: the official host gets automatic prompt caching (1 h); any
/// other Anthropic-compatible endpoint a 5-minute cache. OAuth credentials travel as a bearer.
pub fn anthropic_wire_kind(base_url: &str, oauth: bool) -> WireKind {
    let native = base_url
        .split("://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .is_some_and(|host| host.eq_ignore_ascii_case("api.anthropic.com"));
    WireKind::AnthropicMessages {
        compat: MessagesCompat {
            oauth,
            native_endpoint: native,
            cache: if native {
                CacheRetention::Long
            } else {
                CacheRetention::Short
            },
            ..MessagesCompat::default()
        },
        bearer: oauth,
    }
}

/// Moves the bearer an auth provider adds into the `x-api-key` header Anthropic expects for keys.
struct XApiKeyAuth(SharedAuthProvider);

impl crate::auth::AuthProvider for XApiKeyAuth {
    fn add_auth_headers(&self, headers: &mut HeaderMap) -> Result<(), crate::auth::AuthError> {
        let mut inner = HeaderMap::new();
        self.0.add_auth_headers(&mut inner)?;
        if let Some(value) = inner.remove(http::header::AUTHORIZATION) {
            let key = value
                .to_str()
                .ok()
                .and_then(|v| v.strip_prefix("Bearer "))
                .and_then(|v| HeaderValue::from_str(v.trim()).ok());
            if let Some(mut key) = key {
                key.set_sensitive(true);
                headers.insert("x-api-key", key);
            }
        }
        headers.extend(inner);
        Ok(())
    }

    fn refresh_after_unauthorized(
        &self,
        rejected_headers: HeaderMap,
    ) -> crate::auth::AuthRefreshFuture<'_> {
        self.0.refresh_after_unauthorized(rejected_headers)
    }
}

enum Parser {
    Chat(ChatStream),
    Messages(MessagesStream),
}

impl Parser {
    fn feed(&mut self, data: &str) -> Vec<AdapterEvent> {
        match self {
            Self::Chat(p) => p.feed(data),
            Self::Messages(p) => p.feed(data),
        }
    }

    fn eof(&mut self) -> Vec<AdapterEvent> {
        match self {
            Self::Chat(p) => p.eof(),
            Self::Messages(p) => p.eof(),
        }
    }

    fn is_done(&self) -> bool {
        match self {
            Self::Chat(p) => p.is_done(),
            Self::Messages(p) => p.is_done(),
        }
    }
}

pub struct WireClient<T: HttpTransport> {
    session: EndpointSession<T>,
    kind: WireKind,
    max_output_tokens: Option<u32>,
}

impl<T: HttpTransport> WireClient<T> {
    pub fn new(transport: T, provider: Provider, auth: SharedAuthProvider, kind: WireKind) -> Self {
        let auth: SharedAuthProvider = match &kind {
            WireKind::AnthropicMessages { bearer: false, .. } => Arc::new(XApiKeyAuth(auth)),
            _ => auth,
        };
        Self {
            session: EndpointSession::new(transport, provider, auth),
            kind,
            max_output_tokens: None,
        }
    }

    pub fn with_telemetry(self, request: Option<Arc<dyn RequestTelemetry>>) -> Self {
        Self {
            session: self.session.with_request_telemetry(request),
            ..self
        }
    }

    /// The output limit sent as `max_tokens`; providers that require one need it.
    pub fn with_max_output_tokens(mut self, max_output_tokens: Option<u32>) -> Self {
        self.max_output_tokens = max_output_tokens;
        self
    }

    pub async fn stream_request(
        &self,
        request: ResponsesApiRequest,
        _options: ResponsesOptions,
    ) -> Result<ResponseStream, ApiError> {
        let tools_json: Value = match request.tools.as_ref() {
            Some(tools) => serde_json::from_str(tools.as_raw_value().get()).unwrap_or(Value::Null),
            None => Value::Null,
        };
        let tools = declared_tools(&tools_json);
        let turns = turns_from_items(&request.input);
        let effort = request
            .reasoning
            .as_ref()
            .and_then(|r| r.effort.as_ref())
            .map(ToString::to_string);

        let mut extra_headers = HeaderMap::new();
        let (path, body, parser) = match &self.kind {
            WireKind::AnthropicMessages { compat, .. } => {
                extra_headers.insert(
                    "anthropic-version",
                    HeaderValue::from_static(codex_provider_adapters::messages::ANTHROPIC_VERSION),
                );
                if compat.oauth {
                    extra_headers.insert(
                        "anthropic-beta",
                        HeaderValue::from_static(codex_provider_adapters::messages::OAUTH_BETA),
                    );
                }
                // `.../v1` and bare origins both end up at `{origin}/v1/messages`.
                let messages_path = if self
                    .session
                    .provider()
                    .base_url
                    .trim_end_matches('/')
                    .ends_with("/v1")
                {
                    "/messages"
                } else {
                    "/v1/messages"
                };
                (
                    messages_path,
                    codex_provider_adapters::messages::build_body(
                        &MessagesRequest {
                            model: &request.model,
                            turns: &turns,
                            tools: &tools,
                            tool_choice: &request.tool_choice,
                            parallel_tool_calls: request.parallel_tool_calls,
                            reasoning_effort: effort.as_deref(),
                            max_output_tokens: self.max_output_tokens,
                            json_schema: None,
                        },
                        compat,
                    ),
                    Parser::Messages(MessagesStream::new(compat.clone())),
                )
            }
            WireKind::ChatCompletions(compat) => (
                "/chat/completions",
                codex_provider_adapters::chat::build_body(
                    &ChatRequest {
                        model: &request.model,
                        turns: &turns,
                        tools: &tools,
                        tool_choice: &request.tool_choice,
                        parallel_tool_calls: request.parallel_tool_calls,
                        reasoning_effort: effort.as_deref(),
                        max_output_tokens: self.max_output_tokens,
                        json_schema: None,
                    },
                    compat,
                ),
                Parser::Chat(ChatStream::default()),
            ),
        };
        let body = EncodedJsonBody::encode(&body)
            .map_err(|e| ApiError::Stream(format!("failed to encode request: {e}")))?;

        let response = self
            .session
            .stream_encoded_json_with(Method::POST, path, extra_headers, Some(body), |req| {
                req.headers.insert(
                    http::header::ACCEPT,
                    HeaderValue::from_static("text/event-stream"),
                );
            })
            .await?;

        let idle_timeout = self.session.provider().stream_idle_timeout;
        let upstream_request_id = response
            .headers
            .get("x-request-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let registry = ToolRegistry::new(&tools);
        let (tx_event, rx_event) = mpsc::channel::<Result<ResponseEvent, ApiError>>(1600);
        tokio::spawn(async move {
            pump(response.bytes, tx_event, idle_timeout, registry, parser).await;
        });
        Ok(ResponseStream {
            routing_affinity: None,
            rx_event,
            upstream_request_id,
            interrupt: None,
        })
    }
}

async fn pump(
    bytes: codex_client::ByteStream,
    tx: mpsc::Sender<Result<ResponseEvent, ApiError>>,
    idle_timeout: Duration,
    registry: ToolRegistry,
    mut parser: Parser,
) {
    let mut stream = bytes.eventsource();
    let response_id = format!("resp_{}", uuid::Uuid::new_v4().simple());
    let mut assembler = Assembler::new(registry, response_id);
    loop {
        let events: Vec<AdapterEvent> = tokio::select! {
            biased;
            _ = tx.closed() => return,
            next = timeout(idle_timeout, stream.next()) => match next {
                Ok(Some(Ok(sse))) => parser.feed(&sse.data),
                Ok(Some(Err(e))) => {
                    debug!("chat stream error: {e:#}");
                    let _ = tx.send(Err(ApiError::Stream(e.to_string()))).await;
                    return;
                }
                Ok(None) => parser.eof(),
                Err(_) => {
                    let _ = tx
                        .send(Err(ApiError::Stream("idle timeout waiting for SSE".into())))
                        .await;
                    return;
                }
            },
        };
        for event in events {
            for out in assembler.push(event) {
                if !forward(&tx, out).await {
                    return;
                }
            }
        }
        if assembler.is_finished() {
            return;
        }
        if parser.is_done() {
            // The parser finished without a terminal event for the assembler.
            return;
        }
    }
}

/// `false` when the stream is over (terminal event sent or receiver gone).
async fn forward(tx: &mpsc::Sender<Result<ResponseEvent, ApiError>>, out: Out) -> bool {
    let (event, terminal) = match out {
        Out::Created => (Ok(ResponseEvent::Created { response_id: None }), false),
        Out::ItemAdded(item) => (Ok(ResponseEvent::OutputItemAdded(item)), false),
        Out::ItemDone(item) => (Ok(ResponseEvent::OutputItemDone(item)), false),
        Out::TextDelta(delta) => (Ok(ResponseEvent::OutputTextDelta(delta)), false),
        Out::ToolArgsDelta {
            item_id,
            call_id,
            delta,
        } => (
            Ok(ResponseEvent::ToolCallInputDelta {
                item_id,
                call_id: Some(call_id),
                delta,
            }),
            false,
        ),
        Out::ReasoningSummaryDelta {
            delta,
            summary_index,
        } => (
            Ok(ResponseEvent::ReasoningSummaryDelta {
                delta,
                summary_index,
            }),
            false,
        ),
        Out::ReasoningContentDelta {
            delta,
            content_index,
        } => (
            Ok(ResponseEvent::ReasoningContentDelta {
                delta,
                content_index,
            }),
            false,
        ),
        Out::Completed { response_id, usage } => (
            Ok(ResponseEvent::Completed {
                response_id,
                token_usage: usage.map(token_usage),
                usage_metadata: None,
                end_turn: None,
            }),
            true,
        ),
        Out::Incomplete {
            response_id,
            reason,
            usage,
        } => (Err(incomplete_error(response_id, reason, usage)), true),
        Out::Failed(failure) => (Err(failure_error(failure)), true),
    };
    tx.send(event).await.is_ok() && !terminal
}

fn token_usage(usage: Usage) -> TokenUsage {
    TokenUsage {
        input_tokens: usage.input_tokens,
        cached_input_tokens: usage.cached_input_tokens,
        cache_write_input_tokens: usage.cache_write_input_tokens,
        output_tokens: usage.output_tokens,
        reasoning_output_tokens: usage.reasoning_output_tokens,
        total_tokens: usage.input_tokens + usage.output_tokens,
        codex_rollout_budget_units: None,
    }
}

fn incomplete_error(response_id: String, reason: String, usage: Option<Usage>) -> ApiError {
    match reason.as_str() {
        "content_filter" => ApiError::ContentFilter,
        "context_window" => ApiError::ContextWindowExceeded,
        _ => ApiError::ResponseIncomplete(Box::new(ResponseIncompleteError {
            partial_output: Vec::new(),
            reason,
            response_id: Some(response_id),
            token_usage: usage.map(token_usage),
        })),
    }
}

fn failure_error(failure: AdapterFailure) -> ApiError {
    let AdapterFailure {
        message,
        status,
        error_type,
        code,
    } = failure;
    let quota = code.as_deref() == Some("insufficient_quota")
        || error_type.as_deref() == Some("insufficient_quota");
    let context = matches!(
        code.as_deref(),
        Some("context_length_exceeded" | "context_window_exceeded")
    );
    match status {
        _ if quota => ApiError::QuotaExceeded,
        _ if context => ApiError::ContextWindowExceeded,
        Some(429) => ApiError::RateLimitExceeded {
            message,
            retry_after: None,
        },
        Some(status) if status >= 500 => ApiError::Retryable {
            message,
            retry_after: None,
        },
        Some(400 | 422) => ApiError::InvalidRequest { message },
        Some(status) => match StatusCode::from_u16(status) {
            Ok(status) => ApiError::Api { status, message },
            Err(_) => ApiError::Stream(message),
        },
        None => ApiError::Stream(message),
    }
}
