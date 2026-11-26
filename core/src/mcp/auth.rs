//! Signing in to MCP servers reached over HTTP with OAuth, as the
//! authorization part of each revision describes it: the server's Protected
//! Resource Metadata (RFC 9728), or for a server of 2025-03-26 the metadata
//! at its own origin; the authorization server's metadata (RFC 8414 and
//! OpenID Connect discovery); a client id from `mcp.json`, a Client ID
//! Metadata Document or Dynamic Client Registration (RFC 7591); and the
//! authorization code with PKCE, the `resource` parameter (RFC 8707) and
//! the `iss` check (RFC 9207). The person starts each sign-in; the model
//! never does. Design and sources: docs/evidence/mcp-client.md.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use base64::Engine;
use reqwest::header::{HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// The name the sign-ins are kept under in the credential store.
const PROVIDER: &str = "mcp";
const CALLBACK_PATH: &str = "/callback";
/// How long a sign-in waits for the browser to come back.
const LOGIN_WAIT: Duration = Duration::from_secs(300);
/// How long a sign-in is kept after it started, so its result can be read.
const LOGIN_KEPT: Duration = Duration::from_secs(25 * 60);
/// A token is refreshed when it expires within this many milliseconds.
const EARLY_MS: u64 = 60_000;
/// Bytes of a metadata, registration or token answer read at most.
const BODY_MAX: usize = 64 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a refresh waits for another process refreshing the same sign-in.
const REFRESH_LOCK_WAIT: Duration = Duration::from_secs(60);
/// The revision whose servers have no resource metadata and keep the
/// authorization server's metadata, or its default endpoints, at their own
/// origin.
const LEGACY_VERSION: &str = "2025-03-26";

/// A server's `oauth` settings in `mcp.json`. None is needed: without them
/// inocode registers itself where the authorization server allows it.
#[derive(Deserialize, Clone, Default)]
#[serde(deny_unknown_fields)]
pub(super) struct Settings {
    /// A client the person registered with the authorization server.
    #[serde(default)]
    client_id: Option<String>,
    #[serde(default)]
    client_secret: Option<String>,
    /// The issuer `client_id` belongs to; it is not used with another one.
    #[serde(default)]
    authorization_server: Option<String>,
    /// The HTTPS URL of a Client ID Metadata Document the person hosts,
    /// used as the client id where the authorization server takes such ids.
    #[serde(default)]
    client_metadata_url: Option<String>,
    /// The port of the redirect to this machine, for a client registered
    /// with a fixed redirect URI; any free port otherwise.
    #[serde(default)]
    callback_port: Option<u16>,
}

/// One sign-in as stored. It also reads as a [`crate::oauth::Credential`]
/// (`access`, `refresh`, `expires_at_ms`), as every stored value must.
#[derive(Serialize, Deserialize, Clone)]
struct Grant {
    access: String,
    refresh: String,
    expires_at_ms: Option<u64>,
    /// The authorization server that issued the tokens and the client id.
    issuer: String,
    /// The `resource` the tokens were asked for.
    resource: String,
    token_endpoint: String,
    client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_secret: Option<String>,
    client_auth: ClientAuth,
    /// The scopes asked for, space-separated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    scope: Option<String>,
}

/// How the client proves itself at the token endpoint.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ClientAuth {
    None,
    ClientSecretBasic,
    ClientSecretPost,
}

/// A client id, with its secret and how it is sent.
struct Client {
    id: String,
    secret: Option<String>,
    auth: ClientAuth,
}

/// The parameters of a server's `Bearer` challenge (`WWW-Authenticate`).
#[derive(Clone, Default, Debug, PartialEq)]
pub(super) struct Challenge {
    resource_metadata: Option<String>,
    scope: Option<String>,
    error: Option<String>,
}

/// What this process knows of the servers' sign-ins, and the sign-ins in
/// progress. A sign-in waits on a local port and holds a PKCE verifier, so
/// it does not outlive the process anyway.
#[derive(Default)]
struct Registry {
    /// By sign-in store and MCP URL.
    servers: HashMap<(PathBuf, String), Known>,
    logins: HashMap<String, Login>,
}

#[derive(Default)]
struct Known {
    /// The server's last challenge, whose scopes a sign-in asks for.
    challenge: Option<Challenge>,
    /// Why the server needs a sign-in, once it refused the requests.
    needs: Option<String>,
}

struct Login {
    server: String,
    key: (PathBuf, String),
    started: Instant,
    url: String,
    state: Arc<Mutex<LoginState>>,
    task: tokio::task::AbortHandle,
}

#[derive(Clone)]
enum LoginState {
    Waiting,
    Done,
    Failed(String),
    Cancelled,
}

fn registry() -> std::sync::MutexGuard<'static, Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    let mut registry = REGISTRY
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    registry
        .logins
        .retain(|_, login| login.started.elapsed() < LOGIN_KEPT);
    registry
}

fn key(store: &Path, url: &reqwest::Url) -> (PathBuf, String) {
    (store.to_path_buf(), url.to_string())
}

/// Records why the server at `url` needs a sign-in.
fn needs(store: &Path, url: &reqwest::Url, why: String) {
    registry().servers.entry(key(store, url)).or_default().needs = Some(why);
}

fn client() -> Result<reqwest::Client, String> {
    // Redirects are not followed: metadata and tokens come from the URLs
    // the specifications name, not from wherever those send.
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|error| format!("cannot make the HTTP client: {error}"))
}

/// Whether a URL may be used for OAuth: HTTPS, or HTTP to this machine.
fn allowed(url: &reqwest::Url) -> bool {
    match url.scheme() {
        "https" => true,
        "http" => url.host_str().is_some_and(|host| {
            let host = host.trim_start_matches('[').trim_end_matches(']');
            host.eq_ignore_ascii_case("localhost")
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        }),
        _ => false,
    }
}

fn checked(text: &str, what: &str) -> Result<reqwest::Url, String> {
    let url = reqwest::Url::parse(text)
        .map_err(|error| format!("{what} {text:?} is not a URL: {error}"))?;
    if !allowed(&url) {
        return Err(format!("{what} {url} is neither HTTPS nor on this machine"));
    }
    Ok(url)
}

/// Whether the resource `resource` names the MCP endpoint `url`: the same
/// origin, and a path that is the endpoint's or one of its parents'.
fn covers(resource: &reqwest::Url, url: &reqwest::Url) -> bool {
    if resource.origin() != url.origin() {
        return false;
    }
    let base = resource.path().trim_end_matches('/');
    let path = url.path();
    path == base || path.starts_with(&format!("{base}/")) || base.is_empty()
}

/// The MCP endpoint's URL as a resource identifier: without fragment, and
/// without the trailing slash of an empty path.
fn canonical(url: &reqwest::Url) -> String {
    let mut url = url.clone();
    url.set_fragment(None);
    let text = url.to_string();
    match url.path() == "/" && url.query().is_none() {
        true => text.trim_end_matches('/').to_string(),
        false => text,
    }
}

