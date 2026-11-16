mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use inocode_core::{Core, Llm, LlmConfig, ProviderKind};
use serde_json::{json, Value};
use support::*;

fn core_for(
    mock: &MockApi,
    model: &str,
    provider: ProviderKind,
    base_url: Option<String>,
) -> Arc<Core> {
    let llm = Llm::new(LlmConfig {
        provider,
        auth: inocode_core::oauth::AuthKind::ApiKey,
        base_url: base_url.unwrap_or_else(|| mock.base_url.clone()),
        model: model.to_string(),
        api_key: "test-key".to_string(),
        auth_store_path: None,
    })
    .expect("build the provider client");
    Arc::new(Core::with_llm(llm))
}

fn echo_handler() -> impl Fn(&MockRequest) -> MockReply + Send + Sync + 'static {
    |request: &MockRequest| {
        let text = reply_for(request);
        MockReply::stream(anthropic_stream(&text))
    }
}

/// Reads messages until one for `stream_id` arrives, ignoring the others.
async fn await_stream(client: &mut Client, stream_id: u64) -> Value {
    loop {
        let (stream, message): (u64, Value) = client.0.read_json().await.expect("read message");
        if stream == stream_id {
            return message;
        }
    }
}

/// Sends a chat request and collects its events until a terminal one.
async fn collect_chat(
    client: &mut Client,
    stream_id: u64,
    request: Value,
) -> (Vec<String>, String, Value) {
    client
        .1
        .write_json(stream_id, &request)
        .await
        .expect("send chat");
    let mut kinds = Vec::new();
    let mut text = String::new();
    loop {
        let event = await_stream(client, stream_id).await;
        let kind = event["event"].as_str().unwrap_or_default().to_string();
        if kind == "delta" {
            text.push_str(event["text"].as_str().unwrap_or_default());
        }
        kinds.push(kind.clone());
        if matches!(kind.as_str(), "completed" | "failed" | "cancelled") {
            return (kinds, text, event);
        }
    }
}

/// Sends a cancel request on `cancel_stream` and waits for the terminal event
/// of the generation on `chat_stream`.
async fn cancel_and_read_terminal(
    client: &mut Client,
    chat_stream: u64,
    request_id: u64,
    cancel_stream: u64,
) -> (Value, Vec<String>) {
    client
        .1
        .write_json(
            cancel_stream,
            &json!({"id": 9, "method": "cancel", "params": {"request_id": request_id}}),
        )
        .await
        .expect("send cancel");
    let mut cancel_response = None;
    let mut kinds = Vec::new();
    loop {
        let (stream, message): (u64, Value) = client.0.read_json().await.expect("read message");
        match stream {
            s if s == cancel_stream => {
                if message.get("result").is_some() {
                    cancel_response = Some(message);
                }
            }
            s if s == chat_stream => {
                let kind = message["event"].as_str().unwrap_or_default().to_string();
                kinds.push(kind.clone());
                if matches!(kind.as_str(), "completed" | "failed" | "cancelled") {
                    if let Some(response) = cancel_response {
                        return (response, kinds);
                    }
                }
            }
            other => panic!("unexpected stream {other}"),
        }
    }
}

#[tokio::test]
async fn chat_request_reaches_the_messages_endpoint_with_anthropic_headers() {
    with_timeout(async {
        let mock = start_mock(echo_handler()).await;
        let (addr, _stop, _handle) =
            start_server(core_for(&mock, "test-model", ProviderKind::Anthropic, None)).await;
        let mut client = connect(addr).await;

        let prompt = "海について書いて";
        let (kinds, text, terminal) = collect_chat(
            &mut client,
            7,
            json!({"id": 7, "method": "chat", "params": {"prompt": prompt}}),
        )
        .await;
        assert_eq!(kinds, ["started", "delta", "completed"]);
        assert_eq!(text, format!("reply[{prompt}]"));
        assert_eq!(terminal, json!({"id": 7, "event": "completed"}));

        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/v1/messages");
        // The credential must travel as x-api-key, not as a bearer token.
        assert_eq!(
            request.headers.get("x-api-key").map(String::as_str),
            Some("test-key")
        );
        assert_eq!(request.authorization, None);
        assert_eq!(
            request.headers.get("anthropic-version").map(String::as_str),
            Some("2023-06-01")
        );
        assert_eq!(
            request.headers.get("content-type").map(String::as_str),
            Some("application/json")
        );
        assert_eq!(request.body["model"], "test-model");
        assert_eq!(request.body["max_tokens"], 8192);
        assert_eq!(request.body["stream"], true);
        assert_eq!(request.body["messages"][0]["role"], "user");
        assert_eq!(
            request.body["messages"][0]["content"],
            json!([{"type": "text", "text": prompt}])
        );
    })
    .await;
}

