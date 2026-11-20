//! Streaming clients for OpenAI-compatible and Anthropic-compatible chat APIs.
//!
//! The transport is plain HTTP with server-sent events. The format is chosen by
//! configuration: Chat Completions (`/chat/completions`), Messages
//! (`/messages`), or Responses (`/responses`). Authentication is either a
//! static API key or a stored OAuth/subscription credential
//! ([`crate::oauth`]), refreshed at request time when it has expired.

use std::fmt;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::conversation::{
    self, AssistantPart, Body, Conversation, Message, Origin, Replay, Speaker, StopReason,
    ToolSpec, TurnBuilder,
};
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
/// The key of a colleague model on a static-key provider.
const COLLEAGUE_API_KEY_ENV: &str = "INOCODE_COLLEAGUE_API_KEY";
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
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
    /// Thinking depth and summary sent with every request.
    pub reasoning: crate::reasoning::Reasoning,
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
            .field("reasoning", &self.reasoning)
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
                reasoning: crate::reasoning::Reasoning::from_env()?,
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
            reasoning: crate::reasoning::Reasoning::from_env()?,
        })
    }
}

/// Chat client for one API format. Holds only the HTTP client and configuration.
pub struct Llm {
    client: reqwest::Client,
    config: LlmConfig,
    /// Check-in, catalog and slot state of a DeepAI account.
    deepai: Option<Box<crate::deepai::Session>>,
    /// The configured model's catalog default effort, looked up once.
    catalog_effort: tokio::sync::OnceCell<Option<crate::reasoning::Effort>>,
    /// The configured model's context window, looked up once.
    catalog_window: tokio::sync::OnceCell<Option<u64>>,
    /// The configured model's output limit, looked up once.
    catalog_output: tokio::sync::OnceCell<Option<u64>>,
    /// Per thread, bytes per input token of the latest request this model
    /// measured: threads differ in language and content, so one thread's
    /// rate does not stand for another's.
    bytes_per_token: std::sync::Mutex<std::collections::HashMap<i64, f64>>,
    /// Clients for other models on this provider, made once and kept, so a
    /// DeepAI account is not checked in again for each turn.
    siblings: std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<Llm>>>,
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
            catalog_effort: tokio::sync::OnceCell::new(),
            catalog_window: tokio::sync::OnceCell::new(),
            catalog_output: tokio::sync::OnceCell::new(),
            bytes_per_token: std::sync::Mutex::new(std::collections::HashMap::new()),
            siblings: std::sync::Mutex::new(std::collections::HashMap::new()),
        })
    }

    pub fn from_env() -> Result<Self, ErrorInfo> {
        Self::new(LlmConfig::from_env()?)
    }

    /// The configured model.
    /// The provider as logged: the account kind for sign-in providers, else
    /// the API format and the endpoint's host.
    pub(crate) fn provider_label(&self) -> String {
        if self.config.auth != AuthKind::ApiKey {
            return self.config.auth.store_name().to_string();
        }
        let format = match self.config.provider {
            ProviderKind::OpenAi => "openai",
            ProviderKind::Anthropic => "anthropic",
            ProviderKind::Responses => "responses",
        };
        match reqwest::Url::parse(&self.config.base_url)
            .ok()
            .and_then(|url| url.host_str().map(str::to_string))
        {
            Some(host) => format!("{format}@{host}"),
            None => format.to_string(),
        }
    }

    /// How requests to this provider are paid for, as the catalog shows it.
    pub(crate) fn billing(&self) -> &'static str {
        match self.config.auth {
            AuthKind::ApiKey => "api",
            AuthKind::DeepAi => "theos",
            AuthKind::ClaudeOauth | AuthKind::GptOauth | AuthKind::MuseSubscription => {
                "plan_included"
            }
        }
    }

    pub(crate) fn http(&self) -> &reqwest::Client {
        &self.client
    }

    /// The plan of a Claude sign-in (`rate_limit_tier`, else the
    /// organization type, of `/api/oauth/profile`); `None` for other
    /// providers or when it cannot be read.
    pub(crate) async fn claude_plan(&self) -> Option<String> {
        if self.config.auth != AuthKind::ClaudeOauth {
            return None;
        }
        let credential = self.credential().await.ok()?;
        let response = self
            .client
            .get("https://api.anthropic.com/api/oauth/profile")
            .timeout(MODELS_TIMEOUT)
            .bearer_auth(&credential.access)
            .header("anthropic-beta", "oauth-2025-04-20")
            .send()
            .await
            .ok()?;
        if !response.status().is_success() {
            return None;
        }
        let profile: Value = serde_json::from_str(&read_bounded_body(response).await.ok()?).ok()?;
        claude_plan_of(&profile)
    }

    pub(crate) fn model(&self) -> &str {
        &self.config.model
    }

    /// Bytes per input token this model measured on `thread`'s latest
    /// request; `None` before any request of the thread reported its size.
    pub(crate) fn bytes_per_token(&self, thread: i64) -> Option<f64> {
        self.bytes_per_token
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&thread)
            .copied()
    }

    /// Records that a request of `thread` of `bytes` measured `input` tokens.
    pub(crate) fn measured(&self, thread: i64, bytes: u64, input: u64) {
        if bytes > 0 && input > 0 {
            self.bytes_per_token
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .insert(thread, bytes as f64 / input as f64);
        }
    }

    /// The kept client for `model` on the same provider and credential.
    pub(crate) fn sibling(&self, model: &str) -> Result<std::sync::Arc<Llm>, ErrorInfo> {
        let mut siblings = self
            .siblings
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(sibling) = siblings.get(model) {
            return Ok(sibling.clone());
        }
        let sibling = std::sync::Arc::new(self.with_model(model)?);
        siblings.insert(model.to_string(), sibling.clone());
        Ok(sibling)
    }

    /// A client for `model` on the provider `provider` names: an OAuth
    /// provider, whose credential comes from the same store as this
    /// client's, or `openai`, `anthropic` or `responses` with the key in
    /// `INOCODE_COLLEAGUE_API_KEY`. Without `provider`, a sibling on this
    /// client's provider. Made once and kept.
    pub(crate) fn other(
        &self,
        provider: Option<&str>,
        model: &str,
        base_url: Option<&str>,
    ) -> Result<std::sync::Arc<Llm>, ErrorInfo> {
        let Some(name) = provider.map(str::trim).filter(|name| !name.is_empty()) else {
            return self.sibling(model);
        };
        let key = format!("{name}\n{model}\n{}", base_url.unwrap_or(""));
        let mut siblings = self
            .siblings
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(other) = siblings.get(&key) {
            return Ok(other.clone());
        }
        let config = if let Some(auth) = AuthKind::from_provider_name(name) {
            let default = OAUTH_DEFAULTS
                .iter()
                .find(|row| row.0 == auth)
                .expect("every OAuth kind has defaults");
            LlmConfig {
                provider: default.3,
                auth,
                base_url: base_url.unwrap_or(default.1).to_string(),
                model: model.to_string(),
                api_key: String::new(),
                auth_store_path: self.config.auth_store_path.clone(),
                reasoning: Default::default(),
            }
        } else {
            let provider = match name.to_ascii_lowercase().as_str() {
                "openai" => ProviderKind::OpenAi,
                "anthropic" => ProviderKind::Anthropic,
                "responses" => ProviderKind::Responses,
                _ => {
                    return Err(ErrorInfo::new(
                        "provider_config",
                        format!("unknown provider {name}"),
                    ))
                }
            };
            let api_key = std::env::var(COLLEAGUE_API_KEY_ENV)
                .ok()
                .filter(|key| !key.trim().is_empty())
                .ok_or_else(|| {
                    ErrorInfo::new(
                        "provider_unconfigured",
                        format!("{COLLEAGUE_API_KEY_ENV} is not set"),
                    )
                })?;
            LlmConfig {
                provider,
                auth: AuthKind::ApiKey,
                base_url: base_url.unwrap_or(DEFAULT_BASE_URL).to_string(),
                model: model.to_string(),
                api_key,
                auth_store_path: None,
                reasoning: Default::default(),
            }
        };
        let other = std::sync::Arc::new(Self::new(config)?);
        siblings.insert(key, other.clone());
        Ok(other)
    }

    /// A client for `model` on the same provider and credential.
    pub(crate) fn with_model(&self, model: &str) -> Result<Self, ErrorInfo> {
        Self::new(LlmConfig {
            model: model.to_string(),
            ..self.config.clone()
        })
    }

    /// Whether the provider is a DeepAI account, which has its own tool
    /// review service.
    pub(crate) fn is_deepai(&self) -> bool {
        self.deepai.is_some()
    }

    /// Sends `body` to the DeepAI account's tool review endpoint and returns
    /// its JSON answer.
    pub(crate) async fn deepai_review(&self, body: &Value) -> Result<Value, ErrorInfo> {
        let session = self
            .deepai
            .as_ref()
            .ok_or_else(|| ErrorInfo::new("provider_config", "the provider is not DeepAI"))?;
        let api_key = deepai_api_key(&self.credential().await?)?;
        let request = self
            .client
            .post(self.url("review_tool"))
            .timeout(Duration::from_secs(15))
            .header("content-type", "application/json")
            .json(body);
        let response = session
            .headers(request, &api_key)
            .await
            .send()
            .await
            .map_err(|error| ErrorInfo::new("provider_http", format!("request failed: {error}")))?;
        require_success(response)
            .await?
            .json::<Value>()
            .await
            .map_err(|error| {
                ErrorInfo::new("provider_response", format!("unreadable review: {error}"))
            })
    }

    /// Starts a streaming completion of one user prompt.
    pub async fn chat(&self, prompt: &str) -> Result<ChatStream, ErrorInfo> {
        let conversation = Conversation {
            system: None,
            messages: vec![Message {
                id: "prompt".to_string(),
                body: Body::User {
                    from: Speaker::Person,
                    parts: vec![prompt.to_string()],
                },
            }],
        };
        self.converse(&conversation, &[]).await
    }

    /// Starts a streaming completion of `conversation` offering `tools`. Text
    /// arrives through [`ChatStream::next`]; the whole turn, tool calls
    /// included, through [`ChatStream::assistant`].
    pub async fn converse(
        &self,
        conversation: &Conversation,
        tools: &[ToolSpec],
    ) -> Result<ChatStream, ErrorInfo> {
        self.start(conversation, tools, None).await
    }

    /// Like [`Self::converse`] without tools, asking for an answer in the
    /// JSON shape of `output` (Chat Completions `response_format`, Responses
    /// `text.format`; Anthropic has no such field, so its answer follows the
    /// instructions only).
    pub async fn converse_json(
        &self,
        conversation: &Conversation,
        output: &JsonOutput,
    ) -> Result<ChatStream, ErrorInfo> {
        self.start(conversation, &[], Some(output)).await
    }

    async fn start(
        &self,
        conversation: &Conversation,
        tools: &[ToolSpec],
        output: Option<&JsonOutput>,
    ) -> Result<ChatStream, ErrorInfo> {
        let started = Instant::now();
        if let Some(session) = &self.deepai {
            let mut stream = self
                .deepai_chat(session, conversation, tools, output)
                .await?;
            stream.started = started;
            stream.provider = self.provider_label();
            return Ok(stream);
        }
        let format = self.config.provider;
        let effort = self.effort().await;
        let aliases = if self.config.auth == AuthKind::MuseSubscription {
            muse_aliases(tools)
        } else {
            Vec::new()
        };
        let (aliased_conversation, aliased_tools);
        let (conversation, tools) = if aliases.is_empty() {
            (conversation, tools)
        } else {
            aliased_conversation = alias_conversation(conversation, &aliases);
            aliased_tools = alias_tools(tools, &aliases);
            (&aliased_conversation, aliased_tools.as_slice())
        };
        let mut body = request_body(format, &self.config, effort, conversation, tools, output)?;
        if format == ProviderKind::Anthropic {
            if let Some(limit) = self.output_limit().await {
                crate::reasoning::raise_anthropic_max(&mut body, limit);
            }
        }
        let response = match format {
            ProviderKind::OpenAi => self.openai_chat(&body).await,
            ProviderKind::Anthropic => self.anthropic_chat(body).await,
            ProviderKind::Responses => self.responses_chat(&body).await,
        }?;
        let mut stream = ChatStream::new(format, &self.config.model, response);
        stream.aliases = aliases;
        stream.started = started;
        stream.provider = self.provider_label();
        Ok(stream)
    }

    /// The effort to send: the configured one, else the model's catalog
    /// default, else [`crate::reasoning::FALLBACK_EFFORT`]. Only the ChatGPT
    /// roster publishes defaults, so only it is consulted; a failed lookup
    /// counts as no catalog.
    async fn effort(&self) -> crate::reasoning::Effort {
        if let Some(effort) = self.config.reasoning.effort {
            return effort;
        }
        let catalog = if self.config.auth == AuthKind::GptOauth {
            *self
                .catalog_effort
                .get_or_init(|| async {
                    let models = self.models().await.ok()?;
                    let model = models
                        .into_iter()
                        .find(|model| model.id == self.config.model)?;
                    crate::reasoning::Effort::parse(&model.default_reasoning?)
                })
                .await
        } else {
            None
        };
        catalog.unwrap_or(crate::reasoning::FALLBACK_EFFORT)
    }

    /// Input tokens the configured model accepts, as its provider's model
    /// list states; `None` when the list does not say or cannot be read.
    pub async fn context_window(&self) -> Option<u64> {
        *self
            .catalog_window
            .get_or_init(|| async {
                let models = self.models().await.ok()?;
                models
                    .into_iter()
                    .find(|model| model.id == self.config.model)?
                    .context_window
            })
            .await
    }

    /// Output tokens the configured model may produce in one answer: the
    /// provider's model list (Anthropic `max_tokens`), else, for
    /// api.anthropic.com only, models.dev's `anthropic` entry; `None` when
    /// neither says (docs/evidence/output-limit.md).
    async fn output_limit(&self) -> Option<u64> {
        *self
            .catalog_output
            .get_or_init(|| async {
                let listed = match self.models().await {
                    Ok(models) => models
                        .into_iter()
                        .find(|model| model.id == self.config.model)
                        .and_then(|model| model.max_output),
                    Err(_) => None,
                };
                if listed.is_some()
                    || !destination(self.config.provider, &self.config).first_party_anthropic
                {
                    return listed;
                }
                let response = self.client.get(MODELS_DEV_URL).send().await.ok()?;
                if !response.status().is_success() {
                    return None;
                }
                let catalog: Value =
                    serde_json::from_str(&read_bounded_body(response).await.ok()?).ok()?;
                models_dev_output(&catalog, "anthropic", &self.config.model)
            })
            .await
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

    async fn openai_chat(&self, body: &Value) -> Result<reqwest::Response, ErrorInfo> {
        let response = self
            .client
            .post(self.url("chat/completions"))
            .bearer_auth(&self.config.api_key)
            .header("accept", "text/event-stream")
            .json(body)
            .send()
            .await
            .map_err(|error| ErrorInfo::new("provider_http", format!("request failed: {error}")))?;
        require_success(response).await
    }

    async fn anthropic_chat(&self, mut body: Value) -> Result<reqwest::Response, ErrorInfo> {
        let destination = destination(ProviderKind::Anthropic, &self.config);
        let summary = self.config.reasoning.summary;
        let mut request = self.client.post(self.anthropic_url());
        if self.config.auth == AuthKind::ClaudeOauth {
            // OAuth bearers are only accepted with the beta opt-in and the
            // Claude Code identity block ahead of the conversation.
            let credential = self.credential().await?;
            let mut system = vec![json!({
                "type": "text",
                "text": crate::oauth::CLAUDE_CODE_SYSTEM_INSTRUCTION,
            })];
            if let Some(Value::Array(blocks)) = body.get("system") {
                system.extend(blocks.iter().cloned());
            }
            body["system"] = Value::Array(system);
            let beta = match crate::reasoning::anthropic_beta(&destination, summary) {
                Some(extra) => format!("{},{extra}", crate::oauth::CLAUDE_CODE_BETA),
                None => crate::oauth::CLAUDE_CODE_BETA.to_string(),
            };
            request = request
                .bearer_auth(&credential.access)
                .header("anthropic-beta", beta);
        } else {
            request = request.header("x-api-key", &self.config.api_key);
            if let Some(beta) = crate::reasoning::anthropic_beta(&destination, summary) {
                request = request.header("anthropic-beta", beta);
            }
        }
        let response = request
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("accept", "text/event-stream")
            .json(&body)
            .send()
            .await
            .map_err(|error| ErrorInfo::new("provider_http", format!("request failed: {error}")))?;
        require_success(response).await
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
        conversation: &Conversation,
        tools: &[ToolSpec],
        output: Option<&JsonOutput>,
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
        let (format, url) = if route.responses {
            (ProviderKind::Responses, self.responses_url())
        } else {
            (ProviderKind::OpenAi, self.url("chat/completions"))
        };
        let body = request_body(
            format,
            &self.config,
            self.effort().await,
            conversation,
            tools,
            output,
        )?;
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
                    let mut stream = ChatStream::new(format, &self.config.model, response);
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

    async fn responses_chat(&self, body: &Value) -> Result<reqwest::Response, ErrorInfo> {
        let request = self
            .client
            .post(self.responses_url())
            .header("accept", "text/event-stream");
        let request = match self.config.auth {
            AuthKind::ApiKey => request.bearer_auth(&self.config.api_key),
            // The Muse account token must never be sent to the model API
            // (measured 401); its minted key is the bearer.
            AuthKind::MuseSubscription => request
                .bearer_auth(muse_api_key(&self.credential().await?)?)
                .header("user-agent", crate::oauth::MUSE_USER_AGENT)
                .header("x-api-version", crate::oauth::MUSE_API_VERSION),
            // The ChatGPT account access token is the bearer.
            _ => {
                let credential = self.credential().await?;
                let request = request.bearer_auth(&credential.access);
                if self.config.auth == AuthKind::GptOauth {
                    request.header("chatgpt-account-id", chatgpt_account_id(&credential)?)
                } else {
                    request
                }
            }
        };
        let response =
            request.json(body).send().await.map_err(|error| {
                ErrorInfo::new("provider_http", format!("request failed: {error}"))
            })?;
        require_success(response).await
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
                    reasoning_levels: None,
                    default_reasoning: None,
                    context_window: model.context_window,
                    max_output: None,
                    speed_tiers: None,
                    access_programs: None,
                    theos_per_request: model.theos_per_request.clone(),
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

/// Tool names Meta refuses from a Muse Code subscription key when a request
/// declares them as functions: Muse Code's own builtins (HTTP 403
/// `client_attestation_denied`). List and renaming as in the opencodex fork
/// (ifokeev/opencodex `src/responses/muse-subscription-tool-alias.ts`,
/// measured against api.meta.ai on 2026-10-31).
const MUSE_BUILTIN_TOOLS: &[&str] = &[
    "add_memory",
    "apply_patch",
    "bash",
    "bash_input",
    "create_goal",
    "cron_create",
    "cron_delete",
    "cron_list",
    "edit_file",
    "edit_memory",
    "get_goal",
    "list_peer_sessions",
    "monitor",
    "powershell",
    "powershell_input",
    "read_file",
    "read_memory",
    "read_skill",
    "report_progress",
    "request_user_input",
    "search",
    "send_session_message",
    "shell",
    "snooze_reminder",
    "subagent_cancel",
    "subagent_read_result",
    "subagent_send_message",
    "subagent_spawn",
    "subagent_status",
    "subagent_wait",
    "update_goal",
    "update_plan",
    "web_fetch",
    "web_search",
    "work_list",
    "work_status",
    "work_stop",
    "workflow",
    "write_file",
    "write_todos",
];

/// (declared name, name to send) for each tool whose name is a Muse Code
/// builtin: `client_<name>`, made unique among the declared names.
fn muse_aliases(tools: &[ToolSpec]) -> Vec<(String, String)> {
    let mut taken: std::collections::HashSet<String> =
        tools.iter().map(|tool| tool.name.clone()).collect();
    let mut aliases = Vec::new();
    for tool in tools {
        if !MUSE_BUILTIN_TOOLS.contains(&tool.name.as_str()) {
            continue;
        }
        let base = format!("client_{}", tool.name);
        let mut sent = base.clone();
        let mut salt = 2;
        while taken.contains(&sent) {
            sent = format!("{base}_{salt}");
            salt += 1;
        }
        taken.insert(sent.clone());
        aliases.push((tool.name.clone(), sent));
    }
    aliases
}

fn sent_name<'a>(aliases: &'a [(String, String)], name: &'a str) -> &'a str {
    aliases
        .iter()
        .find(|(declared, _)| declared == name)
        .map_or(name, |(_, sent)| sent.as_str())
}

fn alias_tools(tools: &[ToolSpec], aliases: &[(String, String)]) -> Vec<ToolSpec> {
    tools
        .iter()
        .map(|tool| ToolSpec {
            name: sent_name(aliases, &tool.name).to_string(),
            ..tool.clone()
        })
        .collect()
}

/// The conversation with earlier calls and results under the names sent, so
/// replayed history matches the declared tools.
fn alias_conversation(conversation: &Conversation, aliases: &[(String, String)]) -> Conversation {
    let mut conversation = conversation.clone();
    for message in &mut conversation.messages {
        match &mut message.body {
            Body::Assistant { parts, .. } => {
                for part in parts {
                    if let AssistantPart::ToolCall { name, .. } = part {
                        *name = sent_name(aliases, name).to_string();
                    }
                }
            }
            Body::ToolResult { name, .. } => *name = sent_name(aliases, name).to_string(),
            _ => {}
        }
    }
    conversation
}

/// Reads one provider's usage payload.
type UsageParser = fn(&Value) -> Result<crate::Usage, ErrorInfo>;

/// The streaming request body of `conversation` in `format`. Responses
/// requests are stateless (`store: false`, reference `warmup.ts`), so the
/// reasoning comes back encrypted to be replayed on the next turn.
fn request_body(
    format: ProviderKind,
    config: &LlmConfig,
    effort: crate::reasoning::Effort,
    conversation: &Conversation,
    tools: &[ToolSpec],
    output: Option<&JsonOutput>,
) -> Result<Value, ErrorInfo> {
    let model = config.model.as_str();
    let mut body = match format {
        ProviderKind::OpenAi => conversation::to_chat_completions(conversation, model)?,
        ProviderKind::Anthropic => conversation::to_anthropic(conversation, model)?,
        ProviderKind::Responses => conversation::to_responses(conversation, model)?,
    };
    body.insert("model".into(), json!(model));
    body.insert("stream".into(), json!(true));
    if format == ProviderKind::OpenAi {
        // Without this, Chat Completions streams report no token usage.
        body.insert("stream_options".into(), json!({ "include_usage": true }));
    }
    if format == ProviderKind::Responses {
        body.insert("store".into(), json!(false));
        body.insert("include".into(), json!(["reasoning.encrypted_content"]));
    }
    crate::reasoning::apply(
        &mut body,
        &destination(format, config),
        effort,
        config.reasoning.summary,
    );
    if !tools.is_empty() {
        body.insert(
            "tools".into(),
            conversation::tool_definitions(format, tools)?,
        );
    }
    if let Some(output) = output {
        match format {
            ProviderKind::OpenAi => {
                body.insert(
                    "response_format".into(),
                    json!({
                        "type": "json_schema",
                        "json_schema": {
                            "name": output.name,
                            "strict": true,
                            "schema": output.schema,
                        },
                    }),
                );
            }
            ProviderKind::Responses => {
                body.insert(
                    "text".into(),
                    json!({
                        "format": {
                            "type": "json_schema",
                            "name": output.name,
                            "strict": true,
                            "schema": output.schema,
                        },
                    }),
                );
            }
            ProviderKind::Anthropic => {}
        }
    }
    Ok(Value::Object(body))
}

/// One request to a model as it is logged: when, for what, to whom, what it
/// cost in tokens and how long it took (docs/evidence/usage-log.md).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestRecord {
    /// Milliseconds since the Unix epoch at which the request was sent.
    pub at_ms: u64,
    /// What the request was for: `turn`, `explore`, `colleague`.
    pub purpose: &'static str,
    pub provider: String,
    pub model: String,
    pub usage: Option<TokenUsage>,
    /// From sending to the first text of the answer; `None` when the answer
    /// had no text (only tool calls).
    pub first_output_ms: Option<u64>,
    pub duration_ms: u64,
    /// `completed`, `failed`, or `stopped` when it was not read to the end.
    pub outcome: &'static str,
}

/// Tokens a provider reported for one request. `input` includes the
/// `cached` part.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
    pub cached: u64,
}

/// A JSON Schema the answer must follow, in the strict subset (every
/// property required, no additional properties).
#[derive(Debug, Clone, PartialEq)]
pub struct JsonOutput {
    pub name: String,
    pub schema: Value,
}

/// What the reasoning rules need to know about a request's target.
fn destination(format: ProviderKind, config: &LlmConfig) -> crate::reasoning::Destination<'_> {
    crate::reasoning::Destination {
        format,
        auth: config.auth,
        model: &config.model,
        first_party_anthropic: config.base_url.contains("://api.anthropic.com"),
    }
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
    /// Reasoning efforts the provider lists for the model, as it writes
    /// them; `None` when it publishes no list.
    pub reasoning_levels: Option<Vec<String>>,
    /// The provider's default effort for the model.
    pub default_reasoning: Option<String>,
    /// Input tokens the model accepts, when the provider says.
    pub context_window: Option<u64>,
    /// Output tokens the model may produce in one answer, when the provider
    /// says.
    pub max_output: Option<u64>,
    /// Speed tiers besides the standard one (`fast`, `flex`), as the ChatGPT
    /// roster lists them.
    pub speed_tiers: Option<Vec<String>>,
    /// Access programs the model is offered under (the ChatGPT roster's
    /// cyber programs, such as Daybreak).
    pub access_programs: Option<Vec<String>>,
    /// DeepAI's Theos per request.
    pub theos_per_request: Option<String>,
}

