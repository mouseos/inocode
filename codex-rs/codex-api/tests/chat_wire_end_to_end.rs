#![allow(clippy::expect_used)]
//! `WireClient` against a fixture transport: the request that leaves is a chat/completions body,
//! and the chat stream that comes back arrives as Responses events.

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Result;
use bytes::Bytes;
use codex_api::ApiError;
use codex_api::AuthProvider;
use codex_api::Provider;
use codex_api::ResponseEvent;
use codex_api::ResponsesApiRequest;
use codex_api::ResponsesApiTools;
use codex_api::ResponsesOptions;
use codex_api::WireClient;
use codex_api::WireKind;
use codex_client::HttpTransport;
use codex_client::Request;
use codex_client::RequestBody;
use codex_client::Response;
use codex_client::StreamResponse;
use codex_client::TransportError;
use codex_protocol::models::ResponseItem;
use futures::StreamExt;
use http::HeaderMap;
use http::StatusCode;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;

#[derive(Clone)]
struct ChatTransport {
    sse: String,
    seen: Arc<Mutex<Option<Request>>>,
}

impl HttpTransport for ChatTransport {
    async fn execute(&self, _req: Request) -> Result<Response, TransportError> {
        Err(TransportError::Build("execute should not run".to_string()))
    }

    async fn stream(&self, req: Request) -> Result<StreamResponse, TransportError> {
        *self.seen.lock().expect("lock") = Some(req);
        let stream = futures::stream::iter(vec![Ok::<Bytes, TransportError>(Bytes::from(
            self.sse.clone(),
        ))]);
        Ok(StreamResponse {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            bytes: Box::pin(stream),
        })
    }
}

#[derive(Clone, Default)]
struct KeyAuth;

impl AuthProvider for KeyAuth {
    fn add_auth_headers(&self, headers: &mut HeaderMap) -> Result<(), codex_api::AuthError> {
        headers.insert(
            http::header::AUTHORIZATION,
            "Bearer sk-test-FAKE".parse().expect("header"),
        );
        Ok(())
    }
}

fn provider() -> Provider {
    Provider {
        name: "compat".to_string(),
        base_url: "https://example.test/v1".to_string(),
        query_params: None,
        headers: HeaderMap::new(),
        retry: codex_api::RetryConfig {
            max_attempts: 1,
            base_delay: Duration::from_millis(1),
            retry_429: false,
            retry_5xx: false,
            retry_transport: true,
        },
        stream_idle_timeout: Duration::from_millis(200),
    }
}

fn request() -> ResponsesApiRequest {
    let tools = json!([
        {"type": "function", "name": "shell", "description": "run", "strict": false,
         "parameters": {"type": "object", "properties": {"cmd": {"type": "string"}}}}
    ]);
    let raw = serde_json::value::RawValue::from_string(tools.to_string()).expect("raw");
    let input: Vec<ResponseItem> = serde_json::from_value(json!([
        {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "rules"}]},
        {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "list files"}]}
    ]))
    .expect("input");
    ResponsesApiRequest {
        model: "compat-model".to_string(),
        stream: true,
        service_tier: None,
        input,
        tools: Some(ResponsesApiTools::from(Arc::from(raw))),
        tool_choice: "auto".to_string(),
        parallel_tool_calls: true,
        reasoning: None,
        store: false,
        stream_options: None,
        include: Vec::new(),
        prompt_cache_key: None,
        prompt_cache_options: None,
        text: None,
        client_metadata: None,
        access_programs: None,
    }
}

fn sse(frames: &[&str]) -> String {
    frames.iter().map(|f| format!("data: {f}\n\n")).collect()
}

async fn collect(
    client: &WireClient<ChatTransport>,
) -> Vec<Result<ResponseEvent, ApiError>> {
    let stream = client
        .stream_request(request(), ResponsesOptions::default())
        .await
        .expect("stream");
    stream.collect().await
}

