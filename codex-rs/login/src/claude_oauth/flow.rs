#![allow(clippy::expect_used)]

//! Authorization-code + PKCE flow against Anthropic (ported from OpenCodex `oauth/anthropic.ts`).
//!
//! Two redirects exist: a local listener on `localhost:54545` and the provider-hosted
//! "code display" page (`code#state` is shown for the user to paste, which also works where no
//! browser can reach this process). The same `redirect_uri` must be used for the authorize
//! request and the token exchange.

use super::consts;
use super::store::ClaudeCredentials;
use crate::AuthRouteConfig;
use crate::auth::default_client::create_raw_auth_client;
use crate::pkce::generate_pkce;
use base64::Engine;
use rand::RngCore;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;
use std::io;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

const TOKEN_TIMEOUT: Duration = Duration::from_secs(30);
const PROFILE_TIMEOUT: Duration = Duration::from_secs(10);
const REFRESH_PROFILE_TIMEOUT: Duration = Duration::from_secs(2);

/// A sign-in that has been started. The verifier never leaves this struct.
#[derive(Clone)]
pub struct Pending {
    pub url: String,
    pub state: String,
    pub redirect_uri: String,
    verifier: String,
}

impl std::fmt::Debug for Pending {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pending")
            .field("redirect_uri", &self.redirect_uri)
            .finish_non_exhaustive()
    }
}

pub fn begin(code_mode: bool) -> Pending {
    let pkce = generate_pkce();
    let mut raw = [0u8; 32];
    rand::rng().fill_bytes(&mut raw);
    let state = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw);
    let redirect_uri = if code_mode {
        consts::CODE_REDIRECT_URI.to_string()
    } else {
        consts::callback_redirect_uri()
    };
    let mut url = url::Url::parse(consts::AUTHORIZE_URL).expect("a constant URL parses");
    url.query_pairs_mut()
        .append_pair("code", "true")
        .append_pair("client_id", &consts::client_id())
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("scope", consts::SCOPES)
        .append_pair("code_challenge", &pkce.code_challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state);
    Pending {
        url: url.to_string(),
        state,
        redirect_uri,
        verifier: pkce.code_verifier,
    }
}

/// Accepts what a user can paste: `code#state`, the final redirect URL, or the bare code.
pub fn split_code_input(input: &str) -> (String, Option<String>) {
    let input = input.trim();
    if let Ok(url) = url::Url::parse(input) {
        let pair = |name: &str| {
            url.query_pairs()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.into_owned())
        };
        if let Some(code) = pair("code") {
            return (code, pair("state"));
        }
    }
    match input.split_once('#') {
        Some((code, state)) if !state.is_empty() => (code.to_string(), Some(state.to_string())),
        Some((code, _)) => (code.to_string(), None),
        None => (input.to_string(), None),
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<f64>,
    #[serde(default)]
    account: Option<Account>,
}

#[derive(Deserialize)]
struct Account {
    #[serde(default)]
    uuid: Option<String>,
}

fn credentials_from(response: TokenResponse, refresh_fallback: Option<&str>, now_ms: i64) -> io::Result<ClaudeCredentials> {
    let refresh = response
        .refresh_token
        .filter(|t| !t.is_empty())
        .or_else(|| refresh_fallback.map(str::to_string))
        .ok_or_else(|| io::Error::other("the token response carried no refresh token"))?;
    // A missing, non-finite or negative lifetime would stamp a bogus expiry.
    let seconds = response
        .expires_in
        .filter(|s| s.is_finite() && *s >= 0.0)
        .unwrap_or(3600.0);
    let expires_at_ms = now_ms
        .saturating_add((seconds * 1000.0) as i64)
        .saturating_sub(consts::EXPIRY_MARGIN_MS);
    Ok(ClaudeCredentials {
        access: response.access_token,
        refresh,
        expires_at_ms,
        account_uuid: response
            .account
            .and_then(|a| a.uuid)
            .filter(|u| !u.is_empty() && u.len() <= 128 && u.trim() == u),
        plan: None,
    })
}

