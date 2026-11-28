mod support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use inocode_core::store::Store;
use inocode_core::{Core, Llm, LlmConfig, ProviderKind};
use serde_json::{json, Value};
use support::*;

/// A fresh folder with a database path and settings beside it.
fn database() -> std::path::PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let index = NEXT.fetch_add(1, Ordering::SeqCst);
    let folder = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "inocode-commands-{}-{nanos}-{index}",
        std::process::id()
    ));
    std::fs::create_dir_all(&folder).expect("folder");
    std::fs::write(
        folder.join("settings.json"),
        json!({"language": "Japanese"}).to_string(),
    )
    .expect("settings");
    folder.join("inocode.db")
}

async fn core_for(mock: &MockApi, database: &std::path::Path) -> Arc<Core> {
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
    let store = Store::open(database).await.expect("store");
    Arc::new(Core::with_llm(llm).with_store(store))
}

fn finish() -> Vec<u8> {
    let payload = json!({ "choices": [{ "delta": {}, "finish_reason": "stop" }] });
    format!("data: {payload}\n\n").into_bytes()
}

fn text_turn(text: &str) -> Vec<Vec<u8>> {
    vec![sse_delta(text), finish(), sse_done()]
}

/// A mock that answers its chat requests with `replies` in order, then
/// with "done".
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
                .unwrap_or_else(|| text_turn("done")),
        )
    })
    .await
}

fn chats(mock: &MockApi) -> Vec<MockRequest> {
    mock.requests()
        .into_iter()
        .filter(|request| request.method == "POST")
        .collect()
}

async fn thread(client: &mut Client, project: Option<&std::path::Path>) -> i64 {
    let params = match project {
        Some(project) => json!({"project": project.to_string_lossy()}),
        None => json!({}),
    };
    let response = call(
        client,
        1,
        json!({"id": 1, "method": "thread/start", "params": params}),
    )
    .await;
    let thread = response["result"]["thread_id"].as_i64().expect("thread id");
    name_thread(client, thread).await;
    thread
}

async fn command(client: &mut Client, id: u64, thread: i64, text: &str) -> Value {
    call(
        client,
        id,
        json!({"id": id, "method": "command/run",
               "params": {"thread_id": thread, "text": text}}),
    )
    .await
}

/// Sends a command that starts a turn and collects its events.
async fn command_turn(client: &mut Client, id: u64, thread: i64, text: &str) -> Vec<Value> {
    client
        .1
        .write_json(
            id,
            &json!({"id": id, "method": "command/run",
                    "params": {"thread_id": thread, "text": text}}),
        )
        .await
        .expect("send");
    let mut events = Vec::new();
    loop {
        let event = next_message(client, id).await;
        let end = matches!(
            event["event"].as_str(),
            Some("completed" | "failed" | "cancelled") | None
        );
        events.push(event);
        if end {
            return events;
        }
    }
}

#[tokio::test]
async fn the_list_has_every_built_in_command_and_the_projects_under_their_prefix() {
    with_timeout(async {
        let mock = scripted(Vec::new()).await;
        let database = database();
        let folder = database.parent().expect("folder").to_path_buf();
        let user = folder.join("commands");
        std::fs::create_dir_all(&user).expect("user commands");
        std::fs::write(user.join("hello.md"), "Say hello to $ARGUMENTS.").expect("write");
        std::fs::write(user.join("deploy.md"), "Deploy as the user.").expect("write");
        let project = folder.join("project");
        let commands = project.join(".inocode").join("commands");
        std::fs::create_dir_all(&commands).expect("project commands");
        std::fs::write(commands.join("goal.md"), "Replace the goal.").expect("write");
        std::fs::write(
            commands.join("deploy.md"),
            "---\ndescription: Deploy it\nargument-hint: <where>\n---\nDeploy to $ARGUMENTS.",
        )
        .expect("write");
        let (addr, _stop, _handle) = start_server(core_for(&mock, &database).await).await;
        let mut client = connect(addr).await;

        let listed = call(&mut client, 2, json!({"id": 2, "method": "commands/list"})).await;
        let commands = listed["result"]["commands"].as_array().expect("commands");
        let named = |name: &str| {
            commands
                .iter()
                .find(|command| command["name"] == name)
                .cloned()
                .unwrap_or(Value::Null)
        };
        let goal = named("goal");
        assert_eq!(goal["source"], "builtin");
        assert_eq!(goal["bare"], "status");
        let forms = goal["forms"].as_array().expect("forms");
        assert_eq!(forms[0]["runs"], "turn");
        assert_eq!(forms[0]["when"], "after_turn");
        assert_eq!(forms[1]["sub"], "status");
        assert_eq!(forms[1]["when"], "now");
        assert_eq!(named("model")["forms"][0]["when"], "next_request");
        assert_eq!(named("estimate")["available"], true);
        assert_eq!(named("estimate")["forms"][0]["when"], "now");
        assert_eq!(named("user:hello")["short"], "hello");
        assert_eq!(named("project:goal"), Value::Null, "no thread, no project");

        let thread = thread(&mut client, Some(&project)).await;
        let listed = call(
            &mut client,
            3,
            json!({"id": 3, "method": "commands/list", "params": {"thread_id": thread}}),
        )
        .await;
        let commands = listed["result"]["commands"].as_array().expect("commands");
        let named = |name: &str| {
            commands
                .iter()
                .find(|command| command["name"] == name)
                .cloned()
                .unwrap_or(Value::Null)
        };
        assert_eq!(named("project:goal")["short"], Value::Null);
        assert_eq!(named("project:deploy")["short"], Value::Null);
        assert_eq!(named("user:deploy")["short"], Value::Null);
        assert_eq!(named("project:deploy")["description"], "Deploy it");
        assert_eq!(
            named("project:deploy")["forms"][0]["usage"],
            "/project:deploy [<where>]"
        );
        let conflicts = &listed["result"]["conflicts"];
        assert!(
            conflicts
                .as_array()
                .expect("conflicts")
                .iter()
                .any(|conflict| conflict["name"] == "deploy"
                    && conflict["use"] == json!(["/user:deploy", "/project:deploy"])),
            "{conflicts}"
        );

        // A project command named goal does not replace /goal.
        let status = command(&mut client, 4, thread, "/goal").await;
        assert_eq!(status["result"]["command"], "goal");
        assert_eq!(status["result"]["goal"], Value::Null);
        let ambiguous = command(&mut client, 5, thread, "/deploy prod").await;
        assert_eq!(ambiguous["error"]["code"], "ambiguous_command");

        // A prompt command starts a turn with its text.
        let events = command_turn(&mut client, 6, thread, "/project:deploy prod").await;
        assert_eq!(events[0]["event"], "command", "{events:?}");
        assert_eq!(events[0]["result"]["command"], "project:deploy");
        assert_eq!(events[1]["event"], "started");
        assert_eq!(events.last().unwrap()["event"], "completed");
        let sent = chats(&mock)[0].body["messages"].to_string();
        assert!(sent.contains("Deploy to prod."), "{sent}");
        let events = command_turn(&mut client, 7, thread, "/hello world").await;
        assert_eq!(events[0]["result"]["command"], "user:hello");
        let sent = chats(&mock)[1].body["messages"].to_string();
        assert!(sent.contains("Say hello to world."), "{sent}");
        let _ = std::fs::remove_dir_all(&folder);
    })
    .await;
}