/// The names in a roster field that lists names or objects with a name.
fn names_in(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| match item {
                    Value::String(name) => Some(name.clone()),
                    other => ["name", "id", "slug"]
                        .iter()
                        .find_map(|key| other.get(*key)?.as_str())
                        .map(str::to_string),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The speed tiers of a ChatGPT roster row: `priority` in `service_tiers`
/// or `fast` in `additional_speed_tiers` is the Fast tier; `flex` is Flex.
fn speed_tiers_of(item: &Value) -> Option<Vec<String>> {
    let mut tiers: Vec<String> = Vec::new();
    for name in names_in(item.get("service_tiers"))
        .into_iter()
        .chain(names_in(item.get("additional_speed_tiers")))
    {
        let tier = match name.as_str() {
            "priority" | "fast" => "fast",
            "flex" => "flex",
            _ => continue,
        };
        if !tiers.iter().any(|known| known == tier) {
            tiers.push(tier.to_string());
        }
    }
    (item.get("service_tiers").is_some() || item.get("additional_speed_tiers").is_some())
        .then_some(tiers)
}

/// The plan in a Claude OAuth profile.
fn claude_plan_of(profile: &Value) -> Option<String> {
    let organization = profile.get("organization")?;
    organization
        .get("rate_limit_tier")
        .or_else(|| organization.get("organization_type"))
        .and_then(Value::as_str)
        .filter(|plan| !plan.is_empty())
        .map(str::to_ascii_lowercase)
}

/// The community model catalog used when a provider's own list gives no
/// output limit.
const MODELS_DEV_URL: &str = "https://models.dev/api.json";

/// `limit.output` of `model` under `provider` in a models.dev catalog.
fn models_dev_output(catalog: &Value, provider: &str, model: &str) -> Option<u64> {
    catalog
        .get(provider)?
        .get("models")?
        .get(model)?
        .get("limit")?
        .get("output")?
        .as_u64()
        .filter(|&output| output > 0)
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
                // ChatGPT roster rows: `[{effort, description}]` (reference
                // `validateDiscoveredNativeRows`).
                reasoning_levels: item
                    .get("supported_reasoning_levels")
                    .and_then(Value::as_array)
                    .map(|levels| {
                        levels
                            .iter()
                            .filter_map(|level| level.get("effort")?.as_str())
                            .map(str::to_string)
                            .collect()
                    }),
                default_reasoning: item
                    .get("default_reasoning_level")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                // Anthropic `max_input_tokens`, the ChatGPT roster's
                // `context_window`, OpenRouter-style `context_length`.
                context_window: ["max_input_tokens", "context_window", "context_length"]
                    .iter()
                    .find_map(|field| item.get(*field).and_then(Value::as_u64)),
                // Anthropic `max_tokens`; OpenRouter-style
                // `top_provider.max_completion_tokens` or a top-level
                // `max_completion_tokens`.
                max_output: item
                    .get("max_tokens")
                    .or_else(|| item.get("max_completion_tokens"))
                    .or_else(|| item.get("top_provider")?.get("max_completion_tokens"))
                    .and_then(Value::as_u64)
                    .filter(|&output| output > 0),
                speed_tiers: speed_tiers_of(item),
                access_programs: item
                    .get("available_access_programs")
                    .map(|programs| names_in(programs.get("cyber"))),
                theos_per_request: None,
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

/// What one SSE `data` payload means for the answer.
enum StreamItem {
    Text(String),
    /// Carries no answer text; keep reading.
    Skip,
    /// The provider ended the answer.
    Finish,
}

/// Text deltas of one completion, in generation order, plus the turn they
/// belong to.
pub struct ChatStream {
    format: ProviderKind,
    response: reqwest::Response,
    parser: SseParser,
    body_ended: bool,
    done: bool,
    /// The provider sent its completion signal (`[DONE]`, a finish reason,
    /// or a terminal Responses event).
    signalled: bool,
    /// Keeps a metered DeepAI request's slot until the stream is dropped.
    slot: Option<tokio::sync::OwnedSemaphorePermit>,
    turn: TurnBuilder,
    model: String,
    failed: bool,
    /// (declared name, name sent) of tools renamed for the request.
    aliases: Vec<(String, String)>,
    /// When the request was sent, and when the first text of the answer
    /// arrived.
    started: Instant,
    first_output: Option<Instant>,
    provider: String,
}

impl ChatStream {
    fn new(format: ProviderKind, model: &str, response: reqwest::Response) -> Self {
        Self {
            format,
            response,
            parser: SseParser::new(),
            body_ended: false,
            done: false,
            signalled: false,
            slot: None,
            turn: TurnBuilder::default(),
            model: model.to_string(),
            failed: false,
            aliases: Vec::new(),
            started: Instant::now(),
            first_output: None,
            provider: String::new(),
        }
    }

    /// The request as it is logged, for `purpose`, as read so far.
    pub fn record(&self, purpose: &'static str) -> RequestRecord {
        let elapsed = self.started.elapsed();
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |now| now.as_millis() as u64);
        RequestRecord {
            at_ms: now_ms.saturating_sub(elapsed.as_millis() as u64),
            purpose,
            provider: self.provider.clone(),
            model: self.model.clone(),
            usage: self.usage(),
            first_output_ms: self
                .first_output
                .map(|first| first.duration_since(self.started).as_millis() as u64),
            duration_ms: elapsed.as_millis() as u64,
            outcome: match (self.failed, self.done) {
                (true, _) => "failed",
                (false, true) => "completed",
                (false, false) => "stopped",
            },
        }
    }

    /// Returns the next text delta, or `None` once the provider finished.
    ///
    /// A body that ends before the provider signalled completion is an error,
    /// so a truncated answer is never reported as complete.
    pub async fn next(&mut self) -> Result<Option<String>, ErrorInfo> {
        let next = self.advance().await;
        self.failed |= next.is_err();
        next
    }

    /// Tokens the provider reported for this request, once it has.
    pub fn usage(&self) -> Option<TokenUsage> {
        self.turn.usage()
    }

    /// The turn received so far: complete once [`Self::next`] returned
    /// `None`, `Error` after it failed, `Aborted` when read no further.
    pub fn assistant(&mut self) -> Body {
        let origin = Origin {
            format: self.format,
            model: self.model.clone(),
        };
        let completed = self.done && !self.failed;
        let mut body = self.turn.finish(origin, self.failed, completed);
        if let Body::Assistant { parts, .. } = &mut body {
            for part in parts {
                if let AssistantPart::ToolCall { name, .. } = part {
                    if let Some((declared, _)) = self.aliases.iter().find(|(_, sent)| sent == name)
                    {
                        *name = declared.clone();
                    }
                }
            }
        }
        body
    }

    async fn advance(&mut self) -> Result<Option<String>, ErrorInfo> {
        if self.done {
            return Ok(None);
        }
        loop {
            match self.parser.next_item() {
                Some(Ok(data)) => match interpret(
                    self.format,
                    &data,
                    &mut self.done,
                    &mut self.signalled,
                    &mut self.turn,
                )? {
                    StreamItem::Text(text) => {
                        self.first_output.get_or_insert_with(Instant::now);
                        return Ok(Some(text));
                    }
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

    /// The outcome once the body is over and the parser has nothing left.
    fn end_of_body(&self) -> Result<Option<String>, ErrorInfo> {
        match self.format {
            // Only a provider that signalled completion finished successfully.
            ProviderKind::OpenAi if self.signalled => Ok(None),
            ProviderKind::OpenAi => Err(ErrorInfo::new(
                "provider_stream",
                "the provider stream ended before it signalled completion",
            )),
            // `message_stop` is the only completion signal; a body that ends
            // without it is a truncated answer, never a success.
            ProviderKind::Anthropic => Err(ErrorInfo::new(
                "provider_stream",
                "the provider stream ended before message_stop",
            )),
            // The reference passthrough adapter ends the turn at end of body:
            // buffered deltas are the answer, and only a
            // `response.failed`/`error` event fails it mid-stream.
            ProviderKind::Responses => Ok(None),
        }
    }
}

/// What one SSE `data` payload means, recording it in `turn`.
fn interpret(
    format: ProviderKind,
    data: &str,
    done: &mut bool,
    signalled: &mut bool,
    turn: &mut TurnBuilder,
) -> Result<StreamItem, ErrorInfo> {
    let text = match format {
        ProviderKind::OpenAi => openai_delta(data, done, signalled, turn)?,
        ProviderKind::Anthropic => match anthropic_event(data, turn)? {
            AnthropicEvent::Text(text) => Some(text),
            AnthropicEvent::Stop => return Ok(StreamItem::Finish),
            AnthropicEvent::Ignored => None,
        },
        ProviderKind::Responses => responses_delta(data, done, signalled, turn)?,
    };
    Ok(match text {
        Some(text) => {
            turn.text(&text);
            StreamItem::Text(text)
        }
        None => StreamItem::Skip,
    })
}

/// A string field, or empty when absent.
fn string_at(value: Option<&Value>) -> String {
    value
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Interprets one Chat Completions `data` payload, updating the completion
/// signals on the stream.
fn openai_delta(
    data: &str,
    done: &mut bool,
    signalled: &mut bool,
    turn: &mut TurnBuilder,
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
    if let Some(usage) = value.get("usage").filter(|usage| usage.is_object()) {
        turn.report_usage(
            usage.get("prompt_tokens").and_then(Value::as_u64),
            usage.get("completion_tokens").and_then(Value::as_u64),
            usage
                .get("prompt_tokens_details")
                .and_then(|details| details.get("cached_tokens"))
                .and_then(Value::as_u64),
        );
    }
    let choice = value.get("choices").and_then(|choices| choices.get(0));
    if let Some(reason) = choice
        .and_then(|choice| choice.get("finish_reason"))
        .filter(|reason| !reason.is_null())
    {
        *signalled = true;
        turn.stop(match reason.as_str() {
            Some("length") => StopReason::Length,
            Some("tool_calls" | "function_call") => StopReason::ToolUse,
            _ => StopReason::Completed,
        });
    }
    let delta = choice.and_then(|choice| choice.get("delta"));
    if let Some(delta) = delta {
        // DeepSeek-style `reasoning_content`, or OpenRouter-style `reasoning`.
        if let Some((field, text)) = ["reasoning_content", "reasoning"]
            .into_iter()
            .find_map(|field| Some((field, delta.get(field)?.as_str()?)))
            .filter(|(_, text)| !text.is_empty())
        {
            turn.chat_reasoning(field, text);
        }
        for (position, call) in delta
            .get("tool_calls")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
        {
            let key = call
                .get("index")
                .and_then(Value::as_u64)
                .unwrap_or(position as u64);
            let function = call.get("function");
            let arguments = function
                .and_then(|function| function.get("arguments"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            if turn.started(key) {
                turn.append(key, arguments);
            } else {
                turn.start(
                    key,
                    AssistantPart::ToolCall {
                        call_id: string_at(call.get("id")),
                        name: string_at(function.and_then(|function| function.get("name"))),
                        arguments: arguments.to_string(),
                        replay: None,
                    },
                );
            }
        }
    }
    let content = delta
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

fn anthropic_event(data: &str, turn: &mut TurnBuilder) -> Result<AnthropicEvent, ErrorInfo> {
    let value: Value = serde_json::from_str(data).map_err(|error| {
        ErrorInfo::new("provider_protocol", format!("invalid SSE payload: {error}"))
    })?;
    let key = value.get("index").and_then(Value::as_u64).unwrap_or(0);
    match value.get("type").and_then(Value::as_str) {
        Some("content_block_start") => {
            let block = value.get("content_block");
            match block
                .and_then(|block| block.get("type"))
                .and_then(Value::as_str)
            {
                Some("thinking") => turn.start(
                    key,
                    AssistantPart::Reasoning {
                        text: string_at(block.and_then(|block| block.get("thinking"))),
                        replay: None,
                    },
                ),
                Some("redacted_thinking") => turn.start(
                    key,
                    AssistantPart::Reasoning {
                        text: String::new(),
                        replay: block.cloned().map(Replay),
                    },
                ),
                Some("tool_use") => turn.start(
                    key,
                    AssistantPart::ToolCall {
                        call_id: string_at(block.and_then(|block| block.get("id"))),
                        name: string_at(block.and_then(|block| block.get("name"))),
                        arguments: String::new(),
                        replay: None,
                    },
                ),
                _ => {}
            }
            Ok(AnthropicEvent::Ignored)
        }
        Some("content_block_delta") => {
            let delta = value.get("delta");
            let field = |name: &str| {
                delta
                    .and_then(|delta| delta.get(name))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
            };
            // Only `text_delta` carries answer text.
            match delta
                .and_then(|delta| delta.get("type"))
                .and_then(Value::as_str)
            {
                Some("text_delta") if !field("text").is_empty() => {
                    return Ok(AnthropicEvent::Text(field("text").to_string()))
                }
                Some("thinking_delta") => turn.append(key, field("thinking")),
                Some("signature_delta") => turn.sign(key, field("signature")),
                Some("input_json_delta") => turn.append(key, field("partial_json")),
                _ => {}
            }
            Ok(AnthropicEvent::Ignored)
        }
        Some("message_start") => {
            if let Some(usage) = value
                .get("message")
                .and_then(|message| message.get("usage"))
            {
                let field = |name: &str| usage.get(name).and_then(Value::as_u64);
                let cached = field("cache_read_input_tokens");
                let input = field("input_tokens").map(|input| {
                    input + cached.unwrap_or(0) + field("cache_creation_input_tokens").unwrap_or(0)
                });
                turn.report_usage(input, field("output_tokens"), cached);
            }
            Ok(AnthropicEvent::Ignored)
        }
        Some("message_delta") => {
            if let Some(output) = value
                .get("usage")
                .and_then(|usage| usage.get("output_tokens"))
                .and_then(Value::as_u64)
            {
                turn.report_usage(None, Some(output), None);
            }
            if let Some(reason) = value
                .get("delta")
                .and_then(|delta| delta.get("stop_reason"))
                .and_then(Value::as_str)
            {
                turn.stop(match reason {
                    "max_tokens" => StopReason::Length,
                    "tool_use" => StopReason::ToolUse,
                    _ => StopReason::Completed,
                });
            }
            Ok(AnthropicEvent::Ignored)
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
    turn: &mut TurnBuilder,
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
        // Finished items carry the complete reasoning and call arguments, so
        // the argument deltas need not be joined.
        Some("response.output_item.done") => {
            let item = value.get("item");
            match item
                .and_then(|item| item.get("type"))
                .and_then(Value::as_str)
            {
                Some("reasoning") => turn.push(AssistantPart::Reasoning {
                    text: item
                        .and_then(|item| item.get("summary"))
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(|part| part.get("text").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join("\n\n"),
                    replay: item.cloned().map(|mut item| {
                        if let Some(fields) = item.as_object_mut() {
                            fields.remove("status");
                        }
                        Replay(item)
                    }),
                }),
                Some("function_call") => turn.push(AssistantPart::ToolCall {
                    call_id: string_at(item.and_then(|item| item.get("call_id"))),
                    name: string_at(item.and_then(|item| item.get("name"))),
                    arguments: string_at(item.and_then(|item| item.get("arguments"))),
                    replay: None,
                }),
                _ => {}
            }
            Ok(None)
        }
        // Both end the answer; `incomplete` means the provider stopped early
        // (length or content filter), which still produces the text it sent.
        Some("response.completed") | Some("response.incomplete") => {
            if let Some(usage) = value
                .get("response")
                .and_then(|response| response.get("usage"))
            {
                turn.report_usage(
                    usage.get("input_tokens").and_then(Value::as_u64),
                    usage.get("output_tokens").and_then(Value::as_u64),
                    usage
                        .get("input_tokens_details")
                        .and_then(|details| details.get("cached_tokens"))
                        .and_then(Value::as_u64),
                );
            }
            if value
                .get("response")
                .and_then(|response| response.get("incomplete_details"))
                .and_then(|details| details.get("reason"))
                .and_then(Value::as_str)
                == Some("max_output_tokens")
            {
                turn.stop(StopReason::Length);
            }
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
                &mut TurnBuilder::default(),
            )
            .expect("delta")
            .as_deref(),
            Some("hello")
        );
        assert!(!done && !signalled);
        // Non-JSON data payloads (the terminal `data: [DONE]`, keepalives) are
        // skipped, never errors: the reference `JSON.parse` failure continues.
        assert_eq!(
            responses_delta(
                "[DONE]",
                &mut done,
                &mut signalled,
                &mut TurnBuilder::default()
            )
            .expect("skipped"),
            None
        );
        assert_eq!(
            responses_delta("", &mut done, &mut signalled, &mut TurnBuilder::default())
                .expect("skipped"),
            None
        );
        assert_eq!(
            responses_delta(
                r#"{"type":"response.output_item.added","item":{}}"#,
                &mut done,
                &mut signalled,
                &mut TurnBuilder::default(),
            )
            .expect("ignored"),
            None
        );
        // `completed` is the success terminal.
        responses_delta(
            r#"{"type":"response.completed"}"#,
            &mut done,
            &mut signalled,
            &mut TurnBuilder::default(),
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
            &mut TurnBuilder::default(),
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
            &mut TurnBuilder::default(),
        )
        .expect_err("error event");
        assert_eq!(error.code, "provider_stream");
        assert_eq!(error.message, "quota exceeded");
    }

    #[test]
    fn rates_are_kept_per_thread_and_other_models_are_made_once() {
        let llm = Llm::new(LlmConfig {
            provider: ProviderKind::OpenAi,
            auth: AuthKind::ApiKey,
            base_url: "http://127.0.0.1:1".to_string(),
            model: "test-model".to_string(),
            api_key: "key".to_string(),
            auth_store_path: None,
            reasoning: Default::default(),
        })
        .expect("build the client");
        llm.measured(1, 4_000, 1_000);
        llm.measured(2, 3_000, 1_000);
        llm.measured(3, 0, 1_000);
        assert_eq!(llm.bytes_per_token(1), Some(4.0));
        assert_eq!(llm.bytes_per_token(2), Some(3.0));
        assert_eq!(llm.bytes_per_token(3), None);

        let first = llm.sibling("small").expect("sibling");
        let again = llm.sibling("small").expect("sibling");
        assert!(std::sync::Arc::ptr_eq(&first, &again));
        assert_eq!(first.model(), "small");
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
                reasoning: Default::default(),
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
                reasoning: Default::default(),
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
            {"slug": "gpt-5.2-codex", "supported_in_api": true, "visibility": "show", "display_name": "GPT-5.2 Codex",
             "supported_reasoning_levels": [{"effort": "low", "description": "Fast"}, {"effort": "xhigh", "description": "Deep"}, {"no-effort": 1}],
             "default_reasoning_level": "xhigh"},
            {"slug": "hidden-model", "supported_in_api": true, "visibility": "hide"},
            {"slug": "api-only", "supported_in_api": false, "visibility": "show"},
            {"slug": "no-flag"},
        ]});
        let models = models_from_json(&codex).expect("codex shape");
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "gpt-5.2-codex");
        assert_eq!(models[0].display_name.as_deref(), Some("GPT-5.2 Codex"));
        assert_eq!(
            models[0].reasoning_levels.as_deref(),
            Some(["low".to_string(), "xhigh".to_string()].as_slice())
        );
        assert_eq!(models[0].default_reasoning.as_deref(), Some("xhigh"));
        // A payload without a model list is invalid.
        let error = models_from_json(&serde_json::json!({"other": []})).expect_err("no list");
        assert_eq!(error.code, "provider_protocol");
    }

    #[test]
    fn roster_rows_tell_the_speed_tiers_and_access_programs() {
        let roster = serde_json::json!({"models": [
            {"slug": "gpt-a", "supported_in_api": true, "visibility": "show",
             "service_tiers": ["priority", "flex", "default"],
             "available_access_programs": {"cyber": ["daybreakBlue", {"name": "daybreakRed"}]}},
            {"slug": "gpt-b", "supported_in_api": true, "visibility": "show",
             "additional_speed_tiers": ["fast"]},
            {"slug": "gpt-c", "supported_in_api": true, "visibility": "show"}
        ]});
        let models = models_from_json(&roster).expect("models");
        assert_eq!(
            models[0].speed_tiers,
            Some(vec!["fast".into(), "flex".into()])
        );
        assert_eq!(
            models[0].access_programs,
            Some(vec!["daybreakBlue".into(), "daybreakRed".into()])
        );
        assert_eq!(models[1].speed_tiers, Some(vec!["fast".into()]));
        assert_eq!(
            (
                models[2].speed_tiers.clone(),
                models[2].access_programs.clone()
            ),
            (None, None)
        );
    }

    #[test]
    fn the_claude_plan_is_the_rate_limit_tier() {
        let profile = serde_json::json!({"organization": {
            "rate_limit_tier": "default_claude_max_5x", "organization_type": "claude_max"}});
        assert_eq!(
            claude_plan_of(&profile).as_deref(),
            Some("default_claude_max_5x")
        );
        let typed = serde_json::json!({"organization": {"organization_type": "Claude_Pro"}});
        assert_eq!(claude_plan_of(&typed).as_deref(), Some("claude_pro"));
        assert_eq!(claude_plan_of(&serde_json::json!({})), None);
    }

    #[test]
    fn the_output_limit_comes_from_the_list_or_from_models_dev() {
        let listed = serde_json::json!({"data": [
            {"id": "claude-haiku-5-5", "max_input_tokens": 1_000_000, "max_tokens": 128_000},
            {"id": "router", "top_provider": {"max_completion_tokens": 16_384}},
            {"id": "plain", "max_completion_tokens": 8192},
            {"id": "unknown", "max_tokens": null},
        ]});
        let outputs: Vec<_> = models_from_json(&listed)
            .expect("list")
            .into_iter()
            .map(|model| model.max_output)
            .collect();
        assert_eq!(outputs, [Some(128_000), Some(16_384), Some(8192), None]);

        let catalog = serde_json::json!({"anthropic": {"models": {
            "claude-haiku-5-5": {"limit": {"context": 1_000_000, "output": 128_000}},
            "no-limit": {},
        }}});
        assert_eq!(
            models_dev_output(&catalog, "anthropic", "claude-haiku-5-5"),
            Some(128_000)
        );
        assert_eq!(models_dev_output(&catalog, "anthropic", "no-limit"), None);
        assert_eq!(
            models_dev_output(&catalog, "openai", "claude-haiku-5-5"),
            None
        );
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
                reasoning: Default::default(),
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
            reasoning: Default::default(),
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
        let text = anthropic_event(r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"こん"}}"#, &mut TurnBuilder::default())
        .expect("text delta");
        assert!(matches!(text, AnthropicEvent::Text(ref t) if t == "こん"));

        // Non-text deltas must not become answer text.
        let thinking = anthropic_event(r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hmm"}}"#, &mut TurnBuilder::default())
        .expect("ignored delta");
        assert!(matches!(thinking, AnthropicEvent::Ignored));

        let input_json = anthropic_event(r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"a\":1}"}}"#, &mut TurnBuilder::default())
        .expect("ignored delta");
        assert!(matches!(input_json, AnthropicEvent::Ignored));

        let empty_text = anthropic_event(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":""}}"#,
            &mut TurnBuilder::default(),
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
                    anthropic_event(event, &mut TurnBuilder::default()).expect("ignored event"),
                    AnthropicEvent::Ignored
                ),
                "{event}"
            );
        }

        assert!(matches!(
            anthropic_event(r#"{"type":"message_stop"}"#, &mut TurnBuilder::default())
                .expect("stop"),
            AnthropicEvent::Stop
        ));

        let error = anthropic_event(
            r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
            &mut TurnBuilder::default(),
        )
        .expect_err("must fail");
        assert_eq!(error.code, "provider_stream");
        assert_eq!(error.message, "Overloaded");

        let no_type = anthropic_event(r#"{"nonsense":true}"#, &mut TurnBuilder::default())
            .expect_err("must fail");
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
            &mut TurnBuilder::default(),
        )
        .expect("no error");
        assert!(text.is_none());
        assert!(!done);

        let text = openai_delta(
            r#"{"choices":[{"delta":{"content":"こん"}}]}"#,
            &mut done,
            &mut signalled,
            &mut TurnBuilder::default(),
        )
        .expect("no error");
        assert_eq!(text.as_deref(), Some("こん"));

        openai_delta(
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            &mut done,
            &mut signalled,
            &mut TurnBuilder::default(),
        )
        .expect("no error");
        assert!(signalled);
        assert!(!done);

        openai_delta(
            "[DONE]",
            &mut done,
            &mut signalled,
            &mut TurnBuilder::default(),
        )
        .expect("no error");
        assert!(done);

        let error = openai_delta(
            r#"{"error":{"message":"rate limited"}}"#,
            &mut done,
            &mut signalled,
            &mut TurnBuilder::default(),
        )
        .expect_err("must fail");
        assert_eq!(error.code, "provider_stream");
        assert_eq!(error.message, "rate limited");
    }

    /// Feeds `events` through the stream interpreter and returns the turn.
    fn assemble(format: ProviderKind, events: &[&str]) -> (String, Body) {
        let (mut done, mut signalled) = (false, false);
        let mut turn = TurnBuilder::default();
        let mut text = String::new();
        for data in events {
            if let StreamItem::Text(delta) =
                interpret(format, data, &mut done, &mut signalled, &mut turn).expect("valid")
            {
                text.push_str(&delta);
            }
        }
        let origin = Origin {
            format,
            model: "m".to_string(),
        };
        (text, turn.finish(origin, false, true))
    }

    fn parts_and_stop(body: Body) -> (Vec<AssistantPart>, StopReason) {
        match body {
            Body::Assistant { parts, stop, .. } => (parts, stop),
            other => panic!("not an assistant turn: {other:?}"),
        }
    }

    fn tool_call(call_id: &str, name: &str, arguments: &str) -> AssistantPart {
        AssistantPart::ToolCall {
            call_id: call_id.to_string(),
            name: name.to_string(),
            arguments: arguments.to_string(),
            replay: None,
        }
    }

    #[test]
    fn chat_completions_stream_assembles_reasoning_text_and_calls() {
        let (text, body) = assemble(
            ProviderKind::OpenAi,
            &[
                r#"{"choices":[{"delta":{"reasoning_content":"th"}}]}"#,
                r#"{"choices":[{"delta":{"reasoning_content":"ink"}}]}"#,
                r#"{"choices":[{"delta":{"content":"ok"}}]}"#,
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","type":"function","function":{"name":"read","arguments":""}}]}}]}"#,
                r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"id":"c2","function":{"name":"list","arguments":"{}"}}]}}]}"#,
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"p\":"}}]}}]}"#,
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"1}"}}]}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
                "[DONE]",
            ],
        );
        assert_eq!(text, "ok");
        let (parts, stop) = parts_and_stop(body);
        assert_eq!(stop, StopReason::ToolUse);
        assert_eq!(
            parts,
            [
                AssistantPart::Reasoning {
                    text: "think".to_string(),
                    replay: Some(Replay(json!({"reasoning_content": "think"}))),
                },
                AssistantPart::Text {
                    text: "ok".to_string()
                },
                tool_call("c1", "read", r#"{"p":1}"#),
                tool_call("c2", "list", "{}"),
            ]
        );
    }

    #[test]
    fn anthropic_stream_assembles_signed_thinking_and_tool_input() {
        let (text, body) = assemble(
            ProviderKind::Anthropic,
            &[
                r#"{"type":"message_start","message":{}}"#,
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hm"}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig"}}"#,
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"ok"}}"#,
                r#"{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"t1","name":"read","input":{}}}"#,
                r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"p\""}}"#,
                r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":":1}"}}"#,
                r#"{"type":"content_block_start","index":3,"content_block":{"type":"redacted_thinking","data":"x"}}"#,
                r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#,
                r#"{"type":"message_stop"}"#,
            ],
        );
        assert_eq!(text, "ok");
        let (parts, stop) = parts_and_stop(body);
        assert_eq!(stop, StopReason::ToolUse);
        assert_eq!(
            parts,
            [
                AssistantPart::Reasoning {
                    text: "hm".to_string(),
                    replay: Some(Replay(
                        json!({"type": "thinking", "thinking": "hm", "signature": "sig"})
                    )),
                },
                AssistantPart::Text {
                    text: "ok".to_string()
                },
                tool_call("t1", "read", r#"{"p":1}"#),
                AssistantPart::Reasoning {
                    text: String::new(),
                    replay: Some(Replay(json!({"type": "redacted_thinking", "data": "x"}))),
                },
            ]
        );
    }

    #[test]
    fn responses_stream_takes_finished_items() {
        let reasoning = json!({"type": "reasoning", "id": "rs_1", "summary": [{"type": "summary_text", "text": "plan"}], "encrypted_content": "e"});
        let reasoning_done =
            json!({"type": "response.output_item.done", "item": reasoning}).to_string();
        let (text, body) = assemble(
            ProviderKind::Responses,
            &[
                &reasoning_done,
                r#"{"type":"response.output_text.delta","delta":"ok"}"#,
                r#"{"type":"response.function_call_arguments.delta","delta":"{\"p\""}"#,
                r#"{"type":"response.output_item.done","item":{"type":"function_call","id":"fc_1","call_id":"c1","name":"read","arguments":"{\"p\":1}"}}"#,
                r#"{"type":"response.completed","response":{}}"#,
            ],
        );
        assert_eq!(text, "ok");
        let (parts, stop) = parts_and_stop(body);
        assert_eq!(stop, StopReason::ToolUse);
        assert_eq!(
            parts,
            [
                AssistantPart::Reasoning {
                    text: "plan".to_string(),
                    replay: Some(Replay(reasoning)),
                },
                AssistantPart::Text {
                    text: "ok".to_string()
                },
                tool_call("c1", "read", r#"{"p":1}"#),
            ]
        );
        let (_, body) = assemble(
            ProviderKind::Responses,
            &[
                r#"{"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"}}}"#,
            ],
        );
        assert_eq!(parts_and_stop(body).1, StopReason::Length);
    }

    #[test]
    fn token_usage_is_read_from_each_format() {
        let usage = |format, events: &[&str]| {
            let (mut done, mut signalled) = (false, false);
            let mut turn = TurnBuilder::default();
            for data in events {
                interpret(format, data, &mut done, &mut signalled, &mut turn).expect("valid");
            }
            turn.usage()
        };
        assert_eq!(
            usage(
                ProviderKind::OpenAi,
                &[
                    r#"{"choices":[{"delta":{"content":"ok"}}],"usage":null}"#,
                    r#"{"choices":[],"usage":{"prompt_tokens":120,"completion_tokens":7,"prompt_tokens_details":{"cached_tokens":100}}}"#,
                ]
            ),
            Some(TokenUsage {
                input: 120,
                output: 7,
                cached: 100
            })
        );
        assert_eq!(
            usage(
                ProviderKind::Anthropic,
                &[
                    r#"{"type":"message_start","message":{"usage":{"input_tokens":20,"cache_read_input_tokens":100,"cache_creation_input_tokens":5,"output_tokens":1}}}"#,
                    r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":42}}"#,
                ]
            ),
            Some(TokenUsage {
                input: 125,
                output: 42,
                cached: 100
            })
        );
        assert_eq!(
            usage(
                ProviderKind::Responses,
                &[
                    r#"{"type":"response.completed","response":{"usage":{"input_tokens":300,"output_tokens":9,"input_tokens_details":{"cached_tokens":256}}}}"#
                ]
            ),
            Some(TokenUsage {
                input: 300,
                output: 9,
                cached: 256
            })
        );
        assert_eq!(
            usage(
                ProviderKind::OpenAi,
                &[r#"{"choices":[{"delta":{"content":"x"}}]}"#]
            ),
            None
        );
    }

    #[test]
    fn unfinished_and_failed_turns_say_so() {
        let mut turn = TurnBuilder::default();
        turn.text("half");
        let origin = Origin {
            format: ProviderKind::OpenAi,
            model: "m".to_string(),
        };
        let (_, stop) = parts_and_stop(turn.finish(origin.clone(), false, false));
        assert_eq!(stop, StopReason::Aborted);
        let (_, stop) = parts_and_stop(turn.finish(origin, true, true));
        assert_eq!(stop, StopReason::Error);
    }

    #[test]
    fn request_bodies_carry_tools_and_stateless_reasoning() {
        let conversation = Conversation {
            system: Some("sys".to_string()),
            messages: vec![Message {
                id: "1".to_string(),
                body: Body::User {
                    from: Speaker::Person,
                    parts: vec!["hi".to_string()],
                },
            }],
        };
        let config = LlmConfig {
            provider: ProviderKind::OpenAi,
            auth: AuthKind::ApiKey,
            base_url: DEFAULT_BASE_URL.to_string(),
            model: "m".to_string(),
            api_key: "k".to_string(),
            auth_store_path: None,
            reasoning: Default::default(),
        };
        let tools = [ToolSpec {
            name: "read".to_string(),
            description: "Reads a file".to_string(),
            parameters: json!({"type": "object", "properties": {}}),
        }];
        let chat = request_body(
            ProviderKind::OpenAi,
            &config,
            crate::reasoning::FALLBACK_EFFORT,
            &conversation,
            &tools,
            None,
        )
        .expect("body");
        assert_eq!(chat["tools"][0]["function"]["name"], "read");
        assert_eq!(chat["stream_options"], json!({"include_usage": true}));
        assert_eq!(chat["messages"][0]["role"], "system");
        let anthropic = request_body(
            ProviderKind::Anthropic,
            &config,
            crate::reasoning::FALLBACK_EFFORT,
            &conversation,
            &tools,
            None,
        )
        .expect("body");
        assert_eq!(anthropic["tools"][0]["input_schema"]["type"], "object");
        assert_eq!(
            anthropic["thinking"],
            json!({"type": "enabled", "budget_tokens": 8192})
        );
        assert_eq!(anthropic["max_tokens"], 16_384);
        assert_eq!(chat["reasoning_effort"], "medium");
        let responses = request_body(
            ProviderKind::Responses,
            &config,
            crate::reasoning::FALLBACK_EFFORT,
            &conversation,
            &tools,
            None,
        )
        .expect("body");
        assert_eq!(
            responses["reasoning"],
            json!({"effort": "medium", "summary": "auto"})
        );
        assert_eq!(responses["tools"][0]["strict"], false);
        assert_eq!(responses["instructions"], "sys");
        assert_eq!(responses["store"], false);
        assert_eq!(responses["include"], json!(["reasoning.encrypted_content"]));
        let bare = request_body(
            ProviderKind::OpenAi,
            &config,
            crate::reasoning::FALLBACK_EFFORT,
            &conversation,
            &[],
            None,
        )
        .expect("body");
        assert!(bare.get("tools").is_none());
        let output = JsonOutput {
            name: "shape".to_string(),
            schema: json!({"type": "object"}),
        };
        let shaped = request_body(
            ProviderKind::OpenAi,
            &config,
            crate::reasoning::FALLBACK_EFFORT,
            &conversation,
            &[],
            Some(&output),
        )
        .expect("body");
        assert_eq!(shaped["response_format"]["json_schema"]["name"], "shape");
        assert_eq!(shaped["response_format"]["json_schema"]["strict"], true);
        let shaped = request_body(
            ProviderKind::Responses,
            &config,
            crate::reasoning::FALLBACK_EFFORT,
            &conversation,
            &[],
            Some(&output),
        )
        .expect("body");
        assert_eq!(shaped["text"]["format"]["type"], "json_schema");
        let shaped = request_body(
            ProviderKind::Anthropic,
            &config,
            crate::reasoning::FALLBACK_EFFORT,
            &conversation,
            &[],
            Some(&output),
        )
        .expect("body");
        assert!(shaped.get("response_format").is_none() && shaped.get("text").is_none());
        let twice = [tools[0].clone(), tools[0].clone()];
        assert!(request_body(
            ProviderKind::OpenAi,
            &config,
            crate::reasoning::FALLBACK_EFFORT,
            &conversation,
            &twice,
            None
        )
        .is_err());
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
                    let _ =
                        openai_delta(data, &mut done, &mut signalled, &mut TurnBuilder::default())
                            .expect("valid");
                });
            });
            report("sse + interpret: openai", body.len(), count, times);
            let body = responses_body(EVENTS);
            let times = timed(|| {
                let (mut done, mut signalled) = (false, false);
                count = parse(&body, 16 * 1024, |data| {
                    let _ = responses_delta(
                        data,
                        &mut done,
                        &mut signalled,
                        &mut TurnBuilder::default(),
                    )
                    .expect("valid");
                });
            });
            report("sse + interpret: responses", body.len(), count, times);
            let body = anthropic_body(EVENTS);
            let times = timed(|| {
                count = parse(&body, 16 * 1024, |data| {
                    let _ = anthropic_event(data, &mut TurnBuilder::default()).expect("valid");
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