async fn post_token(route: &AuthRouteConfig, body: Value) -> io::Result<TokenResponse> {
    let client = create_raw_auth_client(consts::TOKEN_URL, route).map_err(io::Error::other)?;
    let request = client
        .post(consts::TOKEN_URL)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .body(body.to_string())
        .send();
    let response = tokio::time::timeout(TOKEN_TIMEOUT, request)
        .await
        .map_err(|_| io::Error::other("the Anthropic token request timed out"))?
        .map_err(io::Error::other)?;
    let status = response.status();
    let text = response.text().await.map_err(io::Error::other)?;
    if !status.is_success() {
        // Only the OAuth error code, never the body: it may echo parts of the request.
        let code = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_string));
        return Err(io::Error::other(match code {
            Some(code) => format!("Anthropic OAuth HTTP {status}: {code}"),
            None => format!("Anthropic OAuth HTTP {status}"),
        }));
    }
    serde_json::from_str(&text).map_err(|_| io::Error::other("the token response was not valid JSON"))
}

/// Best effort: the account's plan, from the profile endpoint.
async fn fetch_plan(route: &AuthRouteConfig, access: &str, timeout: Duration) -> Option<String> {
    let client = create_raw_auth_client(consts::PROFILE_URL, route).ok()?;
    let request = client
        .get(consts::PROFILE_URL)
        .header("Authorization", format!("Bearer {access}"))
        .header("Accept", "application/json")
        .header("anthropic-beta", consts::OAUTH_BETA)
        .send();
    let response = tokio::time::timeout(timeout, request).await.ok()?.ok()?;
    if !response.status().is_success() {
        return None;
    }
    let data: Value = serde_json::from_str(&response.text().await.ok()?).ok()?;
    let organization = data.get("organization")?;
    ["rate_limit_tier", "organization_type"]
        .iter()
        .find_map(|key| organization.get(*key).and_then(Value::as_str))
        .map(|plan| plan.to_ascii_lowercase())
}

/// Exchanges what the user pasted (or the listener received) for credentials.
pub async fn exchange(
    pending: &Pending,
    input: &str,
    route: &AuthRouteConfig,
    now_ms: i64,
) -> io::Result<ClaudeCredentials> {
    let (code, state) = split_code_input(input);
    if code.is_empty() {
        return Err(io::Error::other("no authorization code was given"));
    }
    if let Some(state) = state.as_deref()
        && state != pending.state
    {
        return Err(io::Error::other("the sign-in state does not match; start again"));
    }
    let response = post_token(
        route,
        json!({
            "grant_type": "authorization_code",
            "client_id": consts::client_id(),
            "code": code,
            "state": state.unwrap_or_else(|| pending.state.clone()),
            "redirect_uri": pending.redirect_uri,
            "code_verifier": pending.verifier,
        }),
    )
    .await?;
    let mut credentials = credentials_from(response, None, now_ms)?;
    credentials.plan = fetch_plan(route, &credentials.access, PROFILE_TIMEOUT).await;
    Ok(credentials)
}

pub async fn refresh_credentials(
    credentials: &ClaudeCredentials,
    route: &AuthRouteConfig,
    now_ms: i64,
) -> io::Result<ClaudeCredentials> {
    let response = post_token(
        route,
        json!({
            "grant_type": "refresh_token",
            "client_id": consts::client_id(),
            "refresh_token": credentials.refresh,
        }),
    )
    .await?;
    let mut refreshed = credentials_from(response, Some(&credentials.refresh), now_ms)?;
    if refreshed.account_uuid.is_none() {
        refreshed.account_uuid.clone_from(&credentials.account_uuid);
    }
    refreshed.plan = fetch_plan(route, &refreshed.access, REFRESH_PROFILE_TIMEOUT)
        .await
        .or_else(|| credentials.plan.clone());
    Ok(refreshed)
}

