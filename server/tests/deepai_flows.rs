//! DeepAI Code account flows against a local mock of `api.deepai.org`:
//! sign-in exchange, lane headers, the `/theos` check-in, per-model format
//! routing, the `session_busy` wait, and lane refusal.

mod support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use inocode_core::deepai;
use inocode_core::oauth::{AuthKind, Credential, Store};
use inocode_core::{ErrorInfo, Llm, LlmConfig, ProviderKind};
use serde_json::json;
use support::*;
use tokio::io::AsyncWriteExt;

const NONCE: &str = "0123456789abcdef0123456789abcdef";

fn store_path(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "inocode-deepai-test-{name}-{}-{}.json",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ))
}

fn llm_for(mock: &MockApi, model: &str, name: &str) -> Llm {
    let path = store_path(name);
    // Temporary stores in tests: no OS store entries, no passphrase.
    inocode_core::secrets::set_mode(inocode_core::secrets::StoreMode::Plaintext);
    let mut store = Store::load(path.clone()).expect("load the test store");
    store
        .save(
            "deepai",
            Credential {
                access: String::new(),
                refresh: String::new(),
                expires_at_ms: None,
                account_id: None,
                email: None,
                api_key: Some("deepai-test-key".to_string()),
                oauth_access_token: None,
            },
        )
        .expect("write the test store");
    Llm::new(LlmConfig {
        provider: ProviderKind::OpenAi,
        auth: AuthKind::DeepAi,
        base_url: mock.base_url.clone(),
        model: model.to_string(),
        api_key: String::new(),
        auth_store_path: Some(path),
        reasoning: Default::default(),
    })
    .expect("build the client")
}

fn catalog() -> MockReply {
    MockReply::status(
        200,
        &json!({"default": "glm-5.3-flash", "models": [
            {"id": "glm-5.3-flash", "name": "GLM 5.3 Flash", "reasoning": true,
             "input": ["text"], "contextWindow": 1000, "maxTokens": 100,
             "premier": false, "theosPerRequest": "0"},
            {"id": "gpt-6-astra", "name": "GPT-6 Astra", "reasoning": true,
             "input": ["text"], "contextWindow": 1000, "maxTokens": 100,
             "premier": true, "theosPerRequest": "50", "api": "openai-responses"}
        ]})
        .to_string(),
    )
}

fn theos(seconds: u64) -> MockReply {
    MockReply::status(
        200,
        &json!({"enabled": true, "balance": "200", "granted": "200", "lane_version": 6,
                "check_in": {"nonce": NONCE, "seconds": seconds}})
        .to_string(),
    )
}

fn chat_reply() -> MockReply {
    MockReply::stream(vec![sse_delta("hello"), sse_done()])
}

fn responses_reply() -> MockReply {
    let delta = json!({"type": "response.output_text.delta", "delta": "astra"});
    let done = json!({"type": "response.completed"});
    MockReply::stream(vec![
        format!("data: {delta}\n\n").into_bytes(),
        format!("data: {done}\n\n").into_bytes(),
    ])
}

async fn collect(llm: &Llm) -> Result<String, ErrorInfo> {
    let mut stream = llm.chat("hi").await?;
    let mut text = String::new();
    while let Some(delta) = stream.next().await? {
        text.push_str(&delta);
    }
    Ok(text)
}

fn paths(mock: &MockApi) -> Vec<String> {
    mock.requests().into_iter().map(|r| r.path).collect()
}

#[tokio::test]
async fn chat_checks_in_first_and_sends_the_lane_headers() {
    let mock = start_mock(|request: &MockRequest| match request.path.as_str() {
        "/v1/models" => catalog(),
        "/v1/theos" => theos(600),
        "/v1/chat/completions" => chat_reply(),
        other => panic!("unexpected path {other}"),
    })
    .await;
    let llm = llm_for(&mock, "glm-5.3-flash", "headers");
    assert_eq!(with_timeout(collect(&llm)).await.expect("chat"), "hello");
    assert_eq!(with_timeout(collect(&llm)).await.expect("chat"), "hello");

    // One catalog fetch and one check-in serve both chats: the check-in is
    // renewed only after half of its interval.
    assert_eq!(
        paths(&mock),
        [
            "/v1/models",
            "/v1/theos",
            "/v1/chat/completions",
            "/v1/chat/completions"
        ]
    );
    let requests = mock.requests();
    let instance = requests[0].headers["x-deepai-code-instance"].clone();
    assert_eq!(instance.len(), 32);
    for request in &requests {
        assert_eq!(request.headers["x-deepai-code-theos"], "6");
        assert_eq!(request.headers["api-key"], "deepai-test-key");
        assert_eq!(request.headers["x-deepai-code-instance"], instance);
        assert!(request.authorization.is_none());
        if let Some(machine) = request.headers.get("x-deepai-code-machine") {
            assert_eq!(machine.len(), 64, "the machine id is a SHA-256 digest");
        }
    }
    let check_in = &requests[1];
    assert_beacon_version(&check_in.headers["x-deepai-code-beacon"]);
    assert!(check_in.headers["x-deepai-code-beacon"].contains(";up="));
    assert!(
        !check_in.headers.contains_key("x-deepai-code-check-in"),
        "no nonce has been received yet"
    );
    assert!(!requests[2].headers.contains_key("x-deepai-code-beacon"));
}

