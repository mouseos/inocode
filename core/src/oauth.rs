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

    /// Opens the store at `path`. A missing file is an empty store; a file
    /// that cannot be read or parsed is an error, because treating it as empty
    /// would make the next save discard every other provider's credential.
    pub fn load(path: PathBuf) -> Result<Self, String> {
        let entries = read_entries(&path)?;
        Ok(Self { path, entries })
    }

    pub fn credential(&self, provider: &str) -> Option<&Credential> {
        self.entries.get(provider)
    }

    /// The file this store persists to.
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Persists one provider's credential.
    ///
    /// The file is re-read first and only this provider's entry is replaced,
    /// so entries written by another process since this store was loaded (a
    /// login running next to a server that refreshed a token) are kept. The
    /// new content is written to a sibling file created owner-only where the
    /// platform supports it, then renamed over the store, so a crash never
    /// leaves a truncated file and the credential is never world-readable.
    pub fn save(&mut self, provider: &str, credential: Credential) -> Result<(), String> {
        self.update(provider, |stored| *stored = credential)
    }

    /// Edits one provider's credential as currently stored on disk and
    /// persists it the same way as [`Store::save`].
    pub fn update<F>(&mut self, provider: &str, edit: F) -> Result<(), String>
    where
        F: FnOnce(&mut Credential),
    {
        // Held from the read to the rename, so two processes saving at once
        // cannot both start from the same file and drop each other's entry
        // (reference `auth.store.lock`).
        let _lock = lock_file(&sibling(&self.path, ".lock"))?;
        let mut entries = read_entries(&self.path)?;
        let credential = entries
            .entry(provider.to_string())
            .or_insert_with(|| Credential {
                access: String::new(),
                refresh: String::new(),
                expires_at_ms: None,
                account_id: None,
                email: None,
                api_key: None,
                oauth_access_token: None,
            });
        edit(credential);
        let serialized = serde_json::to_vec_pretty(&entries).map_err(|e| e.to_string())?;
        write_replacing(&self.path, &serialized)?;
        self.entries = entries;
        Ok(())
    }
}

/// `path` with `suffix` appended to its file name.
fn sibling(path: &std::path::Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

fn open_lock_file(path: &std::path::Path) -> Result<std::fs::File, String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .map_err(|e| format!("cannot open the lock file {}: {e}", path.display()))
}

/// Takes the OS's exclusive lock on `path`; dropping the file releases it,
/// and so does the process ending, so a crash never leaves a stale lock.
fn lock_file(path: &std::path::Path) -> Result<std::fs::File, String> {
    let file = open_lock_file(path)?;
    file.lock()
        .map_err(|e| format!("cannot lock {}: {e}", path.display()))?;
    Ok(file)
}

/// Whether `credential` can be used at `now_ms` without a refresh.
pub(crate) fn is_fresh(credential: &Credential, now_ms: u64) -> bool {
    credential
        .expires_at_ms
        .is_none_or(|expires| now_ms < expires)
}

/// How long a request waits for another refresh of the same credential
/// (reference `OAUTH_REFRESH_LOCK_WAIT_MS`).
const REFRESH_LOCK_WAIT: Duration = Duration::from_secs(60);

/// Why [`refresh_stored`] failed.
#[derive(Debug)]
pub(crate) enum RefreshError {
    /// The store or its lock could not be used.
    Store(String),
    /// The token endpoint refused or could not be reached.
    Refresh(String),
}

/// Refreshes one provider's stored credential, at most once at a time across
/// this process and others.
///
/// Rotating refresh tokens are single-use: two requests that both found the
/// credential expired must not both spend it. The second one waits for the
/// refresh lock, finds the credential the first one stored, and uses it. The
/// result is the stored credential, so fields the token response does not
/// carry (account id, email) are kept.
pub(crate) async fn refresh_stored<F, Fut>(
    path: PathBuf,
    provider: &str,
    refresh: F,
) -> Result<Credential, RefreshError>
where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = Result<Credential, String>>,
{
    let lock = open_lock_file(&sibling(&path, &format!(".refresh.{provider}.lock")))
        .map_err(RefreshError::Store)?;
    let deadline = tokio::time::Instant::now() + REFRESH_LOCK_WAIT;
    loop {
        match lock.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(RefreshError::Store(format!(
                        "another refresh of {provider} did not finish in time"
                    )));
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(RefreshError::Store(format!(
                    "cannot lock the {provider} refresh: {error}"
                )))
            }
        }
    }
    let mut store = Store::load(path).map_err(RefreshError::Store)?;
    let current = store.credential(provider).cloned().ok_or_else(|| {
        RefreshError::Store(format!(
            "no stored credential for {provider}; run the login first"
        ))
    })?;
    if is_fresh(&current, now_ms()) {
        return Ok(current);
    }
    let new = refresh(current.refresh.clone())
        .await
        .map_err(RefreshError::Refresh)?;
    store
        .update(provider, |stored| {
            stored.access = new.access;
            stored.refresh = new.refresh;
            stored.expires_at_ms = new.expires_at_ms;
        })
        .map_err(RefreshError::Store)?;
    store.credential(provider).cloned().ok_or_else(|| {
        RefreshError::Store(format!("the {provider} credential vanished after saving"))
    })
}

