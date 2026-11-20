//! Compaction by a model named in `INOCODE_COMPACT_MODEL`. The variable is
//! set for this whole test process, so these tests live apart from the
//! other turn tests.

mod support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use inocode_core::store::Store;
use inocode_core::{Core, Llm, LlmConfig, ProviderKind};
use serde_json::{json, Value};
use support::*;

const SMALL: &str = "small-model";

fn database() -> std::path::PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let index = NEXT.fetch_add(1, Ordering::SeqCst);
    std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "inocode-compaction-{}-{index}.db",
        std::process::id()
    ))
}

async fn core_for(mock: &MockApi) -> Arc<Core> {
    std::env::set_var("INOCODE_COMPACT_MODEL", SMALL);
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

fn models() -> MockReply {
    MockReply::status(
        200,
        &json!({"data": [
            {"id": "test-model", "context_length": 10000},
            {"id": SMALL, "context_length": 2000}
        ]})
        .to_string(),
    )
}

fn call_chunk(id: &str, step: &str) -> Vec<u8> {
    let arguments = json!({"items": [{"step": step, "status": "in_progress"}]});
    let payload = json!({ "choices": [{ "delta": { "tool_calls": [{
        "index": 0, "id": id, "type": "function",
        "function": { "name": "plan", "arguments": arguments.to_string() }
    }] } }] });
    format!("data: {payload}\n\n").into_bytes()
}

fn finish(reason: &str) -> Vec<u8> {
    let payload = json!({ "choices": [{ "delta": {}, "finish_reason": reason }] });
    format!("data: {payload}\n\n").into_bytes()
}

fn usage_chunk(input: u64) -> Vec<u8> {
    let payload =
        json!({ "choices": [], "usage": {"prompt_tokens": input, "completion_tokens": 5} });
    format!("data: {payload}\n\n").into_bytes()
}

/// Three steps with long plan texts; the third reports most of the window.
fn work(step: usize) -> MockReply {
    let long = |mark: &str| format!("{mark} {}", "x".repeat(1_900));
    match step {
        0 => MockReply::stream(vec![
            call_chunk("c1", &long("ONE")),
            finish("tool_calls"),
            usage_chunk(3_000),
            sse_done(),
        ]),
        1 => MockReply::stream(vec![
            call_chunk("c2", &long("TWO")),
            finish("tool_calls"),
            usage_chunk(6_000),
            sse_done(),
        ]),
        2 => MockReply::stream(vec![
            call_chunk("c3", &long("THREE")),
            finish("tool_calls"),
            usage_chunk(9_000),
            sse_done(),
        ]),
        _ => MockReply::stream(vec![sse_delta("done"), finish("stop"), sse_done()]),
    }
}

fn is_summary_request(request: &MockRequest) -> bool {
    request.body["model"] == SMALL
}

async fn run_turn(mock: &MockApi) -> Vec<Value> {
    let (addr, _stop, _handle) = start_server(core_for(mock).await).await;
    let mut client = connect(addr).await;
    let response = call(&mut client, 1, json!({"id": 1, "method": "thread/start"})).await;
    let thread = response["result"]["thread_id"].as_i64().expect("thread");
    client
        .1
        .write_json(
            2,
            &json!({"id": 2, "method": "turn/start",
                    "params": {"thread_id": thread, "prompt": "do the work"}}),
        )
        .await
        .expect("send");
    let mut events = Vec::new();
    loop {
        let event = next_message(&mut client, 2).await;
        let kind = event["event"].as_str().unwrap_or_default().to_string();
        events.push(event);
        if matches!(kind.as_str(), "completed" | "failed" | "cancelled") {
            return events;
        }
    }
}

fn compaction_parts(events: &[Value]) -> Vec<String> {
    let compaction = events
        .iter()
        .find(|event| {
            event["message"]["message"]["body"]["from"]["harness"]
                .as_str()
                .is_some_and(|kind| kind.starts_with("compaction:"))
        })
        .expect("a compaction message");
    compaction["message"]["message"]["body"]["parts"]
        .as_array()
        .expect("parts")
        .iter()
        .map(|part| part.as_str().unwrap_or_default().to_string())
        .collect()
}

#[tokio::test]
async fn the_named_model_summarizes_in_parts_when_the_history_exceeds_its_window() {
    with_timeout(async {
        let step = AtomicUsize::new(0);
        let summaries = AtomicUsize::new(0);
        let mock = start_mock(move |request: &MockRequest| {
            if request.method == "GET" {
                return models();
            }
            if is_summary_request(request) {
                let index = summaries.fetch_add(1, Ordering::SeqCst) + 1;
                return MockReply::stream(vec![
                    sse_delta(&format!("PART SUMMARY {index}")),
                    finish("stop"),
                    sse_done(),
                ]);
            }
            work(step.fetch_add(1, Ordering::SeqCst))
        })
        .await;
        let events = run_turn(&mock).await;
        assert_eq!(events.last().unwrap()["event"], "completed");

        let requests: Vec<MockRequest> = mock
            .requests()
            .into_iter()
            .filter(is_summary_request)
            .collect();
        assert!(requests.len() >= 2, "{} summary requests", requests.len());
        for request in &requests {
            assert!(request.body.get("tools").is_none());
            let text = request.body.to_string();
            assert!(text.contains("too long to read at once"), "{text}");
        }
        let second = requests[1].body.to_string();
        assert!(second.contains("PART SUMMARY 1"), "{second}");

        let parts = compaction_parts(&events);
        assert!(
            parts[1].contains(&format!("PART SUMMARY {}", requests.len())),
            "{}",
            parts[1]
        );
    })
    .await;
}

#[tokio::test]
async fn without_a_summary_the_call_record_stands_in_and_the_work_goes_on() {
    with_timeout(async {
        let step = AtomicUsize::new(0);
        let mock = start_mock(move |request: &MockRequest| {
            if request.method == "GET" {
                return models();
            }
            if is_summary_request(request) {
                return MockReply::status(500, r#"{"error":{"message":"overloaded"}}"#);
            }
            work(step.fetch_add(1, Ordering::SeqCst))
        })
        .await;
        let events = run_turn(&mock).await;
        assert_eq!(events.last().unwrap()["event"], "completed");

        let parts = compaction_parts(&events);
        assert!(parts[0].contains("plan"), "{}", parts[0]);
        assert!(
            parts[1].starts_with("No summary of earlier work could be written"),
            "{}",
            parts[1]
        );
    })
    .await;
}