#[tokio::test]
async fn a_base_url_with_a_trailing_slash_does_not_duplicate_v1() {
    with_timeout(async {
        let mock = start_mock(echo_handler()).await;
        // The mock serves /v1; the configured base already carries it.
        let base = format!("{}/", mock.base_url);
        let (addr, _stop, _handle) = start_server(core_for(
            &mock,
            "test-model",
            ProviderKind::Anthropic,
            Some(base),
        ))
        .await;
        let mut client = connect(addr).await;

        let (_, text, terminal) = collect_chat(
            &mut client,
            1,
            json!({"id": 1, "method": "chat", "params": {"prompt": "slash"}}),
        )
        .await;
        assert_eq!(text, "reply[slash]");
        assert_eq!(terminal["event"], "completed");
        assert_eq!(mock.requests()[0].path, "/v1/messages");
    })
    .await;
}

#[tokio::test]
async fn a_base_url_without_v1_reaches_v1_messages() {
    with_timeout(async {
        let mock = start_mock(echo_handler()).await;
        // Anthropic clients set the base to the host root; the official
        // endpoint path /v1/messages is appended by the client.
        let host_root = mock
            .base_url
            .trim_end_matches('/')
            .trim_end_matches("/v1")
            .to_string();
        let (addr, _stop, _handle) = start_server(core_for(
            &mock,
            "test-model",
            ProviderKind::Anthropic,
            Some(host_root),
        ))
        .await;
        let mut client = connect(addr).await;

        let (_, text, terminal) = collect_chat(
            &mut client,
            1,
            json!({"id": 1, "method": "chat", "params": {"prompt": "root base"}}),
        )
        .await;
        assert_eq!(text, "reply[root base]");
        assert_eq!(terminal["event"], "completed");
        assert_eq!(mock.requests()[0].path, "/v1/messages");
    })
    .await;
}

#[tokio::test]
async fn sse_events_split_at_arbitrary_bytes_are_reassembled() {
    with_timeout(async {
        let mut chunks: Vec<Vec<u8>> = anthropic_stream("分割された応答")
            .into_iter()
            .flat_map(|event: Vec<u8>| {
                event
                    .chunks(7)
                    .map(<[u8]>::to_vec)
                    .collect::<Vec<Vec<u8>>>()
            })
            .collect();
        chunks.push(sse_done());
        let mock = start_mock(move |_: &MockRequest| {
            MockReply::stream(chunks.clone()).with_delay(Duration::from_millis(10))
        })
        .await;
        let (addr, _stop, _handle) =
            start_server(core_for(&mock, "test-model", ProviderKind::Anthropic, None)).await;
        let mut client = connect(addr).await;

        let (kinds, text, terminal) = collect_chat(
            &mut client,
            1,
            json!({"id": 1, "method": "chat", "params": {"prompt": "split"}}),
        )
        .await;
        assert_eq!(kinds, ["started", "delta", "completed"]);
        assert_eq!(text, "分割された応答");
        assert_eq!(terminal["event"], "completed");
    })
    .await;
}

#[tokio::test]
async fn a_stream_that_ends_without_message_stop_fails() {
    with_timeout(async {
        // message_start .. content_block_stop, but no message_stop.
        let expected = "reply[truncate]";
        let events: Vec<Vec<u8>> = anthropic_stream(expected)
            .into_iter()
            .take(4)
            .chain(std::iter::once(anthropic_event(
                "message_delta",
                &json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{}}),
            )))
            .collect();
        let mock = start_mock(move |_: &MockRequest| MockReply::stream(events.clone())).await;
        let (addr, _stop, _handle) =
            start_server(core_for(&mock, "test-model", ProviderKind::Anthropic, None)).await;
        let mut client = connect(addr).await;

        let (kinds, text, terminal) = collect_chat(
            &mut client,
            1,
            json!({"id": 1, "method": "chat", "params": {"prompt": "truncate"}}),
        )
        .await;
        assert_eq!(text, expected);
        assert_eq!(kinds.last(), Some(&"failed".to_string()));
        assert_eq!(terminal["error"]["code"], "provider_stream");
        assert!(
            terminal["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("message_stop"),
            "the failure must name the missing completion signal: {terminal}"
        );
    })
    .await;
}