#[tokio::test]
async fn text_and_a_tool_call_arrive_as_responses_events() -> Result<()> {
    let seen = Arc::new(Mutex::new(None));
    let transport = ChatTransport {
        sse: sse(&[
            r#"{"choices":[{"delta":{"content":"Hi"}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"shell","arguments":"{\"cmd\":\"ls\"}"}}]},"finish_reason":"tool_calls"}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":3,"completion_tokens":2}}"#,
            "[DONE]",
        ]),
        seen: Arc::clone(&seen),
    };
    let client = WireClient::new(
        transport,
        provider(),
        Arc::new(KeyAuth),
        WireKind::ChatCompletions(Default::default()),
    );
    let events = collect(&client).await;
    let events: Vec<ResponseEvent> = events.into_iter().collect::<Result<_, _>>()?;

    assert!(matches!(events[0], ResponseEvent::Created { .. }));
    assert!(matches!(&events[1], ResponseEvent::OutputItemAdded(ResponseItem::Message { .. })));
    assert!(matches!(&events[2], ResponseEvent::OutputTextDelta(t) if t == "Hi"));
    assert!(matches!(&events[3], ResponseEvent::OutputItemDone(ResponseItem::Message { .. })));
    assert!(matches!(&events[4], ResponseEvent::OutputItemAdded(ResponseItem::FunctionCall { name, .. }) if name == "shell"));
    let Some(ResponseEvent::OutputItemDone(ResponseItem::FunctionCall {
        arguments, call_id, ..
    })) = events.get(6)
    else {
        panic!("expected the finished call, got {events:?}");
    };
    assert_eq!(arguments, "{\"cmd\":\"ls\"}");
    assert_eq!(call_id, "c1");
    let Some(ResponseEvent::Completed { token_usage, .. }) = events.last() else {
        panic!("expected completion");
    };
    let usage = token_usage.as_ref().expect("usage");
    assert_eq!((usage.input_tokens, usage.output_tokens), (3, 2));

    // What went out: a chat body at /chat/completions, the key header, and none of the
    // first-party session headers.
    let request = seen.lock().expect("lock").take().expect("request");
    assert_eq!(request.url, "https://example.test/v1/chat/completions");
    assert_eq!(
        request.headers.get(http::header::AUTHORIZATION).and_then(|v| v.to_str().ok()),
        Some("Bearer sk-test-FAKE")
    );
    assert!(request.headers.keys().all(|k| !k.as_str().starts_with("x-codex")));
    let Some(RequestBody::EncodedJson(body)) = request.body else {
        panic!("expected a JSON body");
    };
    let body: Value = serde_json::from_slice(body.as_bytes())?;
    assert_eq!(body["model"], "compat-model");
    assert_eq!(body["stream"], true);
    assert_eq!(body["messages"][0], json!({"role": "system", "content": "rules"}));
    assert_eq!(body["messages"][1], json!({"role": "user", "content": "list files"}));
    assert_eq!(body["tools"][0]["function"]["name"], "shell");
    Ok(())
}

#[tokio::test]
async fn rate_limit_and_truncation_become_the_matching_api_errors() -> Result<()> {
    let limited = WireClient::new(
        ChatTransport {
            sse: sse(&[r#"{"error":{"message":"slow down","type":"rate_limit_error","code":429}}"#]),
            seen: Arc::default(),
        },
        provider(),
        Arc::new(KeyAuth),
        WireKind::ChatCompletions(Default::default()),
    );
    let events = collect(&limited).await;
    assert!(matches!(events.last(), Some(Err(ApiError::RateLimitExceeded { .. }))));

    let cut = WireClient::new(
        ChatTransport {
            sse: sse(&[
                r#"{"choices":[{"delta":{"content":"x"},"finish_reason":"length"}]}"#,
                "[DONE]",
            ]),
            seen: Arc::default(),
        },
        provider(),
        Arc::new(KeyAuth),
        WireKind::ChatCompletions(Default::default()),
    );
    let events = collect(&cut).await;
    assert!(matches!(
        events.last(),
        Some(Err(ApiError::ResponseIncomplete(e))) if e.reason == "max_output_tokens"
    ));
    Ok(())
}
