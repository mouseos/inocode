//! The account pool over real TCP: two Claude accounts signed in against a
//! mock, a quota refusal with `Retry-After` and usage headers that moves the
//! request to the other account, what survives a restart of the core, the
//! pool's status and settings by method and by slash command
//! (docs/evidence/account-pool.md).

mod support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use inocode_core::accounts::LoginEndpoints;
use inocode_core::oauth::FlowEndpoints;
use inocode_core::store::Store;
use inocode_core::Core;
use serde_json::{json, Value};
use support::*;

fn folder() -> std::path::PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let index = NEXT.fetch_add(1, Ordering::SeqCst);
    let folder = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("inocode-pool-{}-{index}", std::process::id()));
    let _ = std::fs::remove_dir_all(&folder);
    std::fs::create_dir_all(&folder).expect("folder");
    folder
}

/// A Claude stand-in. The token endpoint signs `code-<name>` in as
/// `<name>@example.test` with the access token `at-<name>`. The messages
/// endpoint answers with the bearer it got; `at-alice` is refused on quota
/// with `Retry-After` and spent usage windows, `at-carol` with HTTP 503, and
/// every answer carries usage headers.
async fn claude_mock() -> MockApi {
    start_mock(|request: &MockRequest| {
        if request.method == "GET" {
            return MockReply::status(404, "{}");
        }
        if request.path.ends_with("/oauth/token") {
            let name = request.body["code"]
                .as_str()
                .unwrap_or_default()
                .trim_start_matches("code-")
                .to_string();
            return MockReply::status(
                200,
                &json!({
                    "access_token": format!("at-{name}"),
                    "refresh_token": format!("rt-{name}"),
                    "expires_in": 3600,
                    "account": {"email_address": format!("{name}@example.test"),
                                "uuid": format!("uuid-{name}")},
                })
                .to_string(),
            );
        }
        let bearer = request
            .authorization
            .clone()
            .unwrap_or_default()
            .trim_start_matches("Bearer ")
            .to_string();
        match bearer.as_str() {
            "at-alice" => MockReply::status(
                429,
                r#"{"type":"error","error":{"type":"rate_limit_error","message":"limit"}}"#,
            )
            .with_header("retry-after", "3600")
            .with_header("anthropic-ratelimit-unified-5h-utilization", "1.0")
            .with_header("anthropic-ratelimit-unified-5h-status", "rejected")
            .with_header("anthropic-ratelimit-unified-7d-utilization", "0.5"),
            "at-carol" => MockReply::status(503, r#"{"error":"overloaded"}"#),
            _ => MockReply::stream(anthropic_stream(&bearer))
                .with_header("anthropic-ratelimit-unified-5h-utilization", "0.25")
                .with_header("anthropic-ratelimit-unified-7d-utilization", "0.4"),
        }
    })
    .await
}

/// A core whose settings hold the provider `claude` at the mock.
async fn core_for(folder: &std::path::Path, mock: &MockApi) -> Arc<Core> {
    if !folder.join("settings.json").exists() {
        std::fs::write(
            folder.join("settings.json"),
            json!({
                "providers": {"claude": {"kind": "claude-oauth", "base_url": mock.base_url}},
                "default": {"provider": "claude"},
            })
            .to_string(),
        )
        .expect("settings");
    }
    // Temporary stores in tests: no OS store entries, no passphrase.
    inocode_core::secrets::set_mode(inocode_core::secrets::StoreMode::Plaintext);
    let endpoints = LoginEndpoints {
        claude: FlowEndpoints {
            authorize_url: format!("{}/authorize", mock.base_url),
            token_url: format!("{}/oauth/token", mock.base_url),
            callback_port: 0,
            callback_path: "/callback".to_string(),
            code_redirect_uri: Some("https://example.test/code".to_string()),
        },
        ..LoginEndpoints::default()
    };
    let store = Store::open(folder.join("inocode.db")).await.expect("store");
    Arc::new(
        Core::new()
            .with_store(store)
            .with_credentials(folder.join("auth.json"))
            .with_login_endpoints(endpoints),
    )
}

async fn request(client: &mut Client, id: u64, method: &str, params: Value) -> Value {
    call(
        client,
        id,
        json!({"id": id, "method": method, "params": params}),
    )
    .await
}

/// Signs `name` in with a pasted code; the account id it got.
async fn sign_in(client: &mut Client, id: u64, name: &str) -> String {
    let started = request(
        client,
        id,
        "accounts/login/start",
        json!({"provider": "claude", "mode": "code"}),
    )
    .await;
    let url = started["result"]["url"].as_str().expect("url").to_string();
    let state = url
        .split(['?', '&'])
        .find_map(|pair| pair.strip_prefix("state="))
        .expect("a state");
    let done = request(
        client,
        id + 1,
        "accounts/login/code",
        json!({"login_id": started["result"]["login_id"], "code": format!("code-{name}#{state}")}),
    )
    .await;
    assert_eq!(done["result"]["status"], "done", "{done}");
    done["result"]["account"].as_str().unwrap().to_string()
}

async fn turn(client: &mut Client, id: u64, thread: i64) -> Value {
    client
        .1
        .write_json(
            id,
            &json!({"id": id, "method": "turn/start",
                    "params": {"thread_id": thread, "prompt": "hi"}}),
        )
        .await
        .expect("send");
    let mut answer = String::new();
    loop {
        let mut event = next_message(client, id).await;
        if event["event"] == "delta" {
            answer.push_str(event["text"].as_str().unwrap_or_default());
        }
        if matches!(
            event["event"].as_str(),
            Some("completed" | "failed" | "cancelled")
        ) {
            event["answer"] = json!(answer);
            return event;
        }
    }
}

async fn answered_with(client: &mut Client, id: u64, thread: i64) -> String {
    let last = turn(client, id, thread).await;
    assert_eq!(last["event"], "completed", "{last}");
    last["answer"].as_str().unwrap_or_default().to_string()
}

async fn new_thread(client: &mut Client, id: u64) -> i64 {
    request(client, id, "thread/start", json!({})).await["result"]["thread_id"]
        .as_i64()
        .unwrap()
}

/// The bearers the messages endpoint got, in order.
fn bearers(mock: &MockApi) -> Vec<String> {
    mock.requests()
        .into_iter()
        .filter(|request| request.method == "POST" && request.path.ends_with("/messages"))
        .map(|request| {
            request
                .authorization
                .unwrap_or_default()
                .trim_start_matches("Bearer ")
                .to_string()
        })
        .collect()
}

async fn status_of(client: &mut Client, id: u64) -> Value {
    let status = request(client, id, "pool/status", json!({"provider": "claude"})).await;
    status["result"]["providers"][0].clone()
}

fn row<'a>(status: &'a Value, account: &str) -> &'a Value {
    status["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == account)
        .unwrap_or_else(|| panic!("no row of {account}: {status}"))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

#[tokio::test]
async fn a_quota_refusal_moves_the_request_to_the_other_account_and_survives_a_restart() {
    with_timeout(async {
        let folder = folder();
        let mock = claude_mock().await;
        let core = core_for(&folder, &mock).await;
        let (addr, stop, _handle) = start_server(core).await;
        let mut client = connect(addr).await;
        let alice = sign_in(&mut client, 1, "alice").await;
        let bob = sign_in(&mut client, 3, "bob").await;
        assert_eq!(alice, "default");
        let thread = new_thread(&mut client, 5).await;

        // The first of two equal accounts is alice; she refuses on quota,
        // so the same request goes to bob, and the thread stays there.
        assert!(answered_with(&mut client, 6, thread)
            .await
            .contains("at-bob"));
        assert_eq!(bearers(&mock), ["at-alice", "at-bob"]);
        assert!(answered_with(&mut client, 7, thread)
            .await
            .contains("at-bob"));
        assert_eq!(bearers(&mock).len(), 3, "alice is cooling down");

        let before = now_ms();
        let status = status_of(&mut client, 8).await;
        assert_eq!(status["pool"]["strategy"], "quota", "{status}");
        assert_eq!(status["current"], bob.as_str());
        let refused = row(&status, &alice);
        let until = refused["cooldown_until"].as_u64().expect("a cooldown");
        assert!(
            until > before + 3_500_000 && until <= before + 3_600_000 + 5_000,
            "Retry-After: {refused}"
        );
        assert_eq!(refused["cooldown_source"], "retry-after");
        assert_eq!(refused["reason"], "cooldown");
        assert_eq!(refused["last_status"], 429);
        assert!(refused["last_error"].as_str().unwrap().contains("429"));
        assert_eq!(
            refused["score"], 100.0,
            "the spent 5-hour window: {refused}"
        );
        assert_eq!(refused["bound_threads"], 0);
        let serving = row(&status, &bob);
        assert_eq!(serving["score"], 40.0, "{serving}");
        assert_eq!(serving["usage"]["short_percent"], 25.0);
        assert_eq!(serving["bound_threads"], 1);
        assert_eq!(serving["reason"], Value::Null);
        assert_eq!(serving["current"], true);
        let listed = request(
            &mut client,
            9,
            "accounts/list",
            json!({"provider": "claude"}),
        )
        .await;
        let active: Vec<&Value> = listed["result"]["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|account| account["active"] == true)
            .collect();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0]["id"], bob.as_str());

        // A restart keeps the cooldown and the thread's account.
        let _ = stop.send(());
        drop(client);
        let core = core_for(&folder, &mock).await;
        let (addr, _stop, _handle) = start_server(core).await;
        let mut client = connect(addr).await;
        let status = status_of(&mut client, 10).await;
        assert_eq!(row(&status, &alice)["cooldown_until"], until, "{status}");
        assert_eq!(row(&status, &alice)["reason"], "cooldown");
        assert_eq!(row(&status, &bob)["bound_threads"], 1);
        assert!(answered_with(&mut client, 11, thread)
            .await
            .contains("at-bob"));
        let other = new_thread(&mut client, 12).await;
        assert!(answered_with(&mut client, 13, other)
            .await
            .contains("at-bob"));
        assert_eq!(bearers(&mock).len(), 5, "no request went to alice");

        // Clearing the cooldown by hand, and forgetting the threads'
        // accounts.
        let cleared = request(
            &mut client,
            14,
            "accounts/clear_cooldown",
            json!({"provider": "claude", "account": alice}),
        )
        .await;
        assert_eq!(cleared["result"]["cleared"], true, "{cleared}");
        let status = status_of(&mut client, 15).await;
        assert_eq!(row(&status, &alice)["cooldown_until"], Value::Null);
        assert_eq!(row(&status, &alice)["reason"], Value::Null);
        let again = request(
            &mut client,
            16,
            "accounts/clear_cooldown",
            json!({"provider": "claude", "account": alice}),
        )
        .await;
        assert_eq!(again["result"]["cleared"], false);
        let reset = request(
            &mut client,
            17,
            "pool/reset_affinity",
            json!({"provider": "claude"}),
        )
        .await;
        assert_eq!(reset["result"]["cleared"], 2, "{reset}");
        assert_eq!(
            row(&status_of(&mut client, 18).await, &bob)["bound_threads"],
            0
        );
    })
    .await;
}