#[tokio::test]
async fn an_error_event_inside_the_stream_is_a_failure_not_a_completion() {
    with_timeout(async {
        let mock = start_mock(|_: &MockRequest| {
            let mut events: Vec<Vec<u8>> = anthropic_stream("reply[error]")
                .into_iter()
                .take(4)
                .collect();
            events.push(anthropic_error("Overloaded"));
            MockReply::stream(events)
        })
        .await;
        let (addr, _stop, _handle) =
            start_server(core_for(&mock, "test-model", ProviderKind::Anthropic, None)).await;
        let mut client = connect(addr).await;

        let (kinds, text, terminal) = collect_chat(
            &mut client,
            1,
            json!({"id": 1, "method": "chat", "params": {"prompt": "error"}}),
        )
        .await;
        assert_eq!(text, "reply[error]");
        assert_eq!(kinds.last(), Some(&"failed".to_string()));
        assert_eq!(terminal["error"]["code"], "provider_stream");
        assert_eq!(terminal["error"]["message"], "Overloaded");
    })
    .await;
}

#[tokio::test]
async fn non_text_deltas_are_not_joined_into_the_answer() {
    with_timeout(async {
        let mock = start_mock(|request: &MockRequest| {
            let text = reply_for(request);
            // Deliver the text as two text_delta events with a non-text delta
            // between them.
            let half = text.chars().count() / 2;
            let first: String = text.chars().take(half).collect();
            let second: String = text.chars().skip(half).collect();
            let events = vec![
                anthropic_stream(&first).remove(0),
                anthropic_stream(&first).remove(1),
                anthropic_stream(&first).remove(2),
                anthropic_text_delta(&first),
                anthropic_other_delta(),
                anthropic_text_delta(&second),
                anthropic_event(
                    "content_block_stop",
                    &json!({ "type": "content_block_stop", "index": 0 }),
                ),
                anthropic_event(
                    "message_delta",
                    &json!({ "type": "message_delta", "delta": { "stop_reason": "end_turn", "stop_sequence": null }, "usage": { "output_tokens": 15 } }),
                ),
                anthropic_message_stop(),
            ];
            MockReply::stream(events)
        })
        .await;
        let (addr, _stop, _handle) =
            start_server(core_for(&mock, "test-model", ProviderKind::Anthropic, None)).await;
        let mut client = connect(addr).await;

        let (kinds, text, terminal) = collect_chat(
            &mut client,
            1,
            json!({"id": 1, "method": "chat", "params": {"prompt": "mixed"}}),
        )
        .await;
        // The input_json_delta payload must not appear in the answer.
        assert_eq!(text, "reply[mixed]");
        assert!(kinds.iter().filter(|k| k == &&"delta".to_string()).count() >= 2);
        assert_eq!(terminal["event"], "completed");
    })
    .await;
}

#[tokio::test]
async fn provider_http_errors_are_reported_as_failed_events() {
    with_timeout(async {
        let mock = start_mock(|_: &MockRequest| {
            MockReply::status(429, r#"{"error":{"message":"rate limited"}}"#)
        })
        .await;
        let (addr, _stop, _handle) =
            start_server(core_for(&mock, "test-model", ProviderKind::Anthropic, None)).await;
        let mut client = connect(addr).await;

        let (kinds, _, terminal) = collect_chat(
            &mut client,
            1,
            json!({"id": 1, "method": "chat", "params": {"prompt": "fail"}}),
        )
        .await;
        assert_eq!(
            kinds,
            ["failed"],
            "a provider error must not report started"
        );
        assert_eq!(terminal["error"]["code"], "provider_http");
        assert!(
            terminal["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("429"),
            "the error message must name the status: {terminal}"
        );
    })
    .await;
}

#[tokio::test]
async fn cancel_works_while_waiting_for_the_response_headers() {
    with_timeout(async {
        let mock = start_mock(|_: &MockRequest| {
            MockReply::stream(anthropic_stream("late")).with_head_delay(Duration::from_secs(2))
        })
        .await;
        let (addr, _stop, _handle) =
            start_server(core_for(&mock, "test-model", ProviderKind::Anthropic, None)).await;
        let mut client = connect(addr).await;

        client
            .1
            .write_json(
                1,
                &json!({"id": 1, "method": "chat", "params": {"prompt": "slow"}}),
            )
            .await
            .expect("send chat");
        tokio::time::sleep(Duration::from_millis(250)).await;
        let (response, kinds) = cancel_and_read_terminal(&mut client, 1, 1, 2).await;

        assert_eq!(response["result"]["cancelled"], true);
        assert_eq!(kinds, ["cancelled"], "no other event may appear: {kinds:?}");

        let deadline = Instant::now() + Duration::from_secs(10);
        while mock.closed_count() == 0 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            mock.closed_count() > 0,
            "the abandoned HTTP request must be closed"
        );
    })
    .await;
}

#[tokio::test]
async fn cancel_works_while_streaming() {
    with_timeout(async {
        let mock = start_mock(|_: &MockRequest| {
            let mut events: Vec<Vec<u8>> = (0..200)
                .flat_map(|_| {
                    vec![
                        anthropic_text_delta("x"),
                        anthropic_event("ping", &json!({"type":"ping"})),
                    ]
                })
                .collect();
            events.push(anthropic_message_stop());
            MockReply::stream(events).with_delay(Duration::from_millis(30))
        })
        .await;
        let (addr, _stop, _handle) =
            start_server(core_for(&mock, "test-model", ProviderKind::Anthropic, None)).await;
        let mut client = connect(addr).await;

        client
            .1
            .write_json(
                1,
                &json!({"id": 1, "method": "chat", "params": {"prompt": "cancel me"}}),
            )
            .await
            .expect("send chat");
        assert_eq!(await_stream(&mut client, 1).await["event"], "started");
        assert_eq!(await_stream(&mut client, 1).await["event"], "delta");

        let (response, kinds) = cancel_and_read_terminal(&mut client, 1, 1, 2).await;
        assert_eq!(response["result"]["cancelled"], true);
        assert_eq!(kinds.last(), Some(&"cancelled".to_string()));
        assert!(!kinds.contains(&"completed".to_string()));

        let deadline = Instant::now() + Duration::from_secs(10);
        while mock.closed_count() == 0 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            mock.closed_count() > 0,
            "cancelling must close the provider connection"
        );

        // The connection still works after the cancel.
        let mut other = connect(addr).await;
        let ping = call(&mut other, 3, json!({"id": 3, "method": "ping"})).await;
        assert_eq!(ping["result"]["pong"], true);
    })
    .await;
}