/// Waits for the browser to be redirected to `http://localhost:54545/callback` and returns the
/// authorization code. `cancel` ends the wait early.
pub async fn wait_for_callback(
    state: String,
    timeout: Duration,
    cancel: Arc<AtomicBool>,
) -> io::Result<String> {
    tokio::task::spawn_blocking(move || {
        let server = tiny_http::Server::http(("127.0.0.1", consts::CALLBACK_PORT))
            .map_err(|err| io::Error::other(format!("cannot listen on the callback port: {err}")))?;
        let deadline = Instant::now() + timeout;
        loop {
            if cancel.load(Ordering::SeqCst) {
                return Err(io::Error::other("the sign-in was cancelled"));
            }
            if Instant::now() >= deadline {
                return Err(io::Error::other("the sign-in timed out"));
            }
            let Some(request) = server
                .recv_timeout(Duration::from_millis(250))
                .map_err(io::Error::other)?
            else {
                continue;
            };
            let url = url::Url::parse(&format!("http://localhost{}", request.url())).ok();
            let is_callback = url.as_ref().is_some_and(|u| u.path() == consts::CALLBACK_PATH);
            let pair = |name: &str| {
                url.as_ref().and_then(|u| {
                    u.query_pairs()
                        .find(|(k, _)| k == name)
                        .map(|(_, v)| v.into_owned())
                })
            };
            let page = |status: u16, body: &str| {
                let response = tiny_http::Response::from_string(body.to_string())
                    .with_status_code(status)
                    .with_header(
                        tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..])
                            .expect("a constant header parses"),
                    );
                let _ = request.respond(response);
            };
            if !is_callback {
                page(404, "Not found");
                continue;
            }
            if let Some(error) = pair("error") {
                page(400, "Sign-in failed. You can close this tab.");
                return Err(io::Error::other(format!("the sign-in was refused: {error}")));
            }
            match (pair("code"), pair("state")) {
                (Some(code), Some(got)) if got == state => {
                    page(200, "Signed in. You can close this tab and return to InoCode.");
                    return Ok(code);
                }
                _ => page(400, "The sign-in state did not match. Start again."),
            }
        }
    })
    .await
    .map_err(io::Error::other)?
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn the_authorize_url_carries_pkce_state_scope_and_the_chosen_redirect() {
        let pending = begin(false);
        let url = url::Url::parse(&pending.url).expect("url");
        let query: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(url.host_str(), Some("claude.ai"));
        assert_eq!(query["response_type"], "code");
        assert_eq!(query["code_challenge_method"], "S256");
        assert_eq!(query["redirect_uri"], "http://localhost:54545/callback");
        assert_eq!(query["state"], pending.state);
        assert!(query["scope"].contains("user:inference"));
        assert!(!query["client_id"].is_empty());
        assert!(!pending.url.contains(&pending.verifier));
        assert_eq!(begin(true).redirect_uri, consts::CODE_REDIRECT_URI);
        assert_ne!(begin(false).state, begin(false).state);
    }

    #[test]
    fn pasted_input_is_understood_in_all_three_shapes() {
        assert_eq!(split_code_input("abc#st"), ("abc".into(), Some("st".into())));
        assert_eq!(
            split_code_input("http://localhost:54545/callback?code=abc&state=st"),
            ("abc".into(), Some("st".into()))
        );
        assert_eq!(split_code_input("  abc  "), ("abc".into(), None));
        assert_eq!(split_code_input("abc#"), ("abc".into(), None));
    }

    #[test]
    fn token_responses_become_credentials_with_a_safe_expiry() {
        let response = |expires_in: Option<f64>, refresh: Option<&str>| TokenResponse {
            access_token: "a".into(),
            refresh_token: refresh.map(str::to_string),
            expires_in,
            account: Some(Account { uuid: Some("u-1".into()) }),
        };
        let now = 1_000_000;
        let ok = credentials_from(response(Some(3600.0), Some("r")), None, now).expect("ok");
        assert_eq!(ok.expires_at_ms, now + 3_600_000 - consts::EXPIRY_MARGIN_MS);
        assert_eq!(ok.account_uuid.as_deref(), Some("u-1"));
        // A bad lifetime falls back to an hour instead of an already-past or infinite expiry.
        let bad = credentials_from(response(Some(-5.0), Some("r")), None, now).expect("ok");
        assert_eq!(bad.expires_at_ms, now + 3_600_000 - consts::EXPIRY_MARGIN_MS);
        // A refresh response that omits the refresh token keeps the old one.
        let kept = credentials_from(response(Some(60.0), None), Some("old"), now).expect("ok");
        assert_eq!(kept.refresh, "old");
        assert!(credentials_from(response(Some(60.0), None), None, now).is_err());
    }
}