#[tokio::test]
async fn failures_in_a_row_move_new_requests_to_another_account() {
    with_timeout(async {
        let folder = folder();
        let mock = claude_mock().await;
        let core = core_for(&folder, &mock).await;
        let (addr, _stop, _handle) = start_server(core).await;
        let mut client = connect(addr).await;
        let carol = sign_in(&mut client, 1, "carol").await;
        let dave = sign_in(&mut client, 3, "dave").await;
        for id in 0..3 {
            let thread = new_thread(&mut client, 10 + id * 2).await;
            let failed = turn(&mut client, 11 + id * 2, thread).await;
            assert_eq!(failed["event"], "failed", "{failed}");
            assert!(failed["error"]["message"].as_str().unwrap().contains("503"));
        }
        let status = status_of(&mut client, 20).await;
        let failing = row(&status, &carol);
        assert_eq!(failing["consecutive_failures"], 3, "{failing}");
        assert_eq!(failing["reason"], "transient");
        assert!(failing["soft_avoid_until"].as_u64().unwrap() > now_ms());
        assert_eq!(status["current"], dave.as_str());
        let thread = new_thread(&mut client, 21).await;
        assert!(answered_with(&mut client, 22, thread)
            .await
            .contains("at-dave"));
    })
    .await;
}

#[tokio::test]
async fn the_pool_is_set_by_method_and_by_command() {
    with_timeout(async {
        let folder = folder();
        let mock = claude_mock().await;
        let core = core_for(&folder, &mock).await;
        let (addr, _stop, _handle) = start_server(core).await;
        let mut client = connect(addr).await;
        sign_in(&mut client, 1, "erin").await;
        let thread = new_thread(&mut client, 3).await;

        let got = request(&mut client, 4, "pool/get", json!({"provider": "claude"})).await;
        assert_eq!(got["result"]["pool"], Value::Null, "{got}");
        assert_eq!(
            got["result"]["effective"],
            json!({"strategy": "quota", "sticky_limit": 1, "auto_switch_threshold": 80,
                   "failover_threshold": 3, "cache_affinity": true})
        );
        let set = request(
            &mut client,
            5,
            "pool/set",
            json!({"provider": "claude", "strategy": "round-robin", "sticky_limit": 3,
                   "cache_affinity": false}),
        )
        .await;
        assert_eq!(
            set["result"]["pool"],
            json!({"strategy": "round-robin", "sticky_limit": 3, "cache_affinity": false}),
            "{set}"
        );
        assert_eq!(set["result"]["effective"]["auto_switch_threshold"], 80);
        let settings: Value =
            serde_json::from_str(&std::fs::read_to_string(folder.join("settings.json")).unwrap())
                .unwrap();
        assert_eq!(
            settings["providers"]["claude"]["pool"]["strategy"],
            "round-robin"
        );
        let reset = request(
            &mut client,
            6,
            "pool/set",
            json!({"provider": "claude", "sticky_limit": null, "cache_affinity": null}),
        )
        .await;
        assert_eq!(reset["result"]["pool"], json!({"strategy": "round-robin"}));
        for (id, params) in [
            (7, json!({"provider": "claude", "strategy": "random"})),
            (8, json!({"provider": "claude", "sticky_limit": 0})),
            (
                9,
                json!({"provider": "claude", "auto_switch_threshold": 101}),
            ),
            (10, json!({"provider": "claude", "failover_threshold": 21})),
            (11, json!({"provider": "claude", "speed": 1})),
            (12, json!({"provider": "claude"})),
            (13, json!({"provider": "claude-oauth", "strategy": "quota"})),
        ] {
            let refused = request(&mut client, id, "pool/set", params.clone()).await;
            assert_eq!(
                refused["error"]["code"], "invalid_params",
                "{params}: {refused}"
            );
        }

        let ranked = request(
            &mut client,
            14,
            "accounts/priority",
            json!({"provider": "claude", "account": "default", "priority": -7,
                   "auto_switch_threshold": 95}),
        )
        .await;
        assert_eq!(ranked["result"]["account"]["priority"], -7, "{ranked}");
        assert_eq!(ranked["result"]["account"]["auto_switch_threshold"], 95);
        let status = status_of(&mut client, 15).await;
        assert_eq!(row(&status, "default")["threshold"], 95);
        for (id, params) in [
            (
                16,
                json!({"provider": "claude", "account": "default", "priority": 101}),
            ),
            (17, json!({"provider": "claude", "account": "default"})),
            (
                18,
                json!({"provider": "claude", "account": "nobody", "priority": 1}),
            ),
        ] {
            let refused = request(&mut client, id, "accounts/priority", params.clone()).await;
            assert!(refused["error"].is_object(), "{params}: {refused}");
        }

        let command = |text: &'static str| json!({"thread_id": thread, "text": text});
        let shown = request(&mut client, 19, "command/run", command("/accounts status")).await;
        assert_eq!(
            shown["result"]["providers"][0]["pool"]["strategy"], "round-robin",
            "{shown}"
        );
        let strategy = request(
            &mut client,
            20,
            "command/run",
            command("/accounts strategy claude fill-first"),
        )
        .await;
        assert_eq!(
            strategy["result"]["pool"]["strategy"], "fill-first",
            "{strategy}"
        );
        let ranked = request(
            &mut client,
            21,
            "command/run",
            command("/accounts priority claude default 3"),
        )
        .await;
        assert_eq!(ranked["result"]["account"]["priority"], 3, "{ranked}");
        let cleared = request(
            &mut client,
            22,
            "command/run",
            command("/accounts clear-cooldown claude default"),
        )
        .await;
        assert_eq!(cleared["result"]["cleared"], false, "{cleared}");
        let unknown = request(
            &mut client,
            23,
            "command/run",
            command("/accounts strategy claude random"),
        )
        .await;
        assert!(unknown["error"].is_object(), "{unknown}");
        assert!(answered_with(&mut client, 24, thread)
            .await
            .contains("at-erin"));
    })
    .await;
}