#[tokio::test]
async fn concurrent_chats_keep_their_ids_and_texts_apart() {
    with_timeout(async {
        let mock = start_mock(|request: &MockRequest| {
            let text = reply_for(request);
            let half = text.chars().count() / 2;
            let first: String = text.chars().take(half).collect();
            let second: String = text.chars().skip(half).collect();
            let events = vec![
                anthropic_stream(&first).remove(0),
                anthropic_stream(&first).remove(1),
                anthropic_stream(&first).remove(2),
                anthropic_text_delta(&first),
                anthropic_text_delta(&second),
                anthropic_event(
                    "content_block_stop",
                    &json!({ "type": "content_block_stop", "index": 0 }),
                ),
                anthropic_event(
                    "message_delta",
                    &json!({ "type": "message_delta", "delta": { "stop_reason": "end_turn", "stop_sequence": null }, "usage": { "output_tokens": 15 } }),
                ),
                anthropic_message_stop(),
            ];
            MockReply::stream(events).with_delay(Duration::from_millis(20))
        })
        .await;
        let (addr, _stop, _handle) =
            start_server(core_for(&mock, "test-model", ProviderKind::Anthropic, None)).await;

        let mut tasks = Vec::new();
        for client_index in 0..3u64 {
            tasks.push(tokio::spawn(async move {
                let mut client = connect(addr).await;
                for id in 1..=2u64 {
                    let prompt = format!("c{client_index}-r{id}");
                    client
                        .1
                        .write_json(
                            id,
                            &json!({"id": id, "method": "chat", "params": {"prompt": prompt}}),
                        )
                        .await
                        .expect("send chat");
                }
                let mut texts: std::collections::HashMap<u64, String> =
                    std::collections::HashMap::new();
                let mut completed = 0;
                while completed < 2 {
                    let (stream, event): (u64, Value) =
                        client.0.read_json().await.expect("read message");
                    assert_eq!(event["id"].as_u64(), Some(stream), "event id must match its stream");
                    match event["event"].as_str().unwrap_or_default() {
                        "started" => {}
                        "delta" => texts
                            .entry(stream)
                            .or_default()
                            .push_str(event["text"].as_str().unwrap_or_default()),
                        "completed" => completed += 1,
                        other => panic!("unexpected event {other}"),
                    }
                }
                for id in 1..=2u64 {
                    assert_eq!(
                        texts.get(&id).cloned().unwrap_or_default(),
                        format!("reply[c{client_index}-r{id}]"),
                        "client {client_index} request {id} received another answer"
                    );
                }
            }));
        }
        for task in tasks {
            task.await.expect("client task");
        }
    })
    .await;
}

#[tokio::test]
async fn ping_and_echo_still_work_with_the_anthropic_provider() {
    with_timeout(async {
        let mock = start_mock(echo_handler()).await;
        let (addr, _stop, _handle) =
            start_server(core_for(&mock, "test-model", ProviderKind::Anthropic, None)).await;
        let mut client = connect(addr).await;

        let ping = call(&mut client, 1, json!({"id": 1, "method": "ping"})).await;
        assert_eq!(ping, json!({"id": 1, "result": {"pong": true}}));

        let echo = call(
            &mut client,
            2,
            json!({"id": 2, "method": "echo", "params": {"text": "日本語"}}),
        )
        .await;
        assert_eq!(echo, json!({"id": 2, "result": {"text": "日本語"}}));
    })
    .await;
}