fn verdict(verdict: &str, reason: &str) -> Vec<Vec<u8>> {
    let answer = json!({"points": ["it is done"], "verdict": verdict, "reason": reason});
    text_turn(&answer.to_string())
}

#[tokio::test]
async fn goal_sets_a_goal_works_on_it_and_reports_its_status() {
    with_timeout(async {
        let mock = scripted(vec![
            text_turn("It is done."),
            verdict("achieved", "the answer says it"),
        ])
        .await;
        let database = database();
        let (addr, _stop, _handle) = start_server(core_for(&mock, &database).await).await;
        let mut client = connect(addr).await;
        let thread = thread(&mut client, None).await;

        let events = command_turn(&mut client, 2, thread, "/goal say it is done").await;
        assert_eq!(events[0]["event"], "command");
        assert_eq!(events[0]["result"]["goal"]["objective"], "say it is done");
        assert_eq!(events[0]["result"]["goal"]["state"], "active");
        assert_eq!(events.last().unwrap()["event"], "completed", "{events:?}");
        let sent = chats(&mock)[0].body["messages"].to_string();
        assert!(sent.contains("say it is done"), "{sent}");

        let status = command(&mut client, 3, thread, "/goal status").await;
        let result = &status["result"];
        assert_eq!(result["goal"]["state"], "achieved", "{status}");
        assert_eq!(result["last_check"], "the answer says it");
        assert!(result["elapsed_ms"].as_i64().is_some_and(|ms| ms >= 0));
        assert!(result["tokens"].is_u64());
        assert_eq!(result["running"], false);
        assert_eq!(result["estimate"], Value::Null, "nothing is left of it");

        let stopped = command(&mut client, 4, thread, "/goal stop").await;
        assert_eq!(stopped["result"]["goal"]["state"], "achieved");
    })
    .await;
}

/// An answer that reports `input` and 5 output tokens.
fn measured_turn(text: &str, input: u64) -> Vec<Vec<u8>> {
    let usage = json!({ "choices": [], "usage": {"prompt_tokens": input, "completion_tokens": 5} });
    vec![
        sse_delta(text),
        finish(),
        format!("data: {usage}\n\n").into_bytes(),
        sse_done(),
    ]
}

async fn plain_turn(client: &mut Client, id: u64, thread: i64, prompt: &str) -> Vec<Value> {
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
        let end = matches!(
            event["event"].as_str(),
            Some("completed" | "failed" | "cancelled") | None
        );
        events.push(event);
        if end {
            return events;
        }
    }
}

#[tokio::test]
async fn past_turns_estimate_similar_work_before_and_during_it() {
    with_timeout(async {
        let mock = scripted(vec![
            measured_turn("翻訳しました", 1_000),
            measured_turn("翻訳しました", 2_000),
            measured_turn("翻訳しました", 3_000),
            measured_turn("翻訳しました", 500),
        ])
        .await;
        let database = database();
        let (addr, _stop, _handle) = start_server(core_for(&mock, &database).await).await;
        let mut client = connect(addr).await;
        let thread = thread(&mut client, None).await;

        // Without a past task nothing is estimated, and no estimate is sent.
        let none = command(&mut client, 2, thread, "/estimate README を英語に翻訳する").await;
        assert_eq!(none["result"]["estimate"]["method"], "none", "{none}");
        assert_eq!(none["result"]["estimate"]["tokens"], Value::Null);
        let first = plain_turn(&mut client, 3, thread, "README を英語に翻訳する").await;
        assert!(first.iter().all(|event| event["event"] != "estimate"));
        plain_turn(&mut client, 4, thread, "設計の文書を英語に翻訳する").await;
        plain_turn(&mut client, 5, thread, "使い方を英語に翻訳する").await;

        let estimate = command(&mut client, 6, thread, "/estimate 変更履歴を英語に翻訳する").await;
        let result = &estimate["result"];
        assert_eq!(result["command"], "estimate", "{estimate}");
        assert_eq!(result["model"], "test-model");
        let estimate = &result["estimate"];
        assert_eq!(estimate["samples"], 3, "{estimate}");
        assert_eq!(estimate["widened"], true);
        assert_eq!(estimate["method"], "similar");
        // 1,005, 2,005 and 3,005 tokens: the median, half the least and
        // twice the most.
        assert_eq!(
            estimate["tokens"],
            json!({"low": 503, "mid": 2_005, "high": 6_010})
        );
        assert!(estimate["minutes"]["mid"].is_number());
        assert!(estimate["note"]
            .as_str()
            .is_some_and(|note| note.contains("3 past tasks")));

        let by_method = call(
            &mut client,
            7,
            json!({"id": 7, "method": "estimate", "params": {"text": "変更履歴を英語に翻訳する"}}),
        )
        .await;
        assert_eq!(by_method["result"]["estimate"]["samples"], 3, "{by_method}");
        let in_thread = call(
            &mut client,
            8,
            json!({"id": 8, "method": "estimate",
                   "params": {"thread_id": thread, "text": "変更履歴を英語に翻訳する"}}),
        )
        .await;
        assert_eq!(in_thread["result"], by_method["result"]);
        for (id, params) in [
            (9, json!({})),
            (10, json!({"text": "  "})),
            (11, json!({"thread_id": 999, "text": "x"})),
        ] {
            let refused = call(
                &mut client,
                id,
                json!({"id": id, "method": "estimate", "params": params}),
            )
            .await;
            assert!(
                matches!(
                    refused["error"]["code"].as_str(),
                    Some("invalid_params" | "unknown_thread")
                ),
                "{refused}"
            );
        }

        // During a turn: the estimate before the first request, then what
        // is left after it.
        let events = plain_turn(&mut client, 12, thread, "目次を英語に翻訳する").await;
        let sent: Vec<&Value> = events
            .iter()
            .filter(|event| event["event"] == "estimate")
            .map(|event| &event["estimate"])
            .collect();
        assert_eq!(sent.len(), 2, "{events:?}");
        assert_eq!(sent[0]["samples"], 3);
        assert_eq!(sent[0]["so_far"], Value::Null);
        assert_eq!(sent[1]["so_far"]["tokens"], 505);
        // All three past turns had used more than 505 tokens.
        assert_eq!(sent[1]["samples"], 3);
        assert_eq!(sent[1]["tokens"]["mid"], 1_500);
        assert_eq!(events.last().unwrap()["event"], "completed");
    })
    .await;
}