/// `pool/status` of the accounts signed in on this machine and one turn on
/// the provider `INOCODE_TEST_PROVIDER` names through the pool. Prints no
/// secret and no email.
#[tokio::test]
#[ignore = "requires the signed-in accounts of this machine; run by hand"]
async fn live_the_pool_shows_the_machines_accounts_and_answers() {
    with_timeout_of(LIVE_TIMEOUT, async {
        let database = live_folder("pool").join("inocode.db");
        let core = Arc::new(live_core(&database, None).await);
        let (addr, _stop, _handle) = start_server(core).await;
        let mut client = connect(addr).await;
        let status = request(&mut client, 1, "pool/status", json!({})).await;
        for provider in status["result"]["providers"].as_array().expect("providers") {
            println!(
                "provider {} kind {} pool {} current {}",
                provider["provider"], provider["kind"], provider["pool"], provider["current"]
            );
            for account in provider["accounts"].as_array().unwrap() {
                println!(
                    "  account {} score {} reason {} cooldown_until {} bound_threads {}",
                    account["id"],
                    account["score"],
                    account["reason"],
                    account["cooldown_until"],
                    account["bound_threads"]
                );
            }
        }
        let thread = new_thread(&mut client, 2).await;
        let last = turn(&mut client, 3, thread).await;
        println!("turn: {} answer: {}", last["event"], last["answer"]);
        assert_eq!(last["event"], "completed", "{last}");
        let status = request(
            &mut client,
            4,
            "pool/status",
            json!({"provider": LIVE_PROVIDER_ID}),
        )
        .await;
        let live = &status["result"]["providers"][0];
        println!("after the turn: current {}", live["current"]);
        for account in live["accounts"].as_array().unwrap() {
            println!(
                "  account {} score {} usage {} bound_threads {} last_status {}",
                account["id"],
                account["score"],
                account["usage"],
                account["bound_threads"],
                account["last_status"]
            );
        }
    })
    .await;
}
