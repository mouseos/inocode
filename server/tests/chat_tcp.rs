mod support;

use std::time::{Duration, Instant};

use inocode_core::{Core, Llm, LlmConfig};
use serde_json::{json, Value};
use support::*;

fn core_for(mock: &MockApi, model: &str) -> std::sync::Arc<Core> {
    let llm = Llm::new(LlmConfig {
        base_url: mock.base_url.clone(),
        model: model.to_string(),
        api_key: "test-key".to_string(),
    })
    .expect("build the provider client");
    std::sync::Arc::new(Core::with_llm(llm))
}

fn echo_handler() -> impl Fn(&MockRequest) -> MockReply + Send + Sync + 'static {
    |request: &MockRequest| {
        let text = reply_for(request);
        MockReply::stream(vec![sse_delta(&text), sse_done()])
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

async fn call_on(client: &mut Client, stream_id: u64, request: Value) -> Value {
    client
        .1
        .write_json(stream_id, &request)
        .await
        .expect("send request");
    await_stream(client, stream_id).await
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

#[tokio::test]
async fn chat_request_is_sent_to_the_provider_and_answered() {
    with_timeout(async {
        let mock = start_mock(echo_handler()).await;
        let (addr, _stop, _handle) = start_server(core_for(&mock, "test-model")).await;
        let mut client = connect(addr).await;

        let (kinds, text, terminal) = collect_chat(
            &mut client,
            7,
            json!({"id": 7, "method": "chat", "params": {"prompt": "hello"}}),
        )
        .await;
        assert_eq!(kinds, ["started", "delta", "completed"]);
        assert_eq!(text, "reply[hello]");
        assert_eq!(terminal, json!({"id": 7, "event": "completed"}));

        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/v1/chat/completions");
        assert_eq!(request.authorization.as_deref(), Some("Bearer test-key"));
        assert_eq!(request.body["model"], "test-model");
        assert_eq!(request.body["stream"], true);
        assert_eq!(request.body["messages"][0]["role"], "user");
        assert_eq!(request.body["messages"][0]["content"], "hello");
    })
    .await;
}

#[tokio::test]
async fn deltas_are_forwarded_while_the_provider_is_still_generating() {
    with_timeout(async {
        let mock = start_mock(|_: &MockRequest| {
            MockReply::stream(vec![
                sse_delta("一"),
                sse_delta("二"),
                sse_delta("三"),
                sse_delta("四"),
                sse_done(),
            ])
            .with_delay(Duration::from_millis(120))
        })
        .await;
        let (addr, _stop, _handle) = start_server(core_for(&mock, "test-model")).await;
        let mut client = connect(addr).await;

        let sent = Instant::now();
        client
            .1
            .write_json(
                1,
                &json!({"id": 1, "method": "chat", "params": {"prompt": "stream"}}),
            )
            .await
            .expect("send chat");
        let mut first_delta = None;
        let mut text = String::new();
        let completed = loop {
            let event = await_stream(&mut client, 1).await;
            match event["event"].as_str().unwrap_or_default() {
                "started" => {}
                "delta" => {
                    if first_delta.is_none() {
                        first_delta = Some(sent.elapsed());
                    }
                    text.push_str(event["text"].as_str().unwrap_or_default());
                }
                "completed" => break sent.elapsed(),
                other => panic!("unexpected event {other}"),
            }
        };

        assert_eq!(text, "一二三四");
        let first = first_delta.expect("at least one delta");
        assert!(
            first + Duration::from_millis(300) < completed,
            "the first delta arrived at {first:?} of {completed:?}, so it was not streamed"
        );
    })
    .await;
}

#[tokio::test]
async fn sse_bytes_split_across_writes_are_reassembled() {
    with_timeout(async {
        let event = sse_delta("分割された応答");
        let mut chunks: Vec<Vec<u8>> = event.chunks(5).map(<[u8]>::to_vec).collect();
        chunks.push(sse_done());
        let mock = start_mock(move |_: &MockRequest| {
            MockReply::stream(chunks.clone()).with_delay(Duration::from_millis(10))
        })
        .await;
        let (addr, _stop, _handle) = start_server(core_for(&mock, "test-model")).await;
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
async fn cancel_stops_generation_midstream() {
    with_timeout(async {
        let mock = start_mock(|_: &MockRequest| {
            let mut chunks: Vec<Vec<u8>> = (0..200).map(|_| sse_delta("x")).collect();
            chunks.push(sse_done());
            MockReply::stream(chunks).with_delay(Duration::from_millis(50))
        })
        .await;
        let (addr, _stop, _handle) = start_server(core_for(&mock, "test-model")).await;
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

        client
            .1
            .write_json(
                2,
                &json!({"id": 2, "method": "cancel", "params": {"request_id": 1}}),
            )
            .await
            .expect("send cancel");

        let mut cancel_response = None;
        let mut cancelled = false;
        while cancel_response.is_none() || !cancelled {
            let (stream, event): (u64, Value) = client.0.read_json().await.expect("read message");
            match stream {
                1 => match event["event"].as_str().unwrap_or_default() {
                    "cancelled" => cancelled = true,
                    "completed" => panic!("a cancelled generation must not complete"),
                    _ => {}
                },
                2 => cancel_response = Some(event),
                other => panic!("unexpected stream {other}"),
            }
        }
        assert_eq!(
            cancel_response.expect("cancel response")["result"]["cancelled"],
            true
        );

        let deadline = Instant::now() + Duration::from_secs(10);
        while mock.write_errors() == 0 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            mock.write_errors() > 0,
            "cancelling must close the provider connection"
        );
    })
    .await;
}

#[tokio::test]
async fn duplicate_ids_are_rejected_and_unknown_cancels_report_false() {
    with_timeout(async {
        let mock = start_mock(|_: &MockRequest| {
            MockReply::stream(vec![sse_delta("slow"), sse_done()])
                .with_delay(Duration::from_millis(200))
        })
        .await;
        let (addr, _stop, _handle) = start_server(core_for(&mock, "test-model")).await;
        let mut client = connect(addr).await;

        client
            .1
            .write_json(
                1,
                &json!({"id": 1, "method": "chat", "params": {"prompt": "first"}}),
            )
            .await
            .expect("send first chat");
        client
            .1
            .write_json(
                2,
                &json!({"id": 1, "method": "chat", "params": {"prompt": "again"}}),
            )
            .await
            .expect("send duplicate chat");

        let duplicate = await_stream(&mut client, 2).await;
        assert_eq!(duplicate["event"], "failed");
        assert_eq!(duplicate["error"]["code"], "duplicate_id");

        let unknown = call_on(
            &mut client,
            3,
            json!({"id": 3, "method": "cancel", "params": {"request_id": 99}}),
        )
        .await;
        assert_eq!(unknown["result"]["cancelled"], false);

        let cleanup = call_on(
            &mut client,
            4,
            json!({"id": 4, "method": "cancel", "params": {"request_id": 1}}),
        )
        .await;
        assert_eq!(cleanup["result"]["cancelled"], true);
    })
    .await;
}

#[tokio::test]
async fn concurrent_chats_keep_their_ids_and_texts_apart() {
    with_timeout(async {
        let mock = start_mock(|request: &MockRequest| {
            let text = reply_for(request);
            let split = text.len() / 2;
            MockReply::stream(vec![
                sse_delta(&text[..split]),
                sse_delta(&text[split..]),
                sse_done(),
            ])
            .with_delay(Duration::from_millis(20))
        })
        .await;
        let (addr, _stop, _handle) = start_server(core_for(&mock, "test-model")).await;

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
                    assert_eq!(
                        event["id"].as_u64(),
                        Some(stream),
                        "event id must match its stream"
                    );
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
async fn provider_http_errors_are_reported_as_failed_events() {
    with_timeout(async {
        let mock = start_mock(|_: &MockRequest| {
            MockReply::status(429, r#"{"error":{"message":"rate limited"}}"#)
        })
        .await;
        let (addr, _stop, _handle) = start_server(core_for(&mock, "test-model")).await;
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
async fn a_stream_that_ends_before_completion_fails() {
    with_timeout(async {
        let mock = start_mock(|_: &MockRequest| MockReply::stream(vec![sse_delta("half")])).await;
        let (addr, _stop, _handle) = start_server(core_for(&mock, "test-model")).await;
        let mut client = connect(addr).await;

        let (kinds, text, terminal) = collect_chat(
            &mut client,
            1,
            json!({"id": 1, "method": "chat", "params": {"prompt": "truncate"}}),
        )
        .await;
        assert_eq!(text, "half");
        assert_eq!(kinds.last(), Some(&"failed".to_string()));
        assert_eq!(terminal["error"]["code"], "provider_stream");
    })
    .await;
}

#[tokio::test]
async fn a_slow_client_pauses_its_own_delivery_without_losing_events() {
    with_timeout(async {
        const DELTAS: usize = 300;
        let mock = start_mock(|_: &MockRequest| {
            let mut chunks: Vec<Vec<u8>> = (0..DELTAS).map(|_| sse_delta("x")).collect();
            chunks.push(sse_done());
            MockReply::stream(chunks)
        })
        .await;
        let (addr, _stop, _handle) = start_server(core_for(&mock, "test-model")).await;
        let mut client = connect(addr).await;

        client
            .1
            .write_json(
                1,
                &json!({"id": 1, "method": "chat", "params": {"prompt": "slow"}}),
            )
            .await
            .expect("send chat");
        tokio::time::sleep(Duration::from_millis(300)).await;

        let mut deltas = 0;
        loop {
            let event = await_stream(&mut client, 1).await;
            match event["event"].as_str().unwrap_or_default() {
                "started" => {}
                "delta" => deltas += 1,
                "completed" => break,
                other => panic!("unexpected event {other}"),
            }
        }
        assert_eq!(deltas, DELTAS, "every delta must survive a pause");
    })
    .await;
}

#[tokio::test]
async fn the_server_keeps_answering_while_it_generates() {
    with_timeout(async {
        let mock = start_mock(|_: &MockRequest| {
            let mut chunks: Vec<Vec<u8>> = (0..100).map(|_| sse_delta("x")).collect();
            chunks.push(sse_done());
            MockReply::stream(chunks).with_delay(Duration::from_millis(40))
        })
        .await;
        let (addr, _stop, _handle) = start_server(core_for(&mock, "test-model")).await;
        let mut client = connect(addr).await;

        client
            .1
            .write_json(
                1,
                &json!({"id": 1, "method": "chat", "params": {"prompt": "busy"}}),
            )
            .await
            .expect("send chat");
        assert_eq!(await_stream(&mut client, 1).await["event"], "started");

        // Another request on the same connection must be answered during generation.
        client
            .1
            .write_json(2, &json!({"id": 2, "method": "ping"}))
            .await
            .expect("send ping");
        let ping = await_stream(&mut client, 2).await;
        assert_eq!(ping, json!({"id": 2, "result": {"pong": true}}));

        let mut remaining = 0;
        loop {
            let event = await_stream(&mut client, 1).await;
            match event["event"].as_str().unwrap_or_default() {
                "delta" => remaining += 1,
                "completed" => break,
                other => panic!("unexpected event {other}"),
            }
        }
        assert!(
            remaining > 10,
            "the ping answer arrived after only {remaining} deltas, so it was not concurrent"
        );

        // A second client is served at the same time as well.
        let mut other = connect(addr).await;
        let echo = call(
            &mut other,
            3,
            json!({"id": 3, "method": "echo", "params": {"ok": true}}),
        )
        .await;
        assert_eq!(echo["result"]["ok"], true);
    })
    .await;
}

#[tokio::test]
async fn a_client_disconnect_during_generation_keeps_the_server_serving() {
    with_timeout(async {
        let mock = start_mock(|_: &MockRequest| {
            let mut chunks: Vec<Vec<u8>> = (0..200).map(|_| sse_delta("x")).collect();
            chunks.push(sse_done());
            MockReply::stream(chunks).with_delay(Duration::from_millis(30))
        })
        .await;
        let (addr, _stop, _handle) = start_server(core_for(&mock, "test-model")).await;

        let mut client = connect(addr).await;
        client
            .1
            .write_json(
                1,
                &json!({"id": 1, "method": "chat", "params": {"prompt": "drop me"}}),
            )
            .await
            .expect("send chat");
        assert_eq!(await_stream(&mut client, 1).await["event"], "started");
        drop(client);

        let mut other = connect(addr).await;
        let ping = call(&mut other, 2, json!({"id": 2, "method": "ping"})).await;
        assert_eq!(ping["result"]["pong"], true);

        let deadline = Instant::now() + Duration::from_secs(10);
        while mock.write_errors() == 0 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            mock.write_errors() > 0,
            "a generation must stop when its client disconnects"
        );
    })
    .await;
}

#[tokio::test]
async fn chat_without_a_configured_provider_fails() {
    with_timeout(async {
        let (addr, _stop, _handle) = start_server(std::sync::Arc::new(Core::without_llm())).await;
        let mut client = connect(addr).await;

        let (kinds, _, terminal) = collect_chat(
            &mut client,
            1,
            json!({"id": 1, "method": "chat", "params": {"prompt": "hi"}}),
        )
        .await;
        assert_eq!(kinds, ["failed"]);
        assert_eq!(terminal["error"]["code"], "provider_unconfigured");
    })
    .await;
}

#[tokio::test]
async fn ping_and_echo_still_work_with_a_provider_configured() {
    with_timeout(async {
        let mock = start_mock(echo_handler()).await;
        let (addr, _stop, _handle) = start_server(core_for(&mock, "test-model")).await;
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

/// Sends a cancel request on `cancel_stream` and waits for the terminal event
/// of the generation on `chat_stream`.
async fn cancel_and_read_terminal(
    client: &mut Client,
    chat_stream: u64,
    request_id: u64,
    cancel_stream: u64,
) -> (Value, Vec<String>, String) {
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
    let mut text = String::new();
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
                if kind == "delta" {
                    text.push_str(message["text"].as_str().unwrap_or_default());
                }
                kinds.push(kind.clone());
                if matches!(kind.as_str(), "completed" | "failed" | "cancelled") {
                    if let Some(response) = cancel_response {
                        return (response, kinds, text);
                    }
                }
            }
            other => panic!("unexpected stream {other}"),
        }
    }
}

#[tokio::test]
async fn cancel_works_while_waiting_for_the_response_headers() {
    with_timeout(async {
        let mock = start_mock(|_: &MockRequest| {
            MockReply::stream(vec![sse_delta("late"), sse_done()])
                .with_head_delay(Duration::from_secs(2))
        })
        .await;
        let (addr, _stop, _handle) = start_server(core_for(&mock, "test-model")).await;
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
        let (response, kinds, text) = cancel_and_read_terminal(&mut client, 1, 1, 2).await;

        assert_eq!(response["result"]["cancelled"], true);
        assert_eq!(kinds, ["cancelled"], "no other event may appear: {kinds:?}");
        assert!(text.is_empty());

        // Cancelling must close the provider connection. The close is observed
        // by reading on the mock: a single write can still succeed after the
        // peer closed, depending on the operating system.
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
async fn cancel_works_when_the_provider_never_responds() {
    with_timeout(async {
        let mock = start_mock(|request: &MockRequest| {
            let text = reply_for(request);
            MockReply::stream(vec![sse_delta(&text), sse_done()]).with_head_delay(
                if request.body["messages"][0]["content"] == "stuck" {
                    Duration::from_secs(30)
                } else {
                    Duration::ZERO
                },
            )
        })
        .await;
        let (addr, _stop, _handle) = start_server(core_for(&mock, "test-model")).await;
        let mut client = connect(addr).await;

        client
            .1
            .write_json(
                1,
                &json!({"id": 1, "method": "chat", "params": {"prompt": "stuck"}}),
            )
            .await
            .expect("send chat");
        tokio::time::sleep(Duration::from_millis(250)).await;
        let (response, kinds, text) = cancel_and_read_terminal(&mut client, 1, 1, 2).await;

        assert_eq!(response["result"]["cancelled"], true);
        assert_eq!(kinds, ["cancelled"]);
        assert!(text.is_empty());

        // The connection stays usable after the hung request was cancelled.
        let (kinds, text, terminal) = collect_chat(
            &mut client,
            3,
            json!({"id": 3, "method": "chat", "params": {"prompt": "again"}}),
        )
        .await;
        assert_eq!(kinds, ["started", "delta", "completed"]);
        assert_eq!(text, "reply[again]");
        assert_eq!(terminal["event"], "completed");
    })
    .await;
}

#[tokio::test]
async fn a_cancelled_request_id_stays_taken_until_the_old_generation_settles() {
    with_timeout(async {
        let mock = start_mock(|request: &MockRequest| {
            let text = reply_for(request);
            MockReply::stream(vec![sse_delta(&text), sse_done()]).with_head_delay(
                if request.body["messages"][0]["content"] == "first" {
                    Duration::from_secs(20)
                } else {
                    Duration::ZERO
                },
            )
        })
        .await;
        let (addr, _stop, _handle) = start_server(core_for(&mock, "test-model")).await;
        let mut client = connect(addr).await;

        client
            .1
            .write_json(
                1,
                &json!({"id": 1, "method": "chat", "params": {"prompt": "first"}}),
            )
            .await
            .expect("send chat");
        tokio::time::sleep(Duration::from_millis(250)).await;
        let (response, kinds, _) = cancel_and_read_terminal(&mut client, 1, 1, 2).await;
        assert_eq!(response["result"]["cancelled"], true);
        assert_eq!(kinds, ["cancelled"]);

        // Reusing the id right after the cancel answer is either still
        // rejected while the old generation settles, or starts a clean fresh
        // generation once the old task is done. Both are allowed; a mixed or
        // duplicated event stream is not.
        let immediate = call_on(
            &mut client,
            1,
            json!({"id": 1, "method": "chat", "params": {"prompt": "reuse"}}),
        )
        .await;
        match immediate["event"].as_str() {
            Some("started") => {
                // call_on already consumed the started event.
                let (rest, text, terminal) = await_terminal(&mut client, 1).await;
                assert_eq!(rest, ["delta", "completed"]);
                assert_eq!(text, "reply[reuse]");
                assert_eq!(terminal, json!({"id": 1, "event": "completed"}));
            }
            Some("failed") => assert_eq!(immediate["error"]["code"], "duplicate_id"),
            other => panic!("unexpected response to the reused id: {other:?} {immediate}"),
        }
    })
    .await;
}

/// Reads the events of an already started generation until its terminal.
async fn await_terminal(client: &mut Client, stream_id: u64) -> (Vec<String>, String, Value) {
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

#[tokio::test]
async fn the_id_is_reusable_after_the_cancelled_terminal_and_the_old_task_leaves_no_trace() {
    with_timeout(async {
        let mock = start_mock(|request: &MockRequest| {
            let text = reply_for(request);
            MockReply::stream(vec![sse_delta(&text), sse_done()]).with_head_delay(
                if request.body["messages"][0]["content"] == "first" {
                    Duration::from_secs(20)
                } else {
                    Duration::ZERO
                },
            )
        })
        .await;
        let (addr, _stop, _handle) = start_server(core_for(&mock, "test-model")).await;
        let mut client = connect(addr).await;

        client
            .1
            .write_json(
                1,
                &json!({"id": 1, "method": "chat", "params": {"prompt": "first"}}),
            )
            .await
            .expect("send chat");
        tokio::time::sleep(Duration::from_millis(250)).await;
        let (response, kinds, _) = cancel_and_read_terminal(&mut client, 1, 1, 2).await;
        assert_eq!(response["result"]["cancelled"], true);
        assert_eq!(kinds, ["cancelled"]);

        // Wait until the terminal is queued and the id freed, then reuse it.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let probe = call_on(
                &mut client,
                1,
                json!({"id": 1, "method": "chat", "params": {"prompt": "reuse"}}),
            )
            .await;
            if probe["event"] == "started" {
                break;
            }
            assert_eq!(
                probe["error"]["code"], "duplicate_id",
                "unexpected error: {probe}"
            );
            assert!(Instant::now() < deadline, "the id was never freed");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // The reused request must run cleanly: the old task must have removed
        // its own registry entry and never touch the new one. call_on already
        // consumed its started event.
        let (kinds, text, terminal) = await_terminal(&mut client, 1).await;
        assert_eq!(kinds, ["delta", "completed"]);
        assert_eq!(text, "reply[reuse]");
        assert_eq!(terminal, json!({"id": 1, "event": "completed"}));

        // And one more full generation on the same id works as well.
        let (kinds, text, terminal) = collect_chat(
            &mut client,
            4,
            json!({"id": 1, "method": "chat", "params": {"prompt": "next"}}),
        )
        .await;
        assert_eq!(kinds, ["started", "delta", "completed"]);
        assert_eq!(text, "reply[next]");
        assert_eq!(terminal["event"], "completed");
    })
    .await;
}

#[tokio::test]
async fn cancel_works_while_the_send_queue_is_full() {
    with_timeout(async {
        const TOTAL: usize = 300;
        let mock = start_mock(move |_: &MockRequest| {
            let mut chunks: Vec<Vec<u8>> = (0..TOTAL).map(|_| sse_delta("x")).collect();
            chunks.push(sse_done());
            MockReply::stream(chunks).with_delay(Duration::from_millis(30))
        })
        .await;
        let (addr, _stop, _handle) = start_server(core_for(&mock, "test-model")).await;
        let mut client = connect(addr).await;

        client
            .1
            .write_json(
                1,
                &json!({"id": 1, "method": "chat", "params": {"prompt": "flood"}}),
            )
            .await
            .expect("send chat");
        // Let the bounded queue fill while this client refuses to read.
        tokio::time::sleep(Duration::from_millis(2500)).await;
        client
            .1
            .write_json(
                2,
                &json!({"id": 2, "method": "cancel", "params": {"request_id": 1}}),
            )
            .await
            .expect("send cancel");

        let mut cancel_response = None;
        let mut deltas = 0;
        let mut terminal = None;
        while cancel_response.is_none() || terminal.is_none() {
            let (stream, message): (u64, Value) = client.0.read_json().await.expect("read message");
            match stream {
                2 => cancel_response = Some(message),
                1 => match message["event"].as_str().unwrap_or_default() {
                    "delta" => deltas += 1,
                    "started" => {}
                    other => terminal = Some(other.to_string()),
                },
                other => panic!("unexpected stream {other}"),
            }
        }
        assert_eq!(
            cancel_response.expect("cancel response")["result"]["cancelled"],
            true
        );
        assert_eq!(terminal.expect("terminal"), "cancelled");
        assert!(
            deltas < TOTAL,
            "a cancelled generation must not deliver every delta: {deltas}/{TOTAL}"
        );
    })
    .await;
}

#[tokio::test]
async fn disconnecting_clients_leave_no_generation_behind() {
    with_timeout(async {
        let mock = start_mock(|_: &MockRequest| {
            let mut chunks: Vec<Vec<u8>> = (0..200).map(|_| sse_delta("x")).collect();
            chunks.push(sse_done());
            MockReply::stream(chunks).with_delay(Duration::from_millis(40))
        })
        .await;
        let (addr, _stop, _handle) = start_server(core_for(&mock, "test-model")).await;

        for round in 0..4usize {
            let mut client = connect(addr).await;
            client
                .1
                .write_json(
                    1,
                    &json!({"id": round as u64 + 1, "method": "chat", "params": {"prompt": "drop"}}),
                )
                .await
                .expect("send chat");
            assert_eq!(await_stream(&mut client, 1).await["event"], "started");
            drop(client);

            // The abandoned generation must end: the provider connection is
            // closed and the server keeps serving new clients.
            let deadline = Instant::now() + Duration::from_secs(10);
            while mock.write_errors() <= round && Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            assert!(
                mock.write_errors() > round,
                "round {round}: the generation was not abandoned"
            );

            let mut other = connect(addr).await;
            let ping = call(&mut other, 9, json!({"id": 9, "method": "ping"})).await;
            assert_eq!(ping["result"]["pong"], true);
        }
    })
    .await;
}

#[tokio::test]
async fn many_concurrent_generations_stay_isolated_and_complete() {
    with_timeout(async {
        let mock = start_mock(|request: &MockRequest| {
            let text = reply_for(request);
            let split = text.len() / 2;
            MockReply::stream(vec![
                sse_delta(&text[..split]),
                sse_delta(&text[split..]),
                sse_done(),
            ])
            .with_delay(Duration::from_millis(5))
        })
        .await;
        let (addr, _stop, _handle) = start_server(core_for(&mock, "test-model")).await;
        let mut client = connect(addr).await;

        const GENERATIONS: u64 = 30;
        for id in 1..=GENERATIONS {
            client
                .1
                .write_json(
                    id,
                    &json!({"id": id, "method": "chat", "params": {"prompt": format!("gen-{id}")}}),
                )
                .await
                .expect("send chat");
        }

        let mut texts: std::collections::HashMap<u64, String> = std::collections::HashMap::new();
        let mut completed = 0;
        while completed < GENERATIONS {
            let (stream, event): (u64, Value) = client.0.read_json().await.expect("read event");
            assert_eq!(
                event["id"].as_u64(),
                Some(stream),
                "event id must match its stream"
            );
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
        for id in 1..=GENERATIONS {
            assert_eq!(
                texts.get(&id).cloned().unwrap_or_default(),
                format!("reply[gen-{id}]"),
                "generation {id} received another answer"
            );
        }

        // Duplicates are still rejected while a generation runs.
        client
            .1
            .write_json(
                1,
                &json!({"id": 1, "method": "chat", "params": {"prompt": "slow-again"}}),
            )
            .await
            .expect("send chat");
        let duplicate = call_on(
            &mut client,
            99,
            json!({"id": 1, "method": "chat", "params": {"prompt": "x"}}),
        )
        .await;
        assert_eq!(duplicate["error"]["code"], "duplicate_id");
        let cleanup = call_on(
            &mut client,
            98,
            json!({"id": 98, "method": "cancel", "params": {"request_id": 1}}),
        )
        .await;
        assert_eq!(cleanup["result"]["cancelled"], true);
    })
    .await;
}
