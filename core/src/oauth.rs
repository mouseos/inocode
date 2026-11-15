//! Interactive logins for subscription-backed providers and the credential
//! store they fill.
//!
//! Reference implementation: lidge-jun/opencodex `src/oauth/` (see
//! docs/evidence/oauth-providers.md for the recorded wire facts). Every
//! endpoint URL is injectable so tests can point a flow at a mock server; the
//! defaults are the production endpoints. Credentials are stored at
//! `~/.inocode/auth.json` and their `Debug` output is redacted.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// How a provider authenticates its HTTP requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AuthKind {
    /// A static key from `LOGFARE_API_KEY` (the original behaviour).
    #[default]
    ApiKey,
    /// Claude Pro/Max OAuth; refreshes before requests.
    ClaudeOauth,
    /// ChatGPT/Codex OAuth; refreshes before requests.
    GptOauth,
    /// Meta Muse Code subscription; the stored `LLM|` key is the bearer.
    MuseSubscription,
}

impl AuthKind {
    /// The `INOCODE_LLM_PROVIDER` value that selects it, or `None` for the
    /// static-key kinds handled by `LlmConfig` itself.
    pub fn from_provider_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "claude-oauth" => Some(Self::ClaudeOauth),
            "gpt-oauth" => Some(Self::GptOauth),
            "muse-code" => Some(Self::MuseSubscription),
            _ => None,
        }
    }

    /// The key under which this kind's credential is stored.
    pub fn store_name(self) -> &'static str {
        match self {
            Self::ApiKey => "api-key",
            Self::ClaudeOauth => "claude-oauth",
            Self::GptOauth => "gpt-oauth",
            Self::MuseSubscription => "muse-code",
        }
    }
}

/// The credential one provider account holds. Which fields matter depends on
/// the flow: OAuth flows fill `access`/`refresh`/`expires_at`; Muse fills
/// `api_key` (the request bearer) and keeps the Meta account token in
/// `oauth_access_token`, which must never be sent to the model API.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credential {
    pub access: String,
    pub refresh: String,
    pub expires_at_ms: Option<u64>,
    pub account_id: Option<String>,
    pub email: Option<String>,
    pub api_key: Option<String>,
    pub oauth_access_token: Option<String>,
}

impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credential")
            .field("access", &"[redacted]")
            .field(
                "refresh",
                if self.refresh.is_empty() {
                    &"none"
                } else {
                    &"[redacted]"
                },
            )
            .field("expires_at_ms", &self.expires_at_ms)
            .field("account_id", &self.account_id)
            .field("email", &self.email)
            .field(
                "api_key",
                match &self.api_key {
                    Some(_) => &"[redacted]",
                    None => &"none",
                },
            )
            .field(
                "oauth_access_token",
                match &self.oauth_access_token {
                    Some(_) => &"[redacted]",
                    None => &"none",
                },
            )
            .finish()
    }
}

/// `~/.inocode/auth.json` keyed by provider name. The manual `Debug` keeps
/// credentials out of any derived output.
pub struct Store {
    path: PathBuf,
    entries: BTreeMap<String, Credential>,
}

impl fmt::Debug for Store {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Store")
            .field("path", &self.path)
            .field("providers", &self.entries.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl Store {
    /// Opens (or creates) the default store at `~/.inocode/auth.json`.
    pub fn default_path() -> Result<PathBuf, String> {
        let home = std::env::var("USERPROFILE")
            .or_else(|_| std::env::var("HOME"))
            .map_err(|_| "neither USERPROFILE nor HOME is set".to_string())?;
        Ok(PathBuf::from(home).join(".inocode").join("auth.json"))
    }

    pub fn load(path: PathBuf) -> Self {
        let entries = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<BTreeMap<String, Credential>>(&bytes).ok())
            .unwrap_or_default();
        Self { path, entries }
    }

    pub fn credential(&self, provider: &str) -> Option<&Credential> {
        self.entries.get(provider)
    }

    /// The file this store persists to.
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Persists one provider's credential, creating the file with owner-only
    /// permissions where the platform supports it.
    pub fn save(&mut self, provider: &str, credential: Credential) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        self.entries.insert(provider.to_string(), credential);
        let serialized = serde_json::to_vec_pretty(&self.entries).map_err(|e| e.to_string())?;
        std::fs::write(&self.path, &serialized).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }

    /// Replaces one provider's credential and persists it.
    pub fn update<F>(&mut self, provider: &str, edit: F) -> Result<(), String>
    where
        F: FnOnce(&mut Credential),
    {
        let mut credential = self
            .entries
            .get(provider)
            .cloned()
            .unwrap_or_else(|| Credential {
                access: String::new(),
                refresh: String::new(),
                expires_at_ms: None,
                account_id: None,
                email: None,
                api_key: None,
                oauth_access_token: None,
            });
        edit(&mut credential);
        self.save(provider, credential)
    }
}

/// PKCE verifier/challenge pair (RFC 7636 `S256`).
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

pub fn generate_pkce() -> Pkce {
    // RFC 7636 requires an unpredictable verifier: the entropy must come from
    // the OS CSPRNG, not from time- or address-derived hashes.
    let mut verifier_bytes = [0u8; 32];
    getrandom::getrandom(&mut verifier_bytes)
        .map_err(|e| e.to_string())
        .expect("OS CSPRNG");
    let verifier = URL_SAFE_NO_PAD.encode(verifier_bytes);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    Pkce {
        verifier,
        challenge,
    }
}

/// One grant's endpoints; defaults point at the production hosts, tests
/// override them with local mock servers.
#[derive(Debug, Clone)]
pub struct FlowEndpoints {
    pub authorize_url: String,
    pub token_url: String,
    pub callback_port: u16,
    pub callback_path: String,
    /// Used by code-display modes where no local port can receive the browser.
    pub code_redirect_uri: Option<String>,
}

impl FlowEndpoints {
    pub fn anthropic() -> Self {
        Self {
            authorize_url: "https://claude.ai/oauth/authorize".to_string(),
            token_url: "https://api.anthropic.com/v1/oauth/token".to_string(),
            callback_port: 54545,
            callback_path: "/callback".to_string(),
            code_redirect_uri: Some("https://platform.claude.com/oauth/code/callback".to_string()),
        }
    }

    pub fn chatgpt() -> Self {
        Self {
            authorize_url: "https://auth.openai.com/oauth/authorize".to_string(),
            token_url: "https://auth.openai.com/oauth/token".to_string(),
            callback_port: 1455,
            callback_path: "/auth/callback".to_string(),
            code_redirect_uri: None,
        }
    }
}

const ANTHROPIC_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const ANTHROPIC_SCOPES: &str = "org:create_api_key user:profile user:inference";
/// Anthropic's OAuth beta opt-in; requests with an OAuth bearer are rejected
/// without it.
pub const CLAUDE_CODE_BETA: &str = "claude-code-20250219,oauth-2025-04-20";
/// Identity block Claude Code sends ahead of the conversation; the OAuth
/// backend expects it (reference implementation `src/claude/anthropic.ts`).
pub const CLAUDE_CODE_SYSTEM_INSTRUCTION: &str =
    "You are a Claude agent, built on Anthropic's Claude Agent SDK.";
/// Public client id of Meta's own Muse Code CLI; published in the device URL.
const MUSE_CLIENT_ID: &str = "1031625952748946";
const MUSE_DEVICE_AUTH_URL: &str = "https://auth.meta.com/oidc/device/authorization/";
const MUSE_DEVICE_TOKEN_URL: &str = "https://auth.meta.com/oidc/device/token/";
const MUSE_KEY_URL: &str = "https://api.meta.ai/muse-code/key";
const MUSE_DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
/// Meta's Model API clients send these two headers on every request.
pub const MUSE_API_VERSION: &str = "1.0.0";
pub const MUSE_USER_AGENT: &str = "muse-build/1.3.0";