fn read_entries(path: &std::path::Path) -> Result<BTreeMap<String, Credential>, String> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|e| format!("the credential store {} is not valid: {e}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(error) => Err(format!(
            "cannot read the credential store {}: {error}",
            path.display()
        )),
    }
}

fn write_replacing(path: &std::path::Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let mut temp = path.as_os_str().to_owned();
    temp.push(format!(".tmp-{}", std::process::id()));
    let temp = PathBuf::from(temp);
    // A leftover from an interrupted save may carry other permissions; the
    // file must be created fresh for the owner-only mode to apply.
    let _ = std::fs::remove_file(&temp);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let written = options.open(&temp).and_then(|mut file| {
        file.write_all(bytes)?;
        file.sync_all()
    });
    let result = written.and_then(|()| std::fs::rename(&temp, path));
    if let Err(error) = result {
        let _ = std::fs::remove_file(&temp);
        return Err(format!(
            "cannot write the credential store {}: {error}",
            path.display()
        ));
    }
    Ok(())
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
const GPT_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
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

/// Interprets a pasted code. A `code#state` paste is an authorization
/// response and must echo this login's state; a bare code belongs to the same
/// PKCE session and inherits it (reference `#waitForCallback` manual input).
fn pasted_callback(pasted: &str, expected_state: &str) -> Result<Callback, String> {
    let (code, state) = split_code_fragment(pasted.trim());
    if code.is_empty() {
        return Err("the pasted input contains no authorization code".to_string());
    }
    match state {
        Some(state) if state != expected_state => {
            Err("state mismatch - possible CSRF attack".to_string())
        }
        Some(state) => Ok(Callback { code, state }),
        None => Ok(Callback {
            code,
            state: expected_state.to_string(),
        }),
    }
}

/// Runs the localhost callback listeners (IPv4 and, where supported, IPv6 —
/// the advertised redirect is `localhost`, which browsers may resolve to
/// either, reference `loopbackBindHostnames`) and returns the browser
/// redirect carrying a code.
async fn await_callback(port: u16, path: &str, shutdown: Duration) -> Result<Callback, String> {
    let path = path.to_string();
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let v4 = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|e| format!("cannot listen on port {port}: {e}"))?;
    let mut listeners = vec![v4];
    if let Ok(v6) = tokio::net::TcpListener::bind(("::1", port)).await {
        listeners.push(v6);
    }
    // Dropping the set when this function returns stops every listener and
    // every connection still open, so nothing keeps the ports after the flow.
    let mut tasks = tokio::task::JoinSet::new();
    for listener in listeners {
        let sender = sender.clone();
        let path = path.clone();
        tasks.spawn(async move {
            // Each connection is served on its own, so a socket that never
            // sends a request (a browser preconnect) cannot hold up the
            // redirect arriving on another one.
            let mut connections = tokio::task::JoinSet::new();
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let sender = sender.clone();
                let path = path.clone();
                connections.spawn(async move {
                    if let Some(result) = handle_callback_connection(&mut socket, &path).await {
                        let _ = sender.send(result);
                    }
                });
            }
        });
    }
    drop(sender);
    let deadline = tokio::time::Instant::now() + shutdown;
    match tokio::time::timeout_at(deadline, receiver.recv()).await {
        Ok(Some(result)) => result,
        _ => Err("the browser approval did not arrive in time".to_string()),
    }
}