/// The `Bearer` challenge among the `WWW-Authenticate` headers.
pub(super) fn challenge(headers: &HeaderMap) -> Option<Challenge> {
    headers
        .get_all(reqwest::header::WWW_AUTHENTICATE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find_map(bearer_params)
        .map(|params| Challenge {
            resource_metadata: params.get("resource_metadata").cloned(),
            scope: params.get("scope").cloned(),
            error: params.get("error").cloned(),
        })
}

/// The parameters of the `Bearer` challenge in one header value (RFC 9110
/// section 11.6.1): `scheme param=value, param="quoted", other-scheme …`.
fn bearer_params(header: &str) -> Option<HashMap<String, String>> {
    let bytes = header.as_bytes();
    let mut at = 0;
    let mut scheme: Option<String> = None;
    let mut params = HashMap::new();
    let token_end = |from: usize| {
        let mut end = from;
        while end < bytes.len() && !matches!(bytes[end], b' ' | b'\t' | b',' | b'=') {
            end += 1;
        }
        end
    };
    loop {
        while at < bytes.len() && matches!(bytes[at], b' ' | b'\t' | b',') {
            at += 1;
        }
        if at >= bytes.len() {
            break;
        }
        let end = token_end(at);
        let token = header[at..end].to_string();
        at = end;
        let mut probe = at;
        while probe < bytes.len() && matches!(bytes[probe], b' ' | b'\t') {
            probe += 1;
        }
        if probe < bytes.len() && bytes[probe] == b'=' && !token.is_empty() {
            at = probe + 1;
            while at < bytes.len() && matches!(bytes[at], b' ' | b'\t') {
                at += 1;
            }
            let value = if at < bytes.len() && bytes[at] == b'"' {
                let mut value = Vec::new();
                at += 1;
                while at < bytes.len() && bytes[at] != b'"' {
                    if bytes[at] == b'\\' && at + 1 < bytes.len() {
                        at += 1;
                    }
                    value.push(bytes[at]);
                    at += 1;
                }
                at += 1;
                String::from_utf8_lossy(&value).into_owned()
            } else {
                let end = token_end(at);
                let value = header[at..end].to_string();
                at = end;
                value
            };
            if scheme
                .as_deref()
                .is_some_and(|scheme| scheme.eq_ignore_ascii_case("bearer"))
            {
                params.insert(token.to_ascii_lowercase(), value);
            }
        } else {
            if scheme
                .as_deref()
                .is_some_and(|scheme| scheme.eq_ignore_ascii_case("bearer"))
            {
                return Some(params);
            }
            if token.is_empty() {
                at += 1;
            }
            scheme = Some(token);
        }
    }
    scheme
        .filter(|scheme| scheme.eq_ignore_ascii_case("bearer"))
        .map(|_| params)
}

/// Reads a JSON object from `url` with a GET.
async fn fetch_json(
    client: &reqwest::Client,
    url: &str,
    extra: &[(&str, &str)],
) -> Result<Value, String> {
    let mut request = client
        .get(url)
        .header(reqwest::header::ACCEPT, "application/json");
    for (name, value) in extra {
        request = request.header(*name, *value);
    }
    let response = request
        .send()
        .await
        .map_err(|error| format!("{url} could not be reached: {error}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("{url} answered {status}"));
    }
    json_body(response, url).await
}

async fn json_body(mut response: reqwest::Response, what: &str) -> Result<Value, String> {
    let body = crate::llm::read_capped(&mut response, BODY_MAX)
        .await
        .map_err(|error| format!("the answer of {what} could not be read: {error}"))?
        .ok_or_else(|| format!("the answer of {what} is too large"))?;
    serde_json::from_slice::<Value>(&body)
        .ok()
        .filter(Value::is_object)
        .ok_or_else(|| format!("the answer of {what} is not a JSON object"))
}

/// The authorization server's metadata as used here.
#[derive(Clone)]
struct Metadata {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    registration_endpoint: Option<String>,
    scopes_supported: Vec<String>,
    /// `authorization_response_iss_parameter_supported`.
    sends_iss: bool,
    /// `client_id_metadata_document_supported`.
    takes_metadata_documents: bool,
    /// `token_endpoint_auth_methods_supported`, when given.
    auth_methods: Option<Vec<String>>,
}

/// Where and how a server is signed in to.
struct Discovered {
    /// The `resource` asked for: the resource metadata's, or the endpoint's.
    resource: String,
    /// The resource metadata's `scopes_supported`.
    scopes: Option<Vec<String>>,
    metadata: Metadata,
}

fn strings(value: &Value) -> Option<Vec<String>> {
    value.as_array().map(|items| {
        items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect()
    })
}

/// Reads metadata `document`, fetched for `issuer`: its issuer must be
/// `issuer` exactly, its endpoints HTTPS (or on this machine), and it must
/// offer PKCE with S256.
fn metadata_of(document: &Value, issuer: &str) -> Result<Metadata, String> {
    if document.get("issuer").and_then(Value::as_str) != Some(issuer) {
        return Err(format!(
            "its metadata names another issuer than {issuer}, so it was not used"
        ));
    }
    let endpoint = |key: &str| -> Result<String, String> {
        let text = document
            .get(key)
            .and_then(Value::as_str)
            .ok_or_else(|| format!("its metadata has no {key}"))?;
        checked(text, key).map(|url| url.to_string())
    };
    let methods = strings(&document["code_challenge_methods_supported"]).unwrap_or_default();
    if !methods.iter().any(|method| method == "S256") {
        return Err(format!(
            "the authorization server {issuer} does not say it supports PKCE with S256, \
             which signing in needs"
        ));
    }
    Ok(Metadata {
        issuer: issuer.to_string(),
        authorization_endpoint: endpoint("authorization_endpoint")?,
        token_endpoint: endpoint("token_endpoint")?,
        registration_endpoint: match document.get("registration_endpoint") {
            Some(Value::String(_)) => Some(endpoint("registration_endpoint")?),
            _ => None,
        },
        scopes_supported: strings(&document["scopes_supported"]).unwrap_or_default(),
        sends_iss: document["authorization_response_iss_parameter_supported"] == true,
        takes_metadata_documents: document["client_id_metadata_document_supported"] == true,
        auth_methods: strings(&document["token_endpoint_auth_methods_supported"]),
    })
}

/// The metadata of the authorization server `issuer`, from the well-known
/// URLs of OAuth (RFC 8414) and OpenID Connect, in the order the
/// specification gives.
async fn server_metadata(client: &reqwest::Client, issuer: &str) -> Result<Metadata, String> {
    let url = checked(issuer, "the authorization server")?;
    let origin = url.origin().ascii_serialization();
    let path = url.path().trim_end_matches('/');
    let candidates = match path.is_empty() {
        true => vec![
            format!("{origin}/.well-known/oauth-authorization-server"),
            format!("{origin}/.well-known/openid-configuration"),
        ],
        false => vec![
            format!("{origin}/.well-known/oauth-authorization-server{path}"),
            format!("{origin}/.well-known/openid-configuration{path}"),
            format!("{origin}{path}/.well-known/openid-configuration"),
        ],
    };
    let mut errors = Vec::new();
    for candidate in candidates {
        match fetch_json(client, &candidate, &[]).await {
            Ok(document) => match metadata_of(&document, issuer) {
                Ok(metadata) => return Ok(metadata),
                Err(error) => errors.push(error),
            },
            Err(error) => errors.push(error),
        }
    }
    Err(format!(
        "the metadata of the authorization server {issuer} could not be found: {}",
        errors.join("; ")
    ))
}

/// The resource metadata URLs to try for the endpoint `url` when its
/// challenge names none: under the endpoint's path, then at the root.
fn well_known_resource(url: &reqwest::Url) -> Vec<String> {
    let origin = url.origin().ascii_serialization();
    let path = url.path().trim_end_matches('/');
    let mut candidates = Vec::new();
    if !path.is_empty() {
        candidates.push(format!(
            "{origin}/.well-known/oauth-protected-resource{path}"
        ));
    }
    candidates.push(format!("{origin}/.well-known/oauth-protected-resource"));
    candidates
}

/// Finds where the server at `url` is signed in to.
async fn discover(
    client: &reqwest::Client,
    url: &reqwest::Url,
    challenge: Option<&Challenge>,
) -> Result<Discovered, String> {
    let named = challenge.and_then(|challenge| challenge.resource_metadata.clone());
    let candidates = match &named {
        Some(named) => vec![checked(named, "the resource metadata")?.to_string()],
        None => well_known_resource(url),
    };
    let mut found = None;
    let mut errors = Vec::new();
    for candidate in &candidates {
        match fetch_json(client, candidate, &[]).await {
            Ok(document) => {
                found = Some(document);
                break;
            }
            Err(error) => errors.push(error),
        }
    }
    let Some(document) = found else {
        if named.is_some() {
            return Err(format!(
                "its resource metadata could not be read: {}",
                errors.join("; ")
            ));
        }
        return legacy(client, url).await;
    };
    let resource = document
        .get("resource")
        .and_then(Value::as_str)
        .ok_or("its resource metadata names no resource")?;
    let parsed = reqwest::Url::parse(resource)
        .map_err(|_| format!("its resource metadata names {resource:?}, not a URL"))?;
    if !covers(&parsed, url) {
        return Err(format!(
            "its resource metadata is for {resource}, not for {url}"
        ));
    }
    let issuer = document
        .get("authorization_servers")
        .and_then(Value::as_array)
        .and_then(|servers| servers.first())
        .and_then(Value::as_str)
        .ok_or("its resource metadata names no authorization server")?;
    Ok(Discovered {
        resource: resource.to_string(),
        scopes: strings(&document["scopes_supported"]),
        metadata: server_metadata(client, issuer).await?,
    })
}

/// A server of 2025-03-26: the authorization server is at the server's
/// origin, with its metadata there or, without it, the default endpoints.
async fn legacy(client: &reqwest::Client, url: &reqwest::Url) -> Result<Discovered, String> {
    let base = url.origin().ascii_serialization();
    checked(&base, "the server")?;
    let found = fetch_json(
        client,
        &format!("{base}/.well-known/oauth-authorization-server"),
        &[("MCP-Protocol-Version", LEGACY_VERSION)],
    )
    .await;
    let metadata = match found {
        Ok(document) => {
            let issuer = match document.get("issuer").and_then(Value::as_str) {
                Some(issuer) if issuer.trim_end_matches('/') == base => issuer.to_string(),
                _ => base.clone(),
            };
            metadata_of(&document, &issuer)?
        }
        // 2025-03-26 has every client use PKCE, so the defaults are used
        // without metadata saying so.
        Err(_) => Metadata {
            issuer: base.clone(),
            authorization_endpoint: format!("{base}/authorize"),
            token_endpoint: format!("{base}/token"),
            registration_endpoint: Some(format!("{base}/register")),
            scopes_supported: Vec::new(),
            sends_iss: false,
            takes_metadata_documents: false,
            auth_methods: None,
        },
    };
    Ok(Discovered {
        resource: canonical(url),
        scopes: None,
        metadata,
    })
}

/// The client to sign in with, in the order of the specification: the one
/// `mcp.json` names, a Client ID Metadata Document where the authorization
/// server takes them, then Dynamic Client Registration.
async fn client_for(
    client: &reqwest::Client,
    settings: &Settings,
    metadata: &Metadata,
    redirect: &str,
    scope: Option<&str>,
) -> Result<Client, String> {
    if let Some(id) = &settings.client_id {
        if let Some(issuer) = &settings.authorization_server {
            if issuer != &metadata.issuer {
                return Err(format!(
                    "the client id in mcp.json is for the authorization server {issuer}, but the \
                     server now names {}",
                    metadata.issuer
                ));
            }
        }
        let auth = match &settings.client_secret {
            None => ClientAuth::None,
            Some(_) => match &metadata.auth_methods {
                Some(methods)
                    if !methods.iter().any(|m| m == "client_secret_basic")
                        && methods.iter().any(|m| m == "client_secret_post") =>
                {
                    ClientAuth::ClientSecretPost
                }
                _ => ClientAuth::ClientSecretBasic,
            },
        };
        return Ok(Client {
            id: id.clone(),
            secret: settings.client_secret.clone(),
            auth,
        });
    }
    if let (Some(document), true) = (
        &settings.client_metadata_url,
        metadata.takes_metadata_documents,
    ) {
        let url = reqwest::Url::parse(document)
            .ok()
            .filter(|url| url.scheme() == "https" && url.path() != "/")
            .ok_or("client_metadata_url must be an HTTPS URL with a path")?;
        return Ok(Client {
            id: url.to_string(),
            secret: None,
            auth: ClientAuth::None,
        });
    }
    let Some(endpoint) = &metadata.registration_endpoint else {
        return Err(format!(
            "the authorization server {} lets no client register itself; register one there and \
             write its id as oauth.client_id in mcp.json",
            metadata.issuer
        ));
    };
    let mut request = json!({
        "client_name": "inocode",
        "redirect_uris": [redirect],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
        "application_type": "native",
    });
    if let Some(scope) = scope {
        request["scope"] = json!(scope);
    }
    let response = client
        .post(endpoint)
        .header(reqwest::header::ACCEPT, "application/json")
        .json(&request)
        .send()
        .await
        .map_err(|error| format!("the registration could not be sent: {error}"))?;
    let status = response.status();
    if !status.is_success() {
        let answer = json_body(response, "the registration").await.ok();
        let field = |key: &str| {
            answer
                .as_ref()
                .and_then(|answer| answer.get(key))
                .and_then(Value::as_str)
                .map(|text| text.chars().take(300).collect::<String>())
        };
        return Err(format!(
            "the authorization server refused to register inocode ({status}{}{})",
            field("error").map(|e| format!(", {e}")).unwrap_or_default(),
            field("error_description")
                .map(|e| format!(": {e}"))
                .unwrap_or_default()
        ));
    }
    let answer = json_body(response, "the registration").await?;
    let id = answer
        .get("client_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or("the registration answer has no client_id")?;
    let secret = answer
        .get("client_secret")
        .and_then(Value::as_str)
        .map(str::to_string);
    let auth = match (
        &secret,
        answer
            .get("token_endpoint_auth_method")
            .and_then(Value::as_str),
    ) {
        (None, _) | (_, Some("none")) => ClientAuth::None,
        (Some(_), Some("client_secret_post")) => ClientAuth::ClientSecretPost,
        (Some(_), _) => ClientAuth::ClientSecretBasic,
    };
    Ok(Client {
        id: id.to_string(),
        secret,
        auth,
    })
}

/// Why a token request failed.
enum TokenError {
    /// The endpoint refused it (HTTP 400, 401 or 403): only a new sign-in
    /// helps.
    Refused(String),
    Failed(String),
}

impl TokenError {
    fn text(self) -> String {
        match self {
            Self::Refused(text) | Self::Failed(text) => text,
        }
    }
}

/// Posts a token request with the client's credentials; the answer's
/// fields. The answer of a refusal is never shown beyond its `error` code,
/// since it may repeat what was sent.
async fn token_request(
    client: &reqwest::Client,
    endpoint: &str,
    id: &str,
    secret: Option<&str>,
    auth: ClientAuth,
    params: &[(&str, &str)],
) -> Result<Value, TokenError> {
    let mut fields: Vec<(&str, &str)> = params.to_vec();
    match (auth, secret) {
        (ClientAuth::ClientSecretPost, Some(secret)) => {
            fields.push(("client_id", id));
            fields.push(("client_secret", secret));
        }
        (ClientAuth::ClientSecretBasic, Some(_)) => {}
        _ => fields.push(("client_id", id)),
    }
    let body = fields
        .iter()
        .map(|(name, value)| format!("{name}={}", crate::oauth::urlencode(value)))
        .collect::<Vec<_>>()
        .join("&");
    let mut request = client
        .post(endpoint)
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .header(reqwest::header::ACCEPT, "application/json")
        .body(body);
    if let (ClientAuth::ClientSecretBasic, Some(secret)) = (auth, secret) {
        // RFC 6749 section 2.3.1: both parts form-encoded before Base64.
        let pair = format!(
            "{}:{}",
            crate::oauth::urlencode(id),
            crate::oauth::urlencode(secret)
        );
        request = request.header(
            reqwest::header::AUTHORIZATION,
            format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(pair)
            ),
        );
    }
    let response = request
        .send()
        .await
        .map_err(|error| TokenError::Failed(format!("the token request failed: {error}")))?;
    let status = response.status();
    if !status.is_success() {
        let code = json_body(response, "the token endpoint")
            .await
            .ok()
            .and_then(|answer| {
                answer
                    .get("error")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .filter(|code| code.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
            .map(|code| format!(" ({code})"))
            .unwrap_or_default();
        let text = format!("the token endpoint answered {status}{code}");
        return Err(match status.as_u16() {
            400 | 401 | 403 => TokenError::Refused(text),
            _ => TokenError::Failed(text),
        });
    }
    let answer = json_body(response, "the token endpoint")
        .await
        .map_err(TokenError::Failed)?;
    if answer
        .get("access_token")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
    {
        return Err(TokenError::Failed(
            "the token answer has no access_token".into(),
        ));
    }
    match answer.get("token_type").and_then(Value::as_str) {
        Some(kind) if !kind.eq_ignore_ascii_case("bearer") => Err(TokenError::Failed(format!(
            "the token is of type {kind}, not Bearer"
        ))),
        _ => Ok(answer),
    }
}

/// Puts a token answer into `grant`: the new tokens, keeping the refresh
/// token when no new one came.
fn take_tokens(grant: &mut Grant, answer: &Value) {
    grant.access = answer["access_token"].as_str().unwrap_or("").to_string();
    if let Some(refresh) = answer.get("refresh_token").and_then(Value::as_str) {
        grant.refresh = refresh.to_string();
    }
    grant.expires_at_ms = answer
        .get("expires_in")
        .and_then(Value::as_u64)
        .map(|seconds| crate::oauth::now_ms().saturating_add(seconds.saturating_mul(1000)));
    if let Some(scope) = answer.get("scope").and_then(Value::as_str) {
        grant.scope = Some(scope.to_string());
    }
}

/// The store's name of the sign-in to `resource` at `issuer`: one per pair.
fn account_of(issuer: &str, resource: &str) -> String {
    let digest = Sha256::digest(format!("{issuer}\n{resource}").as_bytes());
    digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The stored sign-ins whose resource names the endpoint `url`, with their
/// store names; `refused` ones too when asked.
fn stored(store: &Path, url: &reqwest::Url, refused: bool) -> Result<Vec<(String, Grant)>, String> {
    let credentials = crate::oauth::Store::load(store.to_path_buf())?;
    let refusals: HashSet<String> = credentials
        .accounts(PROVIDER)
        .into_iter()
        .filter(|account| account.info.needs_reauth)
        .map(|account| account.id)
        .collect();
    let mut grants: Vec<(String, Grant)> = credentials
        .records(PROVIDER)?
        .into_iter()
        .filter(|(id, _)| refused || !refusals.contains(id))
        .filter_map(|(id, value)| Some((id, serde_json::from_value::<Grant>(value).ok()?)))
        .filter(|(_, grant)| {
            reqwest::Url::parse(&grant.resource).is_ok_and(|resource| covers(&resource, url))
        })
        .collect();
    // The most specific resource first.
    grants.sort_by_key(|(_, grant)| std::cmp::Reverse(grant.resource.len()));
    Ok(grants)
}

fn save(store: &Path, id: &str, grant: &Grant) -> Result<(), String> {
    let record = serde_json::to_value(grant).map_err(|error| error.to_string())?;
    crate::oauth::Store::load(store.to_path_buf())?.put_record(PROVIDER, id, record)
}

/// The sign-in of one connection to a server: the token sent with every
/// request, refreshed before it expires and when the server refuses it.
pub(super) struct Auth {
    server: String,
    url: reqwest::Url,
    store: PathBuf,
    /// The sign-in in use, once read from the store.
    grant: Mutex<Option<Option<(String, Grant)>>>,
    renewing: tokio::sync::Mutex<()>,
}

/// What to do after the server refused a request.
pub(super) enum Verdict {
    /// Send it again: the token was renewed.
    Again,
    /// It cannot succeed until the person signs in; why, for the person.
    SignIn(String),
    /// The refusal is not about the sign-in.
    Other,
}

impl Auth {
    pub(super) fn new(server: &str, url: reqwest::Url, store: PathBuf) -> Self {
        Self {
            server: server.to_string(),
            url,
            store,
            grant: Mutex::new(None),
            renewing: tokio::sync::Mutex::new(()),
        }
    }

    fn current(&self) -> Option<(String, Grant)> {
        let mut grant = self.grant.lock().expect("grant lock");
        grant
            .get_or_insert_with(|| match stored(&self.store, &self.url, false) {
                Ok(grants) => grants.into_iter().next(),
                Err(error) => {
                    eprintln!(
                        "MCP: the sign-in of server {} cannot be read: {error}",
                        self.server
                    );
                    None
                }
            })
            .clone()
    }

    fn set(&self, grant: Option<(String, Grant)>) {
        *self.grant.lock().expect("grant lock") = Some(grant);
    }

    /// The `Authorization` header, with a token refreshed first when it
    /// expires soon; nothing without a sign-in.
    pub(super) async fn headers(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();
        let Some((_, mut grant)) = self.current() else {
            return headers;
        };
        let expiring = grant
            .expires_at_ms
            .is_some_and(|at| crate::oauth::now_ms().saturating_add(EARLY_MS) >= at);
        if expiring && !grant.refresh.is_empty() {
            match self.renew(&grant.access).await {
                Ok(renewed) => grant = renewed,
                Err(TokenError::Refused(_)) => return headers,
                // The old token is tried; the server says whether it holds.
                Err(TokenError::Failed(_)) => {}
            }
        }
        if let Ok(value) = HeaderValue::from_str(&format!("Bearer {}", grant.access)) {
            headers.insert(reqwest::header::AUTHORIZATION, value);
        }
        headers
    }

    /// The token held now, without refreshing it.
    pub(super) fn held(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();
        let held = self.grant.lock().expect("grant lock").clone().flatten();
        if let Some((_, grant)) = held {
            if let Ok(value) = HeaderValue::from_str(&format!("Bearer {}", grant.access)) {
                headers.insert(reqwest::header::AUTHORIZATION, value);
            }
        }
        headers
    }

    /// Settles the server's refusal `status` of a request sent with the
    /// headers `sent`: a 401 has the token refreshed, once the server is
    /// found to still trust the authorization server that issued it; a 403
    /// for more scopes needs the person to sign in again with them.
    pub(super) async fn refused(
        &self,
        status: reqwest::StatusCode,
        answer: &HeaderMap,
        sent: &HeaderMap,
    ) -> Verdict {
        let challenge = challenge(answer);
        if let Some(challenge) = &challenge {
            let mut registry = registry();
            let known = registry
                .servers
                .entry(key(&self.store, &self.url))
                .or_default();
            let mut kept = challenge.clone();
            // A step-up keeps the scopes asked before (the union of the
            // specification's step-up flow).
            if let (Some(old), Some(new)) = (
                known.challenge.as_ref().and_then(|old| old.scope.clone()),
                &challenge.scope,
            ) {
                kept.scope = Some(union(&[Some(old.as_str()), Some(new.as_str())]).join(" "));
            }
            known.challenge = Some(kept);
        }
        let login = format!("run /mcp login {}", self.server);
        match status.as_u16() {
            403 => {
                let Some(challenge) =
                    challenge.filter(|c| c.error.as_deref() == Some("insufficient_scope"))
                else {
                    return Verdict::Other;
                };
                let why = format!(
                    "the MCP server {} needs more permissions{}: {login} to grant them",
                    self.server,
                    challenge
                        .scope
                        .map(|scope| format!(" ({scope})"))
                        .unwrap_or_default()
                );
                needs(&self.store, &self.url, why.clone());
                Verdict::SignIn(why)
            }
            401 => {
                let sent = sent
                    .get(reqwest::header::AUTHORIZATION)
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.strip_prefix("Bearer "));
                let why = match self.current() {
                    None => format!("the MCP server {} needs a sign-in: {login}", self.server),
                    Some((_, grant)) if sent.is_some_and(|sent| sent != grant.access) => {
                        return Verdict::Again
                    }
                    Some((_, grant)) => {
                        match self.still_trusted(&grant, challenge.as_ref()).await {
                            false => {
                                self.set(None);
                                format!(
                                "the MCP server {} no longer names the authorization server it \
                                 was signed in with: {login}",
                                self.server
                            )
                            }
                            true if grant.refresh.is_empty() => format!(
                                "the sign-in to the MCP server {} has expired: {login}",
                                self.server
                            ),
                            true => match self.renew(&grant.access).await {
                                Ok(_) => return Verdict::Again,
                                Err(error) => format!(
                                    "the sign-in to the MCP server {} could not be renewed ({}): \
                                 {login}",
                                    self.server,
                                    error.text()
                                ),
                            },
                        }
                    }
                };
                needs(&self.store, &self.url, why.clone());
                Verdict::SignIn(why)
            }
            _ => Verdict::Other,
        }
    }

    /// Whether the server's resource metadata still names the authorization
    /// server `grant` came from; tokens are never sent to another's server.
    async fn still_trusted(&self, grant: &Grant, challenge: Option<&Challenge>) -> bool {
        let Ok(client) = client() else { return false };
        let named = challenge.and_then(|challenge| challenge.resource_metadata.clone());
        let candidates = match named {
            Some(named) => vec![named],
            None => well_known_resource(&self.url),
        };
        for candidate in candidates {
            if let Ok(document) = fetch_json(&client, &candidate, &[]).await {
                return strings(&document["authorization_servers"])
                    .is_some_and(|servers| servers.contains(&grant.issuer));
            }
        }
        // A server of 2025-03-26 has its authorization server at its origin.
        grant.issuer.trim_end_matches('/') == self.url.origin().ascii_serialization()
    }

    /// Refreshes the sign-in whose token `rejected` no longer serves, once
    /// across this process and others: a second caller finds the token the
    /// first stored. A refusal marks the sign-in as needing a new one.
    async fn renew(&self, rejected: &str) -> Result<Grant, TokenError> {
        let _one = self.renewing.lock().await;
        let Some((id, _)) = self.current() else {
            return Err(TokenError::Refused("there is no sign-in".into()));
        };
        let lock = crate::secrets::open_lock_file(&crate::secrets::sibling(
            &self.store,
            &format!(".refresh.{PROVIDER}.{id}.lock"),
        ))
        .map_err(TokenError::Failed)?;
        let deadline = tokio::time::Instant::now() + REFRESH_LOCK_WAIT;
        loop {
            match lock.try_lock() {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock)
                    if tokio::time::Instant::now() < deadline =>
                {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Err(_) => {
                    return Err(TokenError::Failed(
                        "another refresh of the sign-in did not finish in time".into(),
                    ))
                }
            }
        }
        let latest = stored(&self.store, &self.url, true)
            .map_err(TokenError::Failed)?
            .into_iter()
            .find(|(stored, _)| *stored == id);
        let Some((_, mut grant)) = latest else {
            self.set(None);
            return Err(TokenError::Refused("the sign-in was removed".into()));
        };
        if grant.access != rejected {
            self.set(Some((id, grant.clone())));
            return Ok(grant);
        }
        let client = client().map_err(TokenError::Failed)?;
        let answer = token_request(
            &client,
            &grant.token_endpoint,
            &grant.client_id,
            grant.client_secret.as_deref(),
            grant.client_auth,
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", &grant.refresh),
                ("resource", &grant.resource),
            ],
        )
        .await;
        match answer {
            Ok(answer) => {
                take_tokens(&mut grant, &answer);
                save(&self.store, &id, &grant).map_err(TokenError::Failed)?;
                self.set(Some((id, grant.clone())));
                Ok(grant)
            }
            Err(TokenError::Refused(text)) => {
                let _ = crate::oauth::mark_needs_reauth(&self.store, PROVIDER, &id);
                self.set(None);
                Err(TokenError::Refused(text))
            }
            Err(error) => Err(error),
        }
    }
}

/// The scopes of `lists` (space-separated), each once, in order.
fn union(lists: &[Option<&str>]) -> Vec<String> {
    let mut seen = Vec::new();
    for scope in lists
        .iter()
        .flatten()
        .flat_map(|list| list.split_whitespace())
    {
        if !seen.iter().any(|kept: &String| kept == scope) {
            seen.push(scope.to_string());
        }
    }
    seen
}

/// A server a sign-in is for: its name, endpoint, the headers `mcp.json`
/// gives it and its `oauth` settings.
pub(super) struct Target {
    pub(super) server: String,
    pub(super) url: String,
    pub(super) headers: HashMap<String, String>,
    pub(super) settings: Settings,
}

/// The scopes a sign-in asks for: those the server last asked for, else the
/// resource metadata's, with those of the earlier sign-in kept; and
/// `offline_access`, for a refresh token, where the authorization server
/// offers it.
fn scopes(
    challenge: Option<&Challenge>,
    discovered: &Discovered,
    earlier: Option<&str>,
) -> Option<String> {
    let resource_scopes = discovered.scopes.as_ref().map(|scopes| scopes.join(" "));
    let asked = challenge
        .and_then(|challenge| challenge.scope.clone())
        .or(resource_scopes);
    let mut scopes = union(&[earlier, asked.as_deref()]);
    if scopes.is_empty() {
        return None;
    }
    if discovered
        .metadata
        .scopes_supported
        .iter()
        .any(|scope| scope == "offline_access")
        && !scopes.iter().any(|scope| scope == "offline_access")
    {
        scopes.push("offline_access".into());
    }
    Some(scopes.join(" "))
}

/// Starts the person's sign-in to `target`: finds the authorization server,
/// registers where needed and gives the page to open; the sign-in finishes
/// in the background when the browser comes back, and `signed_in` is
/// awaited before it is reported done.
pub(super) async fn login_start(
    store: &Path,
    target: Target,
    signed_in: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<Value, String> {
    let url = reqwest::Url::parse(&target.url)
        .map_err(|error| format!("the url of server {} is not valid: {error}", target.server))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!("server {} is not reached over HTTP", target.server));
    }
    if target
        .headers
        .keys()
        .any(|name| name.eq_ignore_ascii_case("authorization"))
    {
        return Err(format!(
            "server {} signs in with the Authorization header of mcp.json",
            target.server
        ));
    }
    let http = client()?;
    let known = registry()
        .servers
        .get(&key(store, &url))
        .and_then(|known| known.challenge.clone());
    let challenge = match known {
        Some(challenge) => Some(challenge),
        None => probe(&http, &url, &target.headers).await,
    };
    let discovered = discover(&http, &url, challenge.as_ref())
        .await
        .map_err(|error| format!("server {} cannot be signed in to: {error}", target.server))?;
    let metadata = discovered.metadata.clone();
    let id = account_of(&metadata.issuer, &discovered.resource);
    let earlier = stored(store, &url, true)?
        .into_iter()
        .find(|(stored, _)| *stored == id)
        .and_then(|(_, grant)| grant.scope);
    let scope = scopes(challenge.as_ref(), &discovered, earlier.as_deref());
    let port = match target.settings.callback_port {
        Some(port) => match crate::oauth::reserve_callback_port(port) {
            reserved if reserved == port => port,
            _ => return Err(format!("the callback port {port} is in use")),
        },
        None => crate::oauth::reserve_callback_port(0),
    };
    let redirect = format!("http://localhost:{port}{CALLBACK_PATH}");
    let client = client_for(
        &http,
        &target.settings,
        &metadata,
        &redirect,
        scope.as_deref(),
    )
    .await
    .map_err(|error| format!("server {}: {error}", target.server))?;
    let pkce = crate::oauth::generate_pkce();
    let state = crate::oauth::generate_state();
    let mut page = reqwest::Url::parse(&metadata.authorization_endpoint)
        .map_err(|error| format!("the authorization endpoint is not a URL: {error}"))?;
    {
        let mut query = page.query_pairs_mut();
        query
            .append_pair("response_type", "code")
            .append_pair("client_id", &client.id)
            .append_pair("redirect_uri", &redirect)
            .append_pair("code_challenge", &pkce.challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", &state)
            .append_pair("resource", &discovered.resource);
        if let Some(scope) = &scope {
            query.append_pair("scope", scope);
        }
    }
    let shown = page.to_string();
    let login_state = Arc::new(Mutex::new(LoginState::Waiting));
    let reported = login_state.clone();
    let server_key = key(store, &url);
    let store_path = store.to_path_buf();
    let finished_key = server_key.clone();
    let task = tokio::spawn(async move {
        let finished = finish(
            &store_path,
            &http,
            port,
            &state,
            &pkce.verifier,
            &redirect,
            &metadata,
            &client,
            &discovered.resource,
            scope,
            &id,
        )
        .await;
        let state = match finished {
            Ok(()) => {
                registry().servers.remove(&finished_key);
                signed_in.await;
                LoginState::Done
            }
            Err(error) => LoginState::Failed(error),
        };
        *reported.lock().expect("login lock") = state;
    });
    let login_id = crate::oauth::generate_state();
    let login = Login {
        server: target.server,
        key: server_key,
        started: Instant::now(),
        url: shown,
        state: login_state,
        task: task.abort_handle(),
    };
    let view = view(&login_id, &login);
    registry().logins.insert(login_id, login);
    Ok(view)
}

/// An unauthenticated request to the endpoint, for the challenge of its
/// refusal.
async fn probe(
    client: &reqwest::Client,
    url: &reqwest::Url,
    headers: &HashMap<String, String>,
) -> Option<Challenge> {
    let mut request = client
        .post(url.clone())
        .header(
            reqwest::header::ACCEPT,
            "application/json, text/event-stream",
        )
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(json!({"jsonrpc": "2.0", "id": 0, "method": "ping"}).to_string());
    for (name, value) in headers {
        request = request.header(name.as_str(), value.as_str());
    }
    let response = request.send().await.ok()?;
    challenge(response.headers())
}

/// Waits for the browser's redirect and exchanges its code for tokens,
/// which are stored.
#[allow(clippy::too_many_arguments)]
async fn finish(
    store: &Path,
    http: &reqwest::Client,
    port: u16,
    state: &str,
    verifier: &str,
    redirect: &str,
    metadata: &Metadata,
    client: &Client,
    resource: &str,
    scope: Option<String>,
    id: &str,
) -> Result<(), String> {
    let callback = crate::oauth::await_callback(port, CALLBACK_PATH, LOGIN_WAIT).await?;
    if callback.state != state {
        return Err("the browser came back with another sign-in's state; it was not used".into());
    }
    // RFC 9207 section 2.4, compared as plain strings.
    match (&callback.iss, metadata.sends_iss) {
        (Some(iss), _) if *iss != metadata.issuer => {
            return Err(
                "the browser came back from another authorization server than the one asked; \
                 its answer was not used"
                    .into(),
            )
        }
        (None, true) => {
            return Err(
                "the authorization server's answer lacks the iss it says it sends; it was not used"
                    .into(),
            )
        }
        _ => {}
    }
    let answer = token_request(
        http,
        &metadata.token_endpoint,
        &client.id,
        client.secret.as_deref(),
        client.auth,
        &[
            ("grant_type", "authorization_code"),
            ("code", &callback.code),
            ("redirect_uri", redirect),
            ("code_verifier", verifier),
            ("resource", resource),
        ],
    )
    .await
    .map_err(TokenError::text)?;
    let mut grant = Grant {
        access: String::new(),
        refresh: String::new(),
        expires_at_ms: None,
        issuer: metadata.issuer.clone(),
        resource: resource.to_string(),
        token_endpoint: metadata.token_endpoint.clone(),
        client_id: client.id.clone(),
        client_secret: client.secret.clone(),
        client_auth: client.auth,
        scope,
    };
    take_tokens(&mut grant, &answer);
    save(store, id, &grant)
}

fn view(id: &str, login: &Login) -> Value {
    let (status, error) = match &*login.state.lock().expect("login lock") {
        LoginState::Waiting => ("waiting", None),
        LoginState::Done => ("done", None),
        LoginState::Failed(error) => ("failed", Some(error.clone())),
        LoginState::Cancelled => ("cancelled", None),
    };
    let mut view = json!({
        "login_id": id,
        "server": login.server,
        "url": login.url,
        "status": status,
        "expires_in": LOGIN_WAIT.saturating_sub(login.started.elapsed()).as_secs(),
    });
    if let Some(error) = error {
        view["error"] = json!(error);
    }
    view
}

fn unknown_login() -> String {
    "no MCP sign-in has this id; it may have ended more than 25 minutes ago or before a restart"
        .into()
}

pub(super) fn login_status(id: &str) -> Result<Value, String> {
    let registry = registry();
    let login = registry.logins.get(id).ok_or_else(unknown_login)?;
    Ok(view(id, login))
}

/// Stops a sign-in that has not finished, and its callback listener.
pub(super) fn login_cancel(id: &str) -> Result<Value, String> {
    let registry = registry();
    let login = registry.logins.get(id).ok_or_else(unknown_login)?;
    let mut state = login.state.lock().expect("login lock");
    if matches!(*state, LoginState::Waiting) {
        login.task.abort();
        *state = LoginState::Cancelled;
    }
    drop(state);
    Ok(view(id, login))
}

/// Removes every stored sign-in for the endpoint `url`; how many there were.
pub(super) fn logout(store: &Path, url: &str) -> Result<usize, String> {
    let url = reqwest::Url::parse(url).map_err(|error| format!("invalid url {url}: {error}"))?;
    let grants = stored(store, &url, true)?;
    let mut credentials = crate::oauth::Store::load(store.to_path_buf())?;
    for (id, _) in &grants {
        credentials.remove_account(PROVIDER, id)?;
    }
    registry().servers.remove(&key(store, &url));
    Ok(grants.len())
}

/// The sign-in state of the server at `url`: `signing_in`, `needs_login`,
/// `signed_in`, `not_needed` (it was reached without one) or `unknown` (it
/// has not been reached yet).
pub(super) fn status(store: &Path, url: &str, reached: bool) -> &'static str {
    let Ok(url) = reqwest::Url::parse(url) else {
        return "unknown";
    };
    let server = key(store, &url);
    {
        let registry = registry();
        if registry.logins.values().any(|login| {
            login.key == server
                && matches!(
                    *login.state.lock().expect("login lock"),
                    LoginState::Waiting
                )
        }) {
            return "signing_in";
        }
        if registry
            .servers
            .get(&server)
            .is_some_and(|known| known.needs.is_some())
        {
            return "needs_login";
        }
    }
    match (stored(store, &url, false), stored(store, &url, true)) {
        (Ok(live), _) if !live.is_empty() => "signed_in",
        // Only sign-ins whose refresh was refused.
        (Ok(_), Ok(all)) if !all.is_empty() => "needs_login",
        (Ok(_), Ok(_)) if reached => "not_needed",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_challenges_are_read_among_others() {
        let headers = |value: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(
                reqwest::header::WWW_AUTHENTICATE,
                HeaderValue::from_str(value).expect("header"),
            );
            headers
        };
        assert_eq!(
            challenge(&headers(
                r#"Bearer resource_metadata="https://mcp.example.com/.well-known/oauth-protected-resource", scope="files:read files:write""#
            )),
            Some(Challenge {
                resource_metadata: Some(
                    "https://mcp.example.com/.well-known/oauth-protected-resource".into()
                ),
                scope: Some("files:read files:write".into()),
                error: None,
            })
        );
        assert_eq!(
            challenge(&headers(
                r#"Basic realm="x", Bearer error="insufficient_scope", scope=admin, error_description="a \"quoted\" word""#
            )),
            Some(Challenge {
                resource_metadata: None,
                scope: Some("admin".into()),
                error: Some("insufficient_scope".into()),
            })
        );
        assert_eq!(challenge(&headers("Bearer")), Some(Challenge::default()));
        assert_eq!(challenge(&headers(r#"Basic realm="x""#)), None);
    }

    #[test]
    fn resources_cover_their_endpoints_and_urls_must_be_safe() {
        let url = |text: &str| reqwest::Url::parse(text).expect("url");
        let endpoint = url("https://mcp.example.com/public/mcp");
        assert!(covers(&url("https://mcp.example.com"), &endpoint));
        assert!(covers(&url("https://mcp.example.com/public"), &endpoint));
        assert!(covers(
            &url("https://mcp.example.com/public/mcp"),
            &endpoint
        ));
        assert!(!covers(&url("https://mcp.example.com/pub"), &endpoint));
        assert!(!covers(&url("https://other.example.com"), &endpoint));
        assert!(!covers(&url("http://mcp.example.com"), &endpoint));
        assert_eq!(
            well_known_resource(&endpoint),
            [
                "https://mcp.example.com/.well-known/oauth-protected-resource/public/mcp",
                "https://mcp.example.com/.well-known/oauth-protected-resource",
            ]
        );
        assert_eq!(
            canonical(&url("https://mcp.example.com/#x")),
            "https://mcp.example.com"
        );
        assert!(allowed(&url("https://auth.example.com/")));
        assert!(allowed(&url("http://127.0.0.1:8080/")));
        assert!(allowed(&url("http://localhost/")));
        assert!(allowed(&url("http://[::1]:9/")));
        assert!(!allowed(&url("http://auth.example.com/")));
        assert_eq!(union(&[Some("a b"), None, Some("b c")]), ["a", "b", "c"]);
    }

    use crate::mcp::tests::{empty, json_reply, serve, Got};

    fn paths(seen: &Arc<Mutex<Vec<Got>>>) -> Vec<String> {
        seen.lock()
            .expect("seen")
            .iter()
            .map(|got| got.path.clone())
            .collect()
    }

    #[tokio::test]
    async fn metadata_is_found_in_the_order_of_the_specification() {
        let issuer: Arc<Mutex<String>> = Arc::default();
        let own = issuer.clone();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let url = serve(seen.clone(), move |got| {
            let issuer = own.lock().expect("issuer").clone();
            match got.path.as_str() {
                "/.well-known/oauth-protected-resource" => json_reply(
                    "200 OK",
                    "",
                    json!({"resource": issuer.replace("/tenant", "/mcp"),
                           "authorization_servers": [issuer]}),
                ),
                // A document naming another issuer is not used.
                "/.well-known/openid-configuration/tenant" => json_reply(
                    "200 OK",
                    "",
                    json!({"issuer": "https://honest.example",
                           "authorization_endpoint": "https://honest.example/a",
                           "token_endpoint": "https://honest.example/t",
                           "code_challenge_methods_supported": ["S256"]}),
                ),
                "/tenant/.well-known/openid-configuration" => json_reply(
                    "200 OK",
                    "",
                    json!({"issuer": issuer,
                           "authorization_endpoint": format!("{issuer}/authorize"),
                           "token_endpoint": format!("{issuer}/token"),
                           "code_challenge_methods_supported": ["plain", "S256"]}),
                ),
                _ => empty("404 Not Found"),
            }
        })
        .await;
        *issuer.lock().expect("issuer") = url.replace("/mcp", "/tenant");
        let endpoint = reqwest::Url::parse(&url).expect("url");
        let found = discover(&client().expect("client"), &endpoint, None)
            .await
            .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(found.resource, url);
        assert_eq!(
            found.metadata.token_endpoint,
            url.replace("/mcp", "/tenant/token")
        );
        assert!(found.metadata.registration_endpoint.is_none());
        assert_eq!(
            paths(&seen),
            [
                "/.well-known/oauth-protected-resource/mcp",
                "/.well-known/oauth-protected-resource",
                "/.well-known/oauth-authorization-server/tenant",
                "/.well-known/openid-configuration/tenant",
                "/tenant/.well-known/openid-configuration",
            ]
        );
    }

    #[tokio::test]
    async fn an_older_server_without_resource_metadata_uses_its_origin() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let url = serve(seen.clone(), |_| empty("404 Not Found")).await;
        let endpoint = reqwest::Url::parse(&url).expect("url");
        let found = discover(&client().expect("client"), &endpoint, None)
            .await
            .unwrap_or_else(|error| panic!("{error}"));
        let base = url.trim_end_matches("/mcp");
        assert_eq!(found.metadata.issuer, base);
        assert_eq!(
            found.metadata.authorization_endpoint,
            format!("{base}/authorize")
        );
        assert_eq!(found.metadata.token_endpoint, format!("{base}/token"));
        assert_eq!(
            found.metadata.registration_endpoint.as_deref(),
            Some(format!("{base}/register").as_str())
        );
        assert_eq!(found.resource, url);
        let seen = seen.lock().expect("seen").clone();
        let asked = seen.last().expect("asked");
        assert_eq!(asked.path, "/.well-known/oauth-authorization-server");
        assert_eq!(asked.headers["mcp-protocol-version"], LEGACY_VERSION);
    }

    #[tokio::test]
    async fn a_server_without_pkce_or_a_named_metadata_document_is_refused() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let origin: Arc<Mutex<String>> = Arc::default();
        let own = origin.clone();
        let url = serve(seen, move |got| {
            let origin = own.lock().expect("origin").clone();
            match got.path.as_str() {
                "/.well-known/oauth-protected-resource" => json_reply(
                    "200 OK",
                    "",
                    json!({"resource": format!("{origin}/mcp"), "authorization_servers": [origin]}),
                ),
                "/.well-known/oauth-authorization-server" => json_reply(
                    "200 OK",
                    "",
                    json!({"issuer": origin,
                           "authorization_endpoint": format!("{origin}/authorize"),
                           "token_endpoint": format!("{origin}/token")}),
                ),
                _ => empty("404 Not Found"),
            }
        })
        .await;
        *origin.lock().expect("origin") = url.trim_end_matches("/mcp").to_string();
        let endpoint = reqwest::Url::parse(&url).expect("url");
        let http = client().expect("client");
        let error = discover(&http, &endpoint, None)
            .await
            .err()
            .expect("refused");
        assert!(error.contains("PKCE with S256"), "{error}");
        // Metadata the challenge names must be read; nothing else is tried.
        let named = Challenge {
            resource_metadata: Some(format!("{}/missing", url.trim_end_matches("/mcp"))),
            ..Challenge::default()
        };
        let error = discover(&http, &endpoint, Some(&named))
            .await
            .err()
            .expect("refused");
        assert!(
            error.contains("resource metadata could not be read"),
            "{error}"
        );
        // An authorization server reached over plain HTTP elsewhere is refused.
        let error = server_metadata(&http, "http://auth.example.com")
            .await
            .err()
            .expect("refused");
        assert!(
            error.contains("neither HTTPS nor on this machine"),
            "{error}"
        );
    }
}