/// Splits a pasted `code#state` (or a plain code) per the Anthropic code mode.
pub fn split_code_fragment(code: &str) -> (String, Option<String>) {
    match code.split_once('#') {
        Some((code, state)) if !state.is_empty() => (code.to_string(), Some(state.to_string())),
        _ => (code.to_string(), None),
    }
}

/// Decodes a JWT payload segment for routing claims; not an authenticity proof.
pub fn jwt_payload(token: &str) -> Option<Value> {
    let mut parts = token.split('.');
    let _header = parts.next()?;
    let payload = parts.next()?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// The ChatGPT account id from the id/access token claims.
pub fn chatgpt_account_id(token: &str) -> Option<String> {
    let payload = jwt_payload(token)?;
    let claims = payload.get("https://api.openai.com/auth")?;
    let id = claims.get("chatgpt_account_id")?.as_str()?;
    let trimmed = id.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

struct Callback {
    code: String,
    state: String,
}

/// Runs a one-shot localhost callback listener and returns the browser redirect.
async fn await_callback(port: u16, path: &str, shutdown: Duration) -> Result<Callback, String> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|e| format!("cannot listen on port {port}: {e}"))?;
    let deadline = tokio::time::Instant::now() + shutdown;
    loop {
        let (mut socket, _) = tokio::time::timeout_at(deadline, listener.accept())
            .await
            .map_err(|_| "the browser approval did not arrive in time".to_string())?
            .map_err(|error| format!("callback accept failed: {error}"))?;
        let mut request_line = String::new();
        use tokio::io::AsyncReadExt;
        // The request line and headers fit in a small fixed read for a browser GET.
        let mut buffer = vec![0u8; 4096];
        let read: usize = socket
            .read(&mut buffer)
            .await
            .map_err(|e| format!("callback read failed: {e}"))?;
        request_line.push_str(&String::from_utf8_lossy(&buffer[..read]));
        let target = request_line
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or_default()
            .to_string();
        let body = if target.contains("error=") {
            "<html><body><h1>login denied</h1></body></html>"
        } else {
            "<html><body><h1>login complete</h1>You can close this window.</body></html>"
        };
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        use tokio::io::AsyncWriteExt;
        let _ = socket.write_all(response.as_bytes()).await;
        let _ = socket.shutdown().await;
        let path_start = target.find(path);
        if path_start.is_none() {
            continue; // favicon or probe: keep waiting
        }
        let query = target.split_once('?').map(|(_, q)| q).unwrap_or_default();
        let mut code = None;
        let mut state = None;
        for pair in query.split('&') {
            match pair.split_once('=') {
                Some(("code", value)) => code = Some(urldecode(value)),
                Some(("state", value)) => state = Some(urldecode(value)),
                _ => {}
            }
        }
        if let Some(code) = code {
            return Ok(Callback {
                code,
                state: state.unwrap_or_default(),
            });
        }
        if target.contains("error=") {
            return Err("the login was denied in the browser".to_string());
        }
    }
}

fn urldecode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
                if let Ok(byte) = u8::from_str_radix(hex, 16) {
                    out.push(byte);
                    index += 3;
                    continue;
                }
                out.push(bytes[index]);
                index += 1;
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

async fn post_token_json(
    client: &reqwest::Client,
    url: &str,
    body: Value,
) -> Result<Value, String> {
    let response = client
        .post(url)
        .header("content-type", "application/json")
        .header("accept", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("token request failed: {e}"))?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        // Never echo the body: it can carry request material.
        return Err(format!("token request returned HTTP {}", status.as_u16()));
    }
    serde_json::from_str(&text).map_err(|_| "token response was not valid JSON".to_string())
}

