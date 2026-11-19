//! Interactive logins and OAuth-backed chat against local mock endpoints.
//!
//! The mock HTTP server here records raw requests (form bodies included) and
//! answers per path, standing in for the token, device, and key endpoints. The
//! real logins are exercised by the user; these tests verify the flows, the
//! credential store, and the request-time OAuth behaviour.

mod support;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use inocode_core::oauth::{self, AuthKind, Credential, FlowEndpoints, MuseEndpoints, Store};
use inocode_core::{Core, Llm, LlmConfig, ProviderKind};
use serde_json::{json, Value};
use support::*;

// ---------------------------------------------------------------------------
// Raw mock: records full requests, replies per path with a canned body.
// ---------------------------------------------------------------------------

struct RawRequest {
    path: String,
    headers: HashMap<String, String>,
    body: String,
}

impl RawRequest {
    fn authorization(&self) -> &str {
        self.headers
            .get("authorization")
            .map(String::as_str)
            .unwrap_or("")
    }
}

type RawLog = Arc<Mutex<Vec<RawRequest>>>;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

type RawHandler = Arc<dyn Fn(&RawRequest) -> (u16, String) + Send + Sync>;

async fn start_raw_mock(handler: RawHandler) -> (String, RawLog) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock listener");
    let addr = listener.local_addr().expect("mock address");
    let log: RawLog = Arc::new(Mutex::new(Vec::new()));
    let log_task = Arc::clone(&log);
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let handler = Arc::clone(&handler);
            let log_task = Arc::clone(&log_task);
            tokio::spawn(async move {
                let Ok(request) = read_raw(&mut stream).await else {
                    return;
                };
                let (status, body) = handler(&request);
                log_task.lock().expect("mock log").push(request);
                let response = format!(
                    "HTTP/1.1 {status} {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    if status == 200 { "OK" } else { "Bad Request" },
                    body.len(),
                );
                stream
                    .write_all(response.as_bytes())
                    .await
                    .expect("write mock response");
                stream.shutdown().await.expect("shutdown mock response");
            });
        }
    });
    (format!("http://{addr}"), log)
}