#[tokio::test]
async fn commands_check_their_arguments_and_report_what_is_missing() {
    with_timeout(async {
        let mock = scripted(Vec::new()).await;
        let database = database();
        let (addr, _stop, _handle) = start_server(core_for(&mock, &database).await).await;
        let mut client = connect(addr).await;
        let thread = thread(&mut client, None).await;

        let unknown = command(&mut client, 2, thread, "/dance").await;
        assert_eq!(unknown["error"]["code"], "unknown_command");
        let wrong = command(&mut client, 3, thread, "/notes vote 1 sideways").await;
        assert_eq!(wrong["error"]["code"], "invalid_arguments");
        let unconfirmed = command(&mut client, 4, thread, "/delete").await;
        assert_eq!(unconfirmed["error"]["code"], "invalid_arguments");
        let estimate = command(&mut client, 5, thread, "/estimate").await;
        assert_eq!(estimate["error"]["code"], "invalid_arguments");
        let edit = command(&mut client, 6, thread, "/rules edit 1 x").await;
        assert_eq!(edit["error"]["code"], "unavailable");
        let no_slash = command(&mut client, 7, thread, "plan").await;
        assert_eq!(no_slash["error"]["code"], "invalid_arguments");
        let by_name = call(
            &mut client,
            8,
            json!({"id": 8, "method": "command/run",
                   "params": {"thread_id": thread, "name": "plan", "args": ""}}),
        )
        .await;
        assert_eq!(by_name["result"], json!({"command": "plan", "items": []}));
        let no_thread = command(&mut client, 9, 999, "/plan").await;
        assert_eq!(no_thread["error"]["code"], "unknown_thread");
    })
    .await;
}

#[tokio::test]
async fn mcp_shows_each_servers_sign_in_and_signs_in_only_where_it_can() {
    with_timeout(async {
        let mock = scripted(Vec::new()).await;
        let database = database();
        std::fs::write(
            database.with_file_name("mcp.json"),
            json!({"mcpServers": {
                "files": {"url": "http://127.0.0.1:9/mcp"},
                "keyed": {"url": "http://127.0.0.1:9/mcp",
                          "headers": {"Authorization": "Bearer kept"}},
                "local": {"command": "no-such-program"}}})
            .to_string(),
        )
        .expect("mcp.json");
        let (addr, _stop, _handle) = start_server(core_for(&mock, &database).await).await;
        let mut client = connect(addr).await;
        let thread = thread(&mut client, None).await;

        let listed = command(&mut client, 2, thread, "/mcp").await;
        let sign_ins: Vec<(String, Value)> = listed["result"]["servers"]
            .as_array()
            .expect("servers")
            .iter()
            .map(|server| {
                (
                    server["name"].as_str().unwrap_or("").to_string(),
                    server["sign_in"].clone(),
                )
            })
            .collect();
        assert_eq!(
            sign_ins,
            [
                ("files".to_string(), json!("unknown")),
                ("keyed".to_string(), json!("headers")),
                ("local".to_string(), Value::Null),
            ]
        );
        let local = command(&mut client, 3, thread, "/mcp login local").await;
        assert_eq!(local["error"]["code"], "invalid_params", "{local}");
        let keyed = command(&mut client, 4, thread, "/mcp login keyed").await;
        assert!(
            keyed["error"]["message"]
                .as_str()
                .unwrap_or("")
                .contains("Authorization header"),
            "{keyed}"
        );
        let status = command(&mut client, 5, thread, "/mcp status nothing").await;
        assert_eq!(status["error"]["code"], "mcp_error", "{status}");
        let out = command(&mut client, 6, thread, "/mcp logout files").await;
        assert_eq!(out["result"]["removed"], 0, "{out}");
        let unknown = call(
            &mut client,
            7,
            json!({"id": 7, "method": "mcp/login/start", "params": {"server": "nobody"}}),
        )
        .await;
        assert_eq!(unknown["error"]["code"], "invalid_params", "{unknown}");
    })
    .await;
}