async fn post_token_form(
    client: &reqwest::Client,
    url: &str,
    body: &[(&str, &str)],
) -> Result<Value, String> {
    let response = client
        .post(url)
        .header("content-type", "application/x-www-form-urlencoded")
        .header("accept", "application/json")
        .body({
            let mut encoded = String::new();
            for (name, value) in body {
                if !encoded.is_empty() {
                    encoded.push('&');
                }
                encoded.push_str(name);
                encoded.push('=');
                encoded.push_str(&urlencode(value));
            }
            encoded
        })
        .send()
        .await
        .map_err(|e| format!("token request failed: {e}"))?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!("token request returned HTTP {}", status.as_u16()));
    }
    serde_json::from_str(&text).map_err(|_| "token response was not valid JSON".to_string())
}

fn urlencode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(*byte as char)
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

fn expiry_from(expires_in: Option<u64>) -> Option<u64> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis() as u64;
    // Five-minute skew so a request started near expiry refreshes first.
    Some(now.saturating_add(expires_in?.saturating_sub(300).saturating_mul(1000)))
}

fn credential_from_token(
    token: &Value,
    fallback_refresh: Option<&str>,
) -> Result<Credential, String> {
    let access = token
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or("token response is missing the access token")?
        .to_string();
    Ok(Credential {
        access,
        refresh: token
            .get("refresh_token")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| fallback_refresh.map(str::to_string))
            .unwrap_or_default(),
        expires_at_ms: expiry_from(token.get("expires_in").and_then(Value::as_u64)),
        account_id: None,
        email: None,
        api_key: None,
        oauth_access_token: None,
    })
}

/// Everything the browser login needs before the user approves.
pub struct ClaudeLoginStart {
    /// The URL to open in a browser.
    pub url: String,
    /// The `redirect_uri` the token exchange must repeat.
    pub redirect: String,
    pub verifier: String,
    pub state: String,
}

/// Builds the authorize URL with a fresh PKCE pair. The `ClaudeLoginStart`
/// returned here must be passed to [`claude_login_finish`].
pub fn claude_login_start(endpoints: &FlowEndpoints, code_mode: bool) -> ClaudeLoginStart {
    let pkce = generate_pkce();
    let state = format!("inocode-{}", generate_state());
    let redirect = match (code_mode, &endpoints.code_redirect_uri) {
        (true, Some(uri)) => uri.clone(),
        _ => format!(
            "http://127.0.0.1:{}{}",
            endpoints.callback_port, endpoints.callback_path
        ),
    };
    let url = format!(
        "{}?code=true&client_id={}&response_type=code&redirect_uri={}&scope={}&code_challenge={}&code_challenge_method=S256&state={}",
        endpoints.authorize_url,
        urlencode(ANTHROPIC_CLIENT_ID),
        urlencode(&redirect),
        urlencode(ANTHROPIC_SCOPES),
        urlencode(&pkce.challenge),
        urlencode(&state),
    );
    ClaudeLoginStart {
        url,
        redirect,
        verifier: pkce.verifier,
        state,
    }
}

/// Exchanges the approval for tokens. In code-display mode the user pasted the
/// `code#state` Anthropic showed, passed as `pasted`; in browser mode
/// (`pasted: None`) a local callback listener receives the redirect.
pub async fn claude_login_finish(
    endpoints: &FlowEndpoints,
    start: ClaudeLoginStart,
    pasted: Option<String>,
) -> Result<Credential, String> {
    let callback = match pasted {
        Some(pasted) => {
            let (code, fragment_state) = split_code_fragment(pasted.trim());
            Callback {
                code,
                state: fragment_state.unwrap_or(start.state),
            }
        }
        None => {
            await_callback(
                endpoints.callback_port,
                &endpoints.callback_path,
                Duration::from_secs(600),
            )
            .await?
        }
    };
    let client = reqwest::Client::new();
    let token = post_token_json(
        &client,
        &endpoints.token_url,
        json!({
            "grant_type": "authorization_code",
            "client_id": ANTHROPIC_CLIENT_ID,
            "code": callback.code,
            "state": callback.state,
            "redirect_uri": start.redirect,
            "code_verifier": start.verifier,
        }),
    )
    .await?;
    let mut credential = credential_from_token(&token, None)?;
    credential.email = token
        .get("account")
        .and_then(|account| account.get("email_address"))
        .and_then(Value::as_str)
        .map(str::to_string);
    credential.account_id = token
        .get("account")
        .and_then(|account| account.get("uuid"))
        .and_then(Value::as_str)
        .map(str::to_string);
    Ok(credential)
}

