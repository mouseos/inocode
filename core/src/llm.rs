//! Streaming clients for OpenAI-compatible and Anthropic-compatible chat APIs.
//!
//! The transport is plain HTTP with server-sent events. The format is chosen by
//! configuration: Chat Completions (`/chat/completions`), Messages
//! (`/messages`), or Responses (`/responses`). Authentication is either a
//! static API key or a stored OAuth/subscription credential
//! ([`crate::oauth`]), refreshed at request time when it has expired.

use std::fmt;
use std::time::Duration;

use serde_json::{json, Value};

use crate::oauth::{AuthKind, Credential, FlowEndpoints};
use crate::sse::SseParser;
use crate::ErrorInfo;

/// Default endpoint base used when `INOCODE_LLM_BASE_URL` is not set.
pub const DEFAULT_BASE_URL: &str = "https://logfare.ai/v1";

/// Default model used when `INOCODE_LLM_MODEL` is not set.
pub const DEFAULT_MODEL: &str = "logfare/auto";

/// `max_tokens` for Anthropic-style requests, which require it. Reasoning
/// tokens share this budget, so it must leave room for the visible answer:
/// with the sample-derived 1024 the routed reasoning models of `logfare/auto`
/// exhausted the budget with thinking alone (`stop_reason=max_tokens`, no
/// answer text; see docs/evidence/anthropic-messages.md).
pub const DEFAULT_MAX_TOKENS: u32 = 8192;

const API_KEY_ENV: &str = "LOGFARE_API_KEY";
const BASE_URL_ENV: &str = "INOCODE_LLM_BASE_URL";
const MODEL_ENV: &str = "INOCODE_LLM_MODEL";
const PROVIDER_ENV: &str = "INOCODE_LLM_PROVIDER";
const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Whole-request bound for model discovery, body included (reference
/// `QODER_MODEL_DISCOVERY_TIMEOUT_MS`), so a stalled provider cannot keep a
/// `models` request open forever.
const MODELS_TIMEOUT: Duration = Duration::from_secs(30);
/// Bound for the ChatGPT and Anthropic usage reads (reference
/// `WHAM_REQUEST_TIMEOUT_MS`, `REQUEST_TIMEOUT_MS` in `quota-wire.ts`).
const USAGE_TIMEOUT: Duration = Duration::from_secs(8);
/// Bound for the Muse key-mint read that carries the subscription usage
/// (reference `meta-muse-device.ts`).
const MUSE_USAGE_TIMEOUT: Duration = Duration::from_secs(20);
/// Largest usage body read (reference `quota-wire.ts`).
const USAGE_BODY_MAX_BYTES: usize = 512 * 1024;
/// Identity the reference sends on the Anthropic usage read
/// (`claude-cli-identity.ts`).
const CLAUDE_USAGE_USER_AGENT: &str = "claude-cli/2.1.321 (external, cli)";
const CLAUDE_USAGE_BETA: &str = "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,context-management-2025-06-27,prompt-caching-scope-2026-01-05";
/// The ChatGPT backend filters `/models` by a required `client_version`
/// parameter; asking as a client newer than any release shows the whole
/// roster (reference `CODEX_ROSTER_DISCOVERY_CLIENT_VERSION`).
const CODEX_DISCOVERY_CLIENT_VERSION: &str = "99.0.0";
/// Overrides the `client_version` reported to the Codex models endpoint.
const CODEX_CLIENT_VERSION_ENV: &str = "INOCODE_CODEX_CLIENT_VERSION";

/// Which API format a [`Llm`] speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProviderKind {
    /// Chat Completions format (`POST {base}/chat/completions`).
    #[default]
    OpenAi,
    /// Anthropic Messages format (`POST {base}/messages`).
    Anthropic,
    /// OpenAI Responses format (`POST {base}/responses`).
    Responses,
}

impl ProviderKind {
    fn from_env() -> Result<Self, ErrorInfo> {
        match std::env::var(PROVIDER_ENV) {
            Err(_) => Ok(Self::default()),
            Ok(value) if value.eq_ignore_ascii_case("openai") => Ok(Self::OpenAi),
            Ok(value) if value.eq_ignore_ascii_case("anthropic") => Ok(Self::Anthropic),
            Ok(value) if value.eq_ignore_ascii_case("responses") => Ok(Self::Responses),
            Ok(value) if value.trim().is_empty() => Ok(Self::default()),
            Ok(value) => Err(ErrorInfo::new(
                "provider_config",
                format!(
                    "{PROVIDER_ENV} must be \"openai\", \"anthropic\", \"responses\", or an OAuth \
                     kind (claude-oauth, gpt-oauth, muse-code), got \"{value}\""
                ),
            )),
        }
    }
}

/// Endpoint base, model, credential, and API format for one provider.
///
/// The manual `Debug` implementation keeps the API key out of any derived
/// output, so a mislogged config cannot leak the credential.
#[derive(Clone, PartialEq, Eq)]
pub struct LlmConfig {
    pub provider: ProviderKind,
    /// Request-time authentication. OAuth kinds read and refresh credentials
    /// from the store at [`crate::oauth::Store::default_path`] unless
    /// [`LlmConfig::auth_store_path`] points elsewhere.
    pub auth: AuthKind,
    pub base_url: String,
    pub model: String,
    pub api_key: String,
    /// Overrides the credential store location; `None` uses the default. The
    /// override exists so tests can exercise the OAuth request paths against
    /// their own store instead of the user's.
    pub auth_store_path: Option<std::path::PathBuf>,
}

impl fmt::Debug for LlmConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LlmConfig")
            .field("provider", &self.provider)
            .field("auth", &self.auth)
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("api_key", &"[redacted]")
            .field("auth_store_path", &self.auth_store_path)
            .finish()
    }
}

/// Per-kind endpoint, model, and API-shape defaults, so selecting an OAuth
/// kind needs only a stored credential. Values follow the reference registry
/// (docs/evidence/oauth-providers.md).
const OAUTH_DEFAULTS: &[(AuthKind, &str, &str, ProviderKind)] = &[
    (
        AuthKind::ClaudeOauth,
        "https://api.anthropic.com",
        "claude-sonnet-5",
        ProviderKind::Anthropic,
    ),
    (
        AuthKind::GptOauth,
        "https://chatgpt.com/backend-api/codex",
        "gpt-5.2-codex",
        ProviderKind::Responses,
    ),
    (
        AuthKind::MuseSubscription,
        "https://api.meta.ai/v1",
        "muse-spark-1.3",
        ProviderKind::Responses,
    ),
    // The format is chosen per model from the account catalog.
    (
        AuthKind::DeepAi,
        crate::deepai::DEEPAI_BASE,
        "glm-5.3-flash",
        ProviderKind::OpenAi,
    ),
];