#[tokio::test]
async fn threads_are_listed_renamed_forked_exported_and_deleted() {
    with_timeout(async {
        let mock = scripted(vec![text_turn("hello back")]).await;
        let database = database();
        let (addr, _stop, _handle) = start_server(core_for(&mock, &database).await).await;
        let mut client = connect(addr).await;
        let first = thread(&mut client, None).await;
        client
            .1
            .write_json(
                2,
                &json!({"id": 2, "method": "turn/start",
                        "params": {"thread_id": first, "prompt": "hello there"}}),
            )
            .await
            .expect("send");
        while !matches!(
            next_message(&mut client, 2).await["event"].as_str(),
            Some("completed" | "failed")
        ) {}

        let renamed = command(&mut client, 3, first, "/rename My first thread").await;
        assert_eq!(renamed["result"]["title"], "My first thread");
        let fork = command(&mut client, 4, first, "/fork").await;
        let forked = fork["result"]["thread_id"].as_i64().expect("fork");
        assert_ne!(forked, first);
        let read = call(
            &mut client,
            5,
            json!({"id": 5, "method": "thread/read", "params": {"thread_id": forked}}),
        )
        .await;
        let messages = read["result"]["messages"].as_array().expect("messages");
        assert_eq!(messages.len(), 2, "{read}");
        assert_eq!(messages[0]["message"]["body"]["parts"][0], "hello there");

        let listed = call(&mut client, 6, json!({"id": 6, "method": "thread/list"})).await;
        let threads = listed["result"]["threads"].as_array().expect("threads");
        assert_eq!(threads.len(), 2);
        assert!(threads
            .iter()
            .all(|thread| thread["title"] == "My first thread" && thread["messages"] == 2));

        let exported = command(&mut client, 7, first, "/export markdown").await;
        let text = exported["result"]["text"].as_str().expect("markdown");
        assert!(text.contains("## Person (#1)\n\nhello there"), "{text}");
        assert!(text.contains("hello back"), "{text}");
        let json_export = command(&mut client, 8, first, "/export json").await;
        let parsed: Value =
            serde_json::from_str(json_export["result"]["text"].as_str().expect("json"))
                .expect("valid json");
        assert_eq!(parsed.as_array().map(Vec::len), Some(2));

        let found = command(&mut client, 9, first, "/history hello").await;
        assert!(found["result"]["hits"]
            .as_array()
            .is_some_and(|hits| hits.len() >= 2));

        // Files kept for the thread beside the database go with it.
        let files = database.with_extension("files");
        let logs = files.join("logs").join(format!("thread-{forked}"));
        std::fs::create_dir_all(&logs).expect("logs");
        std::fs::write(logs.join("job-1.log"), "output").expect("log");
        std::fs::create_dir_all(files.join("messages")).expect("messages");
        let saved = files.join("messages").join(format!("{forked}-1.txt"));
        std::fs::write(&saved, "a long message").expect("message");
        let kept = files.join("messages").join(format!("{first}-1.txt"));
        std::fs::write(&kept, "another thread's").expect("message");

        let deleted = command(&mut client, 10, forked, "/delete confirm").await;
        assert_eq!(deleted["result"]["deleted"], forked);
        assert_eq!(
            deleted["result"]["files_not_removed"],
            json!([]),
            "{deleted}"
        );
        assert!(!logs.exists() && !saved.exists() && kept.exists());
        let gone = call(
            &mut client,
            11,
            json!({"id": 11, "method": "thread/read", "params": {"thread_id": forked}}),
        )
        .await;
        assert_eq!(gone["error"]["code"], "unknown_thread");
        // The search index no longer finds the deleted thread's messages.
        let found = command(&mut client, 12, first, "/history hello").await;
        assert!(found["result"]["hits"]
            .as_array()
            .expect("hits")
            .iter()
            .all(|hit| hit["thread_id"] == first));
        // The deleted thread was the latest; its id is not given again.
        let new = command(&mut client, 13, first, "/new").await;
        assert!(new["result"]["thread_id"]
            .as_i64()
            .is_some_and(|id| id > forked));
        let resume = command(&mut client, 14, first, "/resume").await;
        assert_eq!(
            resume["result"]["threads"].as_array().map(Vec::len),
            Some(2)
        );
        let one = command(&mut client, 15, first, &format!("/resume {first}")).await;
        assert_eq!(one["result"]["thread"]["thread_id"], first);
    })
    .await;
}

#[tokio::test]
async fn during_a_turn_only_what_may_run_now_runs() {
    with_timeout(async {
        let mock = start_mock(|request: &MockRequest| {
            if request.method == "GET" {
                return MockReply::status(404, "{}");
            }
            let mut chunks: Vec<Vec<u8>> = (0..200).map(|_| sse_delta("x")).collect();
            chunks.push(sse_done());
            MockReply::stream(chunks).with_delay(Duration::from_millis(20))
        })
        .await;
        let database = database();
        let (addr, _stop, _handle) = start_server(core_for(&mock, &database).await).await;
        let mut client = connect(addr).await;
        let mut other = connect(addr).await;
        let thread = thread(&mut client, None).await;
        client
            .1
            .write_json(
                2,
                &json!({"id": 2, "method": "turn/start",
                        "params": {"thread_id": thread, "prompt": "long"}}),
            )
            .await
            .expect("send");
        while next_message(&mut client, 2).await["event"] != "delta" {}

        let refused = command(&mut other, 3, thread, "/new").await;
        assert_eq!(refused["error"]["code"], "not_now");
        assert_eq!(refused["error"]["when"], "after_turn");
        let goal = command(&mut other, 4, thread, "/goal finish it").await;
        assert_eq!(goal["error"]["code"], "not_now");
        let plan = command(&mut other, 5, thread, "/plan").await;
        assert_eq!(plan["result"]["items"], json!([]));
        let status = command(&mut other, 6, thread, "/status").await;
        assert_eq!(status["result"]["running"], true);
        let mode = command(&mut other, 7, thread, "/mode full").await;
        assert_eq!(mode["result"]["mode"], "full");
        assert_eq!(mode["result"]["applies"], "next_request");

        call(
            &mut other,
            8,
            json!({"id": 8, "method": "cancel", "params": {"request_id": 2}}),
        )
        .await;
        while !matches!(
            next_message(&mut client, 2).await["event"].as_str(),
            Some("completed" | "failed" | "cancelled")
        ) {}
    })
    .await;
}