async fn read_raw(stream: &mut tokio::net::TcpStream) -> std::io::Result<RawRequest> {
    let mut buffer = Vec::new();
    let head_end = loop {
        let mut chunk = [0u8; 1024];
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "request ended",
            ));
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(position) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            break position + 4;
        }
    };
    let head = String::from_utf8_lossy(&buffer[..head_end]).to_string();
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next().unwrap_or_default().split_whitespace();
    let _method = request_line.next().unwrap_or_default().to_string();
    let path = request_line.next().unwrap_or_default().to_string();
    let mut headers = HashMap::new();
    let mut content_length = 0usize;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            let (name, value) = (name.trim().to_ascii_lowercase(), value.trim().to_string());
            if name == "content-length" {
                content_length = value.parse().unwrap_or(0);
            }
            headers.insert(name, value);
        }
    }
    let mut body = buffer[head_end..].to_vec();
    while body.len() < content_length {
        let mut chunk = [0u8; 1024];
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(content_length);
    Ok(RawRequest {
        path,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

/// An ephemeral port for the local callback listener: bind, read, release.
async fn free_port() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind for a port");
    listener.local_addr().expect("port").port()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_millis() as u64
}

/// A file name no other test in this run uses, even one started in the same
/// millisecond with the same name.
fn temp_store_path(name: &str) -> std::path::PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut path = std::env::temp_dir();
    path.push(format!(
        "inocode-oauth-test-{name}-{}-{}-{}.json",
        std::process::id(),
        now_ms(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    path
}

fn credential_for_store() -> Credential {
    Credential {
        access: "stored-access".to_string(),
        refresh: "stored-refresh".to_string(),
        expires_at_ms: Some(now_ms() + 3_600_000),
        account_id: Some("acct_test".to_string()),
        email: Some("user@example.test".to_string()),
        api_key: None,
        oauth_access_token: None,
    }
}

fn write_store(path: &std::path::Path, provider: &str, credential: Credential) {
    let mut store = Store::load(path.to_path_buf()).expect("load the test store");
    store.save(provider, credential).expect("write the store");
}

/// Drives a full chat over TCP and collects the streamed deltas.
async fn chat_deltas(core: Arc<Core>, prompt: &str) -> (Vec<String>, Value) {
    let (addr, stop, _handle) = start_server(core).await;
    let mut client = connect(addr).await;
    let response = call(
        &mut client,
        7,
        json!({"id": 7, "method": "chat", "params": {"prompt": prompt}}),
    )
    .await;
    assert_eq!(response["event"], "started", "chat must start: {response}");
    let mut deltas = Vec::new();
    let final_message = loop {
        let (stream, message): (u64, Value) = client.0.read_json().await.expect("read message");
        assert_eq!(stream, 7, "events arrive on the request stream");
        match message["event"].as_str() {
            Some("delta") => deltas.push(message["text"].as_str().expect("delta text").to_string()),
            Some("completed") | Some("failed") | Some("cancelled") => break message,
            _ => {}
        }
    };
    drop(client);
    let _ = stop.send(());
    (deltas, final_message)
}

// ---------------------------------------------------------------------------
// Login flows
// ---------------------------------------------------------------------------

#[tokio::test]
async fn claude_login_stores_the_oauth_credential() {
    let (base, log) = start_raw_mock(Arc::new(|request: &RawRequest| {
        assert_eq!(request.path, "/v1/oauth/token");
        let body: Value = serde_json::from_str(&request.body).expect("json token request");
        assert_eq!(body["grant_type"], "authorization_code");
        assert_eq!(body["code"], "auth-code-1");
        // The state echoed by the browser callback (CSRF value of this login).
        assert!(body["state"].as_str().is_some_and(|v| !v.is_empty()));
        assert!(body["code_verifier"]
            .as_str()
            .is_some_and(|v| v.len() == 43));
        (
            200,
            json!({
                "access_token": "at-claude",
                "refresh_token": "rt-claude",
                "expires_in": 3600,
                "account": {"email_address": "user@example.test", "uuid": "acc-1"},
            })
            .to_string(),
        )
    }))
    .await;
    let port = free_port().await;
    let endpoints = FlowEndpoints {
        authorize_url: format!("{base}/authorize"),
        token_url: format!("{base}/v1/oauth/token"),
        callback_port: port,
        callback_path: "/callback".to_string(),
        code_redirect_uri: None,
    };
    let start = oauth::claude_login_start(&endpoints, false);
    let state = start.state.clone();

    let flow = tokio::spawn(async move {
        oauth::claude_login_finish(&endpoints, start, None)
            .await
            .expect("the flow completes")
    });
    // Let the listener bind, then play the browser redirect.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut browser = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect the callback");
    browser
        .write_all(
            format!(
                "GET /callback?code=auth-code-1&state={state} HTTP/1.1\r\nHost: localhost\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .expect("send the redirect");
    let mut response = Vec::new();
    browser
        .read_to_end(&mut response)
        .await
        .expect("read reply");
    let credential = flow.await.expect("the flow task completes");
    assert_eq!(credential.access, "at-claude");
    assert_eq!(credential.refresh, "rt-claude");
    assert_eq!(credential.account_id.as_deref(), Some("acc-1"));
    assert_eq!(credential.email.as_deref(), Some("user@example.test"));
    assert!(credential
        .expires_at_ms
        .is_some_and(|expires| expires > now_ms()));
    assert_eq!(log.lock().expect("log").len(), 1);
}

#[tokio::test]
async fn claude_code_mode_exchanges_the_pasted_code() {
    let (base, _log) = start_raw_mock(Arc::new(|request: &RawRequest| {
        assert_eq!(request.path, "/v1/oauth/token");
        let body: Value = serde_json::from_str(&request.body).expect("json token request");
        assert_eq!(body["code"], "displayed-code");
        assert!(body["state"]
            .as_str()
            .is_some_and(|state| state.starts_with("inocode-")));
        assert_eq!(body["grant_type"], "authorization_code");
        (
            200,
            json!({"access_token": "at-display", "refresh_token": "rt-display", "expires_in": 3600})
                .to_string(),
        )
    }))
    .await;
    let endpoints = FlowEndpoints {
        authorize_url: format!("{base}/authorize"),
        token_url: format!("{base}/v1/oauth/token"),
        callback_port: 54545, // unused in code mode
        callback_path: "/callback".to_string(),
        code_redirect_uri: Some("https://platform.claude.com/oauth/code/callback".to_string()),
    };
    let start = oauth::claude_login_start(&endpoints, true);
    // As typed at the prompt: the fragment echoes this login's state and the
    // line still carries its terminator.
    let pasted = format!(
        "displayed-code#{}
",
        start.state
    );
    let credential = oauth::claude_login_finish(&endpoints, start, Some(pasted))
        .await
        .expect("the flow completes");
    assert_eq!(credential.access, "at-display");
}

#[tokio::test]
async fn gpt_login_extracts_the_account_id_from_the_id_token() {
    let (base, log) = start_raw_mock(Arc::new(|request: &RawRequest| {
        assert_eq!(request.path, "/oauth/token");
        assert!(request.body.contains("grant_type=authorization_code"));
        assert!(request.body.contains("code=auth-code-2"));
        assert!(request.body.contains("code_verifier="));
        // The token endpoint receives a form body, not JSON.
        assert!(!request.body.trim_start().starts_with('{'));
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256"}"#);
        let payload = URL_SAFE_NO_PAD
            .encode(r#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acct-77"}}"#);
        (
            200,
            json!({
                "access_token": "at-gpt",
                "id_token": format!("{header}.{payload}.sig"),
                "refresh_token": "rt-gpt",
                "expires_in": 3600,
            })
            .to_string(),
        )
    }))
    .await;
    let port = free_port().await;
    let endpoints = FlowEndpoints {
        authorize_url: format!("{base}/authorize"),
        token_url: format!("{base}/oauth/token"),
        callback_port: port,
        callback_path: "/auth/callback".to_string(),
        code_redirect_uri: None,
    };
    let start = oauth::gpt_login_start(&endpoints);
    let state = start.state.clone();

    let flow = tokio::spawn(async move {
        oauth::gpt_login_finish(&endpoints, start, None)
            .await
            .expect("the flow completes")
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut browser = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect the callback");
    browser
        .write_all(
            format!(
                "GET /auth/callback?code=auth-code-2&state={state} HTTP/1.1\r\nHost: localhost\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .expect("send the redirect");
    let credential = flow.await.expect("the flow task completes");
    assert_eq!(credential.access, "at-gpt");
    assert_eq!(credential.account_id.as_deref(), Some("acct-77"));
    assert_eq!(log.lock().expect("log").len(), 1);
}

#[tokio::test]
async fn muse_login_polls_then_mints_the_subscription_key() {
    let polls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let poll_counter = Arc::clone(&polls);
    let (base, _log) = start_raw_mock(Arc::new(move |request: &RawRequest| {
        match request.path.as_str() {
            "/oidc/device/authorization/" => (
                200,
                json!({
                    "device_code": "dev-1",
                    "user_code": "ABCD-1234",
                    "verification_uri": "https://auth.meta.test/device",
                    "interval": 1,
                    "expires_in": 300,
                })
                .to_string(),
            ),
            "/oidc/device/token/" => {
                let seen = poll_counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if seen == 0 {
                    // First poll: still waiting for the user's approval.
                    (400, json!({"error": "authorization_pending"}).to_string())
                } else {
                    assert!(request.body.contains(
                        "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code"
                    ));
                    (
                        200,
                        json!({"access_token": "meta-account-token"}).to_string(),
                    )
                }
            }
            "/muse-code/key" => {
                assert_eq!(request.authorization(), "Bearer meta-account-token");
                assert!(request.body.contains("onboard"));
                (200, json!({"api_key": "LLM|minted-key"}).to_string())
            }
            other => panic!("unexpected mock path {other}"),
        }
    }))
    .await;
    let endpoints = MuseEndpoints {
        device_authorization_url: format!("{base}/oidc/device/authorization/"),
        device_token_url: format!("{base}/oidc/device/token/"),
        key_url: format!("{base}/muse-code/key"),
    };
    let credential = oauth::login_muse(&endpoints)
        .await
        .expect("the flow completes");
    // Two polls: one pending, one approved.
    assert_eq!(polls.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(credential.api_key.as_deref(), Some("LLM|minted-key"));
    // The Meta account token is kept for re-minting, not for model requests.
    assert_eq!(
        credential.oauth_access_token.as_deref(),
        Some("meta-account-token")
    );
    assert!(credential.access.is_empty());
}

#[tokio::test]
async fn claude_refresh_exchanges_the_refresh_token() {
    let (base, _log) = start_raw_mock(Arc::new(|request: &RawRequest| {
        assert_eq!(request.path, "/v1/oauth/token");
        let body: Value = serde_json::from_str(&request.body).expect("json token request");
        assert_eq!(body["grant_type"], "refresh_token");
        assert_eq!(body["refresh_token"], "rt-old");
        (
            200,
            json!({"access_token": "at-new", "refresh_token": "rt-new", "expires_in": 3600})
                .to_string(),
        )
    }))
    .await;
    let endpoints = FlowEndpoints {
        authorize_url: format!("{base}/authorize"),
        token_url: format!("{base}/v1/oauth/token"),
        callback_port: 54545,
        callback_path: "/callback".to_string(),
        code_redirect_uri: None,
    };
    let credential = oauth::refresh_claude(&endpoints, "rt-old")
        .await
        .expect("refresh");
    assert_eq!(credential.access, "at-new");
    assert_eq!(credential.refresh, "rt-new");
}

// ---------------------------------------------------------------------------
// OAuth-backed chat over TCP
// ---------------------------------------------------------------------------

fn responses_sse(answer: &str) -> Vec<Vec<u8>> {
    vec![
        format!("data: {}\n\n", json!({"type": "response.created"})).into_bytes(),
        format!(
            "data: {}\n\n",
            json!({"type": "response.output_text.delta", "delta": answer})
        )
        .into_bytes(),
        format!("data: {}\n\n", json!({"type": "response.completed"})).into_bytes(),
        // The Muse backend's tail, as observed: a subscription-usage note and
        // the plain `[DONE]` terminator, which the parser must skip.
        format!(
            "data: {}\n\n",
            json!({"type": "response.subscription_usage"})
        )
        .into_bytes(),
        b"data: [DONE]\n\n".to_vec(),
    ]
}

#[tokio::test]
async fn muse_chat_uses_the_subscription_key_and_muse_headers() {
    let mock = start_mock(|request: &MockRequest| {
        assert_eq!(request.path, "/v1/responses");
        assert_eq!(
            request.authorization.as_deref(),
            Some("Bearer LLM|stored-key")
        );
        assert_eq!(
            request.headers.get("user-agent").map(String::as_str),
            Some(oauth::MUSE_USER_AGENT)
        );
        assert_eq!(
            request.headers.get("x-api-version").map(String::as_str),
            Some("1.0.0")
        );
        assert_eq!(request.body["store"], false);
        assert_eq!(request.body["stream"], true);
        MockReply::stream(responses_sse("muse says hi"))
    })
    .await;
    let store_path = temp_store_path("muse");
    write_store(
        &store_path,
        "muse-code",
        Credential {
            access: String::new(),
            refresh: String::new(),
            expires_at_ms: None,
            account_id: None,
            email: None,
            api_key: Some("LLM|stored-key".to_string()),
            oauth_access_token: Some("meta-account-token".to_string()),
        },
    );
    let llm = Llm::new(LlmConfig {
        provider: ProviderKind::Responses,
        auth: inocode_core::oauth::AuthKind::MuseSubscription,
        base_url: mock.base_url.clone(),
        model: "muse-spark-1.3".to_string(),
        api_key: String::new(),
        auth_store_path: Some(store_path.clone()),
        reasoning: Default::default(),
    })
    .expect("build the client");
    let (deltas, message) = chat_deltas(Arc::new(Core::with_llm(llm)), "hello").await;
    assert_eq!(deltas, vec!["muse says hi".to_string()]);
    assert_eq!(message["event"], "completed");
    let _ = std::fs::remove_file(&store_path);
}

#[tokio::test]
async fn gpt_chat_sends_the_account_id_header() {
    let mock = start_mock(|request: &MockRequest| {
        assert_eq!(request.path, "/v1/responses");
        assert_eq!(
            request.authorization.as_deref(),
            Some("Bearer stored-access")
        );
        assert_eq!(
            request
                .headers
                .get("chatgpt-account-id")
                .map(String::as_str),
            Some("acct_test")
        );
        assert!(!request.headers.contains_key("user-agent"));
        MockReply::stream(responses_sse("codex says hi"))
    })
    .await;
    let store_path = temp_store_path("gpt");
    write_store(&store_path, "gpt-oauth", credential_for_store());
    let llm = Llm::new(LlmConfig {
        provider: ProviderKind::Responses,
        auth: inocode_core::oauth::AuthKind::GptOauth,
        base_url: mock.base_url.clone(),
        model: "gpt-5.2-codex".to_string(),
        api_key: String::new(),
        auth_store_path: Some(store_path.clone()),
        reasoning: Default::default(),
    })
    .expect("build the client");
    let (deltas, message) = chat_deltas(Arc::new(Core::with_llm(llm)), "hello").await;
    assert_eq!(deltas, vec!["codex says hi".to_string()]);
    assert_eq!(message["event"], "completed");
    let _ = std::fs::remove_file(&store_path);
}

#[tokio::test]
async fn gpt_chat_without_a_chosen_effort_uses_the_model_default() {
    let mock = start_mock(|request: &MockRequest| {
        if request.path.starts_with("/v1/models") {
            return MockReply::status(
                200,
                r#"{"models":[{"slug":"gpt-5.2-codex","supported_in_api":true,"visibility":"show","display_name":"GPT-5.2 Codex","supported_reasoning_levels":[{"effort":"low","description":""},{"effort":"high","description":""}],"default_reasoning_level":"high"}]}"#,
            );
        }
        assert_eq!(request.path, "/v1/responses");
        assert_eq!(
            request.body["reasoning"],
            json!({"effort": "high", "summary": "auto"})
        );
        MockReply::stream(responses_sse("deep reply"))
    })
    .await;
    let store_path = temp_store_path("gpt-default-effort");
    write_store(&store_path, "gpt-oauth", credential_for_store());
    let llm = Llm::new(LlmConfig {
        provider: ProviderKind::Responses,
        auth: inocode_core::oauth::AuthKind::GptOauth,
        base_url: mock.base_url.clone(),
        model: "gpt-5.2-codex".to_string(),
        api_key: String::new(),
        auth_store_path: Some(store_path.clone()),
        reasoning: Default::default(),
    })
    .expect("build the client");
    let core = Arc::new(Core::with_llm(llm));
    for _ in 0..2 {
        let (deltas, _) = chat_deltas(Arc::clone(&core), "hello").await;
        assert_eq!(deltas, vec!["deep reply".to_string()]);
    }
    // The catalog is read once, then remembered.
    let listings = mock
        .requests()
        .iter()
        .filter(|request| request.path.starts_with("/v1/models"))
        .count();
    assert_eq!(listings, 1);
    let _ = std::fs::remove_file(&store_path);
}

#[tokio::test]
async fn responses_with_an_api_key_sends_the_key_without_a_credential_store() {
    let mock = start_mock(|request: &MockRequest| {
        assert_eq!(request.path, "/v1/responses");
        assert_eq!(request.authorization.as_deref(), Some("Bearer static-key"));
        MockReply::stream(responses_sse("keyed reply"))
    })
    .await;
    let llm = Llm::new(LlmConfig {
        provider: ProviderKind::Responses,
        auth: inocode_core::oauth::AuthKind::ApiKey,
        base_url: mock.base_url.clone(),
        model: "m".to_string(),
        api_key: "static-key".to_string(),
        auth_store_path: Some(temp_store_path("absent")),
        reasoning: Default::default(),
    })
    .expect("build the client");
    let (deltas, message) = chat_deltas(Arc::new(Core::with_llm(llm)), "hello").await;
    assert_eq!(deltas, vec!["keyed reply".to_string()]);
    assert_eq!(message["event"], "completed");
}

#[tokio::test]
async fn claude_oauth_chat_sends_the_bearer_beta_and_identity_system_block() {
    let mock = start_mock(|request: &MockRequest| {
        assert_eq!(request.path, "/v1/messages");
        assert_eq!(
            request.authorization.as_deref(),
            Some("Bearer stored-access")
        );
        assert_eq!(
            request.headers.get("anthropic-beta").map(String::as_str),
            Some(oauth::CLAUDE_CODE_BETA)
        );
        // The identity block leads the system list.
        assert_eq!(
            request.body["system"][0]["text"],
            oauth::CLAUDE_CODE_SYSTEM_INSTRUCTION
        );
        MockReply::stream(vec![
            anthropic_text_delta("claude says hi"),
            anthropic_message_stop(),
        ])
    })
    .await;
    let store_path = temp_store_path("claude");
    write_store(&store_path, "claude-oauth", credential_for_store());
    let llm = Llm::new(LlmConfig {
        provider: ProviderKind::Anthropic,
        auth: inocode_core::oauth::AuthKind::ClaudeOauth,
        base_url: mock.base_url.clone(),
        model: "claude-sonnet-5".to_string(),
        api_key: String::new(),
        auth_store_path: Some(store_path.clone()),
        reasoning: Default::default(),
    })
    .expect("build the client");
    let (deltas, message) = chat_deltas(Arc::new(Core::with_llm(llm)), "hello").await;
    assert_eq!(deltas, vec!["claude says hi".to_string()]);
    assert_eq!(message["event"], "completed");
    let _ = std::fs::remove_file(&store_path);
}

#[tokio::test]
async fn chat_without_a_stored_credential_reports_provider_config() {
    let store_path = temp_store_path("missing");
    let llm = Llm::new(LlmConfig {
        provider: ProviderKind::Anthropic,
        auth: inocode_core::oauth::AuthKind::ClaudeOauth,
        base_url: "http://127.0.0.1:1".to_string(),
        model: "claude-sonnet-5".to_string(),
        api_key: String::new(),
        auth_store_path: Some(store_path.clone()),
        reasoning: Default::default(),
    })
    .expect("build the client");
    let error = llm.chat("hello").await.err().expect("no credential stored");
    assert_eq!(error.code, "provider_config");
    let _ = std::fs::remove_file(&store_path);
}

// ---------------------------------------------------------------------------
// Model discovery (`models`)
// ---------------------------------------------------------------------------

/// Calls the `models` method over TCP and returns the result list.
async fn models_call(core: Arc<Core>) -> Vec<Value> {
    let (addr, stop, _handle) = start_server(core).await;
    let mut client = connect(addr).await;
    let response = call(&mut client, 9, json!({"id": 9, "method": "models"})).await;
    assert!(
        response.get("result").is_some(),
        "models must answer with a result: {response}"
    );
    let models = response["result"]["models"]
        .as_array()
        .expect("model list")
        .clone();
    drop(client);
    let _ = stop.send(());
    models
}

#[tokio::test]
async fn muse_models_uses_the_subscription_key_and_muse_headers() {
    let mock = start_mock(|request: &MockRequest| {
        assert_eq!(request.path, "/v1/models");
        assert_eq!(
            request.authorization.as_deref(),
            Some("Bearer LLM|stored-key")
        );
        assert_eq!(
            request.headers.get("user-agent").map(String::as_str),
            Some(oauth::MUSE_USER_AGENT)
        );
        assert_eq!(request.headers.get("x-api-version").map(String::as_str), Some("1.0.0"));
        MockReply::status(
            200,
            r#"{"object":"list","data":[{"id":"muse-spark-1.3","owned_by":"meta"},{"id":"muse-wonder-1.0"}]}"#,
        )
    })
    .await;
    let store_path = temp_store_path("muse-models");
    write_store(
        &store_path,
        "muse-code",
        Credential {
            access: String::new(),
            refresh: String::new(),
            expires_at_ms: None,
            account_id: None,
            email: None,
            api_key: Some("LLM|stored-key".to_string()),
            oauth_access_token: Some("meta-account-token".to_string()),
        },
    );
    let llm = Llm::new(LlmConfig {
        provider: ProviderKind::Responses,
        auth: inocode_core::oauth::AuthKind::MuseSubscription,
        base_url: mock.base_url.clone(),
        model: "muse-spark-1.3".to_string(),
        api_key: String::new(),
        auth_store_path: Some(store_path.clone()),
        reasoning: Default::default(),
    })
    .expect("build the client");
    let models = models_call(Arc::new(Core::with_llm(llm))).await;
    assert_eq!(models.len(), 2);
    assert_eq!(models[0]["id"], "muse-spark-1.3");
    assert_eq!(models[0]["owned_by"], "meta");
    // No published list: every effort is offered, with no default.
    assert_eq!(
        models[0]["reasoning_levels"],
        json!(["none", "minimal", "low", "medium", "high", "xhigh", "max"])
    );
    assert_eq!(models[0]["default_reasoning"], Value::Null);
    let _ = std::fs::remove_file(&store_path);
}

#[tokio::test]
async fn gpt_models_sends_the_account_id_header() {
    let mock = start_mock(|request: &MockRequest| {
        assert_eq!(request.path, "/v1/models?client_version=99.0.0");
        assert_eq!(
            request.authorization.as_deref(),
            Some("Bearer stored-access")
        );
        assert_eq!(
            request
                .headers
                .get("chatgpt-account-id")
                .map(String::as_str),
            Some("acct_test")
        );
        MockReply::status(
            200,
            r#"{"object":"list","data":[{"id":"gpt-5.2-codex","owned_by":"openai"}]}"#,
        )
    })
    .await;
    let store_path = temp_store_path("gpt-models");
    write_store(&store_path, "gpt-oauth", credential_for_store());
    let llm = Llm::new(LlmConfig {
        provider: ProviderKind::Responses,
        auth: inocode_core::oauth::AuthKind::GptOauth,
        base_url: mock.base_url.clone(),
        model: "gpt-5.2-codex".to_string(),
        api_key: String::new(),
        auth_store_path: Some(store_path.clone()),
        reasoning: Default::default(),
    })
    .expect("build the client");
    let models = models_call(Arc::new(Core::with_llm(llm))).await;
    assert_eq!(models.len(), 1);
    assert_eq!(models[0]["id"], "gpt-5.2-codex");
    let _ = std::fs::remove_file(&store_path);
}

#[tokio::test]
async fn claude_oauth_models_sends_the_bearer_and_beta_header() {
    let mock = start_mock(|request: &MockRequest| {
        assert_eq!(request.path, "/v1/models?limit=1000");
        assert_eq!(request.authorization.as_deref(), Some("Bearer stored-access"));
        assert_eq!(
            request.headers.get("anthropic-beta").map(String::as_str),
            Some(oauth::CLAUDE_CODE_BETA)
        );
        MockReply::status(
            200,
            r#"{"data":[{"type":"model","id":"claude-sonnet-5","display_name":"Claude Sonnet 5"}],"has_more":false}"#,
        )
    })
    .await;
    let store_path = temp_store_path("claude-models");
    write_store(&store_path, "claude-oauth", credential_for_store());
    let llm = Llm::new(LlmConfig {
        provider: ProviderKind::Anthropic,
        auth: inocode_core::oauth::AuthKind::ClaudeOauth,
        base_url: mock.base_url.clone(),
        model: "claude-sonnet-5".to_string(),
        api_key: String::new(),
        auth_store_path: Some(store_path.clone()),
        reasoning: Default::default(),
    })
    .expect("build the client");
    let models = models_call(Arc::new(Core::with_llm(llm))).await;
    assert_eq!(models.len(), 1);
    assert_eq!(models[0]["id"], "claude-sonnet-5");
    assert_eq!(models[0]["display_name"], "Claude Sonnet 5");
    let _ = std::fs::remove_file(&store_path);
}

#[tokio::test]
async fn api_key_models_use_x_api_key_or_bearer_per_format() {
    // OpenAI Chat Completions: bearer on `{base}/models`.
    let mock = start_mock(|request: &MockRequest| {
        assert_eq!(request.path, "/v1/models");
        assert_eq!(request.authorization.as_deref(), Some("Bearer test-key"));
        MockReply::status(200, r#"{"data":[{"id":"logfare/auto"}]}"#)
    })
    .await;
    let llm = Llm::new(LlmConfig {
        provider: ProviderKind::OpenAi,
        auth: inocode_core::oauth::AuthKind::ApiKey,
        base_url: mock.base_url.clone(),
        model: "logfare/auto".to_string(),
        api_key: "test-key".to_string(),
        auth_store_path: None,
        reasoning: Default::default(),
    })
    .expect("build the client");
    let models = models_call(Arc::new(Core::with_llm(llm))).await;
    assert_eq!(models.len(), 1);
    assert_eq!(models[0]["id"], "logfare/auto");

    // Anthropic Messages: `x-api-key` on `{base}/v1/models?limit=1000`.
    let mock = start_mock(|request: &MockRequest| {
        assert_eq!(request.path, "/v1/models?limit=1000");
        assert!(request.authorization.is_none());
        MockReply::status(
            200,
            r#"{"data":[{"type":"model","id":"claude-haiku-5-5"}],"has_more":false}"#,
        )
    })
    .await;
    let llm = Llm::new(LlmConfig {
        provider: ProviderKind::Anthropic,
        auth: inocode_core::oauth::AuthKind::ApiKey,
        base_url: mock.base_url.clone(),
        model: "claude-haiku-5-5".to_string(),
        api_key: "test-key".to_string(),
        auth_store_path: None,
        reasoning: Default::default(),
    })
    .expect("build the client");
    let models = models_call(Arc::new(Core::with_llm(llm))).await;
    assert_eq!(models.len(), 1);
    assert_eq!(models[0]["id"], "claude-haiku-5-5");
}

#[tokio::test]
async fn slow_discovery_does_not_block_the_connection() {
    let mock = start_mock(|_request: &MockRequest| {
        MockReply::status(200, r#"{"data":[{"id":"late-model"}]}"#)
            .with_head_delay(Duration::from_secs(3))
    })
    .await;
    let llm = Llm::new(LlmConfig {
        provider: ProviderKind::OpenAi,
        auth: inocode_core::oauth::AuthKind::ApiKey,
        base_url: mock.base_url.clone(),
        model: "late-model".to_string(),
        api_key: "test-key".to_string(),
        auth_store_path: None,
        reasoning: Default::default(),
    })
    .expect("build the client");
    let (addr, _stop, _server) = start_server(Arc::new(Core::with_llm(llm))).await;
    let mut client = connect(addr).await;
    client
        .1
        .write_json(1, &json!({"id": 1, "method": "models"}))
        .await
        .expect("send models");
    // The ping is answered while discovery is still waiting on the provider.
    let pong = with_timeout_of(
        Duration::from_secs(1),
        call(&mut client, 2, json!({"id": 2, "method": "ping"})),
    )
    .await;
    assert_eq!(pong["result"]["pong"], true);
    let models = with_timeout(next_message(&mut client, 1)).await;
    assert_eq!(models["result"]["models"][0]["id"], "late-model");
}

#[tokio::test]
async fn an_idle_connection_does_not_hold_up_the_callback() {
    let (base, _log) = start_raw_mock(Arc::new(|_request: &RawRequest| {
        (
            200,
            json!({"access_token": "at-idle", "refresh_token": "rt-idle", "expires_in": 3600})
                .to_string(),
        )
    }))
    .await;
    let port = free_port().await;
    let endpoints = FlowEndpoints {
        authorize_url: format!("{base}/authorize"),
        token_url: format!("{base}/oauth/token"),
        callback_port: port,
        callback_path: "/auth/callback".to_string(),
        code_redirect_uri: None,
    };
    let start = oauth::gpt_login_start(&endpoints);
    let state = start.state.clone();
    let flow = tokio::spawn(async move { oauth::gpt_login_finish(&endpoints, start, None).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    // A preconnected socket that never sends a request stays open.
    let _idle = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("open the idle connection");
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut browser = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect the callback");
    browser
        .write_all(
            format!("GET /auth/callback?code=c&state={state} HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .as_bytes(),
        )
        .await
        .expect("send the redirect");
    let credential = with_timeout_of(Duration::from_secs(5), flow)
        .await
        .expect("the flow task completes")
        .expect("the flow completes");
    assert_eq!(credential.access, "at-idle");
}

#[tokio::test]
async fn oversized_bodies_are_not_read_whole() {
    // An error body past the cap is dropped instead of being read for the
    // snippet.
    let huge = "x".repeat(100 * 1024);
    let error_body = huge.clone();
    let mock = start_mock(move |_request: &MockRequest| MockReply::status(500, &error_body)).await;
    let llm = Llm::new(LlmConfig {
        provider: ProviderKind::OpenAi,
        auth: inocode_core::oauth::AuthKind::ApiKey,
        base_url: mock.base_url.clone(),
        model: "m".to_string(),
        api_key: "test-key".to_string(),
        auth_store_path: None,
        reasoning: Default::default(),
    })
    .expect("build the client");
    let error = llm.models().await.expect_err("HTTP 500 fails");
    assert_eq!(error.message, "provider returned HTTP 500: ");

    // A token response past the cap is rejected rather than parsed.
    let token_body = format!(r#"{{"access_token":"at","padding":"{huge}"}}"#);
    let (base, _log) = start_raw_mock(Arc::new(move |_request: &RawRequest| {
        (200, token_body.clone())
    }))
    .await;
    let error = oauth::refresh_gpt(
        &FlowEndpoints {
            authorize_url: format!("{base}/authorize"),
            token_url: format!("{base}/oauth/token"),
            callback_port: 0,
            callback_path: "/auth/callback".to_string(),
            code_redirect_uri: None,
        },
        "rt",
    )
    .await
    .expect_err("an oversized token response fails");
    assert!(error.contains("exceeds"), "{error}");
}

// ---------------------------------------------------------------------------
// Account usage (`usage`)
// ---------------------------------------------------------------------------

/// Calls the `usage` method over TCP and returns the result.
async fn usage_call(core: Arc<Core>) -> Value {
    let (addr, _stop, _server) = start_server(core).await;
    let mut client = connect(addr).await;
    let response = call(&mut client, 7, json!({"id": 7, "method": "usage"})).await;
    assert!(
        response.get("result").is_some(),
        "usage must answer with a result: {response}"
    );
    response["result"].clone()
}

fn oauth_llm(
    mock: &MockApi,
    auth: AuthKind,
    provider: &str,
    credential: Credential,
) -> (Llm, std::path::PathBuf) {
    let store_path = temp_store_path(&format!("usage-{provider}"));
    write_store(&store_path, provider, credential);
    let llm = Llm::new(LlmConfig {
        provider: ProviderKind::Responses,
        auth,
        base_url: mock.base_url.clone(),
        model: "m".to_string(),
        api_key: String::new(),
        auth_store_path: Some(store_path.clone()),
        reasoning: Default::default(),
    })
    .expect("build the client");
    (llm, store_path)
}

#[tokio::test]
async fn gpt_usage_reads_the_wham_windows() {
    let mock = start_mock(|request: &MockRequest| {
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, "/v1/wham/usage");
        assert_eq!(request.authorization.as_deref(), Some("Bearer stored-access"));
        assert_eq!(request.headers["chatgpt-account-id"], "acct_test");
        MockReply::status(
            200,
            r#"{"plan_type":"plus","rate_limit":{"allowed":true,
                "primary_window":{"used_percent":12,"reset_at":1794894516,"limit_window_seconds":18000},
                "secondary_window":{"used_percent":40,"reset_at":1795000000,"limit_window_seconds":604800}}}"#,
        )
    })
    .await;
    let (llm, path) = oauth_llm(
        &mock,
        AuthKind::GptOauth,
        "gpt-oauth",
        credential_for_store(),
    );
    let usage = usage_call(Arc::new(Core::with_llm(llm))).await;
    assert_eq!(usage["plan"], "plus");
    assert_eq!(usage["windows"][0]["name"], "primary");
    assert_eq!(usage["windows"][0]["used_percent"], 12.0);
    assert_eq!(usage["windows"][1]["window_seconds"], 604800);
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn claude_usage_sends_the_reference_headers() {
    let mock = start_mock(|request: &MockRequest| {
        assert_eq!(request.path, "/api/oauth/usage");
        assert_eq!(request.authorization.as_deref(), Some("Bearer stored-access"));
        assert_eq!(request.headers["user-agent"], "claude-cli/2.1.321 (external, cli)");
        assert!(request.headers["anthropic-beta"].starts_with("claude-code-20250219,oauth-2025-04-20"));
        assert!(!request.headers.contains_key("anthropic-version"));
        MockReply::status(
            200,
            r#"{"five_hour":{"utilization":41,"resets_at":"2026-11-16T15:00:00Z"},"seven_day":{"utilization":7}}"#,
        )
    })
    .await;
    let (llm, path) = oauth_llm(
        &mock,
        AuthKind::ClaudeOauth,
        "claude-oauth",
        credential_for_store(),
    );
    let usage = usage_call(Arc::new(Core::with_llm(llm))).await;
    assert_eq!(usage["windows"][0]["name"], "five_hour");
    assert_eq!(usage["windows"][0]["resets_at"], "2026-11-16T15:00:00Z");
    assert_eq!(usage["windows"][1]["used_percent"], 7.0);
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn muse_usage_reads_the_key_mint_without_onboarding_or_leaking_the_key() {
    let mock = start_mock(|request: &MockRequest| {
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/muse-code/key");
        assert_eq!(request.authorization.as_deref(), Some("Bearer meta-account-token"));
        assert_eq!(request.headers["x-api-version"], "1.0.0");
        assert_eq!(request.body, json!({}), "a usage read must not onboard");
        MockReply::status(
            200,
            r#"{"api_key":"LLM|fresh-secret","is_subs_active":true,"subs_tier_name":"Plus",
                "subs_usage":{"window":{"used_percent":20,"resets_at":1794894516,"window_duration_mins":300},
                              "weekly":{"used_percent":3,"resets_at":1795000000}}}"#,
        )
    })
    .await;
    let mut credential = credential_for_store();
    credential.api_key = Some("LLM|stored".to_string());
    credential.oauth_access_token = Some("meta-account-token".to_string());
    let (llm, path) = oauth_llm(&mock, AuthKind::MuseSubscription, "muse-code", credential);
    let usage = usage_call(Arc::new(Core::with_llm(llm))).await;
    assert_eq!(usage["plan"], "Plus");
    assert_eq!(usage["windows"][0]["window_seconds"], 18000);
    assert!(!usage.to_string().contains("secret"));
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn usage_errors_do_not_echo_the_response_body() {
    let mock = start_mock(|_request: &MockRequest| {
        MockReply::status(401, r#"{"api_key":"LLM|leaked","error":"unauthorized"}"#)
    })
    .await;
    let mut credential = credential_for_store();
    credential.oauth_access_token = Some("meta-account-token".to_string());
    let (llm, path) = oauth_llm(&mock, AuthKind::MuseSubscription, "muse-code", credential);
    let error = llm.usage().await.expect_err("401 fails");
    assert_eq!(error.message, "the usage request returned HTTP 401");
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn muse_renames_builtin_tool_names_and_restores_them_in_the_answer() {
    use inocode_core::conversation::{
        AssistantPart, Body, Conversation, Message, Speaker, ToolSpec,
    };
    let mock = start_mock(|request: &MockRequest| {
        let names: Vec<&str> = request.body["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .map(|tool| tool["name"].as_str().expect("name"))
            .collect();
        assert_eq!(names, vec!["client_shell", "read", "client_search"]);
        let replayed: Vec<&str> = request.body["input"]
            .as_array()
            .expect("input")
            .iter()
            .filter(|item| item["type"] == "function_call")
            .map(|item| item["name"].as_str().expect("name"))
            .collect();
        assert_eq!(replayed, vec!["client_shell"]);
        let call = json!({"type": "function_call", "id": "fc_2", "call_id": "c2",
                          "name": "client_shell", "arguments": "{\"command\":\"ls\"}"});
        MockReply::stream(vec![
            format!("data: {}\n\n", json!({"type": "response.created"})).into_bytes(),
            format!(
                "data: {}\n\n",
                json!({"type": "response.output_item.done", "item": call})
            )
            .into_bytes(),
            format!("data: {}\n\n", json!({"type": "response.completed"})).into_bytes(),
            b"data: [DONE]\n\n".to_vec(),
        ])
    })
    .await;
    let credential = Credential {
        access: String::new(),
        refresh: String::new(),
        expires_at_ms: None,
        account_id: None,
        email: None,
        api_key: Some("LLM|stored-key".to_string()),
        oauth_access_token: None,
    };
    let path = temp_store_path("muse-tool-names");
    write_store(&path, "muse-code", credential);
    let llm = Llm::new(LlmConfig {
        provider: ProviderKind::Responses,
        auth: AuthKind::MuseSubscription,
        base_url: mock.base_url.clone(),
        model: "m".to_string(),
        api_key: String::new(),
        auth_store_path: Some(path.clone()),
        reasoning: Default::default(),
    })
    .expect("build the client");
    let tool = |name: &str| ToolSpec {
        name: name.to_string(),
        description: String::new(),
        parameters: json!({"type": "object"}),
    };
    let message = |id: &str, body: Body| Message {
        id: id.to_string(),
        body,
    };
    let conversation = Conversation {
        system: None,
        messages: vec![
            message(
                "1",
                Body::User {
                    from: Speaker::Person,
                    parts: vec!["list".into()],
                },
            ),
            message(
                "2",
                Body::Assistant {
                    origin: inocode_core::conversation::Origin {
                        format: ProviderKind::Responses,
                        model: "m".into(),
                    },
                    stop: inocode_core::conversation::StopReason::ToolUse,
                    parts: vec![AssistantPart::ToolCall {
                        call_id: "c1".into(),
                        name: "shell".into(),
                        arguments: "{}".into(),
                        replay: None,
                    }],
                },
            ),
            message(
                "3",
                Body::ToolResult {
                    call_id: "c1".into(),
                    name: "shell".into(),
                    content: "a.txt".into(),
                    is_error: false,
                },
            ),
        ],
    };
    let mut stream = llm
        .converse(
            &conversation,
            &[tool("shell"), tool("read"), tool("search")],
        )
        .await
        .expect("request");
    while stream.next().await.expect("stream").is_some() {}
    let Body::Assistant { parts, .. } = stream.assistant() else {
        panic!("an assistant answer");
    };
    let names: Vec<&str> = parts
        .iter()
        .filter_map(|part| match part {
            AssistantPart::ToolCall { name, .. } => Some(name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(names, vec!["shell"]);
    let _ = std::fs::remove_file(&path);
}