impl LlmConfig {
    /// Reads the key from `LOGFARE_API_KEY`; the API format, base URL, and
    /// model can be overridden with `INOCODE_LLM_PROVIDER`,
    /// `INOCODE_LLM_BASE_URL`, and `INOCODE_LLM_MODEL`. The default format is
    /// OpenAI-compatible, so existing configurations behave as before.
    pub fn from_env() -> Result<Self, ErrorInfo> {
        let provider_name = std::env::var(PROVIDER_ENV).unwrap_or_default();
        if let Some(auth) = AuthKind::from_provider_name(&provider_name) {
            // OAuth kinds take their credential from the store instead of the
            // key variable; the key variable stays authoritative for the
            // static-key kinds.
            let default = OAUTH_DEFAULTS
                .iter()
                .find(|row| row.0 == auth)
                .expect("every OAuth kind has defaults");
            return Ok(Self {
                provider: default.3,
                auth,
                base_url: std::env::var(BASE_URL_ENV).unwrap_or_else(|_| default.1.to_string()),
                model: std::env::var(MODEL_ENV).unwrap_or_else(|_| default.2.to_string()),
                api_key: String::new(),
                auth_store_path: None,
            });
        }
        let api_key = std::env::var(API_KEY_ENV).map_err(|_| {
            ErrorInfo::new("provider_unconfigured", format!("{API_KEY_ENV} is not set"))
        })?;
        if api_key.trim().is_empty() {
            return Err(ErrorInfo::new(
                "provider_unconfigured",
                format!("{API_KEY_ENV} is empty"),
            ));
        }
        Ok(Self {
            provider: ProviderKind::from_env()?,
            auth: AuthKind::ApiKey,
            base_url: std::env::var(BASE_URL_ENV).unwrap_or_else(|_| DEFAULT_BASE_URL.to_string()),
            model: std::env::var(MODEL_ENV).unwrap_or_else(|_| DEFAULT_MODEL.to_string()),
            api_key,
            auth_store_path: None,
        })
    }
}

/// Chat client for one API format. Holds only the HTTP client and configuration.
pub struct Llm {
    client: reqwest::Client,
    config: LlmConfig,
    /// Check-in, catalog and slot state of a DeepAI account.
    deepai: Option<Box<crate::deepai::Session>>,
}