#[tokio::test]
async fn records_of_the_agent_are_shown_and_changed_by_commands() {
    with_timeout(async {
        let mock = scripted(Vec::new()).await;
        let database = database();
        let store = Store::open(&database).await.expect("store");
        let lesson = store
            .add_note(inocode_core::store::NewNote {
                project: None,
                kind: inocode_core::store::NoteKind::Lesson,
                title: "Run the tests with one job".into(),
                summary: "the machine has little memory".into(),
                keywords: "cargo tests memory".into(),
                condition: "building on this machine".into(),
                body: "When: building\nDo: use -j 1\nBecause: memory".into(),
                source: inocode_core::store::Source::Model,
                tags: vec!["build".into()],
            })
            .await
            .expect("lesson");
        let (addr, _stop, _handle) = start_server(core_for(&mock, &database).await).await;
        let mut client = connect(addr).await;
        let thread = thread(&mut client, None).await;

        let listed = command(&mut client, 2, thread, "/notes").await;
        assert_eq!(listed["result"]["notes"][0]["id"], lesson.0);
        assert_eq!(listed["result"]["notes"][0]["status"], "candidate");
        let voted = command(
            &mut client,
            3,
            thread,
            &format!("/notes vote {} up", lesson.0),
        )
        .await;
        assert_eq!(voted["result"]["vote"], "up");
        let tagged = command(
            &mut client,
            4,
            thread,
            &format!("/notes tag {} rust", lesson.0),
        )
        .await;
        assert_eq!(tagged["result"]["tags"], json!(["build", "rust"]));
        let shown = command(&mut client, 5, thread, &format!("/notes show {}", lesson.0)).await;
        assert_eq!(shown["result"]["note"]["up"], 1);
        let found = command(&mut client, 6, thread, "/notes search memory").await;
        assert_eq!(found["result"]["notes"][0]["id"], lesson.0);
        let edited = command(
            &mut client,
            7,
            thread,
            &format!("/notes edit {} When: building\nDo: use -j 2", lesson.0),
        )
        .await;
        let new = edited["result"]["id"].as_i64().expect("new note");
        let listed = command(&mut client, 8, thread, "/notes list").await;
        let ids: Vec<i64> = listed["result"]["notes"]
            .as_array()
            .expect("notes")
            .iter()
            .filter_map(|note| note["id"].as_i64())
            .collect();
        assert_eq!(ids, [new], "the old note is kept only as history");
        let retired = command(&mut client, 9, thread, &format!("/notes retire {new}")).await;
        assert_eq!(retired["result"]["status"], "retired");

        let issue = store
            .open_issue(None, "Flaky test", "It fails sometimes", 1, Vec::new())
            .await
            .expect("issue");
        let issues = command(&mut client, 10, thread, "/issues").await;
        assert_eq!(issues["result"]["issues"][0]["id"], issue.0);
        let done = command(
            &mut client,
            11,
            thread,
            &format!("/issues done {} fixed", issue.0),
        )
        .await;
        assert_eq!(done["result"]["state"], "done");
        let issues = command(&mut client, 12, thread, "/issues").await;
        assert_eq!(issues["result"]["issues"], json!([]));

        let promises = command(&mut client, 13, thread, "/promise").await;
        assert_eq!(promises["result"]["items"], json!([]));
        let none = command(&mut client, 14, thread, "/promise remove 1").await;
        assert_eq!(none["error"]["code"], "invalid_arguments");
        // Calls are approved when the turn asks; there is no command for it.
        let approve = command(&mut client, 15, thread, "/approve").await;
        assert_eq!(approve["error"]["code"], "unknown_command");
        let questions = command(&mut client, 16, thread, "/answer").await;
        assert_eq!(questions["result"]["questions"], json!([]));
        let colleague = command(&mut client, 17, thread, "/colleague").await;
        assert_eq!(colleague["result"]["on"], false);
        assert_eq!(colleague["result"]["advice"], json!([]));
        let translate = command(&mut client, 18, thread, "/translate answer on").await;
        assert_eq!(translate["error"]["code"], "not_implemented");
        let help = command(&mut client, 19, thread, "/help goal").await;
        assert_eq!(help["result"]["help"]["name"], "goal");
    })
    .await;
}

fn streamed_text(events: &[Value]) -> String {
    events
        .iter()
        .filter(|event| event["event"] == "delta")
        .filter_map(|event| event["text"].as_str())
        .collect()
}

/// `/goal` with a tiny objective, `/goal status`, and a prompt command, on
/// the provider the test-only `INOCODE_TEST_*` variables name.
#[tokio::test]
#[ignore = "requires a provider credential; run by hand"]
async fn live_goal_and_prompt_command() {
    with_timeout_of(LIVE_TIMEOUT, async {
        let database = live_folder("commands").join("inocode.db");
        let user = database.with_file_name("commands");
        std::fs::create_dir_all(&user).expect("user commands");
        std::fs::write(
            user.join("echo.md"),
            "---\ndescription: Repeat the words\n---\n\
             Reply with exactly these words and nothing else: $ARGUMENTS",
        )
        .expect("write");
        let core = Arc::new(live_core(&database, None).await);
        let (addr, _stop, _handle) = start_server(core).await;
        let mut client = connect(addr).await;
        let thread = thread(&mut client, None).await;

        let events = command_turn(
            &mut client,
            2,
            thread,
            "/goal Reply with the single word READY.",
        )
        .await;
        println!(
            "goal turn: {:?}, ended {}",
            streamed_text(&events),
            events.last().unwrap()
        );
        assert_eq!(events[0]["event"], "command");
        assert_eq!(events.last().unwrap()["event"], "completed");
        let status = command(&mut client, 3, thread, "/goal status").await;
        println!("goal status: {}", status["result"]);
        assert!(status["result"]["goal"]["state"].is_string());

        let events = command_turn(&mut client, 4, thread, "/echo blue whale").await;
        let text = streamed_text(&events);
        println!("prompt command: {text:?}");
        assert_eq!(events[0]["result"]["command"], "user:echo");
        assert!(text.to_lowercase().contains("blue whale"), "{text}");
    })
    .await;
}

#[tokio::test]
async fn title_changes_reach_a_subscriber_while_no_turn_runs() {
    with_timeout(async {
        let mock = scripted(Vec::new()).await;
        let database = database();
        let (addr, _stop, _handle) = start_server(core_for(&mock, &database).await).await;
        let mut watcher = connect(addr).await;
        let mut client = connect(addr).await;
        let first = thread(&mut client, None).await;
        watcher
            .1
            .write_json(
                5,
                &json!({"id": 5, "method": "thread/titles/subscribe", "params": {}}),
            )
            .await
            .expect("subscribe");
        assert_eq!(next_message(&mut watcher, 5).await["event"], "started");

        let renamed = command(&mut client, 2, first, "/rename 実装").await;
        assert_eq!(renamed["result"]["title"], "実装", "{renamed}");
        let updated = next_message(&mut watcher, 5).await;
        assert_eq!(updated["event"], "title_updated", "{updated}");
        assert_eq!(updated["thread_id"], first);
        assert_eq!(updated["title"], "実装");

        watcher
            .1
            .write_json(
                6,
                &json!({"id": 6, "method": "cancel", "params": {"request_id": 5}}),
            )
            .await
            .expect("cancel");
        let mut ends = Vec::new();
        for _ in 0..2 {
            let (stream, frame): (u64, Value) = watcher.0.read_json().await.expect("frame");
            ends.push((stream, frame));
        }
        ends.sort_by_key(|(stream, _)| *stream);
        assert_eq!(ends[0].1["event"], "cancelled");
        assert_eq!(ends[1].1["result"]["cancelled"], true);
    })
    .await;
}