/// Handles one callback connection. `None` keeps waiting (wrong path, a
/// missing code, or a foreign error whose state does not match — the
/// reference only consumes the flow when the state matches).
async fn handle_callback_connection(
    socket: &mut tokio::net::TcpStream,
    path: &str,
) -> Option<Result<Callback, String>> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    // The request line and headers fit in a small fixed read for a browser GET.
    let mut buffer = vec![0u8; 4096];
    let read: usize = socket.read(&mut buffer).await.ok()?;
    let request_line = String::from_utf8_lossy(&buffer[..read]);
    let target = request_line
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or_default()
        .to_string();
    let denied = target.contains("error=");
    let body = if denied {
        "<html><body><h1>login denied</h1></body></html>"
    } else {
        "<html><body><h1>login complete</h1>You can close this window.</body></html>"
    };
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = socket.write_all(response.as_bytes()).await;
    let _ = socket.shutdown().await;
    if !target.contains(path) {
        return None; // favicon or probe: keep waiting
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
        return Some(Ok(Callback {
            code,
            state: state.unwrap_or_default(),
        }));
    }
    if denied {
        return Some(Err("the login was denied in the browser".to_string()));
    }
    None
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

/// One-shot client for the token endpoints: no redirect following (the
/// reference treats a redirect after token rotation as an unknown outcome)
/// and a bounded 30-second request window.
fn token_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| format!("cannot build the token HTTP client: {e}"))
}

/// Largest successful body accepted from the login endpoints (reference
/// `CHATGPT_SUCCESS_BODY_MAX_BYTES`); token and key payloads are far smaller.
const SUCCESS_BODY_MAX_BYTES: usize = 64 * 1024;

async fn read_success_body(mut response: reqwest::Response, what: &str) -> Result<Vec<u8>, String> {
    crate::llm::read_capped(&mut response, SUCCESS_BODY_MAX_BYTES)
        .await
        .map_err(|e| format!("reading the {what} failed: {e}"))?
        .ok_or_else(|| format!("the {what} exceeds {SUCCESS_BODY_MAX_BYTES} bytes"))
}

async fn post_token_json(
    client: &reqwest::Client,
    url: &str,
    body: Value,
) -> Result<Value, String> {
    send_token_request(
        client
            .post(url)
            .header("content-type", "application/json")
            .json(&body),
    )
    .await
}

async fn post_token_form(
    client: &reqwest::Client,
    url: &str,
    body: &[(&str, &str)],
) -> Result<Value, String> {
    let mut encoded = String::new();
    for (name, value) in body {
        if !encoded.is_empty() {
            encoded.push('&');
        }
        encoded.push_str(name);
        encoded.push('=');
        encoded.push_str(&urlencode(value));
    }
    send_token_request(
        client
            .post(url)
            .header("content-type", "application/x-www-form-urlencoded")
            .body(encoded),
    )
    .await
}

async fn send_token_request(request: reqwest::RequestBuilder) -> Result<Value, String> {
    let response = request
        .header("accept", "application/json")
        .send()
        .await
        .map_err(|e| format!("token request failed: {e}"))?;
    let status = response.status();
    if !status.is_success() {
        // Never echo the body: it can carry request material.
        return Err(format!("token request returned HTTP {}", status.as_u16()));
    }
    let bytes = read_success_body(response, "token response").await?;
    serde_json::from_slice(&bytes).map_err(|_| "token response was not valid JSON".to_string())
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

/// Milliseconds since the Unix epoch; a clock set before the epoch reads 0.
pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn expiry_from(expires_in: Option<u64>) -> Option<u64> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis() as u64;
    // Five-minute skew so a request started near expiry refreshes first. A
    // missing expires_in defaults to one hour so refresh semantics still
    // apply (reference `credsFrom`).
    let seconds = expires_in.unwrap_or(3600);
    Some(now.saturating_add(seconds.saturating_sub(300).saturating_mul(1000)))
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
    /// The port the callback listener must bind (may have fallen back from
    /// the preferred one).
    pub callback_port: u16,
}

/// Builds the authorize URL with a fresh PKCE pair. The `ClaudeLoginStart`
/// returned here must be passed to [`claude_login_finish`].
pub fn claude_login_start(endpoints: &FlowEndpoints, code_mode: bool) -> ClaudeLoginStart {
    let pkce = generate_pkce();
    let state = format!("inocode-{}", generate_state());
    let callback_port = reserve_callback_port(endpoints.callback_port);
    let redirect = match (code_mode, &endpoints.code_redirect_uri) {
        (true, Some(uri)) => uri.clone(),
        _ => {
            // The OAuth apps registered `localhost` as the redirect host; it
            // must be advertised verbatim (reference callback-server).
            format!(
                "http://localhost:{}{}",
                callback_port, endpoints.callback_path
            )
        }
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
        callback_port,
    }
}