#[tokio::test]
async fn an_aging_check_in_is_renewed_with_the_received_nonce() {
    let mock = start_mock(|request: &MockRequest| match request.path.as_str() {
        "/v1/models" => catalog(),
        "/v1/theos" => theos(1),
        "/v1/chat/completions" => chat_reply(),
        other => panic!("unexpected path {other}"),
    })
    .await;
    let llm = llm_for(&mock, "glm-5.3-flash", "renew");
    with_timeout(collect(&llm)).await.expect("first chat");
    tokio::time::sleep(Duration::from_millis(700)).await;
    with_timeout(collect(&llm)).await.expect("second chat");

    let check_ins: Vec<MockRequest> = mock
        .requests()
        .into_iter()
        .filter(|r| r.path == "/v1/theos")
        .collect();
    assert_eq!(check_ins.len(), 2);
    assert_eq!(check_ins[1].headers["x-deepai-code-check-in"], NONCE);
    assert!(check_ins[1].headers["x-deepai-code-beacon"].ends_with(";pi=1"));
}

#[tokio::test]
async fn catalog_routes_responses_models_to_the_responses_endpoint() {
    let mock = start_mock(|request: &MockRequest| match request.path.as_str() {
        "/v1/models" => catalog(),
        "/v1/theos" => theos(600),
        "/v1/responses" => responses_reply(),
        other => panic!("unexpected path {other}"),
    })
    .await;
    let llm = llm_for(&mock, "gpt-6-astra", "responses");
    assert_eq!(with_timeout(collect(&llm)).await.expect("chat"), "astra");
    let request = mock
        .requests()
        .into_iter()
        .find(|r| r.path == "/v1/responses")
        .expect("responses request");
    assert_eq!(request.body["model"], "gpt-6-astra");
    assert_eq!(request.body["input"][0]["content"][0]["text"], "hi");
}

#[tokio::test]
async fn session_busy_is_waited_out_and_other_conflicts_fail() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&attempts);
    let mock = start_mock(move |request: &MockRequest| match request.path.as_str() {
        "/v1/models" => catalog(),
        "/v1/theos" => theos(600),
        "/v1/chat/completions" => {
            if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                MockReply::status(
                    409,
                    r#"{"error":{"message":"busy","type":"session_busy","code":"session_busy"}}"#,
                )
            } else {
                chat_reply()
            }
        }
        other => panic!("unexpected path {other}"),
    })
    .await;
    let llm = llm_for(&mock, "glm-5.3-flash", "busy");
    let started = std::time::Instant::now();
    assert_eq!(with_timeout(collect(&llm)).await.expect("chat"), "hello");
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert!(
        started.elapsed() >= Duration::from_secs(2),
        "the default wait without Retry-After is two seconds"
    );

    let mock = start_mock(|request: &MockRequest| match request.path.as_str() {
        "/v1/models" => catalog(),
        "/v1/theos" => theos(600),
        _ => MockReply::status(
            409,
            r#"{"error":{"message":"conflict","code":"other_conflict"}}"#,
        ),
    })
    .await;
    let llm = llm_for(&mock, "glm-5.3-flash", "conflict");
    let error = with_timeout(collect(&llm)).await.expect_err("fails");
    assert!(error.message.contains("HTTP 409"), "{}", error.message);
}