#[tokio::test]
async fn an_installed_plugin_is_turned_off_on_and_deleted_from_a_screen() {
    with_timeout(async {
        let database = database();
        let plugin = database
            .with_file_name("plugins")
            .join("installed")
            .join("test-catalog")
            .join("notes");
        std::fs::create_dir_all(plugin.join("skills").join("write")).expect("plugin");
        std::fs::write(
            plugin.join("skills").join("write").join("SKILL.md"),
            "---\nname: write-note\ndescription: \"Writes a note\"\n---\nWrite it.",
        )
        .expect("skill");
        let mock = scripted(Vec::new()).await;
        let (addr, _stop, _handle) = start_server(core_for(&mock, &database).await).await;
        let mut client = connect(addr).await;
        let thread = thread(&mut client, None).await;
        let request = |id: u64, method: &str, params: Value| {
            json!({"id": id, "method": method, "params": params})
        };
        let skills = |listed: &Value| -> Vec<String> {
            listed["result"]["skills"]
                .as_array()
                .expect("skills")
                .iter()
                .map(|skill| skill["name"].as_str().expect("name").to_string())
                .collect()
        };

        let listed = call(&mut client, 2, request(2, "plugins/list", json!({}))).await;
        let plugins = &listed["result"]["plugins"];
        assert_eq!(plugins[0]["id"], "notes@test-catalog", "{listed}");
        assert_eq!(plugins[0]["enabled"], true);
        assert_eq!(skills(&command(&mut client, 3, thread, "/skills").await).len(), 1);

        let off = call(
            &mut client,
            4,
            request(4, "plugins/disable", json!({"plugin": "notes@test-catalog"})),
        )
        .await;
        assert_eq!(off["result"]["changed"], true, "{off}");
        let again = call(
            &mut client,
            5,
            request(5, "plugins/disable", json!({"plugin": "notes@test-catalog"})),
        )
        .await;
        assert_eq!(again["result"]["changed"], false, "{again}");
        let listed = call(&mut client, 6, request(6, "plugins/list", json!({}))).await;
        assert_eq!(listed["result"]["plugins"][0]["enabled"], false, "{listed}");
        assert!(skills(&command(&mut client, 7, thread, "/skills").await).is_empty());
        assert!(plugin.is_dir(), "turning it off keeps its files");

        let on = command(&mut client, 8, thread, "/plugins enable notes@test-catalog").await;
        assert_eq!(on["result"]["changed"], true, "{on}");
        assert_eq!(skills(&command(&mut client, 9, thread, "/skills").await).len(), 1);

        let removed = call(
            &mut client,
            10,
            request(10, "plugins/remove", json!({"plugin": "notes@test-catalog"})),
        )
        .await;
        assert_eq!(removed["result"]["removed"], "notes@test-catalog", "{removed}");
        assert!(!plugin.exists());
        let listed = call(&mut client, 11, request(11, "plugins/list", json!({}))).await;
        assert_eq!(listed["result"]["plugins"], json!([]), "{listed}");
        let missing = call(
            &mut client,
            12,
            request(12, "plugins/remove", json!({"plugin": "notes@test-catalog"})),
        )
        .await;
        assert_eq!(missing["error"]["code"], "plugins", "{missing}");
        let bad = call(&mut client, 13, request(13, "plugins/enable", json!({}))).await;
        assert_eq!(bad["error"]["code"], "invalid_params", "{bad}");
    })
    .await;
}

#[tokio::test]
async fn a_screen_reads_writes_lists_copies_and_deletes_files_by_absolute_path() {
    with_timeout(async {
        let database = database();
        let base = database.with_file_name("files");
        let mock = scripted(Vec::new()).await;
        let (addr, _stop, _handle) = start_server(core_for(&mock, &database).await).await;
        let mut client = connect(addr).await;
        let mut id = 1;
        let mut fs = |method: &str, params: Value| {
            id += 1;
            (id, json!({"id": id, "method": method, "params": params}))
        };
        let path = |name: &str| base.join(name).to_string_lossy().into_owned();

        let (n, request) = fs(
            "fs/writeFile",
            json!({"path": path("a/b/note.txt"), "text": "hello 世界", "create_parents": true}),
        );
        let written = call(&mut client, n, request).await;
        assert_eq!(written["result"]["written"], 12, "{written}");
        let (n, request) = fs("fs/readFile", json!({"path": path("a/b/note.txt")}));
        let read = call(&mut client, n, request).await;
        assert_eq!(read["result"]["text"], "hello 世界", "{read}");
        assert_eq!(read["result"]["size"], 12);
        let (n, request) = fs(
            "fs/readFile",
            json!({"path": path("a/b/note.txt"), "offset": 6, "limit": 3, "encoding": "base64"}),
        );
        let part = call(&mut client, n, request).await;
        assert_eq!(part["result"]["data"], "5LiW", "{part}");
        let (n, request) = fs(
            "fs/writeFile",
            json!({"path": path("a/bytes.bin"), "data": "//79"}),
        );
        call(&mut client, n, request).await;
        let (n, request) = fs("fs/readFile", json!({"path": path("a/bytes.bin")}));
        let binary = call(&mut client, n, request).await;
        assert_eq!(binary["error"]["code"], "not_text", "{binary}");

        let (n, request) = fs("fs/readDirectory", json!({"path": path("a")}));
        let listed = call(&mut client, n, request).await;
        let entries = listed["result"]["entries"]
            .as_array()
            .expect("entries")
            .clone();
        assert_eq!(entries[0]["name"], "b", "{listed}");
        assert_eq!(entries[0]["kind"], "directory");
        assert_eq!(entries[1]["name"], "bytes.bin");
        assert_eq!(entries[1]["size"], 3);
        let (n, request) = fs("fs/getMetadata", json!({"path": path("a/b/note.txt")}));
        let meta = call(&mut client, n, request).await;
        assert_eq!(meta["result"]["kind"], "file", "{meta}");
        assert!(meta["result"]["modified_ms"].is_u64());

        let (n, request) = fs("fs/copy", json!({"from": path("a"), "to": path("c")}));
        let folder = call(&mut client, n, request).await;
        assert_eq!(folder["error"]["code"], "invalid_params", "{folder}");
        let (n, request) = fs(
            "fs/copy",
            json!({"from": path("a"), "to": path("c"), "recursive": true}),
        );
        call(&mut client, n, request).await;
        assert_eq!(
            std::fs::read_to_string(base.join("c/b/note.txt")).expect("copied"),
            "hello 世界"
        );
        let (n, request) = fs(
            "fs/copy",
            json!({"from": path("a/bytes.bin"), "to": path("c/bytes.bin")}),
        );
        let exists = call(&mut client, n, request).await;
        assert_eq!(exists["error"]["code"], "already_exists", "{exists}");

        let (n, request) = fs("fs/remove", json!({"path": path("c")}));
        let full = call(&mut client, n, request).await;
        assert_eq!(full["error"]["code"], "fs_failed", "{full}");
        let (n, request) = fs("fs/remove", json!({"path": path("c"), "recursive": true}));
        call(&mut client, n, request).await;
        assert!(!base.join("c").exists());
        let (n, request) = fs("fs/createDirectory", json!({"path": path("d/e")}));
        call(&mut client, n, request).await;
        assert!(base.join("d/e").is_dir());
        let (n, request) = fs("fs/getMetadata", json!({"path": path("missing")}));
        let missing = call(&mut client, n, request).await;
        assert_eq!(missing["error"]["code"], "not_found", "{missing}");
        let (n, request) = fs("fs/readFile", json!({"path": "relative.txt"}));
        let relative = call(&mut client, n, request).await;
        assert_eq!(relative["error"]["code"], "invalid_params", "{relative}");
    })
    .await;
}

