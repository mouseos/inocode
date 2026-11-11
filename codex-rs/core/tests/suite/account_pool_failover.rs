//! A usage limit on one account moves the very same request to another pooled account
//! (credentials are fake; the server is a mock).

use anyhow::Result;
use codex_login::AuthCredentialsStoreMode;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_login::account_pool;
use codex_login::account_pool::AccountPool;
use codex_login::account_pool::PoolEnv;
use codex_protocol::protocol::EventMsg;
use codex_protocol::turn_input::TurnInputRequest;
use codex_protocol::user_input::UserInput;
use core_test_support::responses;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use serde_json::json;
use std::sync::Arc;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::header;
use wiremock::matchers::method;
use wiremock::matchers::path;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn usage_limit_retries_the_same_request_on_the_next_account() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(header("authorization", "Bearer sk-test-FAKE-first"))
        .respond_with(ResponseTemplate::new(429).set_body_json(json!({
            "error": {
                "type": "usage_limit_reached",
                "message": "limit reached",
                "resets_at": 4_102_444_800_i64,
                "plan_type": "pro"
            }
        })))
        .expect(1)
        .mount(&server)
        .await;
    let second = responses::mount_sse_once_match(
        &server,
        header("authorization", "Bearer sk-test-FAKE-second"),
        responses::sse(vec![
            ev_response_created("resp-2"),
            ev_assistant_message("msg-1", "done on the second account"),
            ev_completed("resp-2"),
        ]),
    )
    .await;

    let home = tempfile::tempdir()?;
    let primary = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("sk-test-FAKE-first"));
    let pool = AccountPool::open(
        PoolEnv {
            codex_home: home.path().to_path_buf(),
            mode: AuthCredentialsStoreMode::File,
            keyring: Default::default(),
            chatgpt_base_url: None,
            auth_route_config: codex_login::test_support::transport_default_auth_route_config(),
        },
        Arc::clone(&primary),
    )
    .await;
    pool.set_priority(account_pool::DEFAULT_ID, 5)?;
    pool.add_api_key("二番目", "sk-test-FAKE-second", Some(1)).await?;
    let mut switches = pool.subscribe();
    account_pool::install_global(Some(Arc::clone(&pool)));

    let mut builder = test_codex().with_auth_manager(primary);
    let fixture = builder.build(&server).await?;
    let codex = fixture.codex.clone();
    codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "hello".into(),
            text_elements: Vec::new(),
        }]))
        .await?;

    let notice = wait_for_event(&codex, |msg| {
        matches!(msg, EventMsg::Warning(w) if w.message.contains("二番目"))
    })
    .await;
    assert!(matches!(notice, EventMsg::Warning(_)));
    wait_for_event(&codex, |msg| matches!(msg, EventMsg::TurnComplete(_))).await;

    // The retry carried the full conversation, not a continuation of the first account's.
    let body = second.single_request().body_json();
    assert!(body.get("previous_response_id").is_none(), "{body}");
    assert!(
        body.to_string().contains("hello"),
        "the retry resends the user input: {body}"
    );

    let mut saw_switch = false;
    while let Ok(event) = switches.try_recv() {
        saw_switch |= matches!(event, account_pool::PoolEvent::Switched(_));
    }
    assert!(saw_switch);
    account_pool::install_global(None);
    Ok(())
}