/// Refreshes a Claude OAuth credential.
pub async fn refresh_claude(
    endpoints: &FlowEndpoints,
    refresh: &str,
) -> Result<Credential, String> {
    let client = reqwest::Client::new();
    let token = post_token_json(
        &client,
        &endpoints.token_url,
        json!({
            "grant_type": "refresh_token",
            "client_id": ANTHROPIC_CLIENT_ID,
            "refresh_token": refresh,
        }),
    )
    .await?;
    credential_from_token(&token, Some(refresh))
}

/// Runs the ChatGPT/Codex browser login (PKCE, localhost callback).
pub async fn login_gpt(endpoints: &FlowEndpoints) -> Result<Credential, String> {
    let client = reqwest::Client::new();
    let pkce = generate_pkce();
    let state = generate_state();
    let redirect = format!(
        "http://127.0.0.1:{}{}",
        endpoints.callback_port, endpoints.callback_path
    );
    let url = format!(
        "{}?response_type=code&client_id=app_EMoamEEZ73f0CkXaXp7hrann&redirect_uri={}&scope={}&code_challenge={}&code_challenge_method=S256&state={}&codex_cli_simplified_flow=true&originator=codex_cli_rs&id_token_add_organizations=true",
        endpoints.authorize_url,
        urlencode(&redirect),
        urlencode("openid profile email offline_access api.connectors.read api.connectors.invoke"),
        urlencode(&pkce.challenge),
        urlencode(&state),
    );
    println!("Open this URL and approve access:\n{url}");
    let callback = await_callback(
        endpoints.callback_port,
        &endpoints.callback_path,
        Duration::from_secs(600),
    )
    .await?;
    let token = post_token_form(
        &client,
        &endpoints.token_url,
        &[
            ("grant_type", "authorization_code"),
            ("client_id", "app_EMoamEEZ73f0CkXaXp7hrann"),
            ("code", &callback.code),
            ("redirect_uri", &redirect),
            ("code_verifier", &pkce.verifier),
        ],
    )
    .await?;
    let mut credential = credential_from_token(&token, None)?;
    let id_token = token.get("id_token").and_then(Value::as_str).unwrap_or("");
    credential.account_id =
        chatgpt_account_id(id_token).or_else(|| chatgpt_account_id(&credential.access));
    credential.email = token
        .get("email")
        .and_then(Value::as_str)
        .map(str::to_string);
    Ok(credential)
}

/// Refreshes a ChatGPT OAuth credential.
pub async fn refresh_gpt(endpoints: &FlowEndpoints, refresh: &str) -> Result<Credential, String> {
    let client = reqwest::Client::new();
    let token = post_token_form(
        &client,
        &endpoints.token_url,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", "app_EMoamEEZ73f0CkXaXp7hrann"),
            ("refresh_token", refresh),
        ],
    )
    .await?;
    credential_from_token(&token, Some(refresh))
}

fn generate_state() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!("{:x}{:x}", now.as_nanos(), std::process::id())
}

/// Muse Code device-approval state shown to the user.
pub struct MuseDevice {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub interval: Duration,
    pub expires_at_ms: u64,
}

pub struct MuseEndpoints {
    pub device_authorization_url: String,
    pub device_token_url: String,
    pub key_url: String,
}

impl Default for MuseEndpoints {
    fn default() -> Self {
        Self {
            device_authorization_url: MUSE_DEVICE_AUTH_URL.to_string(),
            device_token_url: MUSE_DEVICE_TOKEN_URL.to_string(),
            key_url: MUSE_KEY_URL.to_string(),
        }
    }
}