fn git(folder: &std::path::Path, arguments: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args([
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.invalid",
        ])
        .args(arguments)
        .current_dir(folder)
        .output()
        .expect("git");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[tokio::test]
async fn a_thread_moves_to_another_folder_and_to_its_own_worktree_removed_with_it() {
    with_timeout(async {
        let database = database();
        let repo = database.with_file_name("repo");
        std::fs::create_dir_all(&repo).expect("repo");
        git(&repo, &["init", "-q"]);
        std::fs::write(repo.join("kept.txt"), "kept\n").expect("file");
        git(&repo, &["add", "kept.txt"]);
        git(&repo, &["commit", "-q", "-m", "first"]);
        let other = database.with_file_name("other");
        std::fs::create_dir_all(&other).expect("other");
        let mock = scripted(Vec::new()).await;
        let (addr, _stop, _handle) = start_server(core_for(&mock, &database).await).await;
        let mut client = connect(addr).await;
        let first = thread(&mut client, Some(&repo)).await;
        let request = |id: u64, method: &str, params: Value| {
            json!({"id": id, "method": method, "params": params})
        };

        let made = call(
            &mut client,
            2,
            request(2, "thread/worktree", json!({"thread_id": first})),
        )
        .await;
        let branch = format!("inocode/thread-{first}");
        assert_eq!(made["result"]["branch"], branch, "{made}");
        let place = std::path::PathBuf::from(made["result"]["project"].as_str().unwrap());
        assert!(place.join("kept.txt").is_file(), "{made}");
        assert!(git(&repo, &["branch", "--list", &branch]).contains(&branch));
        // The thread works in its worktree from now on.
        let found = call(
            &mut client,
            3,
            request(3, "files/search", json!({"thread_id": first, "query": "kept"})),
        )
        .await;
        assert_eq!(found["result"]["files"][0]["path"], "kept.txt", "{found}");
        let again = call(
            &mut client,
            4,
            request(4, "thread/worktree", json!({"thread_id": first})),
        )
        .await;
        assert_eq!(again["error"]["code"], "already_exists", "{again}");

        let moved = call(
            &mut client,
            5,
            request(5, "thread/folder", json!({"thread_id": first, "project": other.to_string_lossy()})),
        )
        .await;
        assert_eq!(moved["result"]["project"], other.to_string_lossy().as_ref(), "{moved}");
        let relative = call(
            &mut client,
            6,
            request(6, "thread/folder", json!({"thread_id": first, "project": "other"})),
        )
        .await;
        assert_eq!(relative["error"]["code"], "invalid_params", "{relative}");
        let back = command(
            &mut client,
            7,
            first,
            &format!("/cd {}", place.to_string_lossy()),
        )
        .await;
        assert_eq!(back["result"]["project"], place.to_string_lossy().as_ref(), "{back}");

        let deleted = call(
            &mut client,
            8,
            request(8, "thread/delete", json!({"thread_id": first})),
        )
        .await;
        assert_eq!(deleted["result"]["worktree"]["removed"], true, "{deleted}");
        assert!(!place.exists());
        assert!(git(&repo, &["branch", "--list", &branch]).contains(&branch), "the branch stays");

        // A worktree with work not committed stays.
        let second = thread(&mut client, Some(&repo)).await;
        let made = command(&mut client, 9, second, "/worktree feature-x").await;
        assert_eq!(made["result"]["branch"], "feature-x", "{made}");
        let place = std::path::PathBuf::from(made["result"]["project"].as_str().unwrap());
        std::fs::write(place.join("new.txt"), "work\n").expect("work");
        let deleted = call(
            &mut client,
            10,
            request(10, "thread/delete", json!({"thread_id": second})),
        )
        .await;
        assert_eq!(deleted["result"]["worktree"]["removed"], false, "{deleted}");
        assert!(place.join("new.txt").is_file());
    })
    .await;
}

#[tokio::test]
async fn a_watched_folder_reports_its_changes_until_it_is_unwatched() {
    with_timeout(async {
        let database = database();
        let folder = database.with_file_name("watched");
        std::fs::create_dir_all(folder.join("inner")).expect("folder");
        let mock = scripted(Vec::new()).await;
        let (addr, _stop, _handle) = start_server(core_for(&mock, &database).await).await;
        let mut client = connect(addr).await;
        client
            .1
            .write_json(
                2,
                &json!({"id": 2, "method": "fs/watch",
                        "params": {"path": folder.to_string_lossy(), "recursive": true}}),
            )
            .await
            .expect("watch");
        let started = next_message(&mut client, 2).await;
        assert_eq!(started["event"], "started", "{started}");
        std::fs::write(folder.join("inner").join("made.txt"), "x").expect("write");
        let changed = loop {
            let event = next_message(&mut client, 2).await;
            assert_eq!(event["event"], "changed", "{event}");
            if event["result"]["paths"].to_string().contains("made.txt") {
                break event;
            }
        };
        assert!(
            ["created", "modified", "other"].contains(&changed["result"]["kind"].as_str().unwrap()),
            "{changed}"
        );

        client
            .1
            .write_json(
                3,
                &json!({"id": 3, "method": "fs/unwatch", "params": {"watch_id": 2}}),
            )
            .await
            .expect("unwatch");
        let (mut unwatched, mut ended) = (None, false);
        while unwatched.is_none() || !ended {
            let (stream, message): (u64, Value) = client.0.read_json().await.expect("read");
            match stream {
                3 => unwatched = Some(message),
                2 => ended = message["event"] == "cancelled",
                _ => panic!("{message}"),
            }
        }
        assert_eq!(unwatched.unwrap()["result"]["unwatched"], true);
        let missing = call(
            &mut client,
            4,
            json!({"id": 4, "method": "fs/watch",
                   "params": {"path": folder.join("missing").to_string_lossy()}}),
        )
        .await;
        assert_eq!(missing["event"], "failed", "{missing}");
        assert_eq!(missing["error"]["code"], "not_found", "{missing}");
    })
    .await;
}

#[tokio::test]
async fn projects_and_threads_sit_in_groups_nested_to_any_depth() {
    with_timeout(async {
        let database = database();
        let folder = database.with_file_name("z581kl");
        std::fs::create_dir_all(&folder).expect("folder");
        let mock = scripted(Vec::new()).await;
        let (addr, _stop, _handle) = start_server(core_for(&mock, &database).await).await;
        let mut client = connect(addr).await;
        let mut next = 1;
        let mut ask = |method: &str, params: Value| {
            next += 1;
            (
                next,
                json!({"id": next, "method": method, "params": params}),
            )
        };

        let (n, request) = ask("groups/create", json!({"name": "android"}));
        let android = call(&mut client, n, request).await["result"]["group_id"]
            .as_i64()
            .unwrap();
        let (n, request) = ask(
            "groups/create",
            json!({"name": "bootloader", "parent_id": android}),
        );
        let bootloader = call(&mut client, n, request).await["result"]["group_id"]
            .as_i64()
            .unwrap();
        let (n, request) = ask(
            "projects/group",
            json!({"project": folder.to_string_lossy(), "group_id": bootloader}),
        );
        let grouped = call(&mut client, n, request).await;
        assert_eq!(grouped["result"]["group_id"], bootloader, "{grouped}");
        let in_project = thread(&mut client, Some(&folder)).await;
        let (n, request) = ask("thread/start", json!({"group_id": android}));
        let notes = call(&mut client, n, request).await["result"]["thread_id"]
            .as_i64()
            .unwrap();

        let (n, request) = ask("groups/list", json!({}));
        let listed = call(&mut client, n, request).await;
        let groups = listed["result"]["groups"].as_array().unwrap().clone();
        assert_eq!(groups.len(), 2, "{listed}");
        assert!(groups
            .iter()
            .any(|group| group["name"] == "bootloader" && group["parent_id"] == android));
        assert!(listed["result"]["projects"]
            .as_array()
            .unwrap()
            .iter()
            .any(
                |project| project["project"] == folder.to_string_lossy().as_ref()
                    && project["group_id"] == bootloader
            ));
        let (n, request) = ask("thread/list", json!({}));
        let threads = call(&mut client, n, request).await;
        let group_of = |thread: i64| {
            threads["result"]["threads"]
                .as_array()
                .unwrap()
                .iter()
                .find(|entry| entry["thread_id"] == thread)
                .map(|entry| entry["group_id"].clone())
        };
        assert_eq!(group_of(in_project), Some(json!(bootloader)), "{threads}");
        assert_eq!(group_of(notes), Some(json!(android)), "{threads}");

        let (n, request) = ask(
            "groups/move",
            json!({"group_id": android, "parent_id": bootloader}),
        );
        let cycle = call(&mut client, n, request).await;
        assert_eq!(cycle["error"]["code"], "invalid_params", "{cycle}");
        let (n, request) = ask("groups/delete", json!({"group_id": android}));
        let full = call(&mut client, n, request).await;
        assert_eq!(full["error"]["code"], "group_not_empty", "{full}");
        let (n, request) = ask(
            "thread/group",
            json!({"thread_id": in_project, "group_id": android}),
        );
        let refused = call(&mut client, n, request).await;
        assert_eq!(refused["error"]["code"], "invalid_params", "{refused}");
        let (n, request) = ask(
            "groups/rename",
            json!({"group_id": bootloader, "name": "boot"}),
        );
        assert_eq!(
            call(&mut client, n, request).await["result"]["name"],
            "boot"
        );

        // Emptied, a group can go.
        let (n, request) = ask(
            "projects/group",
            json!({"project": folder.to_string_lossy(), "group_id": null}),
        );
        call(&mut client, n, request).await;
        let (n, request) = ask("groups/delete", json!({"group_id": bootloader}));
        assert_eq!(
            call(&mut client, n, request).await["result"]["deleted"],
            bootloader
        );
        let (n, request) = ask(
            "thread/group",
            json!({"thread_id": notes, "group_id": null}),
        );
        call(&mut client, n, request).await;
        let (n, request) = ask("groups/delete", json!({"group_id": android}));
        assert_eq!(
            call(&mut client, n, request).await["result"]["deleted"],
            android
        );
        let (n, request) = ask("thread/start", json!({"group_id": android}));
        let gone = call(&mut client, n, request).await;
        assert_eq!(gone["error"]["code"], "unknown_group", "{gone}");
    })
    .await;
}