impl Llm {
    pub fn new(config: LlmConfig) -> Result<Self, ErrorInfo> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|error| {
                ErrorInfo::new(
                    "provider_config",
                    format!("cannot build the HTTP client: {error}"),
                )
            })?;
        let deepai =
            (config.auth == AuthKind::DeepAi).then(|| Box::new(crate::deepai::Session::new()));
        Ok(Self {
            client,
            config,
            deepai,
        })
    }

    pub fn from_env() -> Result<Self, ErrorInfo> {
        Self::new(LlmConfig::from_env()?)
    }

    /// Starts a streaming completion and returns the deltas as they arrive.
    pub async fn chat(&self, prompt: &str) -> Result<ChatStream, ErrorInfo> {
        if let Some(session) = &self.deepai {
            return self.deepai_chat(session, prompt).await;
        }
        match self.config.provider {
            ProviderKind::OpenAi => self.openai_chat(prompt).await,
            ProviderKind::Anthropic => self.anthropic_chat(prompt).await,
            ProviderKind::Responses => self.responses_chat(prompt).await,
        }
    }

    /// Loads the credential for an OAuth kind, refreshing it first when it is
    /// expired. Static-key kinds never touch the store.
    async fn credential(&self) -> Result<Credential, ErrorInfo> {
        let path = match &self.config.auth_store_path {
            Some(path) => path.clone(),
            None => crate::oauth::Store::default_path()
                .map_err(|error| ErrorInfo::new("provider_config", error))?,
        };
        let store = crate::oauth::Store::load(path.clone())
            .map_err(|error| ErrorInfo::new("provider_config", error))?;
        let name = self.config.auth.store_name();
        let credential = store.credential(name).cloned().ok_or_else(|| {
            ErrorInfo::new(
                "provider_config",
                format!("no stored credential for {name}; run the login first"),
            )
        })?;
        if crate::oauth::is_fresh(&credential, crate::oauth::now_ms()) {
            return Ok(credential);
        }
        let auth = self.config.auth;
        if matches!(
            auth,
            AuthKind::MuseSubscription | AuthKind::ApiKey | AuthKind::DeepAi
        ) {
            // The Muse and DeepAI keys are static; the Muse account token
            // exists only to mint its key.
            return Ok(credential);
        }
        crate::oauth::refresh_stored(path, name, |refresh| async move {
            if auth == AuthKind::GptOauth {
                crate::oauth::refresh_gpt(&FlowEndpoints::chatgpt(), &refresh).await
            } else {
                crate::oauth::refresh_claude(&FlowEndpoints::anthropic(), &refresh).await
            }
        })
        .await
        .map_err(|error| match error {
            crate::oauth::RefreshError::Store(error) => ErrorInfo::new("provider_config", error),
            crate::oauth::RefreshError::Refresh(error) => ErrorInfo::new(
                "provider_http",
                format!("credential refresh failed: {error}"),
            ),
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{}", self.config.base_url.trim_end_matches('/'), path)
    }

    /// Anthropic clients conventionally receive the base URL without `/v1`
    /// (the official SDK and `ANTHROPIC_BASE_URL` users set the host root, and
    /// the API path is `/v1/messages`). A base that ends in `/v1` — such as the
    /// OpenAI-style default `https://logfare.ai/v1` — is trimmed first so both
    /// forms reach the same endpoint.
    fn anthropic_url(&self) -> String {
        let base = self.config.base_url.trim_end_matches('/');
        let base = base.strip_suffix("/v1").unwrap_or(base);
        format!("{base}/v1/messages")
    }

    async fn openai_chat(&self, prompt: &str) -> Result<ChatStream, ErrorInfo> {
        let body = chat_completions_body(&self.config.model, prompt);
        let response = self
            .client
            .post(self.url("chat/completions"))
            .bearer_auth(&self.config.api_key)
            .header("accept", "text/event-stream")
            .json(&body)
            .send()
            .await
            .map_err(|error| ErrorInfo::new("provider_http", format!("request failed: {error}")))?;
        let response = require_success(response).await?;
        Ok(ChatStream::new(StreamFormat::OpenAi, response))
    }

    async fn anthropic_chat(&self, prompt: &str) -> Result<ChatStream, ErrorInfo> {
        let mut body = json!({
            "model": self.config.model,
            "max_tokens": DEFAULT_MAX_TOKENS,
            "messages": [{ "role": "user", "content": prompt }],
            "stream": true,
        });
        let mut request = self.client.post(self.anthropic_url());
        if self.config.auth == AuthKind::ClaudeOauth {
            // OAuth bearers are only accepted with the beta opt-in and the
            // Claude Code identity block ahead of the conversation.
            let credential = self.credential().await?;
            body["system"] = json!([{
                "type": "text",
                "text": crate::oauth::CLAUDE_CODE_SYSTEM_INSTRUCTION,
            }]);
            request = request
                .bearer_auth(&credential.access)
                .header("anthropic-beta", crate::oauth::CLAUDE_CODE_BETA);
        } else {
            request = request.header("x-api-key", &self.config.api_key);
        }
        let response = request
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("accept", "text/event-stream")
            .json(&body)
            .send()
            .await
            .map_err(|error| ErrorInfo::new("provider_http", format!("request failed: {error}")))?;
        let response = require_success(response).await?;
        Ok(ChatStream::new(StreamFormat::Anthropic, response))
    }

    /// The account's usage: rate-limit windows for the OAuth kinds, the
    /// Theos allowance for DeepAI. Static-key kinds have none.
    pub async fn usage(&self) -> Result<crate::Usage, ErrorInfo> {
        if let Some(session) = &self.deepai {
            let api_key = deepai_api_key(&self.credential().await?)?;
            let theos = session
                .theos(&self.client, &self.config.base_url, &api_key)
                .await?;
            return Ok(crate::Usage {
                plan: None,
                windows: Vec::new(),
                theos: Some(theos),
            });
        }
        let base = self.config.base_url.trim_end_matches('/');
        let (request, parse): (reqwest::RequestBuilder, UsageParser) =
            match self.config.auth {
                AuthKind::GptOauth => {
                    let credential = self.credential().await?;
                    let backend = base.strip_suffix("/codex").unwrap_or(base);
                    (
                        self.client
                            .get(format!("{backend}/wham/usage"))
                            .timeout(USAGE_TIMEOUT)
                            .bearer_auth(&credential.access)
                            .header("ChatGPT-Account-Id", chatgpt_account_id(&credential)?),
                        crate::usage::from_wham,
                    )
                }
                AuthKind::ClaudeOauth => {
                    let credential = self.credential().await?;
                    let host = base.strip_suffix("/v1").unwrap_or(base);
                    (
                        self.client
                            .get(format!("{host}/api/oauth/usage"))
                            .timeout(USAGE_TIMEOUT)
                            .header("accept", "application/json, text/plain, */*")
                            .header("content-type", "application/json")
                            .header("user-agent", CLAUDE_USAGE_USER_AGENT)
                            .header("anthropic-beta", CLAUDE_USAGE_BETA)
                            .bearer_auth(&credential.access),
                        crate::usage::from_anthropic,
                    )
                }
                AuthKind::MuseSubscription => {
                    let credential = self.credential().await?;
                    let token = credential.oauth_access_token.clone().ok_or_else(|| {
                        ErrorInfo::new(
                        "provider_config",
                        "the stored Muse credential has no Meta account token; run the login again",
                    )
                    })?;
                    let host = base.strip_suffix("/v1").unwrap_or(base);
                    // No `onboard`: this is a read, and onboarding on a read
                    // would change the user's account (reference
                    // `muse-key-quota.ts`).
                    (
                        self.client
                            .post(format!("{host}/muse-code/key"))
                            .timeout(MUSE_USAGE_TIMEOUT)
                            .header("accept", "application/json")
                            .header("content-type", "application/json")
                            .header("x-api-version", crate::oauth::MUSE_API_VERSION)
                            .bearer_auth(token)
                            .body("{}"),
                        crate::usage::from_muse,
                    )
                }
                AuthKind::ApiKey | AuthKind::DeepAi => return Err(ErrorInfo::new(
                    "provider_unsupported",
                    "usage is only available for claude-oauth, gpt-oauth, muse-code, and deepai",
                )),
            };
        let response = request
            .send()
            .await
            .map_err(|error| ErrorInfo::new("provider_http", format!("request failed: {error}")))?;
        let status = response.status();
        if !status.is_success() {
            // The body is not echoed: the Muse answer can carry its key.
            return Err(ErrorInfo::new(
                "provider_http",
                format!("the usage request returned HTTP {}", status.as_u16()),
            ));
        }
        let mut response = response;
        let bytes = read_capped(&mut response, USAGE_BODY_MAX_BYTES)
            .await
            .map_err(|error| ErrorInfo::new("provider_stream", format!("stream failed: {error}")))?
            .ok_or_else(|| {
                ErrorInfo::new("provider_protocol", "the usage response is too large")
            })?;
        let value: Value = serde_json::from_slice(&bytes)
            .map_err(|_| ErrorInfo::new("provider_protocol", "the usage response is not JSON"))?;
        parse(&value)
    }

    /// A DeepAI chat: the catalog picks the format, a metered model waits
    /// for the single metered slot, and every attempt renews the check-in
    /// first and waits out `session_busy` (reference `localTransport`).
    async fn deepai_chat(
        &self,
        session: &crate::deepai::Session,
        prompt: &str,
    ) -> Result<ChatStream, ErrorInfo> {
        let api_key = deepai_api_key(&self.credential().await?)?;
        let base = &self.config.base_url;
        let route = session
            .route(&self.client, base, &api_key, &self.config.model)
            .await?;
        let slot = if route.metered {
            Some(session.metered_slot().await)
        } else {
            None
        };
        let (format, url, body) = if route.responses {
            (
                StreamFormat::Responses,
                self.responses_url(),
                responses_body(&self.config.model, prompt),
            )
        } else {
            (
                StreamFormat::OpenAi,
                self.url("chat/completions"),
                chat_completions_body(&self.config.model, prompt),
            )
        };
        loop {
            session
                .ensure_checked_in(&self.client, base, &api_key)
                .await?;
            let request = self
                .client
                .post(&url)
                .header("content-type", "application/json")
                .header("accept", "text/event-stream")
                .json(&body);
            let response = session
                .headers(request, &api_key)
                .await
                .send()
                .await
                .map_err(|error| {
                    ErrorInfo::new("provider_http", format!("request failed: {error}"))
                })?;
            match crate::deepai::admission(response).await {
                crate::deepai::Admission::Answered(response) => {
                    let response = crate::deepai::require_lane(response).await?;
                    let mut stream = ChatStream::new(format, response);
                    stream.slot = slot;
                    return Ok(stream);
                }
                crate::deepai::Admission::Busy(delay) => tokio::time::sleep(delay).await,
                crate::deepai::Admission::Refused(error) => return Err(error),
            }
        }
    }

    /// The Responses backend of the ChatGPT/Codex and Meta Muse providers.
    /// The path is appended to the base as-is; the Muse registry base already
    /// carries `/v1`.
    fn responses_url(&self) -> String {
        format!("{}/responses", self.config.base_url.trim_end_matches('/'))
    }

    async fn responses_chat(&self, prompt: &str) -> Result<ChatStream, ErrorInfo> {
        let credential = self.credential().await?;
        let bearer = match self.config.auth {
            AuthKind::MuseSubscription => muse_api_key(&credential)?,
            // The ChatGPT account access token is the bearer; the Muse account
            // token must never be sent to the model API (measured 401).
            _ => credential.access.clone(),
        };
        let body = responses_body(&self.config.model, prompt);
        let mut request = self
            .client
            .post(self.responses_url())
            .bearer_auth(&bearer)
            .header("accept", "text/event-stream");
        request = match self.config.auth {
            AuthKind::GptOauth => {
                request.header("chatgpt-account-id", chatgpt_account_id(&credential)?)
            }
            AuthKind::MuseSubscription => request
                .header("user-agent", crate::oauth::MUSE_USER_AGENT)
                .header("x-api-version", crate::oauth::MUSE_API_VERSION),
            _ => request,
        };
        let response =
            request.json(&body).send().await.map_err(|error| {
                ErrorInfo::new("provider_http", format!("request failed: {error}"))
            })?;
        let response = require_success(response).await?;
        Ok(ChatStream::new(StreamFormat::Responses, response))
    }

    /// The discovery endpoint for the configured kind, following the
    /// reference `buildModelsRequest`: OpenAI-style `{base}/models` for
    /// everything except Anthropic, whose endpoint is `{base}/v1/models`
    /// with the maximum page size. The ChatGPT backend additionally requires
    /// `client_version` on `/models`.
    fn models_url(&self) -> String {
        let base = self.config.base_url.trim_end_matches('/');
        match self.config.provider {
            ProviderKind::Anthropic => {
                let base = base.strip_suffix("/v1").unwrap_or(base);
                format!("{base}/v1/models?limit=1000")
            }
            ProviderKind::Responses if self.config.auth == AuthKind::GptOauth => {
                let base = base.strip_suffix("/models").unwrap_or(base);
                format!("{base}/models?client_version={}", codex_client_version())
            }
            _ => {
                // The reference `providerModelsUrl` also tolerates a base that
                // already ends in `/models`.
                let base = base.strip_suffix("/models").unwrap_or(base);
                format!("{base}/models")
            }
        }
    }

    /// Lists the models the configured endpoint advertises.
    pub async fn models(&self) -> Result<Vec<ModelInfo>, ErrorInfo> {
        if let Some(session) = &self.deepai {
            let api_key = deepai_api_key(&self.credential().await?)?;
            let catalog = session
                .catalog(&self.client, &self.config.base_url, &api_key, true)
                .await?;
            return Ok(catalog
                .into_iter()
                .map(|model| ModelInfo {
                    id: model.id,
                    display_name: model.name,
                    owned_by: None,
                })
                .collect());
        }
        let mut request = self
            .client
            .get(self.models_url())
            .timeout(MODELS_TIMEOUT)
            .header("accept", "application/json");
        request = match (self.config.provider, self.config.auth) {
            (ProviderKind::Anthropic, AuthKind::ClaudeOauth) => {
                let credential = self.credential().await?;
                request
                    .bearer_auth(&credential.access)
                    .header("anthropic-beta", crate::oauth::CLAUDE_CODE_BETA)
            }
            (ProviderKind::Anthropic, _) => request.header("x-api-key", &self.config.api_key),
            (_, AuthKind::MuseSubscription) => {
                // The Meta account token is rejected by `/v1/models`; the
                // subscription key is the bearer, as for inference.
                let credential = self.credential().await?;
                request
                    .bearer_auth(muse_api_key(&credential)?)
                    .header("user-agent", crate::oauth::MUSE_USER_AGENT)
                    .header("x-api-version", crate::oauth::MUSE_API_VERSION)
            }
            (_, AuthKind::GptOauth) => {
                let credential = self.credential().await?;
                request
                    .bearer_auth(&credential.access)
                    .header("chatgpt-account-id", chatgpt_account_id(&credential)?)
            }
            _ => request.bearer_auth(&self.config.api_key),
        };
        // Every Anthropic request carries the version header, including OAuth
        // (the API rejects discovery without it).
        if self.config.provider == ProviderKind::Anthropic {
            request = request.header("anthropic-version", ANTHROPIC_VERSION);
        }
        let response = request
            .send()
            .await
            .map_err(|error| ErrorInfo::new("provider_http", format!("request failed: {error}")))?;
        let response = require_success(response).await?;
        let body = read_bounded_body(response).await?;
        let value: Value = serde_json::from_str(&body).map_err(|error| {
            ErrorInfo::new(
                "provider_protocol",
                format!("invalid discovery payload: {error}"),
            )
        })?;
        models_from_json(&value)
    }
}

/// Reads one provider's usage payload.
type UsageParser = fn(&Value) -> Result<crate::Usage, ErrorInfo>;

/// Chat Completions body for one user prompt.
fn chat_completions_body(model: &str, prompt: &str) -> Value {
    json!({
        "model": model,
        "stream": true,
        "messages": [{ "role": "user", "content": prompt }],
    })
}

/// The minimal Responses body the real backends accept (reference
/// `warmup.ts`).
fn responses_body(model: &str, prompt: &str) -> Value {
    json!({
        "model": model,
        "input": [{
            "type": "message",
            "role": "user",
            "content": [{ "type": "input_text", "text": prompt }],
        }],
        "stream": true,
        "store": false,
    })
}

/// The DeepAI account key, sent as `api-key`.
fn deepai_api_key(credential: &Credential) -> Result<String, ErrorInfo> {
    credential.api_key.clone().ok_or_else(|| {
        ErrorInfo::new(
            "provider_config",
            "the stored DeepAI credential has no api_key; run the login again",
        )
    })
}

/// The Muse subscription key, the bearer of every Muse model request.
fn muse_api_key(credential: &Credential) -> Result<String, ErrorInfo> {
    credential.api_key.clone().ok_or_else(|| {
        ErrorInfo::new(
            "provider_config",
            "the stored Muse credential has no api_key; run the login again",
        )
    })
}

/// The ChatGPT account the Codex backend routes a request to.
fn chatgpt_account_id(credential: &Credential) -> Result<String, ErrorInfo> {
    credential.account_id.clone().ok_or_else(|| {
        ErrorInfo::new(
            "provider_config",
            "the stored ChatGPT credential has no account id; run the login again",
        )
    })
}

/// One model advertised by the provider's discovery endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelInfo {
    pub id: String,
    pub display_name: Option<String>,
    pub owned_by: Option<String>,
}

