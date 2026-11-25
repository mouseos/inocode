//! The company over real TCP with mock providers: members and allow lists
//! over the protocol, `delegate` to a member that edits a file, `review` by
//! a member of another vendor, refusals of what is not allowed, the token
//! budget, and when delegated tasks run at the same time
//! (docs/evidence/company.md).

mod support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use inocode_core::store::Store;
use inocode_core::Core;
use serde_json::{json, Value};
use support::*;

/// A folder of its own for each test, with a project folder inside.
fn folder() -> std::path::PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let index = NEXT.fetch_add(1, Ordering::SeqCst);
    let folder = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("inocode-company-{}-{index}", std::process::id()));
    let _ = std::fs::remove_dir_all(&folder);
    std::fs::create_dir_all(folder.join("project")).expect("folder");
    folder
}

/// The lead runs on `lead` (an OpenAI model), the member `coder` on
/// `member` (an Anthropic model), the strict reviewer `doubter` on
/// `reviewer` (a GLM model); `same` is a strict reviewer of the lead's own
/// vendor. All four are allowed by default.
fn settings(lead: &MockApi, member: &MockApi, reviewer: &MockApi) -> Value {
    json!({
        "language": "Japanese",
        "providers": {
            "mock": {"kind": "openai", "base_url": lead.base_url, "default_model": "gpt-lead"},
            "second": {"kind": "openai", "base_url": member.base_url, "default_model": "claude-member"},
            "listed": {"kind": "openai", "base_url": reviewer.base_url, "default_model": "glm-reviewer"}
        },
        "default": {"provider": "mock"},
        "members": [
            {"name": "coder", "provider": "second", "model": "claude-member", "role": "implements simple logic"},
            {"name": "doubter", "provider": "listed", "model": "glm-reviewer",
             "role": "always doubts other vendors' code; strict reviewer"},
            {"name": "same", "provider": "mock", "model": "gpt-other", "role": "strict reviewer"}
        ],
        "allow": {"default": ["coder", "doubter", "same"]}
    })
}

async fn core_with(folder: &std::path::Path, settings: Value) -> Arc<Core> {
    std::fs::write(folder.join("settings.json"), settings.to_string()).expect("settings");
    let credentials = folder.join("auth.json");
    inocode_core::secrets::set_mode(inocode_core::secrets::StoreMode::Plaintext);
    let mut store = inocode_core::oauth::Store::load(credentials.clone()).expect("credentials");
    for id in ["mock", "second", "listed"] {
        store
            .update(&inocode_core::settings::api_key_name(id), |credential| {
                credential.api_key = Some("test-key".into())
            })
            .expect("key");
    }
    let store = Store::open(folder.join("inocode.db")).await.expect("store");
    Arc::new(Core::new().with_store(store).with_credentials(credentials))
}

fn finish(reason: &str) -> Vec<u8> {
    let payload = json!({ "choices": [{ "delta": {}, "finish_reason": reason }] });
    format!("data: {payload}\n\n").into_bytes()
}

fn call_chunk(id: &str, name: &str, arguments: &Value) -> Vec<u8> {
    call_chunk_at(0, id, name, arguments)
}

fn call_chunk_at(index: usize, id: &str, name: &str, arguments: &Value) -> Vec<u8> {
    let payload = json!({ "choices": [{ "delta": { "tool_calls": [{
        "index": index, "id": id, "type": "function",
        "function": { "name": name, "arguments": arguments.to_string() }
    }] } }] });
    format!("data: {payload}\n\n").into_bytes()
}

fn tool_turn(id: &str, name: &str, arguments: Value) -> Vec<Vec<u8>> {
    vec![
        call_chunk(id, name, &arguments),
        finish("tool_calls"),
        sse_done(),
    ]
}

fn text_turn(text: &str) -> Vec<Vec<u8>> {
    vec![sse_delta(text), finish("stop"), sse_done()]
}

fn usage_chunk(input: u64) -> Vec<u8> {
    let payload =
        json!({ "choices": [], "usage": {"prompt_tokens": input, "completion_tokens": 5} });
    format!("data: {payload}\n\n").into_bytes()
}

