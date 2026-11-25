//! The account registry over real TCP: sign-ins started over the protocol
//! against mock authorize and token endpoints, the accounts they add, and
//! listing, naming, pausing, removing and choosing them, by method and by
//! slash command (docs/evidence/account-pool.md).

mod support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use inocode_core::accounts::LoginEndpoints;
use inocode_core::oauth::FlowEndpoints;
use inocode_core::store::Store;
use inocode_core::Core;
use serde_json::{json, Value};
use support::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn folder() -> std::path::PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let index = NEXT.fetch_add(1, Ordering::SeqCst);
    let folder = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("inocode-accounts-{}-{index}", std::process::id()));
    let _ = std::fs::remove_dir_all(&folder);
    std::fs::create_dir_all(&folder).expect("folder");
    folder
}

/// A Claude stand-in. The token endpoint signs `code-<name>` in as
/// `<name>@example.test` with the access token `at-<name>` and refuses
/// every refresh; the messages endpoint answers with the bearer it got,
/// except that it refuses `at-revoked`.
async fn claude_mock() -> MockApi {
    start_mock(|request: &MockRequest| {
        if request.method == "GET" {
            return MockReply::status(404, "{}");
        }
        match request.path.as_str() {
            path if path.ends_with("/oauth/token") => {
                if request.body["grant_type"] != "authorization_code" {
                    return MockReply::status(400, r#"{"error":"invalid_grant"}"#);
                }
                let name = request.body["code"]
                    .as_str()
                    .unwrap_or_default()
                    .trim_start_matches("code-")
                    .to_string();
                MockReply::status(
                    200,
                    &json!({
                        "access_token": format!("at-{name}"),
                        "refresh_token": format!("rt-{name}"),
                        "expires_in": 3600,
                        "account": {"email_address": format!("{name}@example.test"),
                                    "uuid": format!("uuid-{name}")},
                    })
                    .to_string(),
                )
            }
            _ => {
                let bearer = request
                    .authorization
                    .clone()
                    .unwrap_or_default()
                    .trim_start_matches("Bearer ")
                    .to_string();
                if bearer == "at-revoked" {
                    return MockReply::status(401, r#"{"error":"unauthorized"}"#);
                }
                MockReply::stream(anthropic_stream(&bearer))
            }
        }
    })
    .await
}

/// A core whose settings hold the provider `claude` (kind `claude-oauth`)
/// at the mock, and whose sign-ins go to the mock.
async fn core_for(folder: &std::path::Path, mock: &MockApi, callback_port: u16) -> Arc<Core> {
    std::fs::write(
        folder.join("settings.json"),
        json!({
            "providers": {"claude": {"kind": "claude-oauth", "base_url": mock.base_url}},
            "default": {"provider": "claude"},
        })
        .to_string(),
    )
    .expect("settings");
    // Temporary stores in tests: no OS store entries, no passphrase.
    inocode_core::secrets::set_mode(inocode_core::secrets::StoreMode::Plaintext);
    let endpoints = LoginEndpoints {
        claude: FlowEndpoints {
            authorize_url: format!("{}/authorize", mock.base_url),
            token_url: format!("{}/oauth/token", mock.base_url),
            callback_port,
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

/// The `state` the authorize URL carries.
fn state_of(url: &str) -> String {
    url.split(['?', '&'])
        .find_map(|pair| pair.strip_prefix("state="))
        .expect("a state")
        .to_string()
}

/// Signs `name` in with a pasted code and returns the finished status.
async fn sign_in(client: &mut Client, id: u64, name: &str) -> Value {
    let started = request(
        client,
        id,
        "accounts/login/start",
        json!({"provider": "claude", "mode": "code"}),
    )
    .await;
    let started = &started["result"];
    assert_eq!(started["status"], "waiting", "{started}");
    assert_eq!(started["mode"], "code");
    let url = started["url"].as_str().expect("url");
    assert!(url.contains("/authorize?"), "{url}");
    let code = format!("code-{name}#{}", state_of(url));
    let done = request(
        client,
        id + 1,
        "accounts/login/code",
        json!({"login_id": started["login_id"], "code": code}),
    )
    .await;
    assert_eq!(done["result"]["status"], "done", "{done}");
    done["result"].clone()
}

async fn accounts(client: &mut Client, id: u64) -> Vec<Value> {
    request(client, id, "accounts/list", json!({})).await["result"]["accounts"]
        .as_array()
        .expect("accounts")
        .clone()
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

/// The answer of a turn that completed: the mock repeats the bearer.
async fn answered_with(client: &mut Client, id: u64, thread: i64) -> String {
    let last = turn(client, id, thread).await;
    assert_eq!(last["event"], "completed", "{last}");
    last["answer"].as_str().unwrap_or_default().to_string()
}

#[tokio::test]
async fn sign_ins_add_accounts_and_the_provider_uses_the_chosen_one() {
    with_timeout(async {
        let folder = folder();
        let mock = claude_mock().await;
        let core = core_for(&folder, &mock, 0).await;
        let (addr, _stop, _handle) = start_server(core).await;
        let mut client = connect(addr).await;
        let thread = request(&mut client, 1, "thread/start", json!({})).await["result"]
            ["thread_id"]
            .as_i64()
            .unwrap();

        let first = sign_in(&mut client, 2, "alice").await;
        assert_eq!(first["account"], "default", "the first account");
        assert_eq!(first["added"], true);
        let listed = accounts(&mut client, 4).await;
        assert_eq!(listed.len(), 1, "{listed:?}");
        assert_eq!(listed[0]["email"], "alice@example.test");
        assert_eq!(listed[0]["account_id"], "uuid-alice");
        assert_eq!(listed[0]["provider"], "claude");
        assert_eq!(listed[0]["kind"], "claude-oauth");
        assert_eq!(listed[0]["active"], true, "the only account");
        assert!(listed[0]["added"].as_u64().is_some());
        assert!(
            !listed[0].to_string().contains("at-alice") && !listed[0].to_string().contains("rt-"),
            "no secret is listed: {}",
            listed[0]
        );
        assert!(answered_with(&mut client, 5, thread)
            .await
            .contains("at-alice"));

        // The same person again replaces the credential instead of adding.
        let again = sign_in(&mut client, 6, "alice").await;
        assert_eq!(again["account"], "default");
        assert_eq!(again["added"], false);
        assert_eq!(accounts(&mut client, 8).await.len(), 1);

        let second = sign_in(&mut client, 9, "bob").await;
        assert_eq!(second["added"], true);
        let bob = second["account"].as_str().unwrap().to_string();
        assert_ne!(bob, "default");
        let listed = accounts(&mut client, 11).await;
        assert_eq!(listed.len(), 2);
        assert!(listed.iter().all(|account| account["active"] == false));
        let refused = turn(&mut client, 12, thread).await;
        assert_eq!(refused["event"], "failed", "{refused}");
        assert_eq!(refused["error"]["code"], "account_ambiguous", "{refused}");
        assert!(refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("/accounts use"));

        let named = request(
            &mut client,
            13,
            "accounts/rename",
            json!({"provider": "claude", "account": bob, "alias": "work"}),
        )
        .await;
        assert_eq!(named["result"]["account"]["alias"], "work", "{named}");
        let taken = request(
            &mut client,
            14,
            "accounts/rename",
            json!({"provider": "claude", "account": "default", "alias": "WORK"}),
        )
        .await;
        assert_eq!(taken["error"]["code"], "invalid_params", "{taken}");

        let used = request(
            &mut client,
            15,
            "accounts/use",
            json!({"provider": "claude", "account": "work"}),
        )
        .await;
        assert_eq!(used["result"]["active_account"], bob.as_str(), "{used}");
        let providers = request(&mut client, 16, "providers/list", json!({})).await;
        assert_eq!(
            providers["result"]["providers"][0]["active_account"],
            bob.as_str()
        );
        assert!(answered_with(&mut client, 17, thread)
            .await
            .contains("at-bob"));

        let paused = request(
            &mut client,
            18,
            "accounts/pause",
            json!({"provider": "claude", "account": bob, "paused": true}),
        )
        .await;
        assert_eq!(paused["result"]["account"]["paused"], true, "{paused}");
        assert_eq!(paused["result"]["account"]["active"], false);
        let refused = turn(&mut client, 19, thread).await;
        assert_eq!(refused["error"]["code"], "account_paused", "{refused}");
        request(
            &mut client,
            20,
            "accounts/pause",
            json!({"provider": "claude", "account": bob, "paused": false}),
        )
        .await;
        assert!(answered_with(&mut client, 21, thread)
            .await
            .contains("at-bob"));

        let removed = request(
            &mut client,
            22,
            "accounts/remove",
            json!({"provider": "claude", "account": bob}),
        )
        .await;
        assert_eq!(removed["result"]["removed"], bob.as_str(), "{removed}");
        let stored = std::fs::read_to_string(folder.join("auth.json")).unwrap();
        assert!(!stored.contains("at-bob"), "its secret is gone");
        let providers = request(&mut client, 23, "providers/list", json!({})).await;
        assert_eq!(
            providers["result"]["providers"][0]["active_account"],
            Value::Null,
            "the choice of the removed account is gone"
        );
        assert!(answered_with(&mut client, 24, thread)
            .await
            .contains("at-alice"));

        let unknown = request(
            &mut client,
            25,
            "accounts/use",
            json!({"provider": "claude", "account": "nobody"}),
        )
        .await;
        assert_eq!(unknown["error"]["code"], "account_unknown");
        let api_key = request(
            &mut client,
            26,
            "accounts/login/start",
            json!({"provider": "openai"}),
        )
        .await;
        assert_eq!(api_key["error"]["code"], "invalid_params");
        let wrong_mode = request(
            &mut client,
            27,
            "accounts/login/start",
            json!({"provider": "claude", "mode": "device"}),
        )
        .await;
        assert_eq!(wrong_mode["error"]["code"], "invalid_params");
    })
    .await;
}

#[tokio::test]
async fn a_refused_account_waits_for_a_new_sign_in() {
    with_timeout(async {
        let folder = folder();
        let mock = claude_mock().await;
        let core = core_for(&folder, &mock, 0).await;
        let (addr, _stop, _handle) = start_server(core).await;
        let mut client = connect(addr).await;
        let thread = request(&mut client, 1, "thread/start", json!({})).await["result"]
            ["thread_id"]
            .as_i64()
            .unwrap();
        // `revoked` gets the access token the messages endpoint refuses,
        // and its refresh is refused too.
        sign_in(&mut client, 2, "revoked").await;
        let refused = turn(&mut client, 4, thread).await;
        assert_eq!(refused["event"], "failed", "{refused}");
        let listed = accounts(&mut client, 5).await;
        assert_eq!(listed[0]["needs_reauth"], true, "{listed:?}");
        let waiting = turn(&mut client, 6, thread).await;
        assert_eq!(
            waiting["error"]["code"], "account_needs_reauth",
            "{waiting}"
        );
        assert!(waiting["error"]["message"]
            .as_str()
            .unwrap()
            .contains("/login"));
        // Signing the same person in again makes the account usable.
        let again = sign_in(&mut client, 7, "revoked").await;
        assert_eq!(again["added"], false);
        assert_eq!(accounts(&mut client, 9).await[0]["needs_reauth"], false);
    })
    .await;
}

#[tokio::test]
async fn a_browser_callback_finishes_a_sign_in_and_a_sign_in_can_be_cancelled() {
    with_timeout(async {
        let folder = folder();
        let mock = claude_mock().await;
        let port = {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            listener.local_addr().unwrap().port()
        };
        let core = core_for(&folder, &mock, port).await;
        let (addr, _stop, _handle) = start_server(core).await;
        let mut client = connect(addr).await;

        let started = request(
            &mut client,
            1,
            "accounts/login/start",
            json!({"provider": "claude-oauth"}),
        )
        .await;
        let started = &started["result"];
        assert_eq!(started["mode"], "callback", "{started}");
        assert_eq!(started["provider"], Value::Null, "named by its kind");
        let url = started["url"].as_str().unwrap();
        assert!(url.contains(&format!("localhost%3A{port}")), "{url}");
        let refused_code = request(
            &mut client,
            2,
            "accounts/login/code",
            json!({"login_id": started["login_id"], "code": "x"}),
        )
        .await;
        assert_eq!(refused_code["error"]["code"], "invalid_params");
        // The browser's redirect, once the listener is up.
        let redirect = format!(
            "GET /callback?code=code-carol&state={} HTTP/1.1\r\nHost: localhost\r\n\r\n",
            state_of(url)
        );
        let mut browser = loop {
            match tokio::net::TcpStream::connect(("127.0.0.1", port)).await {
                Ok(stream) => break stream,
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        };
        browser.write_all(redirect.as_bytes()).await.unwrap();
        let mut reply = Vec::new();
        browser.read_to_end(&mut reply).await.unwrap();
        let status = loop {
            let status = request(
                &mut client,
                3,
                "accounts/login/status",
                json!({"login_id": started["login_id"]}),
            )
            .await;
            if status["result"]["status"] != "waiting" {
                break status;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        assert_eq!(status["result"]["status"], "done", "{status}");
        assert_eq!(status["result"]["account"], "default");
        assert_eq!(
            accounts(&mut client, 4).await[0]["email"],
            "carol@example.test"
        );

        let second = request(
            &mut client,
            5,
            "accounts/login/start",
            json!({"provider": "claude", "mode": "code"}),
        )
        .await;
        let cancelled = request(
            &mut client,
            6,
            "accounts/login/cancel",
            json!({"login_id": second["result"]["login_id"]}),
        )
        .await;
        assert_eq!(cancelled["result"]["status"], "cancelled", "{cancelled}");
        let late = request(
            &mut client,
            7,
            "accounts/login/code",
            json!({"login_id": second["result"]["login_id"], "code": "code-dave#x"}),
        )
        .await;
        assert_eq!(late["result"]["status"], "cancelled", "{late}");
        let unknown = request(
            &mut client,
            8,
            "accounts/login/status",
            json!({"login_id": "nothing"}),
        )
        .await;
        assert_eq!(unknown["error"]["code"], "unknown_login");
        assert_eq!(accounts(&mut client, 9).await.len(), 1);
    })
    .await;
}

async fn command(client: &mut Client, id: u64, thread: i64, text: &str) -> Value {
    let answer = request(
        client,
        id,
        "command/run",
        json!({"thread_id": thread, "text": text}),
    )
    .await;
    assert!(answer.get("result").is_some(), "{text}: {answer}");
    answer["result"].clone()
}

#[tokio::test]
async fn the_account_commands_sign_in_list_choose_and_sign_out() {
    with_timeout(async {
        let folder = folder();
        let mock = claude_mock().await;
        let core = core_for(&folder, &mock, 0).await;
        let (addr, _stop, _handle) = start_server(core).await;
        let mut client = connect(addr).await;
        let thread = request(&mut client, 1, "thread/start", json!({})).await["result"]
            ["thread_id"]
            .as_i64()
            .unwrap();

        let listed = request(&mut client, 2, "commands/list", json!({})).await;
        for name in ["login", "logout", "accounts"] {
            let entry = listed["result"]["commands"]
                .as_array()
                .unwrap()
                .iter()
                .find(|command| command["name"] == name)
                .cloned()
                .unwrap();
            assert_eq!(entry["available"], true, "{entry}");
        }

        // The thread's provider when none is named.
        let started = command(&mut client, 3, thread, "/login claude code").await;
        assert_eq!(started["command"], "login");
        let code = format!("code-erin#{}", state_of(started["url"].as_str().unwrap()));
        let done = command(
            &mut client,
            4,
            thread,
            &format!(
                "/login code {} {code}",
                started["login_id"].as_str().unwrap()
            ),
        )
        .await;
        assert_eq!(done["status"], "done", "{done}");
        let started = command(&mut client, 5, thread, "/login").await;
        assert_eq!(started["provider"], "claude");
        let status = command(
            &mut client,
            6,
            thread,
            &format!("/login status {}", started["login_id"].as_str().unwrap()),
        )
        .await;
        assert_eq!(status["status"], "waiting");
        let cancelled = command(
            &mut client,
            7,
            thread,
            &format!("/login cancel {}", started["login_id"].as_str().unwrap()),
        )
        .await;
        assert_eq!(cancelled["status"], "cancelled");

        let listed = command(&mut client, 8, thread, "/accounts").await;
        assert_eq!(listed["accounts"][0]["email"], "erin@example.test");
        let renamed = command(
            &mut client,
            9,
            thread,
            "/accounts rename claude default my own",
        )
        .await;
        assert_eq!(renamed["account"]["alias"], "my own");
        let used = command(&mut client, 10, thread, "/accounts use claude default").await;
        assert_eq!(used["active_account"], "default");
        let paused = command(&mut client, 11, thread, "/accounts pause claude default").await;
        assert_eq!(paused["account"]["paused"], true);
        let resumed = command(&mut client, 12, thread, "/accounts resume claude default").await;
        assert_eq!(resumed["account"]["paused"], false);
        let listed = command(&mut client, 13, thread, "/accounts list claude").await;
        assert_eq!(listed["accounts"][0]["active"], true);

        let out = command(&mut client, 14, thread, "/logout").await;
        assert_eq!(out["removed"], "default", "{out}");
        let listed = command(&mut client, 15, thread, "/accounts list").await;
        assert_eq!(listed["accounts"], json!([]));
        let missing = request(
            &mut client,
            16,
            "command/run",
            json!({"thread_id": thread, "text": "/accounts remove claude default"}),
        )
        .await;
        assert_eq!(missing["error"]["code"], "account_unknown", "{missing}");
    })
    .await;
}

/// Lists the accounts signed in on this machine and runs one turn on the
/// provider `INOCODE_TEST_PROVIDER` names with them. Prints no secret and
/// no email, only whether one is known.
#[tokio::test]
#[ignore = "requires the signed-in accounts of this machine; run by hand"]
async fn live_the_machines_accounts_are_listed_and_used() {
    with_timeout_of(LIVE_TIMEOUT, async {
        let database = live_folder("accounts").join("inocode.db");
        let core = Arc::new(live_core(&database, None).await);
        let (addr, _stop, _handle) = start_server(core).await;
        let mut client = connect(addr).await;
        let listed = accounts(&mut client, 1).await;
        for account in &listed {
            println!(
                "account {} kind {} provider {} active {} paused {} needs_reauth {} email known {}",
                account["id"],
                account["kind"],
                account["provider"],
                account["active"],
                account["paused"],
                account["needs_reauth"],
                account["email"].is_string()
            );
        }
        let live = listed
            .iter()
            .find(|account| account["provider"] == LIVE_PROVIDER_ID)
            .expect("an account of the live provider");
        assert_eq!(live["active"], true, "{live}");
        let thread = request(&mut client, 2, "thread/start", json!({})).await["result"]
            ["thread_id"]
            .as_i64()
            .unwrap();
        let last = turn(&mut client, 3, thread).await;
        println!("turn: {} answer: {}", last["event"], last["answer"]);
        assert_eq!(last["event"], "completed", "{last}");
    })
    .await;
}
