//! The settings methods over real TCP: providers, models per provider,
//! what a thread runs on, and the `default`, `roles` and `server` sections
//! (docs/evidence/account-pool.md).

mod support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use inocode_core::store::Store;
use inocode_core::Core;
use serde_json::{json, Value};
use support::*;

/// A folder of its own for each test: tests run in parallel.
fn folder() -> std::path::PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let index = NEXT.fetch_add(1, Ordering::SeqCst);
    let folder = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("inocode-settings-{}-{index}", std::process::id()));
    let _ = std::fs::remove_dir_all(&folder);
    std::fs::create_dir_all(&folder).expect("folder");
    folder
}

/// A core whose settings hold `settings` and whose credential store holds
/// the key `test-key` for the providers `mock`, `second` and `listed`.
async fn core_with(folder: &std::path::Path, settings: Value) -> Arc<Core> {
    std::fs::write(folder.join("settings.json"), settings.to_string()).expect("settings");
    let credentials = folder.join("auth.json");
    // Temporary stores in tests: no OS store entries, no passphrase.
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

/// Settings with the mock as the default provider.
fn mock_settings(mock: &MockApi) -> Value {
    json!({
        "language": "Japanese",
        "providers": {"mock": {"kind": "openai", "base_url": mock.base_url, "default_model": "model-a"}},
        "default": {"provider": "mock"},
    })
}

/// A mock provider: its model list, and answers that finish at once, the
/// first `delayed` of them late.
async fn mock(delayed: usize, first: Vec<Vec<u8>>) -> MockApi {
    let count = AtomicUsize::new(0);
    start_mock(move |request: &MockRequest| {
        if request.method == "GET" {
            return MockReply::status(
                200,
                &json!({"data": [{"id": "model-a"}, {"id": "model-b"}]}).to_string(),
            );
        }
        let index = count.fetch_add(1, Ordering::SeqCst);
        let reply = if index == 0 && !first.is_empty() {
            MockReply::stream(first.clone())
        } else {
            MockReply::stream(vec![sse_delta("done"), finish("stop"), sse_done()])
        };
        if index < delayed {
            reply.with_head_delay(Duration::from_millis(1500))
        } else {
            reply
        }
    })
    .await
}

fn finish(reason: &str) -> Vec<u8> {
    let payload = json!({ "choices": [{ "delta": {}, "finish_reason": reason }] });
    format!("data: {payload}\n\n").into_bytes()
}

fn read_call() -> Vec<Vec<u8>> {
    let payload = json!({ "choices": [{ "delta": { "tool_calls": [{
        "index": 0, "id": "call_1", "type": "function",
        "function": { "name": "read", "arguments": "{\"path\":\"a\"}" }
    }] } }] });
    vec![
        format!("data: {payload}\n\n").into_bytes(),
        finish("tool_calls"),
        sse_done(),
    ]
}

async fn request(client: &mut Client, id: u64, method: &str, params: Value) -> Value {
    call(
        client,
        id,
        json!({"id": id, "method": method, "params": params}),
    )
    .await
}

async fn turn(client: &mut Client, stream_id: u64, params: Value) -> Vec<Value> {
    client
        .1
        .write_json(
            stream_id,
            &json!({"id": stream_id, "method": "turn/start", "params": params}),
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

fn models_sent(mock: &MockApi) -> Vec<String> {
    mock.requests()
        .iter()
        .filter(|request| request.method == "POST")
        .map(|request| {
            request.body["model"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        })
        .collect()
}

#[tokio::test]
async fn providers_are_added_listed_changed_and_removed() {
    with_timeout(async {
        let folder = folder();
        let core = core_with(&folder, json!({})).await;
        let (addr, _stop, _handle) = start_server(core).await;
        let mut client = connect(addr).await;

        let listed = request(&mut client, 1, "providers/list", json!({})).await;
        assert_eq!(listed["result"]["providers"], json!([]), "{listed}");

        let added = request(
            &mut client,
            2,
            "providers/add",
            json!({"id": "mock", "kind": "anthropic", "default_model": "claude-haiku-5-5",
                   "reasoning": {"effort": "low"},
                   "translation": {"outgoing": false, "model_language": "en"}}),
        )
        .await;
        let provider = &added["result"]["provider"];
        assert_eq!(provider["id"], "mock", "{added}");
        assert_eq!(provider["kind"], "anthropic");
        assert_eq!(provider["effective_base_url"], "https://logfare.ai/v1");
        assert_eq!(provider["translation"]["model_language"], "en");
        assert_eq!(provider["credential"], true, "the key stored for mock");

        let refused = [
            (json!({"id": "mock", "kind": "openai"}), "already exists"),
            (json!({"id": "other", "kind": "ollama"}), "unknown variant"),
            (
                json!({"id": "other", "kind": "openai", "base_url": "http://example.com/v1"}),
                "must be https",
            ),
            (json!({"id": "bad id", "kind": "openai"}), "provider id"),
            (json!({"kind": "openai"}), "\"id\""),
            (
                json!({"id": "other", "kind": "openai", "reasoning": {"effort": "huge"}}),
                "effort",
            ),
            (
                json!({"id": "other", "kind": "openai", "translation": {"outgoing": true}}),
                "not implemented",
            ),
        ];
        for (index, (params, expected)) in refused.into_iter().enumerate() {
            let response = request(&mut client, 10 + index as u64, "providers/add", params).await;
            assert_eq!(response["error"]["code"], "invalid_params", "{response}");
            assert!(
                response["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains(expected),
                "{response}"
            );
        }

        let changed = request(
            &mut client,
            20,
            "providers/update",
            json!({"id": "mock", "base_url": "http://127.0.0.1:9/v1", "disabled": true,
                   "reasoning": null}),
        )
        .await;
        assert_eq!(
            changed["result"]["provider"]["base_url"], "http://127.0.0.1:9/v1",
            "{changed}"
        );
        assert_eq!(changed["result"]["provider"]["disabled"], true);
        assert!(changed["result"]["provider"].get("reasoning").is_none());
        let missing = request(&mut client, 21, "providers/update", json!({"id": "nobody"})).await;
        assert_eq!(missing["error"]["code"], "invalid_params", "{missing}");

        // Chosen by a role: removing it is refused with where it is chosen.
        let set = request(
            &mut client,
            30,
            "settings/set",
            json!({"roles": {"review": {"provider": "mock", "model": "m"}}}),
        )
        .await;
        assert!(set.get("result").is_some(), "{set}");
        let in_use = request(&mut client, 31, "providers/remove", json!({"id": "mock"})).await;
        assert_eq!(in_use["error"]["code"], "provider_in_use", "{in_use}");
        assert!(in_use["error"]["message"]
            .as_str()
            .unwrap()
            .contains("roles.review"));
        request(
            &mut client,
            32,
            "settings/set",
            json!({"roles": {"review": null}}),
        )
        .await;
        let removed = request(&mut client, 33, "providers/remove", json!({"id": "mock"})).await;
        assert_eq!(removed["result"]["removed"], "mock", "{removed}");
        let listed = request(&mut client, 34, "providers/list", json!({})).await;
        assert_eq!(listed["result"]["providers"], json!([]));

        let file: Value =
            serde_json::from_str(&std::fs::read_to_string(folder.join("settings.json")).unwrap())
                .unwrap();
        assert!(file.get("roles").is_some());
        assert!(
            !std::fs::read_to_string(folder.join("settings.json"))
                .unwrap()
                .contains("test-key"),
            "keys stay in the credential store"
        );
    })
    .await;
}

#[tokio::test]
async fn models_come_from_the_settings_list_or_from_the_provider() {
    with_timeout(async {
        let folder = folder();
        let mock = mock(0, Vec::new()).await;
        let mut settings = mock_settings(&mock);
        settings["providers"]["listed"] =
            json!({"kind": "openai", "base_url": mock.base_url, "models": ["x", "y"]});
        let core = core_with(&folder, settings).await;
        let (addr, _stop, _handle) = start_server(core).await;
        let mut client = connect(addr).await;

        let listed = request(&mut client, 1, "models/list", json!({"provider": "listed"})).await;
        let ids: Vec<&str> = listed["result"]["models"]
            .as_array()
            .unwrap()
            .iter()
            .map(|model| model["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["x", "y"], "{listed}");
        assert!(mock.requests().is_empty(), "a settings list is not fetched");

        let fetched = request(&mut client, 2, "models/list", json!({"provider": "mock"})).await;
        assert_eq!(fetched["result"]["models"][1]["id"], "model-b", "{fetched}");
        let asked = &mock.requests()[0];
        assert!(asked.path.ends_with("/models"), "{}", asked.path);
        assert_eq!(asked.authorization.as_deref(), Some("Bearer test-key"));

        let unknown = request(&mut client, 3, "models/list", json!({"provider": "nobody"})).await;
        assert_eq!(unknown["error"]["code"], "invalid_params", "{unknown}");

        // `models` lists the conversation's provider.
        let thread = request(&mut client, 4, "thread/start", json!({})).await["result"]
            ["thread_id"]
            .as_i64()
            .unwrap();
        name_thread(&mut client, thread).await;
        let chosen = request(
            &mut client,
            5,
            "thread/settings",
            json!({"thread_id": thread, "provider": "listed"}),
        )
        .await;
        assert_eq!(chosen["result"]["provider"], "listed", "{chosen}");
        let own = request(&mut client, 6, "models", json!({"thread_id": thread})).await;
        assert_eq!(own["result"]["models"][0]["id"], "x", "{own}");
        let default = request(&mut client, 7, "models", json!({})).await;
        assert_eq!(default["result"]["models"][0]["id"], "model-a", "{default}");
    })
    .await;
}

#[tokio::test]
async fn a_thread_runs_on_its_choice_and_a_change_applies_from_the_next_request() {
    with_timeout(async {
        let folder = folder();
        let mock = mock(1, read_call()).await;
        let core = core_with(&folder, mock_settings(&mock)).await;
        let (addr, _stop, _handle) = start_server(core).await;
        let mut client = connect(addr).await;
        let mut other = connect(addr).await;

        let thread = request(&mut client, 1, "thread/start", json!({})).await["result"]
            ["thread_id"]
            .as_i64()
            .unwrap();
        name_thread(&mut client, thread).await;
        let shown = request(
            &mut client,
            2,
            "thread/settings",
            json!({"thread_id": thread}),
        )
        .await;
        assert_eq!(
            shown["result"],
            json!({"thread_id": thread, "provider": "mock", "model": "model-a",
                   "effort": null, "summary": null, "mode": "on-request"}),
            "{shown}"
        );

        // The first request is answered late; the change made meanwhile is
        // used by the request after the tool call.
        let turn_task = tokio::spawn(async move {
            let events = turn(&mut client, 3, json!({"thread_id": thread, "prompt": "hi"})).await;
            (client, events)
        });
        tokio::time::sleep(Duration::from_millis(500)).await;
        let changed = request(
            &mut other,
            4,
            "thread/settings",
            json!({"thread_id": thread, "model": "model-b", "effort": "high", "mode": "full"}),
        )
        .await;
        assert_eq!(changed["result"]["model"], "model-b", "{changed}");
        assert_eq!(changed["result"]["effort"], "high");
        assert_eq!(changed["result"]["mode"], "full");
        let (mut client, events) = turn_task.await.unwrap();
        assert_eq!(events.last().unwrap()["event"], "completed", "{events:?}");
        assert_eq!(models_sent(&mock), ["model-a", "model-b"]);
        let second = mock
            .requests()
            .into_iter()
            .filter(|request| request.method == "POST")
            .nth(1)
            .unwrap();
        assert_eq!(second.body["reasoning_effort"], "high", "{}", second.body);

        // The next turn starts on the new choice.
        let events = turn(
            &mut client,
            5,
            json!({"thread_id": thread, "prompt": "again"}),
        )
        .await;
        assert_eq!(events.last().unwrap()["event"], "completed", "{events:?}");
        assert_eq!(models_sent(&mock).last().unwrap(), "model-b");

        // Refused choices are not kept.
        for (index, (patch, expected)) in [
            (json!({"provider": "nobody"}), "unknown provider"),
            (json!({"effort": "huge"}), "effort"),
            (json!({"mode": "always"}), "mode"),
            (json!({"colour": "red"}), "not a thread setting"),
        ]
        .into_iter()
        .enumerate()
        {
            let mut params = patch;
            params["thread_id"] = json!(thread);
            let response = request(&mut client, 10 + index as u64, "thread/settings", params).await;
            assert_eq!(response["error"]["code"], "invalid_params", "{response}");
            assert!(
                response["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains(expected),
                "{response}"
            );
        }
        // `null` returns a field to the default.
        let reset = request(
            &mut client,
            20,
            "thread/settings",
            json!({"thread_id": thread, "model": null, "mode": null}),
        )
        .await;
        assert_eq!(reset["result"]["model"], "model-a", "{reset}");
        assert_eq!(reset["result"]["mode"], "on-request");
    })
    .await;
}

#[tokio::test]
async fn a_thread_keeps_the_default_it_started_with() {
    with_timeout(async {
        let folder = folder();
        let mock = mock(0, Vec::new()).await;
        let mut settings = mock_settings(&mock);
        settings["providers"]["second"] =
            json!({"kind": "openai", "base_url": mock.base_url, "default_model": "model-b"});
        settings["providers"]["keyless"] = json!({"kind": "openai", "base_url": mock.base_url});
        let core = core_with(&folder, settings).await;
        let (addr, _stop, _handle) = start_server(core).await;
        let mut client = connect(addr).await;

        let first = request(&mut client, 1, "thread/start", json!({})).await["result"]["thread_id"]
            .as_i64()
            .unwrap();
        name_thread(&mut client, first).await;
        let set = request(
            &mut client,
            2,
            "settings/set",
            json!({"default": {"provider": "second"}}),
        )
        .await;
        assert_eq!(set["result"]["default"]["provider"], "second", "{set}");
        let second = request(&mut client, 3, "thread/start", json!({})).await["result"]
            ["thread_id"]
            .as_i64()
            .unwrap();
        name_thread(&mut client, second).await;
        turn(&mut client, 4, json!({"thread_id": first, "prompt": "a"})).await;
        turn(&mut client, 5, json!({"thread_id": second, "prompt": "b"})).await;
        assert_eq!(models_sent(&mock), ["model-a", "model-b"]);

        // A provider without a stored key: choosing it for a thread is
        // refused, and says how to store one.
        let refused = request(
            &mut client,
            6,
            "thread/settings",
            json!({"thread_id": first, "provider": "keyless"}),
        )
        .await;
        assert_eq!(
            refused["error"]["code"], "provider_unconfigured",
            "{refused}"
        );
        assert!(refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("inocode-server key set keyless"));
    })
    .await;
}

#[tokio::test]
async fn the_default_roles_and_server_sections_are_read_and_changed() {
    with_timeout(async {
        let folder = folder();
        let mock = mock(0, Vec::new()).await;
        let core = core_with(&folder, mock_settings(&mock)).await;
        let (addr, _stop, _handle) = start_server(core).await;
        let mut client = connect(addr).await;

        let read = request(&mut client, 1, "settings/get", json!({})).await;
        let sections = |result: &Value| {
            json!({"default": result["default"], "roles": result["roles"], "server": result["server"]})
        };
        assert_eq!(
            sections(&read["result"]),
            json!({"default": {"provider": "mock"}, "roles": {}, "server": {}}),
            "{read}"
        );
        assert_eq!(read["result"]["effective"]["stream_idle_seconds"], 300);
        assert_eq!(read["result"]["effective"]["claude_cli_channel"], "latest");
        // npm's channels when it answered, null when it did not.
        let channels = &read["result"]["claude_cli_channels"];
        assert!(channels.is_null() || channels["latest"].is_string(), "{read}");
        // The test process keeps credentials in plain files while the file
        // names no store (auto): that waits for a restart.
        assert_eq!(read["result"]["restart_for"], json!(["credentials"]), "{read}");
        assert_eq!(read["result"]["restart_required"], true, "{read}");
        let set = request(
            &mut client,
            2,
            "settings/set",
            json!({
                "default": {"model": "model-b", "effort": "low"},
                "roles": {"compact": {"model": "model-a"}, "colleague": {"provider": "mock"}},
                "server": {"port": 9100, "sandbox": false},
            }),
        )
        .await;
        assert_eq!(
            sections(&set["result"]),
            json!({
                "default": {"provider": "mock", "model": "model-b", "effort": "low"},
                "roles": {"compact": {"model": "model-a"}, "colleague": {"provider": "mock"}},
                "server": {"port": 9100, "sandbox": false},
            }),
            "{set}"
        );
        // The sandbox follows at once; the core of this test was given no
        // port, so a written port is not compared.
        assert_eq!(set["result"]["effective"]["sandbox"], false, "{set}");
        let back = request(
            &mut client,
            3,
            "settings/set",
            json!({"server": {"sandbox": null, "stream_idle_seconds": 60}}),
        )
        .await;
        assert_eq!(back["result"]["effective"]["sandbox"], true, "{back}");
        assert_eq!(back["result"]["effective"]["stream_idle_seconds"], 60);
        for (index, patch) in [
            json!({"providers": {}}),
            json!({"default": {"provider": "nobody"}}),
            json!({"server": {"port": 0}}),
            json!({"server": {"claude_cli_channel": "next; evil"}}),
            json!({"roles": {"explore": {"effort": "huge"}}}),
            json!({"roles": {"judge": {"model": "m"}}}),
        ]
        .into_iter()
        .enumerate()
        {
            let response = request(&mut client, 10 + index as u64, "settings/set", patch).await;
            assert_eq!(response["error"]["code"], "invalid_params", "{response}");
        }
        let file: Value =
            serde_json::from_str(&std::fs::read_to_string(folder.join("settings.json")).unwrap())
                .unwrap();
        assert_eq!(file["language"], "Japanese", "other sections are kept");
        assert_eq!(file["server"]["port"], 9100);

        // A file broken already is not reported as a wrong change.
        std::fs::write(folder.join("settings.json"), "{ not json").unwrap();
        let broken = request(
            &mut client,
            20,
            "settings/set",
            json!({"default": {"model": "model-a"}}),
        )
        .await;
        assert_eq!(broken["error"]["code"], "settings_invalid", "{broken}");
    })
    .await;
}

#[tokio::test]
async fn a_key_set_while_the_server_runs_is_used_from_the_next_request() {
    with_timeout(async {
        let folder = folder();
        let mock = mock(0, Vec::new()).await;
        let core = core_with(&folder, mock_settings(&mock)).await;
        let (addr, _stop, _handle) = start_server(core).await;
        let mut client = connect(addr).await;
        let thread = request(&mut client, 1, "thread/start", json!({})).await["result"]
            ["thread_id"]
            .as_i64()
            .unwrap();
        name_thread(&mut client, thread).await;
        turn(
            &mut client,
            2,
            json!({"thread_id": thread, "prompt": "one"}),
        )
        .await;
        let mut store =
            inocode_core::oauth::Store::load(folder.join("auth.json")).expect("credentials");
        store
            .update(
                &inocode_core::settings::api_key_name("mock"),
                |credential| credential.api_key = Some("new-key".into()),
            )
            .expect("key");
        turn(
            &mut client,
            3,
            json!({"thread_id": thread, "prompt": "two"}),
        )
        .await;
        let keys: Vec<Option<String>> = mock
            .requests()
            .iter()
            .filter(|request| request.method == "POST")
            .map(|request| request.authorization.clone())
            .collect();
        assert_eq!(
            keys,
            [
                Some("Bearer test-key".to_string()),
                Some("Bearer new-key".to_string())
            ]
        );
    })
    .await;
}

#[tokio::test]
async fn a_provider_a_thread_chose_is_kept_and_a_removed_one_takes_its_key_along() {
    with_timeout(async {
        let folder = folder();
        let mock = mock(0, Vec::new()).await;
        let mut settings = mock_settings(&mock);
        settings["providers"]["second"] = json!({"kind": "openai", "base_url": mock.base_url});
        let core = core_with(&folder, settings).await;
        let (addr, _stop, _handle) = start_server(core).await;
        let mut client = connect(addr).await;
        let thread = request(&mut client, 1, "thread/start", json!({})).await["result"]
            ["thread_id"]
            .as_i64()
            .unwrap();
        name_thread(&mut client, thread).await;
        request(
            &mut client,
            2,
            "thread/settings",
            json!({"thread_id": thread, "provider": "second"}),
        )
        .await;
        let in_use = request(&mut client, 3, "providers/remove", json!({"id": "second"})).await;
        assert_eq!(in_use["error"]["code"], "provider_in_use", "{in_use}");
        assert!(in_use["error"]["message"]
            .as_str()
            .unwrap()
            .contains(&thread.to_string()));

        request(
            &mut client,
            4,
            "thread/settings",
            json!({"thread_id": thread, "provider": null}),
        )
        .await;
        let removed = request(&mut client, 5, "providers/remove", json!({"id": "second"})).await;
        assert_eq!(removed["result"]["removed"], "second", "{removed}");
        let again = request(
            &mut client,
            6,
            "providers/add",
            json!({"id": "second", "kind": "openai", "base_url": mock.base_url}),
        )
        .await;
        assert_eq!(
            again["result"]["provider"]["credential"], false,
            "the old key went with the old provider: {again}"
        );
    })
    .await;
}

#[tokio::test]
async fn without_a_provider_a_turn_says_how_to_add_one() {
    with_timeout(async {
        let folder = folder();
        let core = core_with(&folder, json!({})).await;
        let (addr, _stop, _handle) = start_server(core).await;
        let mut client = connect(addr).await;
        let thread = request(&mut client, 1, "thread/start", json!({})).await["result"]
            ["thread_id"]
            .as_i64()
            .unwrap();
        name_thread(&mut client, thread).await;
        let events = turn(&mut client, 2, json!({"thread_id": thread, "prompt": "hi"})).await;
        let last = events.last().unwrap();
        assert_eq!(last["event"], "failed", "{events:?}");
        assert_eq!(last["error"]["code"], "provider_unconfigured");
        assert!(last["error"]["message"]
            .as_str()
            .unwrap()
            .contains("providers/add"));
    })
    .await;
}

#[tokio::test]
async fn providers_and_models_are_hidden_from_the_lists_and_shown_again() {
    with_timeout(async {
        let mock = mock(0, Vec::new()).await;
        let folder = folder();
        let mut settings = mock_settings(&mock);
        settings["providers"]["listed"] =
            json!({"kind": "openai", "base_url": mock.base_url, "models": ["x", "y"]});
        let core = core_with(&folder, settings).await;
        let (addr, _stop, _handle) = start_server(core).await;
        let mut client = connect(addr).await;

        let hidden = request(
            &mut client,
            1,
            "models/visibility",
            json!({"provider": "listed", "model": "y", "hidden": true}),
        )
        .await;
        assert_eq!(hidden["result"]["hidden_models"], json!(["y"]), "{hidden}");
        // Every list for choosing leaves it out, unless asked for it.
        let listed = request(
            &mut client,
            10,
            "models/list",
            json!({"provider": "listed"}),
        )
        .await;
        let ids = |answer: &Value| -> Vec<Value> {
            answer["result"]["models"]
                .as_array()
                .expect("models")
                .iter()
                .map(|model| model["id"].clone())
                .collect()
        };
        assert_eq!(ids(&listed), [json!("x")], "{listed}");
        let all = request(
            &mut client,
            11,
            "models/list",
            json!({"provider": "listed", "include_hidden": true}),
        )
        .await;
        assert_eq!(ids(&all), [json!("x"), json!("y")], "{all}");
        let provider = request(
            &mut client,
            2,
            "providers/update",
            json!({"id": "mock", "hidden": true}),
        )
        .await;
        assert_eq!(provider["result"]["provider"]["hidden"], true, "{provider}");

        let written: Value = serde_json::from_str(
            &std::fs::read_to_string(folder.join("settings.json")).expect("settings"),
        )
        .expect("json");
        assert_eq!(
            written["providers"]["listed"]["hidden_models"],
            json!(["y"])
        );
        assert_eq!(written["providers"]["mock"]["hidden"], true);

        let shown = request(
            &mut client,
            3,
            "models/visibility",
            json!({"provider": "listed", "model": "y", "hidden": false}),
        )
        .await;
        assert_eq!(shown["result"]["hidden_models"], json!([]), "{shown}");
        let refused = request(
            &mut client,
            4,
            "models/visibility",
            json!({"provider": "nope", "model": "y", "hidden": true}),
        )
        .await;
        assert_eq!(refused["error"]["code"], "invalid_params", "{refused}");
    })
    .await;
}

#[tokio::test]
async fn switching_to_auto_during_a_turn_uses_the_reviewer_the_settings_name() {
    with_timeout(async {
        let count = AtomicUsize::new(0);
        let mock = start_mock(move |request: &MockRequest| {
            if request.method == "GET" {
                return MockReply::status(200, &json!({"data": [{"id": "model-a"}]}).to_string());
            }
            if request.body["model"] == "reviewer-model" {
                return MockReply::stream(vec![
                    sse_delta("<block>yes</block><reason>it deletes files</reason>"),
                    finish("stop"),
                    sse_done(),
                ]);
            }
            let call = |id: &str, name: &str, arguments: Value| {
                let payload = json!({ "choices": [{ "delta": { "tool_calls": [{
                    "index": 0, "id": id, "type": "function",
                    "function": {"name": name, "arguments": arguments.to_string()}
                }] } }] });
                MockReply::stream(vec![
                    format!("data: {payload}\n\n").into_bytes(),
                    finish("tool_calls"),
                    sse_done(),
                ])
            };
            match count.fetch_add(1, Ordering::SeqCst) {
                // Answers late, so the mode is changed while the turn waits;
                // the change applies from the next request.
                0 => call(
                    "p1",
                    "plan",
                    json!({"items": [{"step": "clean", "status": "in_progress"}]}),
                )
                .with_head_delay(Duration::from_millis(1500)),
                1 => call("s1", "shell", json!({"command": "rm -rf build"})),
                _ => MockReply::stream(vec![sse_delta("ok"), finish("stop"), sse_done()]),
            }
        })
        .await;
        let mut settings = mock_settings(&mock);
        settings["roles"] = json!({"review": {"model": "reviewer-model"}});
        let core = core_with(&folder(), settings).await;
        let (addr, _stop, _handle) = start_server(core).await;
        let mut client = connect(addr).await;
        let thread = request(&mut client, 1, "thread/start", json!({})).await["result"]
            ["thread_id"]
            .as_i64()
            .unwrap();
        name_thread(&mut client, thread).await;
        client
            .1
            .write_json(
                2,
                &json!({"id": 2, "method": "turn/start",
                        "params": {"thread_id": thread, "prompt": "clean", "mode": "on-request"}}),
            )
            .await
            .expect("send");
        let mut other = connect(addr).await;
        let changed = request(
            &mut other,
            3,
            "thread/settings",
            json!({"thread_id": thread, "mode": "auto"}),
        )
        .await;
        assert_eq!(changed["result"]["mode"], "auto", "{changed}");
        let mut events = Vec::new();
        loop {
            let event = next_message(&mut client, 2).await;
            let end = matches!(
                event["event"].as_str(),
                Some("completed" | "failed" | "cancelled")
            );
            events.push(event);
            if end {
                break;
            }
        }
        let reviewed: Vec<String> = mock
            .requests()
            .iter()
            .filter(|request| request.method == "POST")
            .map(|request| {
                request.body["model"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string()
            })
            .collect();
        assert!(
            reviewed.contains(&"reviewer-model".to_string()),
            "the reviewer of the settings was asked: {reviewed:?}"
        );
        assert!(
            events
                .iter()
                .any(|event| event.to_string().contains("Denied by the reviewer")),
            "{events:?}"
        );
    })
    .await;
}