/// Step 1: request a device/user code pair.
pub async fn muse_device_authorization(
    client: &reqwest::Client,
    endpoints: &MuseEndpoints,
) -> Result<MuseDevice, String> {
    let response = client
        .post(&endpoints.device_authorization_url)
        .header("accept", "application/json")
        .header("x-api-version", MUSE_API_VERSION)
        .body(format!("client_id={}", urlencode(MUSE_CLIENT_ID)))
        .send()
        .await
        .map_err(|e| format!("device authorization request failed: {e}"))?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!(
            "device authorization returned HTTP {}",
            status.as_u16()
        ));
    }
    let payload: Value = serde_json::from_str(&text)
        .map_err(|_| "device authorization response was not valid JSON".to_string())?;
    let device_code = payload
        .get("device_code")
        .and_then(Value::as_str)
        .ok_or("device authorization response is missing device_code")?
        .to_string();
    let user_code = payload
        .get("user_code")
        .and_then(Value::as_str)
        .ok_or("device authorization response is missing user_code")?
        .to_string();
    let verification_uri = payload
        .get("verification_uri")
        .and_then(Value::as_str)
        .unwrap_or("https://auth.meta.com/oauth/device/")
        .to_string();
    let interval = Duration::from_secs(
        payload
            .get("interval")
            .and_then(Value::as_u64)
            .unwrap_or(5)
            .max(1),
    );
    let expires = payload
        .get("expires_in")
        .and_then(Value::as_u64)
        .unwrap_or(900);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    Ok(MuseDevice {
        device_code,
        user_code,
        verification_uri,
        interval,
        expires_at_ms: now.saturating_add(expires.saturating_mul(1000)),
    })
}

/// Step 2: poll until approval; returns the Meta account access token.
pub async fn muse_device_token(
    client: &reqwest::Client,
    endpoints: &MuseEndpoints,
    device: &MuseDevice,
) -> Result<String, String> {
    let mut interval = device.interval;
    loop {
        let response = client
            .post(&endpoints.device_token_url)
            .header("accept", "application/json")
            .header("x-api-version", MUSE_API_VERSION)
            .body(format!(
                "client_id={}&device_code={}&grant_type={}",
                urlencode(MUSE_CLIENT_ID),
                urlencode(&device.device_code),
                urlencode(MUSE_DEVICE_GRANT),
            ))
            .send()
            .await
            .map_err(|e| format!("device token poll failed: {e}"))?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        let payload: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if status.is_success() {
            return payload
                .get("access_token")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(|value| value.to_string())
                .ok_or_else(|| "device token response is missing the access token".to_string());
        }
        let code = payload
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        match code.as_str() {
            "access_denied" => return Err("the login was denied in the browser".to_string()),
            "expired_token" => {
                return Err("the device code expired; start the login again".to_string())
            }
            "slow_down" => interval += Duration::from_secs(5),
            "authorization_pending" => {}
            other => {
                return Err(format!(
                    "device token poll failed: HTTP {} ({other})",
                    status.as_u16()
                ))
            }
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        if now >= device.expires_at_ms {
            return Err("the device authorization expired before approval".to_string());
        }
        tokio::time::sleep(interval.min(Duration::from_secs(60))).await;
    }
}

/// Step 3: mint the subscription key from the account token.
pub async fn muse_mint_key(
    client: &reqwest::Client,
    endpoints: &MuseEndpoints,
    account_token: &str,
) -> Result<Credential, String> {
    let response = client
        .post(&endpoints.key_url)
        .header("accept", "application/json")
        .header("authorization", format!("Bearer {account_token}"))
        .header("content-type", "application/json")
        .header("x-api-version", MUSE_API_VERSION)
        .json(&json!({ "onboard": true }))
        .send()
        .await
        .map_err(|e| format!("the Muse key request failed: {e}"))?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        // The body can contain the key itself; report the status only.
        return Err(format!(
            "the Muse key request returned HTTP {}",
            status.as_u16()
        ));
    }
    let payload: Value = serde_json::from_str(&text)
        .map_err(|_| "the Muse key response was not valid JSON".to_string())?;
    let api_key = payload
        .get("api_key")
        .and_then(Value::as_str)
        .filter(|value| value.starts_with("LLM|"))
        .ok_or("the Muse key response did not contain a usable api_key")?
        .to_string();
    Ok(Credential {
        access: String::new(),
        refresh: String::new(),
        expires_at_ms: None,
        account_id: payload
            .get("user_id")
            .and_then(Value::as_str)
            .map(str::to_string),
        email: payload
            .get("user_email")
            .and_then(Value::as_str)
            .map(str::to_string),
        api_key: Some(api_key),
        oauth_access_token: Some(account_token.to_string()),
    })
}