/// A mock that answers its chat requests with `replies` in order, then
/// with a plain "done", each after `delay`; model discovery gets 404.
async fn scripted_after(replies: Vec<Vec<Vec<u8>>>, delay: Duration) -> MockApi {
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
                .unwrap_or_else(|| text_turn("done")),
        )
        .with_head_delay(delay)
    })
    .await
}

async fn scripted(replies: Vec<Vec<Vec<u8>>>) -> MockApi {
    scripted_after(replies, Duration::ZERO).await
}

fn chats(mock: &MockApi) -> Vec<MockRequest> {
    mock.requests()
        .into_iter()
        .filter(|request| request.method == "POST")
        .collect()
}

fn tool_names(request: &MockRequest) -> Vec<String> {
    request.body["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|tool| tool["function"]["name"].as_str().map(str::to_string))
        .collect()
}

async fn project_thread(client: &mut Client, project: &std::path::Path) -> i64 {
    let response = call(
        client,
        1,
        json!({"id": 1, "method": "thread/start",
               "params": {"project": project.to_string_lossy()}}),
    )
    .await;
    response["result"]["thread_id"].as_i64().expect("thread")
}

async fn turn(client: &mut Client, id: u64, thread: i64, prompt: &str) -> Vec<Value> {
    client
        .1
        .write_json(
            id,
            &json!({"id": id, "method": "turn/start",
                    "params": {"thread_id": thread, "prompt": prompt}}),
        )
        .await
        .expect("send");
    let mut events = Vec::new();
    loop {
        let event = next_message(client, id).await;
        let kind = event["event"].as_str().unwrap_or_default().to_string();
        events.push(event);
        if matches!(kind.as_str(), "completed" | "failed" | "cancelled") {
            return events;
        }
    }
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

async fn request(client: &mut Client, id: u64, method: &str, params: Value) -> Value {
    call(
        client,
        id,
        json!({"id": id, "method": method, "params": params}),
    )
    .await
}

#[tokio::test]
async fn a_delegated_member_edits_a_file_and_a_member_of_another_vendor_reviews() {
    with_timeout(async {
        let folder = folder();
        let project = folder.join("project");
        let lead = scripted(vec![
            tool_turn(
                "d1",
                "delegate",
                json!({"member": "coder", "task": "Write hello into b.txt.", "files": ["b.txt"]}),
            ),
            tool_turn("r1", "review", json!({"what": "b.txt, written for the person"})),
            text_turn("all done"),
        ])
        .await;
        let member = scripted(vec![
            tool_turn("w1", "write", json!({"path": "b.txt", "content": "hello\n"})),
            text_turn("Wrote b.txt."),
        ])
        .await;
        let reviewer = scripted(vec![text_turn("No findings.")]).await;
        let core = core_with(&folder, settings(&lead, &member, &reviewer)).await;
        let (addr, _stop, _handle) = start_server(core).await;
        let mut client = connect(addr).await;
        let thread = project_thread(&mut client, &project).await;

        let events = turn(&mut client, 2, thread, "put hello in b.txt").await;
        assert_eq!(events.last().unwrap()["event"], "completed", "{events:?}");
        assert_eq!(
            std::fs::read_to_string(project.join("b.txt")).expect("written"),
            "hello\n"
        );
        let results = tool_results(&events);
        assert_eq!(results.len(), 2, "{results:?}");
        let delegated = &results[0];
        assert!(
            delegated.starts_with("member coder (second/claude-member), thread "),
            "{delegated}"
        );
        assert!(delegated.contains(": completed\n"), "{delegated}");
        assert!(delegated.contains("\nedited: b.txt\n"), "{delegated}");
        assert!(delegated.ends_with("report:\nWrote b.txt."), "{delegated}");
        // The review went to the strict member of another vendor than the
        // lead's (openai), not to the strict one of its own vendor.
        let reviewed = &results[1];
        assert!(
            reviewed.starts_with("reviewer doubter (listed/glm-reviewer, vendor zai):"),
            "{reviewed}"
        );
        assert!(reviewed.ends_with("No findings."), "{reviewed}");

        // The lead was offered both tools, with the members named.
        let lead_chats = chats(&lead);
        let offered = tool_names(&lead_chats[0]);
        assert!(offered.contains(&"delegate".to_string()) && offered.contains(&"review".to_string()));
        let tools = lead_chats[0].body["tools"].to_string();
        assert!(tools.contains("coder | implements simple logic | second/claude-member | vendor anthropic"), "{tools}");
        // The member worked on its own model with the same tools as the
        // lead, without delegate or ask, and saw only the task.
        let member_chats = chats(&member);
        assert_eq!(member_chats.len(), 2);
        assert_eq!(member_chats[0].body["model"], "claude-member");
        let tools = tool_names(&member_chats[0]);
        for name in ["read", "edit", "write", "shell", "review"] {
            assert!(tools.contains(&name.to_string()), "{name} in {tools:?}");
        }
        assert!(!tools.contains(&"delegate".to_string()) && !tools.contains(&"ask".to_string()));
        let sent = member_chats[0].body["messages"].to_string();
        assert!(sent.contains("Task from the lead agent for you, the member coder. Your role: implements simple logic"), "{sent}");
        assert!(sent.contains("Write hello into b.txt."), "{sent}");
        // The person's request comes as theirs, the lead's task as not.
        assert!(sent.contains("[command file] /delegate put hello in b.txt\\nTask from"), "{sent}");
        // The reviewer only reads, with its own instructions.
        let review = &chats(&reviewer)[0];
        assert_eq!(tool_names(review), ["find", "read"]);
        assert!(review.body["messages"][0]["content"]
            .as_str()
            .is_some_and(|text| text.starts_with("You review another coding agent's work")));
        let asked = review.body["messages"][1]["content"].as_str().unwrap();
        assert!(asked.starts_with("Your role: always doubts other vendors' code; strict reviewer"), "{asked}");
        assert!(asked.contains("Work by the lead agent (gpt-lead) to review:\nb.txt, written for the person"), "{asked}");

        // The member's thread is kept and linked to the lead's.
        let listed = call(
            &mut client,
            3,
            json!({"id": 3, "method": "command/run",
                   "params": {"thread_id": thread, "text": "/members"}}),
        )
        .await;
        let delegated = &listed["result"]["delegated"][0];
        assert_eq!(delegated["member"], "coder");
        let child = delegated["thread_id"].as_i64().expect("thread");
        assert!(results[0].contains(&format!("thread {child}: completed")));
        let read = request(&mut client, 4, "thread/read", json!({"thread_id": child})).await;
        let messages = read["result"]["messages"].as_array().expect("messages");
        assert_eq!(messages.len(), 4, "{read}");
        // `/profiles` is the same command.
        let aliased = call(
            &mut client,
            5,
            json!({"id": 5, "method": "command/run",
                   "params": {"thread_id": thread, "text": "/profiles"}}),
        )
        .await;
        assert_eq!(aliased["result"]["members"].as_array().map(Vec::len), Some(3));
        // The lead's usage counts the member's and the reviewer's requests.
        assert_eq!(events.last().unwrap()["usage"]["requests"], 6);
    })
    .await;
}

#[tokio::test]
async fn what_is_not_allowed_is_never_called() {
    with_timeout(async {
        let folder = folder();
        let project = folder.join("project");
        let lead = scripted(vec![
            tool_turn(
                "d1",
                "delegate",
                json!({"member": "outsider", "task": "anything"}),
            ),
            tool_turn(
                "r1",
                "review",
                json!({"member": "doubter", "what": "a.txt"}),
            ),
            text_turn("ok"),
        ])
        .await;
        let member = scripted(Vec::new()).await;
        let reviewer = scripted(Vec::new()).await;
        let core = core_with(&folder, settings(&lead, &member, &reviewer)).await;
        let (addr, _stop, _handle) = start_server(core).await;
        let mut client = connect(addr).await;
        let thread = project_thread(&mut client, &project).await;
        // This project allows only the coder.
        let key = project.to_string_lossy().to_string();
        let set = request(
            &mut client,
            2,
            "allow/set",
            json!({"project": key, "list": ["coder"]}),
        )
        .await;
        assert_eq!(set["result"]["effective"], json!(["coder"]), "{set}");
        assert_eq!(set["result"]["source"], "project");

        let events = turn(&mut client, 3, thread, "try").await;
        let results = tool_results(&events);
        assert_eq!(
            results,
            [
                "Not run: outsider is not on this project's allow list of members.",
                "Not run: doubter is not on this project's allow list of members."
            ]
        );
        assert!(chats(&member).is_empty() && chats(&reviewer).is_empty());
        // The tools name only the allowed member.
        let tools = chats(&lead)[0].body["tools"].to_string();
        assert!(
            tools.contains("- coder |") && !tools.contains("- doubter |"),
            "{tools}"
        );
    })
    .await;
}

#[tokio::test]
async fn a_member_stops_at_its_budget_and_a_budget_over_the_maximum_is_refused() {
    with_timeout(async {
        let folder = folder();
        let project = folder.join("project");
        let lead = scripted(vec![
            tool_turn(
                "d1",
                "delegate",
                json!({"member": "coder", "task": "Read a.txt again and again.", "budget_tokens": 50}),
            ),
            tool_turn(
                "d2",
                "delegate",
                json!({"member": "coder", "task": "Big work.", "budget_tokens": 5000}),
            ),
            text_turn("ok"),
        ])
        .await;
        let member = scripted(vec![vec![
            sse_delta("Reading first."),
            call_chunk("r1", "read", &json!({"path": "a.txt"})),
            usage_chunk(100),
            finish("tool_calls"),
            sse_done(),
        ]])
        .await;
        let reviewer = scripted(Vec::new()).await;
        let mut written = settings(&lead, &member, &reviewer);
        written["company"] = json!({"max_budget_tokens": 1000});
        let core = core_with(&folder, written).await;
        let (addr, _stop, _handle) = start_server(core).await;
        let mut client = connect(addr).await;
        let thread = project_thread(&mut client, &project).await;

        let events = turn(&mut client, 2, thread, "go").await;
        let results = tool_results(&events);
        assert!(results[0].contains(": stopped at its token budget of 50\n"), "{}", results[0]);
        assert!(results[0].contains("output 5, requests 1, budget 50"), "{}", results[0]);
        assert!(results[0].ends_with("report:\nReading first."), "{}", results[0]);
        assert_eq!(chats(&member).len(), 1, "no request after the budget was used up");
        assert_eq!(
            results[1],
            "Not run: budget_tokens 5000 is over the person's maximum of 1000 \
             (company.max_budget_tokens)."
        );
    })
    .await;
}

/// The lead's two delegate calls in one answer, with the delays of the
/// member's answers showing whether they ran at the same time.
async fn two_delegations(second: Value) -> Duration {
    let folder = folder();
    let project = folder.join("project");
    let first = json!({"member": "coder", "task": "a", "files": ["a.txt"], "independent": true});
    let lead = scripted(vec![
        vec![
            call_chunk_at(0, "d1", "delegate", &first),
            call_chunk_at(1, "d2", "delegate", &second),
            finish("tool_calls"),
            sse_done(),
        ],
        text_turn("ok"),
    ])
    .await;
    let arrived = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&arrived);
    let member = start_mock(move |request: &MockRequest| {
        if request.method == "GET" {
            return MockReply::status(404, "{}");
        }
        seen.lock().unwrap().push(Instant::now());
        MockReply::stream(text_turn("done")).with_head_delay(Duration::from_millis(1500))
    })
    .await;
    let reviewer = scripted(Vec::new()).await;
    let core = core_with(&folder, settings(&lead, &member, &reviewer)).await;
    let (addr, _stop, _handle) = start_server(core).await;
    let mut client = connect(addr).await;
    let thread = project_thread(&mut client, &project).await;
    let events = turn(&mut client, 2, thread, "two tasks").await;
    let results = tool_results(&events);
    assert_eq!(results.len(), 2);
    assert!(
        results
            .iter()
            .all(|result| result.contains(": completed\n")),
        "{results:?}"
    );
    let arrived = arrived.lock().unwrap();
    assert_eq!(arrived.len(), 2);
    arrived[1] - arrived[0]
}

#[tokio::test]
async fn independent_tasks_on_separate_files_run_together_and_others_one_after_another() {
    with_timeout(async {
        let apart =
            json!({"member": "coder", "task": "b", "files": ["b.txt"], "independent": true});
        assert!(two_delegations(apart).await < Duration::from_millis(1000));
        let shared =
            json!({"member": "coder", "task": "b", "files": ["./A.txt"], "independent": true});
        assert!(two_delegations(shared).await >= Duration::from_millis(1500));
        let dependent = json!({"member": "coder", "task": "b", "files": ["b.txt"]});
        assert!(two_delegations(dependent).await >= Duration::from_millis(1500));
    })
    .await;
}

#[tokio::test]
async fn members_and_allow_lists_are_kept_and_the_repository_list_needs_approval() {
    with_timeout(async {
        let folder = folder();
        let project = folder.join("project");
        let (lead, member, reviewer) = (
            scripted(Vec::new()).await,
            scripted(Vec::new()).await,
            scripted(Vec::new()).await,
        );
        let core = core_with(&folder, settings(&lead, &member, &reviewer)).await;
        let (addr, _stop, _handle) = start_server(core).await;
        let mut client = connect(addr).await;

        let unknown = request(
            &mut client,
            1,
            "members/add",
            json!({"name": "gemma", "provider": "nowhere", "model": "gemma-4"}),
        )
        .await;
        assert_eq!(unknown["error"]["code"], "invalid_params", "{unknown}");
        let added = request(
            &mut client,
            2,
            "members/add",
            json!({"name": "gemma", "provider": "second", "model": "gemma-4", "role": "implements simple logic"}),
        )
        .await;
        assert_eq!(added["result"]["member"]["name"], "gemma", "{added}");
        let twice = request(
            &mut client,
            3,
            "members/add",
            json!({"name": "gemma", "provider": "second", "model": "gemma-4"}),
        )
        .await;
        assert!(twice["error"].is_object(), "{twice}");
        let updated = request(
            &mut client,
            4,
            "members/update",
            json!({"name": "gemma", "effort": "low", "role": "writes tests"}),
        )
        .await;
        assert_eq!(updated["result"]["member"]["effort"], "low", "{updated}");
        assert_eq!(updated["result"]["member"]["role"], "writes tests");
        let listed = request(&mut client, 5, "members/list", json!({})).await;
        assert_eq!(listed["result"]["members"].as_array().map(Vec::len), Some(4));
        let removed = request(&mut client, 6, "members/remove", json!({"name": "gemma"})).await;
        assert_eq!(removed["result"]["removed"], "gemma");

        let key = project.to_string_lossy().to_string();
        std::fs::create_dir_all(project.join(".inocode")).unwrap();
        let file = project.join(".inocode").join("models.json");
        std::fs::write(&file, r#"{"allow": ["doubter"]}"#).unwrap();
        let shown = request(&mut client, 7, "allow/get", json!({"project": key})).await;
        let result = &shown["result"];
        assert_eq!(result["project"]["repository"]["status"]["state"], "not_approved", "{shown}");
        assert_eq!(result["source"], "default");
        assert_eq!(result["effective"], json!(["coder", "doubter", "same"]));
        let sha = result["project"]["repository"]["status"]["sha256"]
            .as_str()
            .unwrap()
            .to_string();
        let stale = request(
            &mut client,
            8,
            "allow/approve_repository",
            json!({"project": key, "sha256": "0".repeat(64)}),
        )
        .await;
        assert_eq!(stale["error"]["code"], "changed", "{stale}");
        let approved = request(
            &mut client,
            9,
            "allow/approve_repository",
            json!({"project": key, "sha256": sha}),
        )
        .await;
        assert_eq!(approved["result"]["effective"], json!(["doubter"]), "{approved}");
        assert_eq!(approved["result"]["source"], "project");
        // A change of the file asks again.
        std::fs::write(&file, r#"{"allow": ["doubter", "same"]}"#).unwrap();
        let changed = request(&mut client, 10, "allow/get", json!({"project": key})).await;
        assert_eq!(changed["result"]["project"]["repository"]["status"]["state"], "not_approved");
        assert_eq!(changed["result"]["source"], "default");
        let default = request(&mut client, 11, "allow/set", json!({"default": ["coder"]})).await;
        assert_eq!(default["result"]["default"], json!(["coder"]), "{default}");
    })
    .await;
}
