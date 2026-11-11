use super::ClaudeAuthManager;
use super::ClaudeCredentials;
use super::begin;
use super::consts;
use super::exchange;
use super::refresh_credentials;
use crate::test_support::transport_default_auth_route_config;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use serial_test::serial;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

const NOW: i64 = 1_000_000_000;

struct Endpoints;

impl Endpoints {
    fn point_at(server: &MockServer) -> Self {
        // SAFETY: these tests are `#[serial]`, so nothing else reads the environment meanwhile.
        unsafe {
            std::env::set_var(consts::TOKEN_URL_ENV, format!("{}/token", server.uri()));
            std::env::set_var(consts::PROFILE_URL_ENV, format!("{}/profile", server.uri()));
        }
        Self
    }
}

impl Drop for Endpoints {
    fn drop(&mut self) {
        // SAFETY: see `point_at`.
        unsafe {
            std::env::remove_var(consts::TOKEN_URL_ENV);
            std::env::remove_var(consts::PROFILE_URL_ENV);
        }
    }
}

async fn profile(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/profile"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "account": {"uuid": "u-1"},
            "organization": {"rate_limit_tier": "Default_Claude_Max_5x"}
        })))
        .mount(server)
        .await;
}

#[tokio::test]
#[serial]
async fn exchanging_a_pasted_code_posts_the_pkce_request_and_returns_credentials() {
    let server = MockServer::start().await;
    let _endpoints = Endpoints::point_at(&server);
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "access-FAKE",
            "refresh_token": "refresh-FAKE",
            "expires_in": 3600,
            "account": {"uuid": "u-1", "email_address": "someone@example.test"}
        })))
        .expect(1)
        .mount(&server)
        .await;
    profile(&server).await;

    let pending = begin(true);
    let pasted = format!("CODE123#{}", pending.state);
    let credentials = exchange(&pending, &pasted, &transport_default_auth_route_config(), NOW)
        .await
        .expect("exchange");
    assert_eq!(credentials.access, "access-FAKE");
    assert_eq!(credentials.refresh, "refresh-FAKE");
    assert_eq!(credentials.account_uuid.as_deref(), Some("u-1"));
    assert_eq!(credentials.plan.as_deref(), Some("default_claude_max_5x"));
    assert_eq!(credentials.expires_at_ms, NOW + 3_600_000 - consts::EXPIRY_MARGIN_MS);
    // The address in the token response is not kept anywhere.
    assert!(!serde_json::to_string(&credentials).expect("json").contains("example.test"));

    let requests = server.received_requests().await.expect("requests");
    let token_request = requests.iter().find(|r| r.url.path() == "/token").expect("token request");
    let body: Value = serde_json::from_slice(&token_request.body).expect("json body");
    assert_eq!(body["grant_type"], "authorization_code");
    assert_eq!(body["code"], "CODE123");
    assert_eq!(body["redirect_uri"], consts::CODE_REDIRECT_URI);
    assert!(body["code_verifier"].as_str().is_some_and(|v| v.len() >= 43));
    assert_eq!(body["state"], pending.state);
}

#[tokio::test]
#[serial]
async fn a_wrong_state_or_a_rejected_code_fails_without_leaking_secrets() {
    let server = MockServer::start().await;
    let _endpoints = Endpoints::point_at(&server);
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": "invalid_grant",
            "error_description": "refresh-FAKE was already used"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let pending = begin(false);
    let route = transport_default_auth_route_config();
    let wrong = exchange(&pending, "CODE#someone-elses-state", &route, NOW).await;
    assert!(wrong.is_err(), "a state that does not match is refused before any request");

    let rejected = exchange(&pending, "CODE", &route, NOW).await.expect_err("rejected");
    let message = rejected.to_string();
    assert!(message.contains("invalid_grant"), "{message}");
    assert!(!message.contains("refresh-FAKE"), "{message}");
}

#[tokio::test]
#[serial]
async fn the_manager_refreshes_an_expired_token_once_and_persists_it() {
    let server = MockServer::start().await;
    let _endpoints = Endpoints::point_at(&server);
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "access-NEW",
            "refresh_token": "refresh-NEW",
            "expires_in": 7200
        })))
        .expect(1)
        .mount(&server)
        .await;
    profile(&server).await;

    let dir = tempfile::tempdir().expect("tempdir");
    let manager = ClaudeAuthManager::create(
        dir.path().to_path_buf(),
        transport_default_auth_route_config(),
        ClaudeCredentials {
            access: "access-OLD".into(),
            refresh: "refresh-OLD".into(),
            expires_at_ms: 1, // long expired
            account_uuid: Some("u-1".into()),
            plan: None,
        },
    )
    .expect("create");

    assert_eq!(manager.access_token().await.expect("token"), "access-NEW");
    // The second call uses the refreshed token; the mock expects exactly one refresh.
    assert_eq!(manager.access_token().await.expect("token"), "access-NEW");
    assert_eq!(manager.account_uuid().await.as_deref(), Some("u-1"));

    // A new manager over the same directory (a restart) sees the rotated credentials.
    let reopened = ClaudeAuthManager::open(dir.path().to_path_buf(), transport_default_auth_route_config());
    assert_eq!(reopened.access_token().await.expect("token"), "access-NEW");
}

#[tokio::test]
#[serial]
async fn a_missing_refresh_token_in_the_response_keeps_the_old_one() {
    let server = MockServer::start().await;
    let _endpoints = Endpoints::point_at(&server);
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "access-NEW",
            "expires_in": 60
        })))
        .mount(&server)
        .await;
    let old = ClaudeCredentials {
        access: "access-OLD".into(),
        refresh: "refresh-KEEP".into(),
        expires_at_ms: 1,
        account_uuid: Some("u-1".into()),
        plan: Some("max".into()),
    };
    let refreshed = refresh_credentials(&old, &transport_default_auth_route_config(), NOW)
        .await
        .expect("refresh");
    assert_eq!(refreshed.refresh, "refresh-KEEP");
    assert_eq!(refreshed.account_uuid.as_deref(), Some("u-1"));
    // The profile endpoint is not mocked here (404): the stored plan survives.
    assert_eq!(refreshed.plan.as_deref(), Some("max"));
}
