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

/// A bearer token that is produced asynchronously (and may need refreshing), such as a Claude
/// OAuth access token.
pub trait BearerSource: Send + Sync {
    fn token(&self) -> crate::auth::AuthProviderFutureOf<'_, Result<String, String>>;
    /// After a 401: obtain a new token. `true` when the caller may retry.
    fn refresh(&self) -> crate::auth::AuthRefreshFuture<'_>;
}

/// Authenticates requests with a [`BearerSource`].
pub struct BearerSourceAuth(pub Arc<dyn BearerSource>);

impl crate::auth::AuthProvider for BearerSourceAuth {
    fn add_auth_headers(&self, _headers: &mut HeaderMap) -> Result<(), crate::auth::AuthError> {
        // The token is fetched asynchronously, in `apply_auth_headers`.
        Err(crate::auth::AuthError::Build(
            "the bearer token is resolved asynchronously".to_string(),
        ))
    }

    fn auth_header_telemetry(
        &self,
    ) -> Result<crate::auth::AuthHeaderTelemetry, crate::auth::AuthError> {
        Ok(crate::auth::AuthHeaderTelemetry {
            attached: true,
            name: Some("authorization"),
        })
    }

    fn apply_auth_headers(&self, mut headers: HeaderMap) -> crate::auth::AuthHeadersFuture<'_> {
        Box::pin(async move {
            let token = self
                .0
                .token()
                .await
                .map_err(|_| crate::auth::AuthError::Transient("no usable sign-in".to_string()))?;
            let mut value = HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|_| crate::auth::AuthError::Build("invalid bearer token".to_string()))?;
            value.set_sensitive(true);
            headers.insert(http::header::AUTHORIZATION, value);
            Ok(headers)
        })
    }

    fn refresh_after_unauthorized(&self, _rejected: HeaderMap) -> crate::auth::AuthRefreshFuture<'_> {
        self.0.refresh()
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

    fn auth_header_telemetry(
        &self,
    ) -> Result<crate::auth::AuthHeaderTelemetry, crate::auth::AuthError> {
        self.0.auth_header_telemetry()
    }

    fn apply_auth_headers(&self, headers: HeaderMap) -> crate::auth::AuthHeadersFuture<'_> {
        Box::pin(async move {
            let mut headers = self.0.apply_auth_headers(headers).await?;
            if let Some(value) = headers.remove(http::header::AUTHORIZATION) {
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
            Ok(headers)
        })
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
        // Claude reports the subscription window usage in response headers.
        let limits = matches!(self.kind, WireKind::AnthropicMessages { .. })
            .then(|| anthropic_rate_limits(&response.headers))
            .flatten();
        let (tx_event, rx_event) = mpsc::channel::<Result<ResponseEvent, ApiError>>(1600);
        tokio::spawn(async move {
            if let Some(snapshot) = limits {
                let _ = tx_event.send(Ok(ResponseEvent::RateLimits(snapshot))).await;
            }
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

/// `anthropic-ratelimit-unified-{5h,7d}-{utilization,reset}`: utilization is a 0-1 fraction,
/// reset is epoch seconds. `None` when the response carries neither window.
pub(crate) fn anthropic_rate_limits(headers: &HeaderMap) -> Option<codex_protocol::protocol::RateLimitSnapshot> {
    let number = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<f64>().ok())
            .filter(|n| n.is_finite())
    };
    let window = |wire: &str, minutes: i64| {
        let used = number(&format!("anthropic-ratelimit-unified-{wire}-utilization"))
            .filter(|fraction| (0.0..=1.0).contains(fraction))?;
        let resets_at = number(&format!("anthropic-ratelimit-unified-{wire}-reset"))
            .filter(|seconds| *seconds > 0.0)
            .map(|seconds| seconds as i64);
        Some(codex_protocol::protocol::RateLimitWindow {
            used_percent: (used * 10_000.0).round() / 100.0,
            window_minutes: Some(minutes),
            resets_at,
        })
    };
    let primary = window("5h", 300);
    let secondary = window("7d", 10_080);
    if primary.is_none() && secondary.is_none() {
        return None;
    }
    Some(codex_protocol::protocol::RateLimitSnapshot {
        limit_id: Some("claude".to_string()),
        limit_name: None,
        normal_model_slug: None,
        primary,
        secondary,
        credits: None,
        individual_limit: None,
        spend_control_reached: None,
        plan_type: None,
        rate_limit_reached_type: None,
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_window_usage_comes_from_the_unified_ratelimit_headers() {
        let mut headers = HeaderMap::new();
        for (name, value) in [
            ("anthropic-ratelimit-unified-5h-utilization", "0.42"),
            ("anthropic-ratelimit-unified-5h-reset", "1794398400"),
            ("anthropic-ratelimit-unified-7d-utilization", "0.1875"),
            ("anthropic-ratelimit-unified-7d-reset", "1794744000"),
        ] {
            headers.insert(name, HeaderValue::from_static(value));
        }
        let snapshot = anthropic_rate_limits(&headers).expect("snapshot");
        let primary = snapshot.primary.expect("5h");
        assert_eq!((primary.used_percent, primary.window_minutes, primary.resets_at), (42.0, Some(300), Some(1_794_398_400)));
        let secondary = snapshot.secondary.expect("7d");
        assert_eq!((secondary.used_percent, secondary.window_minutes), (18.75, Some(10_080)));
        // Out-of-range values and responses without the headers carry no usage.
        let mut bad = HeaderMap::new();
        bad.insert("anthropic-ratelimit-unified-5h-utilization", HeaderValue::from_static("42"));
        assert!(anthropic_rate_limits(&bad).is_none());
        assert!(anthropic_rate_limits(&HeaderMap::new()).is_none());
    }
}