/// Extracts the model list from a discovery payload. OpenAI-style responses
/// carry `owned_by`, Anthropic's carry `display_name`, and the ChatGPT/Codex
/// roster uses `models` with `slug` rows gated by `supported_in_api` and
/// `visibility` (reference `parseAccountModels`). Entries without a usable id
/// are skipped, and a payload without a list is invalid.
fn models_from_json(value: &Value) -> Result<Vec<ModelInfo>, ErrorInfo> {
    let data = value
        .get("data")
        .and_then(Value::as_array)
        .or_else(|| value.get("models").and_then(Value::as_array))
        .ok_or_else(|| {
            ErrorInfo::new("provider_protocol", "discovery response has no model list")
        })?;
    Ok(data
        .iter()
        .filter_map(|item| {
            let id = item
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.trim().is_empty())
                .or_else(|| {
                    // Codex roster rows: only `slug`s the account may use.
                    let usable = item.get("supported_in_api").and_then(Value::as_bool)
                        == Some(true)
                        && item.get("visibility").and_then(Value::as_str) != Some("hide");
                    usable
                        .then(|| item.get("slug"))
                        .flatten()
                        .and_then(Value::as_str)
                        .filter(|slug| !slug.trim().is_empty())
                })?
                .trim();
            Some(ModelInfo {
                id: id.to_string(),
                display_name: item
                    .get("display_name")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                owned_by: item
                    .get("owned_by")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            })
        })
        .collect())
}

