mod support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use inocode_core::store::Store;
use inocode_core::{Core, Llm, LlmConfig, ProviderKind};
use serde_json::{json, Value};
use support::*;

/// A fresh database path: tests run in parallel and the clock may not tell
/// them apart.
fn database() -> std::path::PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let index = NEXT.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!(
        "inocode-turn-{}-{nanos}-{index}.db",
        std::process::id()
    ))
}

async fn core_for(mock: &MockApi) -> Arc<Core> {
    let llm = Llm::new(LlmConfig {
        provider: ProviderKind::OpenAi,
        auth: inocode_core::oauth::AuthKind::ApiKey,
        base_url: mock.base_url.clone(),
        model: "test-model".to_string(),
        api_key: "test-key".to_string(),
        auth_store_path: None,
        reasoning: Default::default(),
    })
    .expect("client");
    let store = Store::open(database()).await.expect("store");
    Arc::new(Core::with_llm(llm).with_store(store))
}

fn tool_call_chunk() -> Vec<u8> {
    let payload = json!({ "choices": [{ "delta": { "tool_calls": [{
        "index": 0, "id": "call_1", "type": "function",
        "function": { "name": "read", "arguments": "{\"path\":\"a\"}" }
    }] } }] });
    format!("data: {payload}\n\n").into_bytes()
}

fn finish(reason: &str) -> Vec<u8> {
    let payload = json!({ "choices": [{ "delta": {}, "finish_reason": reason }] });
    format!("data: {payload}\n\n").into_bytes()
}

async fn thread(client: &mut Client) -> i64 {
    let response = call(client, 1, json!({"id": 1, "method": "thread/start"})).await;
    response["result"]["thread_id"].as_i64().expect("thread id")
}

/// Sends `turn/start` and collects its events through the terminal one.
async fn turn(client: &mut Client, stream_id: u64, thread: i64, prompt: &str) -> Vec<Value> {
    client
        .1
        .write_json(
            stream_id,
            &json!({"id": stream_id, "method": "turn/start",
                    "params": {"thread_id": thread, "prompt": prompt}}),
        )
        .await
        .expect("send");
    let mut events = Vec::new();
    loop {
        let event = next_message(client, stream_id).await;
        let kind = event["event"].as_str().unwrap_or_default().to_string();
        events.push(event);
        if matches!(kind.as_str(), "completed" | "failed" | "cancelled") {
            return events;
        }
    }
}

fn kinds(events: &[Value]) -> Vec<&str> {
    events
        .iter()
        .map(|event| event["event"].as_str().unwrap_or_default())
        .collect()
}

fn roles(events: &[Value]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|event| event["message"]["message"]["body"]["role"].as_str())
        .collect()
}

#[tokio::test]
async fn a_turn_streams_stores_and_reports_why_it_ended() {
    with_timeout(async {
        let mock = start_mock(|_: &MockRequest| {
            MockReply::stream(vec![
                sse_delta("he"),
                sse_delta("llo"),
                finish("stop"),
                sse_done(),
            ])
        })
        .await;
        let (addr, _stop, _handle) = start_server(core_for(&mock).await).await;
        let mut client = connect(addr).await;
        let thread = thread(&mut client).await;

        let events = turn(&mut client, 2, thread, "hi").await;
        assert_eq!(
            kinds(&events),
            [
                "started",
                "message",
                "delta",
                "delta",
                "message",
                "completed"
            ]
        );
        assert_eq!(roles(&events), ["user", "assistant"]);
        let last = events.last().unwrap();
        assert_eq!(last["reason"], "completed");
        assert_eq!(last["usage"]["requests"], 1);

        let read = call(
            &mut client,
            3,
            json!({"id": 3, "method": "thread/read", "params": {"thread_id": thread}}),
        )
        .await;
        let messages = read["result"]["messages"].as_array().expect("messages");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1]["message"]["body"]["parts"][0]["text"], "hello");

        // The next turn sends the stored history.
        turn(&mut client, 4, thread, "again").await;
        let sent = &mock.requests()[1].body["messages"];
        assert_eq!(sent.as_array().map(Vec::len), Some(3));
        assert_eq!(sent[1]["content"], "hello");
    })
    .await;
}