#[tokio::test]
async fn refusals_are_reported_with_their_cause() {
    // A newer lane on the server stops every request with 426.
    let mock = start_mock(|_request: &MockRequest| {
        MockReply::status(
            426,
            r#"{"error":{"message":"no longer supported","code":"lane_version_unsupported"}}"#,
        )
    })
    .await;
    let llm = llm_for(&mock, "glm-5.3-flash", "lane");
    let error = with_timeout(collect(&llm)).await.expect_err("fails");
    assert!(error.message.contains("lane 6"), "{}", error.message);

    // A check-in refusal that names its error is passed on.
    let mock = start_mock(|request: &MockRequest| match request.path.as_str() {
        "/v1/models" => catalog(),
        "/v1/theos" => MockReply::status(
            403,
            r#"{"error":{"message":"Verify your email to get Theos","code":"email_unverified"}}"#,
        ),
        other => panic!("unexpected path {other}"),
    })
    .await;
    let llm = llm_for(&mock, "glm-5.3-flash", "refused");
    let error = with_timeout(collect(&llm)).await.expect_err("fails");
    assert!(
        error.message.contains("email_unverified"),
        "{}",
        error.message
    );

    // An answer without a check-in from a current server is not a check-in.
    let mock = start_mock(|request: &MockRequest| match request.path.as_str() {
        "/v1/models" => catalog(),
        "/v1/theos" => MockReply::status(200, r#"{"enabled":true,"lane_version":6}"#),
        other => panic!("unexpected path {other}"),
    })
    .await;
    let llm = llm_for(&mock, "glm-5.3-flash", "no-check-in");
    let error = with_timeout(collect(&llm)).await.expect_err("fails");
    assert_eq!(error.code, "check_in_unavailable");
}

#[tokio::test]
async fn models_lists_the_account_catalog() {
    let mock = start_mock(|request: &MockRequest| match request.path.as_str() {
        "/v1/models" => catalog(),
        other => panic!("unexpected path {other}"),
    })
    .await;
    let llm = llm_for(&mock, "glm-5.3-flash", "models");
    let models = with_timeout(llm.models()).await.expect("models");
    let listed: Vec<(String, Option<String>)> =
        models.into_iter().map(|m| (m.id, m.display_name)).collect();
    assert_eq!(
        listed,
        [
            (
                "glm-5.3-flash".to_string(),
                Some("GLM 5.3 Flash".to_string())
            ),
            ("gpt-6-astra".to_string(), Some("GPT-6 Astra".to_string()))
        ]
    );
    assert_eq!(mock.requests()[0].headers["api-key"], "deepai-test-key");
}

#[tokio::test]
async fn sign_in_exchanges_the_callback_code_for_the_api_key() {
    let mock = start_mock(|request: &MockRequest| {
        assert_eq!(request.path, "/v1/desktop_auth/exchange/");
        assert_eq!(request.method, "POST");
        if request.body["code"] == "good-code" {
            MockReply::status(
                200,
                r#"{"api_key":"issued-key","username":"someone","is_pro":false}"#,
            )
        } else {
            MockReply::status(400, r#"{"error":"invalid or expired code"}"#)
        }
    })
    .await;
    let endpoints = deepai::SignInEndpoints {
        base: mock.base_url.clone(),
    };
    let start = deepai::sign_in_start(&endpoints);
    let query = start.url.split_once('?').expect("query").1.to_string();
    let field = |name: &str| {
        query
            .split('&')
            .find_map(|pair| pair.strip_prefix(&format!("{name}=")))
            .expect("field")
            .to_string()
    };
    let (state, port) = (field("state"), field("port"));
    assert_eq!(state.len(), 64);

    let flow = tokio::spawn(async move { deepai::sign_in_finish(&endpoints, start).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut browser = tokio::net::TcpStream::connect(("127.0.0.1", port.parse::<u16>().unwrap()))
        .await
        .expect("connect the callback");
    browser
        .write_all(
            format!("GET /auth-callback?code=good-code&state={state} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
                .as_bytes(),
        )
        .await
        .expect("send the redirect");
    let credential = with_timeout(flow)
        .await
        .expect("flow task")
        .expect("sign-in completes");
    assert_eq!(credential.api_key.as_deref(), Some("issued-key"));

    let endpoints = deepai::SignInEndpoints {
        base: mock.base_url.clone(),
    };
    let error = deepai::exchange_code(&endpoints, "stale-code")
        .await
        .expect_err("a stale code fails");
    assert!(error.contains("expired"), "{error}");
}

#[test]
fn deepai_is_selected_by_its_provider_name() {
    assert_eq!(
        AuthKind::from_provider_name("deepai"),
        Some(AuthKind::DeepAi)
    );
    assert_eq!(AuthKind::DeepAi.store_name(), "deepai");
}

#[tokio::test]
async fn usage_reads_the_theos_allowance_and_keeps_its_check_in() {
    let mock = start_mock(|request: &MockRequest| match request.path.as_str() {
        "/v1/theos" => theos(600),
        "/v1/models" => catalog(),
        "/v1/chat/completions" => chat_reply(),
        other => panic!("unexpected path {other}"),
    })
    .await;
    let llm = llm_for(&mock, "glm-5.3-flash", "theos");
    let usage = with_timeout(llm.usage()).await.expect("usage");
    let theos = usage.theos.expect("theos");
    assert!(theos.enabled);
    assert_eq!(theos.balance.as_deref(), Some("200"));
    assert_eq!(theos.granted.as_deref(), Some("200"));
    assert!(usage.windows.is_empty());
    let check_in = &mock.requests()[0];
    assert_beacon_version(&check_in.headers["x-deepai-code-beacon"]);

    // The allowance read was a check-in, so the next chat needs no other.
    with_timeout(collect(&llm)).await.expect("chat");
    assert_eq!(
        paths(&mock),
        ["/v1/theos", "/v1/models", "/v1/chat/completions"]
    );
}

/// The beacon starts with `v=` and DeepAI Code's release: the one its
/// update feed names, or the built-in one when the feed does not answer.
fn assert_beacon_version(beacon: &str) {
    let version = beacon
        .strip_prefix("v=")
        .and_then(|rest| rest.split(';').next())
        .unwrap_or_else(|| panic!("{beacon}"));
    assert!(
        version.split('.').all(|part| part.parse::<u32>().is_ok()),
        "{beacon}"
    );
}