/// The `client_version` reported to the ChatGPT models endpoint: the env
/// override when it is a usable version string, else the discovery version
/// that sees the whole roster (the reference rejects `0.0.0`-style
/// placeholders because upstream answers them with an empty list).
fn codex_client_version() -> String {
    let candidate = std::env::var(CODEX_CLIENT_VERSION_ENV).unwrap_or_default();
    let trimmed = candidate.trim();
    let usable = !trimmed.is_empty()
        && trimmed.len() <= 64
        && trimmed != "0.0.0"
        && trimmed
            .split(['-', '+'])
            .next()
            .map(|core| {
                core.split('.').any(|segment| {
                    segment
                        .parse::<u64>()
                        .map(|value| value != 0)
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false)
        && trimmed
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '+');
    if usable {
        trimmed.to_string()
    } else {
        CODEX_DISCOVERY_CLIENT_VERSION.to_string()
    }
}

/// Reads the response body with the same 16 MiB cap as the SSE streams, so a
/// discovery endpoint cannot make the client allocate without bound.
pub(crate) async fn read_bounded_body(
    mut response: reqwest::Response,
) -> Result<String, ErrorInfo> {
    const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;
    let body = read_capped(&mut response, MAX_BODY_BYTES)
        .await
        .map_err(|error| ErrorInfo::new("provider_stream", format!("stream failed: {error}")))?
        .ok_or_else(|| ErrorInfo::new("provider_protocol", "discovery response exceeds 16 MiB"))?;
    String::from_utf8(body)
        .map_err(|_| ErrorInfo::new("provider_protocol", "discovery response is not valid UTF-8"))
}

/// Largest error body read for the failure message (reference
/// `CHATGPT_ERROR_BODY_MAX_BYTES`); only a short snippet is shown anyway.
pub(crate) const ERROR_BODY_MAX_BYTES: usize = 16 * 1024;

/// Reads at most `max` body bytes. `None` means the body is longer: reading
/// stops there, so an oversized body never costs more than `max` bytes.
pub(crate) async fn read_capped(
    response: &mut reqwest::Response,
    max: usize,
) -> Result<Option<Vec<u8>>, reqwest::Error> {
    let mut body: Vec<u8> = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len() + chunk.len() > max {
            return Ok(None);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Some(body))
}

pub(crate) async fn require_success(
    mut response: reqwest::Response,
) -> Result<reqwest::Response, ErrorInfo> {
    let status = response.status();
    if !status.is_success() {
        let body = match read_capped(&mut response, ERROR_BODY_MAX_BYTES).await {
            Ok(Some(bytes)) => String::from_utf8_lossy(&bytes).into_owned(),
            Ok(None) | Err(_) => String::new(),
        };
        return Err(ErrorInfo::new(
            "provider_http",
            format!(
                "provider returned HTTP {}: {}",
                status.as_u16(),
                snippet(&body)
            ),
        ));
    }
    Ok(response)
}

fn snippet(body: &str) -> String {
    const LIMIT: usize = 300;
    let trimmed = body.trim();
    if trimmed.chars().count() <= LIMIT {
        trimmed.to_string()
    } else {
        format!("{}…", trimmed.chars().take(LIMIT).collect::<String>())
    }
}

/// The SSE format a [`ChatStream`] reads.
#[derive(Debug, Clone, Copy)]
enum StreamFormat {
    OpenAi,
    Anthropic,
    Responses,
}

/// What one SSE `data` payload means for the answer.
enum StreamItem {
    Text(String),
    /// Carries no answer text; keep reading.
    Skip,
    /// The provider ended the answer.
    Finish,
}

/// Text deltas of one completion, in generation order.
pub struct ChatStream {
    format: StreamFormat,
    response: reqwest::Response,
    parser: SseParser,
    body_ended: bool,
    done: bool,
    /// The provider sent its completion signal (`[DONE]`, a finish reason,
    /// or a terminal Responses event).
    signalled: bool,
    /// Keeps a metered DeepAI request's slot until the stream is dropped.
    slot: Option<tokio::sync::OwnedSemaphorePermit>,
}

impl ChatStream {
    fn new(format: StreamFormat, response: reqwest::Response) -> Self {
        Self {
            format,
            response,
            parser: SseParser::new(),
            body_ended: false,
            done: false,
            signalled: false,
            slot: None,
        }
    }

    /// Returns the next text delta, or `None` once the provider finished.
    ///
    /// A body that ends before the provider signalled completion is an error,
    /// so a truncated answer is never reported as complete.
    pub async fn next(&mut self) -> Result<Option<String>, ErrorInfo> {
        if self.done {
            return Ok(None);
        }
        loop {
            match self.parser.next_item() {
                Some(Ok(data)) => match self.interpret(&data)? {
                    StreamItem::Text(text) => return Ok(Some(text)),
                    StreamItem::Skip => continue,
                    StreamItem::Finish => {
                        self.done = true;
                        return Ok(None);
                    }
                },
                Some(Err(error)) => {
                    self.done = true;
                    return Err(error);
                }
                None => {}
            }
            if self.body_ended {
                self.done = true;
                return self.end_of_body();
            }
            match self.response.chunk().await {
                Ok(Some(chunk)) => self.parser.push(&chunk),
                Ok(None) => {
                    self.parser.end();
                    self.body_ended = true;
                }
                Err(error) => {
                    self.done = true;
                    return Err(ErrorInfo::new(
                        "provider_stream",
                        format!("stream failed: {error}"),
                    ));
                }
            }
        }
    }

    fn interpret(&mut self, data: &str) -> Result<StreamItem, ErrorInfo> {
        Ok(match self.format {
            StreamFormat::OpenAi => {
                text_or_skip(openai_delta(data, &mut self.done, &mut self.signalled)?)
            }
            StreamFormat::Anthropic => match anthropic_event(data)? {
                AnthropicEvent::Text(text) => StreamItem::Text(text),
                AnthropicEvent::Stop => StreamItem::Finish,
                AnthropicEvent::Ignored => StreamItem::Skip,
            },
            StreamFormat::Responses => {
                text_or_skip(responses_delta(data, &mut self.done, &mut self.signalled)?)
            }
        })
    }

    /// The outcome once the body is over and the parser has nothing left.
    fn end_of_body(&self) -> Result<Option<String>, ErrorInfo> {
        match self.format {
            // Only a provider that signalled completion finished successfully.
            StreamFormat::OpenAi if self.signalled => Ok(None),
            StreamFormat::OpenAi => Err(ErrorInfo::new(
                "provider_stream",
                "the provider stream ended before it signalled completion",
            )),
            // `message_stop` is the only completion signal; a body that ends
            // without it is a truncated answer, never a success.
            StreamFormat::Anthropic => Err(ErrorInfo::new(
                "provider_stream",
                "the provider stream ended before message_stop",
            )),
            // The reference passthrough adapter ends the turn at end of body:
            // buffered deltas are the answer, and only a
            // `response.failed`/`error` event fails it mid-stream.
            StreamFormat::Responses => Ok(None),
        }
    }
}

fn text_or_skip(text: Option<String>) -> StreamItem {
    match text {
        Some(text) => StreamItem::Text(text),
        None => StreamItem::Skip,
    }
}

/// Interprets one Chat Completions `data` payload, updating the completion
/// signals on the stream.
fn openai_delta(
    data: &str,
    done: &mut bool,
    signalled: &mut bool,
) -> Result<Option<String>, ErrorInfo> {
    if data.trim() == "[DONE]" {
        *done = true;
        *signalled = true;
        return Ok(None);
    }
    let value: Value = serde_json::from_str(data).map_err(|error| {
        ErrorInfo::new("provider_protocol", format!("invalid SSE payload: {error}"))
    })?;
    if let Some(error) = value.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("provider reported an error in the stream");
        return Err(ErrorInfo::new("provider_stream", message.to_string()));
    }
    let choice = value.get("choices").and_then(|choices| choices.get(0));
    if choice
        .and_then(|choice| choice.get("finish_reason"))
        .is_some_and(|reason| !reason.is_null())
    {
        *signalled = true;
    }
    let content = choice
        .and_then(|choice| choice.get("delta"))
        .and_then(|delta| delta.get("content"))
        .and_then(Value::as_str)
        .unwrap_or("");
    Ok(if content.is_empty() {
        None
    } else {
        Some(content.to_string())
    })
}

/// One Anthropic Messages SSE event reduced to what this protocol needs.
#[derive(Debug)]
enum AnthropicEvent {
    Text(String),
    Stop,
    Ignored,
}

fn anthropic_event(data: &str) -> Result<AnthropicEvent, ErrorInfo> {
    let value: Value = serde_json::from_str(data).map_err(|error| {
        ErrorInfo::new("provider_protocol", format!("invalid SSE payload: {error}"))
    })?;
    match value.get("type").and_then(Value::as_str) {
        Some("content_block_delta") => {
            let delta = value.get("delta");
            // Only `text_delta` carries answer text; `input_json_delta` and
            // other delta types must not be concatenated into the answer.
            if delta
                .and_then(|delta| delta.get("type"))
                .and_then(Value::as_str)
                == Some("text_delta")
            {
                Ok(
                    match delta
                        .and_then(|delta| delta.get("text"))
                        .and_then(Value::as_str)
                    {
                        Some(text) if !text.is_empty() => AnthropicEvent::Text(text.to_string()),
                        _ => AnthropicEvent::Ignored,
                    },
                )
            } else {
                Ok(AnthropicEvent::Ignored)
            }
        }
        Some("message_stop") => Ok(AnthropicEvent::Stop),
        Some("error") => {
            let message = value
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("provider reported an error in the stream");
            Err(ErrorInfo::new("provider_stream", message.to_string()))
        }
        // `message_start`, `content_block_start`/`_stop`, `message_delta`,
        // `ping`, and unknown event types carry no answer text. The streaming
        // spec requires handling unknown event types gracefully.
        Some(_) => Ok(AnthropicEvent::Ignored),
        None => Err(ErrorInfo::new(
            "provider_protocol",
            "SSE payload has no \"type\" field",
        )),
    }
}

/// Interprets one Responses `data` payload, updating the completion signals on
/// the stream. Terminal and error rules follow the reference passthrough
/// adapter (`parseStream` in `src/adapters/openai-responses/passthrough.ts`):
/// a data payload that is not a JSON object is skipped, never an error, which
/// is how the reference absorbs the terminal `data: [DONE]` line.
fn responses_delta(
    data: &str,
    done: &mut bool,
    signalled: &mut bool,
) -> Result<Option<String>, ErrorInfo> {
    let value: Value = match serde_json::from_str(data) {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    if !value.is_object() {
        return Ok(None);
    }
    match value.get("type").and_then(Value::as_str) {
        // The only event carrying answer text.
        Some("response.output_text.delta") => {
            let text = value.get("delta").and_then(Value::as_str).unwrap_or("");
            Ok(if text.is_empty() {
                None
            } else {
                Some(text.to_string())
            })
        }
        // Both end the answer; `incomplete` means the provider stopped early
        // (length or content filter), which still produces the text it sent.
        Some("response.completed") | Some("response.incomplete") => {
            *done = true;
            *signalled = true;
            Ok(None)
        }
        Some("response.failed") | Some("error") => {
            let inner = if value.get("type").and_then(Value::as_str) == Some("response.failed") {
                value.get("response").unwrap_or(&value).clone()
            } else {
                value.clone()
            };
            *done = true;
            Err(ErrorInfo::new(
                "provider_stream",
                responses_error_message(&inner),
            ))
        }
        // `response.created`, `response.output_item.*`,
        // `response.content_part.*`, `response.output_text.done`,
        // `response.subscription_usage`, `response.heartbeat`, reasoning
        // summary deltas, and unknown types carry no answer text.
        Some(_) => Ok(None),
        None => Ok(None),
    }
}

/// The failure text of a `response.failed`/`error` payload, following the
/// reference `responsesErrorMessage`: a string `error`, then `error.message`,
/// then `incomplete_details.reason`, then a fallback.
fn responses_error_message(payload: &Value) -> String {
    match payload.get("error") {
        Some(Value::String(message)) => return message.clone(),
        Some(error) if error.is_object() => {
            if let Some(message) = error.get("message").and_then(Value::as_str) {
                return message.to_string();
            }
        }
        _ => {}
    }
    if let Some(reason) = payload
        .get("incomplete_details")
        .and_then(|details| details.get("reason"))
        .and_then(Value::as_str)
    {
        return reason.to_string();
    }
    "provider reported an error in the stream".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn responses_delta_reads_deltas_and_ignores_non_json_data_lines() {
        let mut done = false;
        let mut signalled = false;
        assert_eq!(
            responses_delta(
                r#"{"type":"response.output_text.delta","delta":"hello"}"#,
                &mut done,
                &mut signalled,
            )
            .expect("delta")
            .as_deref(),
            Some("hello")
        );
        assert!(!done && !signalled);
        // Non-JSON data payloads (the terminal `data: [DONE]`, keepalives) are
        // skipped, never errors: the reference `JSON.parse` failure continues.
        assert_eq!(
            responses_delta("[DONE]", &mut done, &mut signalled).expect("skipped"),
            None
        );
        assert_eq!(
            responses_delta("", &mut done, &mut signalled).expect("skipped"),
            None
        );
        assert_eq!(
            responses_delta(
                r#"{"type":"response.output_item.added","item":{}}"#,
                &mut done,
                &mut signalled,
            )
            .expect("ignored"),
            None
        );
        // `completed` is the success terminal.
        responses_delta(
            r#"{"type":"response.completed"}"#,
            &mut done,
            &mut signalled,
        )
        .expect("terminal");
        assert!(done && signalled);
    }

    #[test]
    fn responses_failed_and_error_events_fail_the_stream() {
        let mut done = false;
        let mut signalled = false;
        let error = responses_delta(
            r#"{"type":"response.failed","response":{"error":{"message":"quota"}}}"#,
            &mut done,
            &mut signalled,
        )
        .expect_err("failed event");
        assert_eq!(error.code, "provider_stream");
        assert_eq!(error.message, "quota");
        assert!(done);
        // The reference message lookup also accepts a string `error` and
        // `incomplete_details.reason`.
        assert_eq!(
            responses_error_message(&serde_json::json!({"error": "boom"})),
            "boom"
        );
        assert_eq!(
            responses_error_message(&serde_json::json!({
                "incomplete_details": {"reason": "max_output_tokens"}
            })),
            "max_output_tokens"
        );
        let error = responses_delta(
            r#"{"type":"error","error":"quota exceeded"}"#,
            &mut done,
            &mut signalled,
        )
        .expect_err("error event");
        assert_eq!(error.code, "provider_stream");
        assert_eq!(error.message, "quota exceeded");
    }

    #[test]
    fn models_url_follows_the_reference_discovery_shapes() {
        let build_gpt = |base: &str| {
            Llm::new(LlmConfig {
                provider: ProviderKind::Responses,
                auth: AuthKind::GptOauth,
                base_url: base.to_string(),
                model: "test-model".to_string(),
                api_key: String::new(),
                auth_store_path: Some(std::env::temp_dir().join("inocode-gpt-models-url-test")),
            })
            .expect("build the client")
        };
        let build = |provider: ProviderKind, base: &str| {
            Llm::new(LlmConfig {
                provider,
                auth: AuthKind::ApiKey,
                base_url: base.to_string(),
                model: "test-model".to_string(),
                api_key: "test-key".to_string(),
                auth_store_path: None,
            })
            .expect("build the client")
        };
        // OpenAI-style: `{base}/models`, tolerating a base that already ends
        // in `/models`.
        assert_eq!(
            build(ProviderKind::OpenAi, "https://api.example.test/v1").models_url(),
            "https://api.example.test/v1/models"
        );
        assert_eq!(
            build(ProviderKind::Responses, "https://api.meta.ai/v1").models_url(),
            "https://api.meta.ai/v1/models"
        );
        // The ChatGPT backend filters `/models` by a required `client_version`;
        // without it the request fails (see the Codex roster notes).
        assert_eq!(
            build_gpt("https://chatgpt.com/backend-api/codex/").models_url(),
            "https://chatgpt.com/backend-api/codex/models?client_version=99.0.0"
        );
        assert_eq!(
            build(
                ProviderKind::Responses,
                "https://chatgpt.com/backend-api/codex/"
            )
            .models_url(),
            "https://chatgpt.com/backend-api/codex/models"
        );
        // Anthropic: the base loses `/v1`, then `/v1/models` with the maximum
        // page size.
        assert_eq!(
            build(ProviderKind::Anthropic, "https://api.anthropic.com").models_url(),
            "https://api.anthropic.com/v1/models?limit=1000"
        );
        assert_eq!(
            build(ProviderKind::Anthropic, "https://logfare.ai/v1").models_url(),
            "https://logfare.ai/v1/models?limit=1000"
        );
    }

    #[test]
    fn codex_client_version_prefers_a_usable_override() {
        std::env::set_var(CODEX_CLIENT_VERSION_ENV, "0.146.0");
        assert_eq!(codex_client_version(), "0.146.0");
        std::env::set_var(CODEX_CLIENT_VERSION_ENV, "0.0.0");
        assert_eq!(codex_client_version(), "99.0.0");
        std::env::set_var(CODEX_CLIENT_VERSION_ENV, " ");
        assert_eq!(codex_client_version(), "99.0.0");
        std::env::remove_var(CODEX_CLIENT_VERSION_ENV);
        assert_eq!(codex_client_version(), "99.0.0");
    }

    #[test]
    fn models_from_json_reads_both_shapes_and_skips_unusable_entries() {
        // OpenAI shape: id + owned_by.
        let openai = serde_json::json!({"data": [
            {"id": "muse-spark-1.3", "owned_by": "meta"},
            {"id": ""},
            {"no-id": true},
        ]});
        let models = models_from_json(&openai).expect("openai shape");
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "muse-spark-1.3");
        assert_eq!(models[0].owned_by.as_deref(), Some("meta"));
        assert_eq!(models[0].display_name, None);
        // Anthropic shape: id + display_name.
        let anthropic = serde_json::json!({"data": [
            {"type": "model", "id": "claude-sonnet-5", "display_name": "Claude Sonnet 5"},
        ]});
        let models = models_from_json(&anthropic).expect("anthropic shape");
        assert_eq!(models[0].display_name.as_deref(), Some("Claude Sonnet 5"));
        // Codex roster shape: `models` with gated `slug` rows (the account may
        // only use API-enabled, visible models).
        let codex = serde_json::json!({"models": [
            {"slug": "gpt-5.2-codex", "supported_in_api": true, "visibility": "show", "display_name": "GPT-5.2 Codex"},
            {"slug": "hidden-model", "supported_in_api": true, "visibility": "hide"},
            {"slug": "api-only", "supported_in_api": false, "visibility": "show"},
            {"slug": "no-flag"},
        ]});
        let models = models_from_json(&codex).expect("codex shape");
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "gpt-5.2-codex");
        assert_eq!(models[0].display_name.as_deref(), Some("GPT-5.2 Codex"));
        // A payload without a model list is invalid.
        let error = models_from_json(&serde_json::json!({"other": []})).expect_err("no list");
        assert_eq!(error.code, "provider_protocol");
    }

    #[test]
    fn anthropic_url_matches_the_official_client_convention() {
        let build = |base: &str| {
            Llm::new(LlmConfig {
                provider: ProviderKind::Anthropic,
                auth: AuthKind::ApiKey,
                base_url: base.to_string(),
                model: "test-model".to_string(),
                api_key: "test-key".to_string(),
                auth_store_path: None,
            })
            .expect("build the client")
        };
        // The official convention: the base carries no `/v1`; the client appends
        // `/v1/messages`.
        assert_eq!(
            build("https://api.anthropic.com").anthropic_url(),
            "https://api.anthropic.com/v1/messages"
        );
        // An OpenAI-style base that ends in `/v1` reaches the same endpoint.
        assert_eq!(
            build("https://logfare.ai/v1").anthropic_url(),
            "https://logfare.ai/v1/messages"
        );
        assert_eq!(
            build("https://logfare.ai/v1/").anthropic_url(),
            "https://logfare.ai/v1/messages"
        );
    }

    #[test]
    fn debug_output_does_not_contain_the_api_key() {
        let config = LlmConfig {
            provider: ProviderKind::Anthropic,
            auth: AuthKind::ApiKey,
            base_url: "https://example.test/v1".to_string(),
            model: "test-model".to_string(),
            api_key: "super-secret-key-material".to_string(),
            auth_store_path: None,
        };
        let rendered = format!("{config:?}");
        assert!(!rendered.contains("super-secret-key-material"));
        assert!(rendered.contains("[redacted]"));
        assert!(rendered.contains("Anthropic"));

        // Cloning keeps the redaction: the clone must not leak either.
        let rendered = format!("{:?}", config.clone());
        assert!(!rendered.contains("super-secret-key-material"));
    }

    #[test]
    fn provider_selection_reads_the_environment() {
        let key = "provider-selection-test-key";
        std::env::set_var(API_KEY_ENV, key);
        std::env::remove_var(PROVIDER_ENV);
        assert_eq!(
            LlmConfig::from_env().expect("configured").provider,
            ProviderKind::OpenAi,
            "unset provider must default to OpenAI-compatible"
        );

        std::env::set_var(PROVIDER_ENV, "anthropic");
        assert_eq!(
            LlmConfig::from_env().expect("configured").provider,
            ProviderKind::Anthropic
        );
        std::env::set_var(PROVIDER_ENV, "OpenAI");
        assert_eq!(
            LlmConfig::from_env().expect("configured").provider,
            ProviderKind::OpenAi
        );

        std::env::set_var(PROVIDER_ENV, "ollama");
        assert_eq!(
            LlmConfig::from_env().expect_err("must fail").code,
            "provider_config"
        );

        std::env::set_var(PROVIDER_ENV, " ");
        assert_eq!(
            LlmConfig::from_env().expect("configured").provider,
            ProviderKind::OpenAi
        );

        std::env::remove_var(PROVIDER_ENV);
        std::env::remove_var(API_KEY_ENV);
    }

    #[test]
    fn anthropic_events_reduce_to_text_stop_and_ignored() {
        let text = anthropic_event(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"こん"}}"#,
        )
        .expect("text delta");
        assert!(matches!(text, AnthropicEvent::Text(ref t) if t == "こん"));

        // Non-text deltas must not become answer text.
        let thinking = anthropic_event(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hmm"}}"#,
        )
        .expect("ignored delta");
        assert!(matches!(thinking, AnthropicEvent::Ignored));

        let input_json = anthropic_event(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"a\":1}"}}"#,
        )
        .expect("ignored delta");
        assert!(matches!(input_json, AnthropicEvent::Ignored));

        let empty_text = anthropic_event(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":""}}"#,
        )
        .expect("empty text delta");
        assert!(matches!(empty_text, AnthropicEvent::Ignored));

        for event in [
            r#"{"type":"message_start","message":{}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":15}}"#,
            r#"{"type":"ping"}"#,
            r#"{"type":"message_created_someday"}"#,
        ] {
            assert!(
                matches!(
                    anthropic_event(event).expect("ignored event"),
                    AnthropicEvent::Ignored
                ),
                "{event}"
            );
        }

        assert!(matches!(
            anthropic_event(r#"{"type":"message_stop"}"#).expect("stop"),
            AnthropicEvent::Stop
        ));

        let error = anthropic_event(
            r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
        )
        .expect_err("must fail");
        assert_eq!(error.code, "provider_stream");
        assert_eq!(error.message, "Overloaded");

        let no_type = anthropic_event(r#"{"nonsense":true}"#).expect_err("must fail");
        assert_eq!(no_type.code, "provider_protocol");
    }

    #[test]
    fn openai_deltas_extract_content_and_completion_signals() {
        let mut done = false;
        let mut signalled = false;
        let text = openai_delta(
            r#"{"choices":[{"delta":{"role":"assistant"}}]}"#,
            &mut done,
            &mut signalled,
        )
        .expect("no error");
        assert!(text.is_none());
        assert!(!done);

        let text = openai_delta(
            r#"{"choices":[{"delta":{"content":"こん"}}]}"#,
            &mut done,
            &mut signalled,
        )
        .expect("no error");
        assert_eq!(text.as_deref(), Some("こん"));

        openai_delta(
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            &mut done,
            &mut signalled,
        )
        .expect("no error");
        assert!(signalled);
        assert!(!done);

        openai_delta("[DONE]", &mut done, &mut signalled).expect("no error");
        assert!(done);

        let error = openai_delta(
            r#"{"error":{"message":"rate limited"}}"#,
            &mut done,
            &mut signalled,
        )
        .expect_err("must fail");
        assert_eq!(error.code, "provider_stream");
        assert_eq!(error.message, "rate limited");
    }

    /// Release-mode parsing measurements for the streaming path. They print
    /// numbers instead of asserting; run them with
    /// `cargo test --release -p inocode-core measure_ -- --ignored --nocapture --test-threads=1`.
    mod measure {
        use super::super::*;
        use std::time::Instant;

        const ROUNDS: usize = 7;

        /// Runs `body` ROUNDS times and returns the median and minimum seconds.
        fn timed(mut body: impl FnMut()) -> (f64, f64) {
            let mut samples: Vec<f64> = (0..ROUNDS)
                .map(|_| {
                    let started = Instant::now();
                    body();
                    started.elapsed().as_secs_f64()
                })
                .collect();
            samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
            (samples[ROUNDS / 2], samples[0])
        }

        fn report(name: &str, bytes: usize, events: usize, (median, min): (f64, f64)) {
            println!(
                "{name:<44} median {:>9.3} ms  min {:>9.3} ms  {:>8.1} MiB/s  {:>6.0} ns/event",
                median * 1e3,
                min * 1e3,
                bytes as f64 / median / (1024.0 * 1024.0),
                median * 1e9 / events.max(1) as f64,
            );
        }

        /// Feeds `body` in `chunk`-byte pushes, draining after each push the
        /// way the stream does, and returns the number of data payloads.
        fn parse(body: &[u8], chunk: usize, mut each: impl FnMut(&str)) -> usize {
            let mut parser = SseParser::new();
            let mut count = 0;
            for piece in body.chunks(chunk) {
                parser.push(piece);
                while let Some(item) = parser.next_item() {
                    each(&item.expect("valid SSE"));
                    count += 1;
                }
            }
            parser.end();
            while let Some(item) = parser.next_item() {
                each(&item.expect("valid SSE"));
                count += 1;
            }
            count
        }

        fn openai_body(events: usize) -> Vec<u8> {
            let mut body = Vec::new();
            for index in 0..events {
                let payload = json!({
                    "id": "chatcmpl-measure",
                    "object": "chat.completion.chunk",
                    "model": "measure-model",
                    "choices": [{ "index": 0, "delta": { "content": format!("word{index} ") }, "finish_reason": null }],
                });
                body.extend_from_slice(format!("data: {payload}\n\n").as_bytes());
            }
            body.extend_from_slice(b"data: [DONE]\n\n");
            body
        }

        fn responses_body(events: usize) -> Vec<u8> {
            let mut body = Vec::new();
            for index in 0..events {
                let payload = json!({
                    "type": "response.output_text.delta",
                    "item_id": "msg_measure",
                    "output_index": 0,
                    "content_index": 0,
                    "delta": format!("word{index} "),
                    "sequence_number": index,
                });
                body.extend_from_slice(
                    format!("event: response.output_text.delta\ndata: {payload}\n\n").as_bytes(),
                );
            }
            body
        }

        fn anthropic_body(events: usize) -> Vec<u8> {
            let mut body = Vec::new();
            for index in 0..events {
                let payload = json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": { "type": "text_delta", "text": format!("word{index} ") },
                });
                body.extend_from_slice(
                    format!("event: content_block_delta\ndata: {payload}\n\n").as_bytes(),
                );
            }
            body.extend_from_slice(b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");
            body
        }

        #[test]
        #[ignore = "measurement: run with --release --ignored --nocapture --test-threads=1"]
        fn measure_stream_parsing() {
            const EVENTS: usize = 50_000;
            println!();
            for (name, body) in [
                ("openai 50k deltas, 16 KiB pushes", openai_body(EVENTS)),
                (
                    "responses 50k deltas, 16 KiB pushes",
                    responses_body(EVENTS),
                ),
                (
                    "anthropic 50k deltas, 16 KiB pushes",
                    anthropic_body(EVENTS),
                ),
            ] {
                let mut count = 0;
                let times = timed(|| count = parse(&body, 16 * 1024, |_| {}));
                report(&format!("sse only: {name}"), body.len(), count, times);
            }

            let body = openai_body(EVENTS);
            let mut count = 0;
            let times = timed(|| {
                let (mut done, mut signalled) = (false, false);
                count = parse(&body, 16 * 1024, |data| {
                    let _ = openai_delta(data, &mut done, &mut signalled).expect("valid");
                });
            });
            report("sse + interpret: openai", body.len(), count, times);
            let body = responses_body(EVENTS);
            let times = timed(|| {
                let (mut done, mut signalled) = (false, false);
                count = parse(&body, 16 * 1024, |data| {
                    let _ = responses_delta(data, &mut done, &mut signalled).expect("valid");
                });
            });
            report("sse + interpret: responses", body.len(), count, times);
            let body = anthropic_body(EVENTS);
            let times = timed(|| {
                count = parse(&body, 16 * 1024, |data| {
                    let _ = anthropic_event(data).expect("valid");
                });
            });
            report("sse + interpret: anthropic", body.len(), count, times);
        }

        #[test]
        #[ignore = "measurement: run with --release --ignored --nocapture --test-threads=1"]
        fn measure_credential_store_read() {
            const LOADS: usize = 1000;
            let path =
                std::env::temp_dir().join(format!("inocode-measure-store-{}", std::process::id()));
            let mut store = crate::oauth::Store::load(path.clone()).expect("load");
            for name in ["claude-oauth", "gpt-oauth", "muse-code"] {
                store
                    .update(name, |credential| {
                        credential.access = "a".repeat(1500);
                        credential.refresh = "r".repeat(100);
                        credential.expires_at_ms = Some(u64::MAX);
                    })
                    .expect("save");
            }
            let bytes = std::fs::metadata(&path).expect("stat").len() as usize;
            let times = timed(|| {
                for _ in 0..LOADS {
                    let store = crate::oauth::Store::load(path.clone()).expect("load");
                    assert!(store.credential("gpt-oauth").is_some());
                }
            });
            println!();
            report(
                &format!("Store::load of a {bytes}-byte auth.json x{LOADS}"),
                bytes * LOADS,
                LOADS,
                times,
            );
            std::fs::remove_file(&path).expect("remove");
        }

        #[test]
        #[ignore = "measurement: run with --release --ignored --nocapture --test-threads=1"]
        fn measure_sse_worst_cases() {
            println!();
            // One long data line arriving in small pieces: every push rescans
            // the whole pending line.
            let mut long = b"data: ".to_vec();
            long.extend(std::iter::repeat_n(b'a', 4 * 1024 * 1024));
            long.extend_from_slice(b"\n\n");
            let times = timed(|| {
                parse(&long, 4 * 1024, |_| {});
            });
            report("one 4 MiB line, 4 KiB pushes", long.len(), 1, times);

            // Many short events in one large push: every line shifts the rest
            // of the buffer.
            let mut many = Vec::new();
            for index in 0..20_000 {
                many.extend_from_slice(format!("data: {index}\n\n").as_bytes());
            }
            let mut count = 0;
            let times = timed(|| count = parse(&many, many.len(), |_| {}));
            report("20k short events in one push", many.len(), count, times);
        }
    }
}