/// Full Muse Code login: device approval, then the subscription key mint.
pub async fn login_muse(endpoints: &MuseEndpoints) -> Result<Credential, String> {
    let client = reqwest::Client::new();
    let device = muse_device_authorization(&client, endpoints).await?;
    println!(
        "Open {} and approve the code: {}",
        device.verification_uri, device.user_code
    );
    let account_token = muse_device_token(&client, endpoints, &device).await?;
    muse_mint_key(&client, endpoints, &account_token).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_is_the_sha256_of_the_verifier() {
        let pkce = generate_pkce();
        assert_eq!(pkce.verifier.len(), 43); // 32 bytes, base64url unpadded
        let expected = URL_SAFE_NO_PAD.encode(Sha256::digest(pkce.verifier.as_bytes()));
        assert_eq!(pkce.challenge, expected);
    }

    #[test]
    fn code_fragment_splitting_matches_the_anthropic_code_mode() {
        let (code, state) = split_code_fragment("abc123#state456");
        assert_eq!(code, "abc123");
        assert_eq!(state.as_deref(), Some("state456"));
        let (code, state) = split_code_fragment("plain");
        assert_eq!(code, "plain");
        assert!(state.is_none());
    }

    #[test]
    fn jwt_claims_yield_the_chatgpt_account_id() {
        // payload: {"https://api.openai.com/auth":{"chatgpt_account_id":"acc-1"}}
        let payload = r#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acc-1"}}"#;
        let encoded = URL_SAFE_NO_PAD.encode(payload.as_bytes());
        let token = format!("h.{encoded}.s");
        assert_eq!(chatgpt_account_id(&token).as_deref(), Some("acc-1"));
        assert_eq!(chatgpt_account_id("not-a-jwt"), None);
    }

    #[test]
    fn store_redacts_credentials_in_debug_output() {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "inocode-store-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let mut store = Store::load(path.clone());
        store
            .save(
                "claude-oauth",
                Credential {
                    access: "super-secret-access".to_string(),
                    refresh: "super-secret-refresh".to_string(),
                    expires_at_ms: Some(123),
                    account_id: Some("acc".to_string()),
                    email: Some("user@example.test".to_string()),
                    api_key: Some("LLM|secret".to_string()),
                    oauth_access_token: Some("meta-token".to_string()),
                },
            )
            .expect("save");
        let rendered = format!("{store:?}");
        assert!(!rendered.contains("super-secret"));
        assert!(!rendered.contains("LLM|secret"));
        assert!(rendered.contains("claude-oauth"));

        // A reload round-trips the values without leaking them in Debug.
        let reloaded = Store::load(path.clone());
        let credential = reloaded.credential("claude-oauth").expect("entry");
        assert_eq!(credential.access, "super-secret-access");
        assert_eq!(credential.api_key.as_deref(), Some("LLM|secret"));
        std::fs::remove_file(&path).expect("remove the test store");
    }

    #[test]
    fn urldecode_and_urlencode_round_trip() {
        assert_eq!(urldecode("a%20b%2Fc+d"), "a b/c d");
        assert_eq!(urlencode("a b/c"), "a%20b%2Fc");
    }
}