/// Reserves the callback port: the preferred one is tried first and only a
/// busy port falls back to an ephemeral one (reference port fallback, which
/// is possible whenever no fixed redirect URI forces the port). The probe
/// listener is dropped so the flow can bind it again; the window between the
/// probe and the bind mirrors the reference's own fallback race.
fn reserve_callback_port(preferred: u16) -> u16 {
    match std::net::TcpListener::bind(("127.0.0.1", preferred)) {
        Ok(listener) => {
            let port = listener
                .local_addr()
                .map(|addr| addr.port())
                .unwrap_or(preferred);
            drop(listener);
            port
        }
        Err(_) => match std::net::TcpListener::bind(("127.0.0.1", 0)) {
            Ok(listener) => listener
                .local_addr()
                .map(|addr| addr.port())
                .unwrap_or(preferred),
            Err(_) => preferred,
        },
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
        Some(pasted) => pasted_callback(&pasted, &start.state)?,
        None => {
            await_callback(
                start.callback_port,
                &endpoints.callback_path,
                Duration::from_secs(300),
            )
            .await?
        }
    };
    // CSRF check: the callback must echo the state this login generated
    // (reference `stateMatches`).
    if callback.state != start.state {
        return Err("state mismatch - possible CSRF attack".to_string());
    }
    let client = token_client()?;
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
    let client = token_client()?;
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

/// Everything the ChatGPT/Codex browser login needs before the user approves.
pub struct GptLoginStart {
    pub url: String,
    pub redirect: String,
    pub verifier: String,
    pub state: String,
    /// The port the callback listener must bind (may have fallen back).
    pub callback_port: u16,
}

/// Builds the ChatGPT authorize URL with a fresh PKCE pair; pass the result
/// to [`gpt_login_finish`].
pub fn gpt_login_start(endpoints: &FlowEndpoints) -> GptLoginStart {
    let pkce = generate_pkce();
    let state = generate_state();
    let callback_port = reserve_callback_port(endpoints.callback_port);
    let redirect = format!(
        "http://localhost:{}{}",
        callback_port, endpoints.callback_path
    );
    let url = format!(
        "{}?response_type=code&client_id={}&redirect_uri={}&scope={}&code_challenge={}&code_challenge_method=S256&state={}&codex_cli_simplified_flow=true&originator=codex_cli_rs&id_token_add_organizations=true",
        endpoints.authorize_url,
        urlencode(GPT_CLIENT_ID),
        urlencode(&redirect),
        urlencode("openid profile email offline_access api.connectors.read api.connectors.invoke"),
        urlencode(&pkce.challenge),
        urlencode(&state),
    );
    GptLoginStart {
        url,
        redirect,
        verifier: pkce.verifier,
        state,
        callback_port,
    }
}

/// Exchanges the browser approval for tokens. The token exchange itself
/// carries no state (reference `exchangeToken`), but the callback must echo
/// this login's state.
pub async fn gpt_login_finish(
    endpoints: &FlowEndpoints,
    start: GptLoginStart,
    pasted: Option<String>,
) -> Result<Credential, String> {
    let callback = match pasted {
        Some(pasted) => pasted_callback(&pasted, &start.state)?,
        None => {
            await_callback(
                start.callback_port,
                &endpoints.callback_path,
                Duration::from_secs(300),
            )
            .await?
        }
    };
    if callback.state != start.state {
        return Err("state mismatch - possible CSRF attack".to_string());
    }
    let token = post_token_form(
        &token_client()?,
        &endpoints.token_url,
        &[
            ("grant_type", "authorization_code"),
            ("client_id", GPT_CLIENT_ID),
            ("code", &callback.code),
            ("redirect_uri", &start.redirect),
            ("code_verifier", &start.verifier),
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
    let client = token_client()?;
    let token = post_token_form(
        &client,
        &endpoints.token_url,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", GPT_CLIENT_ID),
            ("refresh_token", refresh),
        ],
    )
    .await?;
    credential_from_token(&token, Some(refresh))
}

fn generate_state() -> String {
    // The CSRF state must be unguessable: 16 bytes from the OS CSPRNG,
    // hex-encoded (reference `generateState`).
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes)
        .map_err(|e| e.to_string())
        .expect("OS CSPRNG");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
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
        .header("content-type", "application/x-www-form-urlencoded")
        .header("x-api-version", MUSE_API_VERSION)
        .header("user-agent", MUSE_USER_AGENT)
        .body(format!("client_id={}", urlencode(MUSE_CLIENT_ID)))
        .send()
        .await
        .map_err(|e| format!("device authorization request failed: {e}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!(
            "device authorization returned HTTP {}",
            status.as_u16()
        ));
    }
    let bytes = read_success_body(response, "device authorization response").await?;
    let payload: Value = serde_json::from_slice(&bytes)
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
    let now = now_ms();
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
            .header("content-type", "application/x-www-form-urlencoded")
            .header("x-api-version", MUSE_API_VERSION)
            .header("user-agent", MUSE_USER_AGENT)
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
        let max = if status.is_success() {
            SUCCESS_BODY_MAX_BYTES
        } else {
            crate::llm::ERROR_BODY_MAX_BYTES
        };
        let mut response = response;
        let bytes = crate::llm::read_capped(&mut response, max)
            .await
            .map_err(|e| format!("device token poll failed: {e}"))?
            .unwrap_or_default();
        let payload: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
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
        if now_ms() >= device.expires_at_ms {
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
        .header("user-agent", MUSE_USER_AGENT)
        .header("authorization", format!("Bearer {account_token}"))
        .header("content-type", "application/json")
        .header("x-api-version", MUSE_API_VERSION)
        .json(&json!({ "onboard": true }))
        .send()
        .await
        .map_err(|e| format!("the Muse key request failed: {e}"))?;
    let status = response.status();
    if !status.is_success() {
        // The body can contain the key itself; report the status only.
        return Err(format!(
            "the Muse key request returned HTTP {}",
            status.as_u16()
        ));
    }
    let bytes = read_success_body(response, "Muse key response").await?;
    let payload: Value = serde_json::from_slice(&bytes)
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
    // Every Muse login request is bounded (reference `REQUEST_TIMEOUT_MS`),
    // so a stalled endpoint cannot park the login indefinitely.
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| format!("cannot build the Muse HTTP client: {e}"))?;
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
    fn pasted_code_is_trimmed_and_must_echo_the_login_state() {
        let callback = pasted_callback(
            "abc#expected
",
            "expected",
        )
        .expect("matching state");
        assert_eq!(callback.code, "abc");
        assert_eq!(callback.state, "expected");
        let callback = pasted_callback(
            "  bare-code
",
            "expected",
        )
        .expect("bare code");
        assert_eq!(callback.code, "bare-code");
        assert_eq!(callback.state, "expected");
        assert!(pasted_callback("abc#forged", "expected").is_err());
        assert!(pasted_callback(
            "
", "expected"
        )
        .is_err());
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
        let mut store = Store::load(path.clone()).expect("load the test store");
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
        let reloaded = Store::load(path.clone()).expect("load the test store");
        let credential = reloaded.credential("claude-oauth").expect("entry");
        assert_eq!(credential.access, "super-secret-access");
        assert_eq!(credential.api_key.as_deref(), Some("LLM|secret"));
        std::fs::remove_file(&path).expect("remove the test store");
    }

    fn temp_store_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "inocode-store-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ))
    }

    fn sample_credential(access: &str) -> Credential {
        Credential {
            access: access.to_string(),
            refresh: String::new(),
            expires_at_ms: None,
            account_id: None,
            email: None,
            api_key: None,
            oauth_access_token: None,
        }
    }

    #[test]
    fn unreadable_store_is_an_error_and_is_never_overwritten() {
        let path = temp_store_path("corrupt");
        std::fs::write(&path, b"{not json").expect("write");
        assert!(Store::load(path.clone()).is_err());
        let mut store = Store {
            path: path.clone(),
            entries: BTreeMap::new(),
        };
        assert!(store.save("gpt-oauth", sample_credential("a")).is_err());
        assert_eq!(std::fs::read(&path).expect("read"), b"{not json");
        std::fs::remove_file(&path).expect("remove");
    }

    #[test]
    fn saving_keeps_entries_written_after_this_store_was_loaded() {
        let path = temp_store_path("merge");
        let mut early = Store::load(path.clone()).expect("load");
        let mut other = Store::load(path.clone()).expect("load");
        other
            .save("claude-oauth", sample_credential("refreshed"))
            .expect("save");
        early
            .save("gpt-oauth", sample_credential("logged-in"))
            .expect("save");
        let reloaded = Store::load(path.clone()).expect("load");
        assert_eq!(
            reloaded
                .credential("claude-oauth")
                .map(|c| c.access.as_str()),
            Some("refreshed")
        );
        assert_eq!(
            reloaded.credential("gpt-oauth").map(|c| c.access.as_str()),
            Some("logged-in")
        );
        let leftovers = std::fs::read_dir(std::env::temp_dir())
            .expect("list")
            .filter_map(Result::ok)
            .filter(|entry| {
                entry.file_name().to_string_lossy().starts_with(&format!(
                    "{}.tmp-",
                    path.file_name().unwrap().to_string_lossy()
                ))
            })
            .count();
        assert_eq!(leftovers, 0);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        std::fs::remove_file(&path).expect("remove");
    }

    #[test]
    fn concurrent_saves_from_separate_stores_keep_every_entry() {
        let path = temp_store_path("concurrent");
        let writers: Vec<_> = (0..8)
            .map(|writer| {
                let path = path.clone();
                std::thread::spawn(move || {
                    for round in 0..25 {
                        let mut store = Store::load(path.clone()).expect("load");
                        store
                            .save(
                                &format!("provider-{writer}"),
                                sample_credential(&format!("round-{round}")),
                            )
                            .expect("save");
                    }
                })
            })
            .collect();
        for writer in writers {
            writer.join().expect("writer thread");
        }
        let store = Store::load(path.clone()).expect("load");
        for writer in 0..8 {
            assert_eq!(
                store
                    .credential(&format!("provider-{writer}"))
                    .map(|c| c.access.as_str()),
                Some("round-24"),
                "provider-{writer}"
            );
        }
        std::fs::remove_file(&path).expect("remove");
        let _ = std::fs::remove_file(sibling(&path, ".lock"));
    }

    #[test]
    fn concurrent_refreshes_spend_the_refresh_token_once() {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_time()
            .build()
            .expect("runtime")
            .block_on(concurrent_refreshes());
    }

    async fn concurrent_refreshes() {
        let path = temp_store_path("refresh");
        let mut store = Store::load(path.clone()).expect("load");
        store
            .save(
                "gpt-oauth",
                Credential {
                    access: "expired-access".to_string(),
                    refresh: "rt-1".to_string(),
                    expires_at_ms: Some(1),
                    account_id: Some("acct-1".to_string()),
                    email: None,
                    api_key: None,
                    oauth_access_token: None,
                },
            )
            .expect("save");
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let tasks: Vec<_> = (0..4)
            .map(|_| {
                let path = path.clone();
                let calls = std::sync::Arc::clone(&calls);
                tokio::spawn(async move {
                    refresh_stored(path, "gpt-oauth", |refresh| async move {
                        assert_eq!(refresh, "rt-1", "only the stored token is spent");
                        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        Ok(Credential {
                            access: "fresh-access".to_string(),
                            refresh: "rt-2".to_string(),
                            expires_at_ms: Some(u64::MAX),
                            account_id: None,
                            email: None,
                            api_key: None,
                            oauth_access_token: None,
                        })
                    })
                    .await
                    .expect("refresh")
                })
            })
            .collect();
        for task in tasks {
            let credential = task.await.expect("task");
            assert_eq!(credential.access, "fresh-access");
            // The token response carries no account id; the stored one stays.
            assert_eq!(credential.account_id.as_deref(), Some("acct-1"));
        }
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        let stored = Store::load(path.clone()).expect("load");
        assert_eq!(
            stored.credential("gpt-oauth").map(|c| c.refresh.as_str()),
            Some("rt-2")
        );
        std::fs::remove_file(&path).expect("remove");
        let _ = std::fs::remove_file(sibling(&path, ".lock"));
        let _ = std::fs::remove_file(sibling(&path, ".refresh.gpt-oauth.lock"));
    }

    #[test]
    fn urldecode_and_urlencode_round_trip() {
        assert_eq!(urldecode("a%20b%2Fc+d"), "a b/c d");
        assert_eq!(urlencode("a b/c"), "a%20b%2Fc");
    }
}