#[tokio::test]
async fn a_turn_goes_on_after_tool_calls_and_answers_each_one() {
    with_timeout(async {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let mock = start_mock(move |_: &MockRequest| {
            if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                MockReply::stream(vec![tool_call_chunk(), finish("tool_calls"), sse_done()])
            } else {
                MockReply::stream(vec![sse_delta("done"), finish("stop"), sse_done()])
            }
        })
        .await;
        let (addr, _stop, _handle) = start_server(core_for(&mock).await).await;
        let mut client = connect(addr).await;
        let thread = thread(&mut client).await;

        let events = turn(&mut client, 2, thread, "read a").await;
        assert_eq!(
            roles(&events),
            ["user", "assistant", "tool_result", "assistant"]
        );
        let last = events.last().unwrap();
        assert_eq!(last["event"], "completed");
        assert_eq!(last["usage"]["requests"], 2);
        let second = &mock.requests()[1].body["messages"];
        assert_eq!(second[2]["role"], "tool");
        assert_eq!(second[2]["tool_call_id"], "call_1");
    })
    .await;
}

#[tokio::test]
async fn one_turn_runs_per_thread_and_it_can_be_steered_and_interrupted() {
    with_timeout(async {
        let mock = start_mock(|_: &MockRequest| {
            let mut chunks: Vec<Vec<u8>> = (0..200).map(|_| sse_delta("x")).collect();
            chunks.push(sse_done());
            MockReply::stream(chunks).with_delay(Duration::from_millis(20))
        })
        .await;
        let (addr, _stop, _handle) = start_server(core_for(&mock).await).await;
        let mut client = connect(addr).await;
        let thread = thread(&mut client).await;

        client
            .1
            .write_json(
                2,
                &json!({"id": 2, "method": "turn/start",
                        "params": {"thread_id": thread, "prompt": "long"}}),
            )
            .await
            .expect("send");
        loop {
            if next_message(&mut client, 2).await["event"] == "delta" {
                break;
            }
        }

        let busy = turn(&mut client, 3, thread, "second").await;
        assert_eq!(busy[0]["event"], "failed");
        assert_eq!(busy[0]["error"]["code"], "turn_running");

        let steered = call(
            &mut client,
            4,
            json!({"id": 4, "method": "turn/steer",
                   "params": {"thread_id": thread, "prompt": "also this"}}),
        )
        .await;
        assert_eq!(steered["result"]["accepted"], true);

        let cancel = call(
            &mut client,
            5,
            json!({"id": 5, "method": "cancel", "params": {"request_id": 2}}),
        )
        .await;
        assert_eq!(cancel["result"]["cancelled"], true);
        let mut last = Value::Null;
        while !matches!(
            last["event"].as_str(),
            Some("completed" | "failed" | "cancelled")
        ) {
            last = next_message(&mut client, 2).await;
        }
        assert_eq!(last["event"], "cancelled");

        // The interrupted answer and the steering input are stored; the
        // thread is free again.
        let read = call(
            &mut client,
            6,
            json!({"id": 6, "method": "thread/read", "params": {"thread_id": thread}}),
        )
        .await;
        let messages = read["result"]["messages"].as_array().expect("messages");
        assert_eq!(messages[1]["message"]["body"]["stop"], "aborted");
        assert_eq!(messages[2]["message"]["body"]["parts"][0], "also this");
        let idle = call(
            &mut client,
            7,
            json!({"id": 7, "method": "turn/steer",
                   "params": {"thread_id": thread, "prompt": "late"}}),
        )
        .await;
        assert_eq!(idle["result"]["accepted"], false);
    })
    .await;
}

#[tokio::test]
async fn thread_requests_report_their_errors() {
    with_timeout(async {
        let mock = start_mock(|_: &MockRequest| MockReply::stream(vec![sse_done()])).await;
        let (addr, _stop, _handle) = start_server(core_for(&mock).await).await;
        let mut client = connect(addr).await;

        let unknown = turn(&mut client, 2, 999, "hi").await;
        assert_eq!(unknown[0]["error"]["code"], "unknown_thread");
        let missing = turn(&mut client, 3, 0, "  ").await;
        assert_eq!(missing[0]["error"]["code"], "invalid_params");
        let read = call(
            &mut client,
            4,
            json!({"id": 4, "method": "thread/read", "params": {"thread_id": 1, "limit": 0}}),
        )
        .await;
        assert_eq!(read["error"]["code"], "invalid_params");

        let (addr, _stop2, _handle2) = start_server(Arc::new(Core::without_llm())).await;
        let mut client = connect(addr).await;
        let response = call(&mut client, 1, json!({"id": 1, "method": "thread/start"})).await;
        assert_eq!(response["error"]["code"], "store_unconfigured");
    })
    .await;
}
