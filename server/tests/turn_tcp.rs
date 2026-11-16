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
    std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
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
        let chats = chats(&mock);
        let sent = &chats[1].body["messages"];
        assert_eq!(sent.as_array().map(Vec::len), Some(4));
        assert_eq!(sent[0]["role"], "system");
        assert_eq!(sent[2]["content"], "hello");
    })
    .await;
}

#[tokio::test]
async fn a_turn_goes_on_after_tool_calls_and_answers_each_one() {
    with_timeout(async {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let mock = start_mock(move |request: &MockRequest| {
            if request.method == "GET" {
                return MockReply::status(404, "{}");
            }
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
        let chats = chats(&mock);
        let second = &chats[1].body["messages"];
        assert_eq!(second[3]["role"], "tool");
        assert_eq!(second[3]["tool_call_id"], "call_1");
        assert_eq!(second[3]["content"], "no tool named read is available");
    })
    .await;
}

fn call_chunk(id: &str, name: &str, arguments: &Value) -> Vec<u8> {
    let payload = json!({ "choices": [{ "delta": { "tool_calls": [{
        "index": 0, "id": id, "type": "function",
        "function": { "name": name, "arguments": arguments.to_string() }
    }] } }] });
    format!("data: {payload}\n\n").into_bytes()
}

/// A mock that answers its requests with `replies` in order, then with a
/// plain "done".
async fn scripted(replies: Vec<Vec<Vec<u8>>>) -> MockApi {
    let count = AtomicUsize::new(0);
    start_mock(move |request: &MockRequest| {
        if request.method == "GET" {
            return MockReply::status(404, "{}");
        }
        let index = count.fetch_add(1, Ordering::SeqCst);
        MockReply::stream(
            replies
                .get(index)
                .cloned()
                .unwrap_or_else(|| vec![sse_delta("done"), finish("stop"), sse_done()]),
        )
    })
    .await
}

fn tool_turn(id: &str, name: &str, arguments: Value) -> Vec<Vec<u8>> {
    vec![
        call_chunk(id, name, &arguments),
        finish("tool_calls"),
        sse_done(),
    ]
}

async fn core_and_store(mock: &MockApi) -> (Arc<Core>, Store) {
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
    (
        Arc::new(Core::with_llm(llm).with_store(store.clone())),
        store,
    )
}

fn last_message(request: &MockRequest) -> &Value {
    let messages = request.body["messages"].as_array().expect("messages");
    messages.last().expect("a message")
}

fn tool_results(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .filter(|event| event["message"]["message"]["body"]["role"] == "tool_result")
        .map(|event| {
            event["message"]["message"]["body"]["content"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        })
        .collect()
}

#[tokio::test]
async fn the_plan_is_shown_after_the_history_with_every_request() {
    with_timeout(async {
        let items = json!({"items": [
            {"step": "write the parser", "status": "completed"},
            {"step": "test it", "status": "in_progress"}
        ]});
        let mock = scripted(vec![tool_turn("p1", "plan", items)]).await;
        let (core, _store) = core_and_store(&mock).await;
        let (addr, _stop, _handle) = start_server(core).await;
        let mut client = connect(addr).await;
        let thread = thread(&mut client).await;

        let events = turn(&mut client, 2, thread, "build it").await;
        assert_eq!(
            tool_results(&events),
            ["Plan updated: 0 pending, 1 in progress, 1 completed."]
        );
        let requests = chats(&mock);
        assert!(requests[0].body["tools"]
            .as_array()
            .is_some_and(|tools| tools.len() == 3));
        let state = last_message(&requests[1]);
        assert_eq!(state["role"], "user");
        assert_eq!(
            state["content"],
            "Current plan:\n[x] write the parser\n[>] test it\n"
        );
        // The plan is not stored in the history.
        let read = call(
            &mut client,
            3,
            json!({"id": 3, "method": "thread/read", "params": {"thread_id": thread}}),
        )
        .await;
        assert_eq!(read["result"]["messages"].as_array().map(Vec::len), Some(4));

        let bad = json!({"items": [
            {"step": "a", "status": "in_progress"}, {"step": "b", "status": "in_progress"}
        ]});
        let mock = scripted(vec![tool_turn("p2", "plan", bad)]).await;
        let (core, _store) = core_and_store(&mock).await;
        let (addr, _stop2, _handle2) = start_server(core).await;
        let mut client = connect(addr).await;
        let second = self::thread(&mut client).await;
        let events = turn(&mut client, 2, second, "go").await;
        assert_eq!(
            tool_results(&events),
            ["at most one step may be in_progress"]
        );
    })
    .await;
}

#[tokio::test]
async fn recall_reads_earlier_messages_word_for_word() {
    with_timeout(async {
        let mock = scripted(vec![
            vec![sse_delta("first answer"), finish("stop"), sse_done()],
            tool_turn("r1", "recall", json!({"from_seq": 1, "to_seq": 2})),
            tool_turn("r2", "recall", json!({"query": "first"})),
        ])
        .await;
        let (core, _store) = core_and_store(&mock).await;
        let (addr, _stop, _handle) = start_server(core).await;
        let mut client = connect(addr).await;
        let thread = thread(&mut client).await;
        turn(&mut client, 2, thread, "the first question").await;

        let events = turn(&mut client, 3, thread, "what did I ask?").await;
        let results = tool_results(&events);
        assert_eq!(
            results[0],
            "#1 user: the first question\n#2 assistant: first answer\n"
        );
        assert!(results[1].contains("#1 user"), "{}", results[1]);
    })
    .await;
}

#[tokio::test]
async fn lessons_need_a_condition_and_rules_are_not_duplicated() {
    with_timeout(async {
        let project = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "inocode-project-{}-{}",
            std::process::id(),
            database().file_name().unwrap().to_string_lossy()
        ));
        std::fs::create_dir_all(&project).expect("project");
        std::fs::write(
            project.join("AGENTS.md"),
            "# 規則\n- 依存関係を勝手に追加しない。\n",
        )
        .expect("rules");

        let mock = scripted(vec![
            tool_turn(
                "l1",
                "lesson",
                json!({
                    "when": "the tablet is in fastboot mode",
                    "do": "use fastboot commands; adb works only once Android or recovery runs",
                    "because": "adb has no device to talk to in fastboot",
                    "relation": "new"
                }),
            ),
            tool_turn(
                "l2",
                "lesson",
                json!({
                    "when": "x", "do": "", "because": "y", "relation": "new"
                }),
            ),
            tool_turn(
                "l3",
                "lesson",
                json!({
                    "when": "adding a crate", "do": "ask first", "because": "rule",
                    "relation": "restates_rule", "rule_quote": "依存関係を勝手に追加しない"
                }),
            ),
            tool_turn(
                "l4",
                "lesson",
                json!({
                    "when": "adding a crate", "do": "add it freely", "because": "faster",
                    "relation": "contradicts_rule", "rule_quote": "依存関係を**勝手に**追加しない。"
                }),
            ),
            tool_turn(
                "l5",
                "lesson",
                json!({
                    "when": "a", "do": "b", "because": "c",
                    "relation": "restates_rule", "rule_quote": "not in the file"
                }),
            ),
        ])
        .await;
        let (core, store) = core_and_store(&mock).await;
        let (addr, _stop, _handle) = start_server(core).await;
        let mut client = connect(addr).await;
        let response = call(
            &mut client,
            1,
            json!({"id": 1, "method": "thread/start",
                   "params": {"project": project.to_string_lossy()}}),
        )
        .await;
        let thread = response["result"]["thread_id"].as_i64().expect("thread");

        let events = turn(&mut client, 2, thread, "flash the tablet").await;
        let results = tool_results(&events);
        assert!(
            results[0].starts_with("Stored as candidate lesson"),
            "{}",
            results[0]
        );
        assert_eq!(results[1], "when, do, and because are all required");
        assert_eq!(
            results[2],
            "Not stored: the project rules already say this. Follow the rule."
        );
        assert!(results[3].contains("Opened issue"), "{}", results[3]);
        assert_eq!(
            results[4],
            "rule_quote must be text from the project rules (AGENTS.md)"
        );

        let chats = chats(&mock);
        let system = &chats[0].body["messages"][0];
        assert!(system["content"]
            .as_str()
            .is_some_and(|text| text.contains("依存関係を勝手に追加しない。")));

        let project_id = store
            .project(&project.to_string_lossy())
            .await
            .expect("project");
        let issues = store
            .issues(Some(project_id), false, 10)
            .await
            .expect("issues");
        assert_eq!(issues.len(), 1);
        let _ = std::fs::remove_dir_all(&project);
    })
    .await;
}

#[tokio::test]
async fn ask_rules_hold_calls_and_the_turn_goes_on() {
    with_timeout(async {
        let plan = json!({"items": [{"step": "deploy to prod", "status": "pending"}]});
        let mock = scripted(vec![
            tool_turn("a1", "plan", plan.clone()),
            vec![sse_delta("did the rest"), finish("stop"), sse_done()],
            tool_turn("a2", "plan", plan),
        ])
        .await;
        let (core, store) = core_and_store(&mock).await;
        let ask = store
            .add_rule(None, "Ask before anything touches prod.")
            .await
            .expect("rule");
        store
            .propose_enforcement(
                ask,
                inocode_core::rules::Enforcement {
                    tool: "plan".into(),
                    patterns: vec!["prod".into()],
                    action: inocode_core::rules::Action::Ask,
                },
            )
            .await
            .expect("propose");
        store.approve_enforcement(ask).await.expect("approve");
        let (addr, _stop, _handle) = start_server(core).await;
        let mut client = connect(addr).await;
        let thread = thread(&mut client).await;

        let events = turn(&mut client, 2, thread, "ship it").await;
        assert!(tool_results(&events)[0].starts_with("Held for the person's approval"));
        let last = events.last().unwrap();
        assert_eq!(last["event"], "completed");
        assert_eq!(last["held"][0]["rules"][0], ask.0);
        assert_eq!(last["held"][0]["tool"], "plan");

        // Approved for the next turn, the same call runs.
        client
            .1
            .write_json(
                3,
                &json!({"id": 3, "method": "turn/start",
                        "params": {"thread_id": thread, "prompt": "approved", "allow": [ask.0]}}),
            )
            .await
            .expect("send");
        let mut results = Vec::new();
        loop {
            let event = next_message(&mut client, 3).await;
            if event["message"]["message"]["body"]["role"] == "tool_result" {
                results.push(event["message"]["message"]["body"]["content"].clone());
            }
            if event["event"] == "completed" {
                assert_eq!(event["held"], json!([]));
                break;
            }
        }
        assert_eq!(
            results[0],
            "Plan updated: 1 pending, 0 in progress, 0 completed."
        );
    })
    .await;
}

#[tokio::test]
async fn the_third_identical_failure_carries_a_warning() {
    with_timeout(async {
        let same = |id: &str| tool_turn(id, "missing", json!({"a": 1}));
        let mock = scripted(vec![same("x1"), same("x2"), same("x3")]).await;
        let (core, _store) = core_and_store(&mock).await;
        let (addr, _stop, _handle) = start_server(core).await;
        let mut client = connect(addr).await;
        let thread = thread(&mut client).await;
        let events = turn(&mut client, 2, thread, "go").await;
        let results = tool_results(&events);
        assert_eq!(results[0], "no tool named missing is available");
        assert_eq!(results[1], results[0]);
        assert!(
            results[2].contains("failed the same way 3 times"),
            "{}",
            results[2]
        );
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

/// The chat requests the mock received, without model discovery.
fn chats(mock: &MockApi) -> Vec<MockRequest> {
    mock.requests()
        .into_iter()
        .filter(|request| request.method == "POST")
        .collect()
}

fn usage_chunk(input: u64) -> Vec<u8> {
    let payload =
        json!({ "choices": [], "usage": {"prompt_tokens": input, "completion_tokens": 5} });
    format!("data: {payload}\n\n").into_bytes()
}

#[tokio::test]
async fn near_the_window_older_work_is_compacted_and_the_person_kept() {
    with_timeout(async {
        let step = AtomicUsize::new(0);
        let mock = start_mock(move |request: &MockRequest| {
            if request.method == "GET" {
                return MockReply::status(
                    200,
                    &json!({"data": [{"id": "test-model", "context_length": 1000}]}).to_string(),
                );
            }
            if request
                .body
                .to_string()
                .contains("will be replaced by your summary")
            {
                return MockReply::stream(vec![
                    sse_delta("SUMMARY TEXT"),
                    finish("stop"),
                    sse_done(),
                ]);
            }
            let plan = |step: &str| json!({"items": [{"step": step, "status": "in_progress"}]});
            match step.fetch_add(1, Ordering::SeqCst) {
                0 => MockReply::stream(vec![
                    call_chunk("c1", "plan", &plan("one")),
                    finish("tool_calls"),
                    usage_chunk(100),
                    sse_done(),
                ]),
                1 => MockReply::stream(vec![
                    call_chunk("c2", "plan", &plan("two")),
                    finish("tool_calls"),
                    usage_chunk(900),
                    sse_done(),
                ]),
                _ => MockReply::stream(vec![sse_delta("done"), finish("stop"), sse_done()]),
            }
        })
        .await;
        let (core, _store) = core_and_store(&mock).await;
        let (addr, _stop, _handle) = start_server(core).await;
        let mut client = connect(addr).await;
        let thread = thread(&mut client).await;

        let events = turn(&mut client, 2, thread, "do the work").await;
        assert_eq!(events.last().unwrap()["event"], "completed");
        let compaction = events
            .iter()
            .find(|event| {
                event["message"]["message"]["body"]["from"]["harness"]
                    .as_str()
                    .is_some_and(|kind| kind.starts_with("compaction:"))
            })
            .expect("a compaction message");
        let parts = &compaction["message"]["message"]["body"]["parts"];
        assert!(parts[0].as_str().unwrap().contains("plan"));
        assert!(parts[1].as_str().unwrap().contains("SUMMARY TEXT"));

        let chats = chats(&mock);
        let last = &chats.last().unwrap().body["messages"];
        let text = last.to_string();
        assert!(text.contains("do the work"), "{text}");
        assert!(text.contains("SUMMARY TEXT"), "{text}");
        assert!(
            !text.contains("\"c1\""),
            "the first call is replaced: {text}"
        );
        assert!(text.contains("\"c2\""), "the latest call is kept: {text}");
        assert_eq!(last[0]["role"], "system");

        // Nothing stored is lost.
        let read = call(
            &mut client,
            3,
            json!({"id": 3, "method": "thread/read", "params": {"thread_id": thread}}),
        )
        .await;
        let stored = read["result"]["messages"].to_string();
        assert!(stored.contains("\"c1\""));
    })
    .await;
}
