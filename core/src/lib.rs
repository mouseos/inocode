//! Request handling for InoCode.
//!
//! A [`Request`] is decoded from a JSON frame payload and answered either with
//! a [`Response`] (immediate methods such as `ping` and `echo`) or, for `chat`,
//! with a stream of [`EventPayload`] events. The core knows nothing about TCP,
//! framing, or connections, so the server owns all transport concerns.

pub mod accounts;
mod agent_tools;
mod ask;
mod audio;
mod camera;
#[cfg(windows)]
mod camera_mode;
pub mod catalog;
mod colleague;
pub mod commands;
mod compaction;
pub mod company;
mod connectors;
pub mod conversation;
pub mod deepai;
mod edit;
pub mod estimate;
mod explore;
mod fetch;
mod find;
mod git;
pub mod goal;
pub mod hidden_input;
mod language;
mod llm;
mod mcp;
pub mod oauth;
mod outline;
mod pdf;
mod picture;
mod plugins;
pub mod pool;
mod promise;
mod read;
pub mod reasoning;
mod review;
pub mod rules;
mod rules_import;
pub mod sandbox;
pub mod secrets;
pub mod settings;
mod shell;
mod sse;
pub mod store;
mod store_pool;
mod store_threads;
mod transcribe;
pub mod translate;
pub mod turn;
mod usage;
pub mod usage_log;
mod web_search;

pub use usage::{Usage, UsageWindow};

pub use llm::{
    ChatStream, JsonOutput, Llm, LlmConfig, ModelInfo, Piece, ProviderKind, RequestRecord,
    TokenUsage, DEFAULT_BASE_URL, DEFAULT_MAX_TOKENS, DEFAULT_MODEL,
};

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{mpsc, watch};

use store::{SessionId, Store, StoredMessage};
use turn::{Outcome, TurnEvent};

/// One request decoded from a JSON frame payload.
///
/// Requests are only received, so the type deserializes but does not need to
/// serialize.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Request {
    /// Correlation id that responses and events must echo.
    pub id: u64,
    /// Method to run.
    pub method: String,
    /// Method arguments. `None` when the client omitted `params`.
    #[serde(default)]
    pub params: Option<Value>,
}

impl Request {
    /// Prompt of a `chat` request.
    pub fn prompt(&self) -> Result<&str, ErrorInfo> {
        self.params
            .as_ref()
            .and_then(|params| params.get("prompt"))
            .and_then(Value::as_str)
            .filter(|prompt| !prompt.trim().is_empty())
            .ok_or_else(|| {
                ErrorInfo::new("invalid_params", "requires a non-empty \"prompt\" string")
            })
    }

    /// The thread a `thread/read` or `turn/*` request names.
    pub fn thread(&self) -> Result<store::SessionId, ErrorInfo> {
        self.params
            .as_ref()
            .and_then(|params| params.get("thread_id"))
            .and_then(Value::as_i64)
            .map(store::SessionId)
            .ok_or_else(|| ErrorInfo::new("invalid_params", "requires a \"thread_id\" number"))
    }

    /// The `ask` rules a `turn/start` request approves for that turn
    /// (`allow`, rule ids); none when absent.
    pub fn allowed_rules(&self) -> Result<Vec<store::NoteId>, ErrorInfo> {
        let invalid = || ErrorInfo::new("invalid_params", "\"allow\" must be a list of rule ids");
        match self.params.as_ref().and_then(|params| params.get("allow")) {
            None | Some(Value::Null) => Ok(Vec::new()),
            Some(Value::Array(ids)) => ids
                .iter()
                .map(|id| id.as_i64().map(store::NoteId).ok_or_else(invalid))
                .collect(),
            Some(_) => Err(invalid()),
        }
    }

    /// What a `turn/start` request allows: `mode` (`ask`, `on-request`,
    /// `auto` or `full`; `on-request` when absent), the `ask` rules in
    /// `allow`, and the held calls in `approve`, each `{"tool", "arguments"}`
    /// as the turn reported them.
    pub fn approvals(&self) -> Result<turn::Approvals, ErrorInfo> {
        let params = self.params.as_ref();
        let mode = match params.and_then(|params| params.get("mode")) {
            None | Some(Value::Null) => turn::Mode::default(),
            Some(value) => value.as_str().and_then(turn::Mode::parse).ok_or_else(|| {
                ErrorInfo::new(
                    "invalid_params",
                    "\"mode\" must be \"ask\", \"on-request\", \"auto\" or \"full\"",
                )
            })?,
        };
        let invalid = || {
            ErrorInfo::new(
                "invalid_params",
                "\"approve\" must be a list of {\"tool\", \"arguments\"} strings",
            )
        };
        let calls = match params.and_then(|params| params.get("approve")) {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(calls)) => calls
                .iter()
                .map(|call| {
                    let tool = call.get("tool").and_then(Value::as_str);
                    let arguments = call.get("arguments").and_then(Value::as_str);
                    match (tool, arguments) {
                        (Some(tool), Some(arguments)) => {
                            Ok((tool.to_string(), arguments.to_string()))
                        }
                        _ => Err(invalid()),
                    }
                })
                .collect::<Result<_, _>>()?,
            Some(_) => return Err(invalid()),
        };
        Ok(turn::Approvals {
            mode,
            rules: self.allowed_rules()?,
            calls,
        })
    }

    /// Whether a `turn/start` request names its `mode`; one that does not
    /// takes the mode its thread chose.
    pub fn names_mode(&self) -> bool {
        self.params
            .as_ref()
            .and_then(|params| params.get("mode"))
            .is_some_and(|mode| !mode.is_null())
    }

    /// The earlier message of the thread a `turn/start` prompt replies to
    /// (`reply_to`, its seq); none when absent.
    pub fn reply_to(&self) -> Result<Option<i64>, ErrorInfo> {
        match self
            .params
            .as_ref()
            .and_then(|params| params.get("reply_to"))
        {
            None | Some(Value::Null) => Ok(None),
            Some(value) => value.as_i64().map(Some).ok_or_else(|| {
                ErrorInfo::new("invalid_params", "\"reply_to\" must be a message seq")
            }),
        }
    }

    /// The answer of a `turn/answer` request: the question's id
    /// (`elicitation`), and `action` (`accept`, `decline` or `cancel`) with
    /// `content`, an object of the answers, when it accepts.
    pub fn elicitation_answer(&self) -> Result<(u64, Value), ErrorInfo> {
        let params = self.params.as_ref();
        let id = params
            .and_then(|params| params.get("elicitation"))
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                ErrorInfo::new("invalid_params", "requires an \"elicitation\" number")
            })?;
        let action = params
            .and_then(|params| params.get("action"))
            .and_then(Value::as_str)
            .filter(|action| ["accept", "decline", "cancel"].contains(action))
            .ok_or_else(|| {
                ErrorInfo::new(
                    "invalid_params",
                    "\"action\" must be \"accept\", \"decline\" or \"cancel\"",
                )
            })?;
        let answer = match action {
            "accept" => {
                let content = params
                    .and_then(|params| params.get("content"))
                    .filter(|content| content.is_object())
                    .ok_or_else(|| {
                        ErrorInfo::new("invalid_params", "accept requires a \"content\" object")
                    })?;
                json!({ "action": "accept", "content": content })
            }
            action => json!({ "action": action }),
        };
        Ok((id, answer))
    }

    /// The window of a `stats/models` request: `days` back from now (30
    /// when absent, at most 3,650), and `utc_offset_minutes`, the caller's
    /// time zone for the hours (0 when absent).
    pub fn stats_window(&self) -> Result<(u64, i64), ErrorInfo> {
        let params = self.params.as_ref();
        let days = match params.and_then(|params| params.get("days")) {
            None | Some(Value::Null) => 30,
            Some(value) => value
                .as_u64()
                .filter(|days| (1..=3_650).contains(days))
                .ok_or_else(|| {
                    ErrorInfo::new("invalid_params", "\"days\" must be from 1 to 3650")
                })?,
        };
        let offset = match params.and_then(|params| params.get("utc_offset_minutes")) {
            None | Some(Value::Null) => 0,
            Some(value) => value
                .as_i64()
                .filter(|offset| (-720..=840).contains(offset))
                .ok_or_else(|| {
                    ErrorInfo::new(
                        "invalid_params",
                        "\"utc_offset_minutes\" must be from -720 to 840",
                    )
                })?,
        };
        Ok((days, offset))
    }

    /// Request id that a `cancel` request refers to.
    pub fn cancel_target(&self) -> Result<u64, ErrorInfo> {
        self.params
            .as_ref()
            .and_then(|params| params.get("request_id"))
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                ErrorInfo::new("invalid_params", "cancel requires a \"request_id\" number")
            })
    }
}

/// Response for an immediate request.
#[derive(Debug, Serialize)]
pub struct Response {
    /// Echoed request id, or `null` when the payload was not a valid request.
    pub id: Option<u64>,
    /// Serialized next to `id`, so the payload is either `result` or `error`.
    #[serde(flatten)]
    pub body: ResponseBody,
}

/// The outcome of an immediate request.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum ResponseBody {
    Result { result: Value },
    Error { error: ErrorInfo },
}

/// Failure reported to the client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ErrorInfo {
    pub code: &'static str,
    pub message: String,
}

impl ErrorInfo {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// One event of a `chat` request.
///
/// `event` is `started`, `delta`, `completed`, `failed`, or `cancelled`;
/// `text` carries a delta and `error` carries a failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EventPayload {
    pub id: u64,
    pub event: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorInfo>,
    /// A stored message of a turn, as `{seq, message}`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<Value>,
    /// Why a turn completed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<turn::TurnUsage>,
    /// A question of an MCP server for the person, during a turn.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elicitation: Option<Value>,
    /// Calls of a turn that wait for the person's approval.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub held: Option<Vec<turn::Held>>,
    /// Questions of a turn for the person, from `ask`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub questions: Option<Vec<turn::Asked>>,
    /// What a `command/run` that starts a turn did before the turn.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// What went wrong beside a turn's work without ending it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warnings: Option<Vec<String>>,
    /// What is left of a turn's task by the estimate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimate: Option<Value>,
}

fn translation_service(name: &str) -> Result<translate::Service, ErrorInfo> {
    translate::Service::parse(name).ok_or_else(|| {
        ErrorInfo::new(
            "invalid_params",
            format!("unknown translation service {name:?}"),
        )
    })
}

impl EventPayload {
    pub fn started(id: u64) -> Self {
        Self::new(id, "started")
    }

    pub fn delta(id: u64, text: impl Into<String>) -> Self {
        Self {
            text: Some(text.into()),
            ..Self::new(id, "delta")
        }
    }

    /// The model's thinking as it streams.
    pub fn thinking(id: u64, text: impl Into<String>) -> Self {
        Self {
            text: Some(text.into()),
            ..Self::new(id, "thinking")
        }
    }

    /// A translated piece of the model's thinking; with `error`, the piece
    /// as written, which could not be translated.
    pub fn thinking_translation(id: u64, piece: translate::Translated) -> Self {
        Self {
            text: Some(piece.text),
            error: piece.error,
            ..Self::new(id, "thinking_translation")
        }
    }

    /// The result of a command that goes on with a turn; the turn's
    /// events follow.
    pub fn command(id: u64, result: Value) -> Self {
        Self {
            result: Some(result),
            ..Self::new(id, "command")
        }
    }

    pub fn elicitation(id: u64, elicitation: Value) -> Self {
        Self {
            elicitation: Some(elicitation),
            ..Self::new(id, "elicitation")
        }
    }

    /// What is left of a running turn's task by the estimate.
    pub fn estimate(id: u64, estimate: &estimate::Estimate) -> Self {
        Self {
            estimate: serde_json::to_value(estimate).ok(),
            ..Self::new(id, "estimate")
        }
    }

    pub fn message(id: u64, seq: i64, message: &conversation::Message) -> Self {
        Self {
            message: Some(json!({ "seq": seq, "message": message })),
            ..Self::new(id, "message")
        }
    }

    /// The end of a turn that was neither interrupted nor failed.
    pub fn turn_completed(id: u64, reason: &'static str, outcome: turn::Outcome) -> Self {
        Self {
            reason: Some(reason),
            usage: Some(outcome.usage),
            held: Some(outcome.held),
            questions: Some(outcome.questions),
            warnings: (!outcome.warnings.is_empty()).then_some(outcome.warnings),
            ..Self::new(id, "completed")
        }
    }

    pub fn completed(id: u64) -> Self {
        Self::new(id, "completed")
    }

    pub fn cancelled(id: u64) -> Self {
        Self::new(id, "cancelled")
    }

    pub fn failed(id: u64, error: ErrorInfo) -> Self {
        Self {
            error: Some(error),
            ..Self::new(id, "failed")
        }
    }

    fn new(id: u64, event: &'static str) -> Self {
        Self {
            id,
            event,
            text: None,
            error: None,
            message: None,
            reason: None,
            usage: None,
            elicitation: None,
            held: None,
            questions: None,
            result: None,
            warnings: None,
            estimate: None,
        }
    }
}

/// Where a [`Core`]'s clients come from.
enum Provider {
    /// Made from `settings.json` beside the store and the credential store.
    Settings,
    /// One client for every choice that names no configured provider, for
    /// tests and embedders that bring their own.
    Fixed(Arc<Llm>),
}

/// A turn that runs: where its steering input goes, where a new model
/// chosen for its thread goes, and how any connection interrupts it.
struct Running {
    steer: mpsc::UnboundedSender<String>,
    model: watch::Sender<Arc<Llm>>,
    mode: watch::Sender<Option<turn::Mode>>,
    cancel: watch::Sender<bool>,
}

/// What [`Core::begin_turn`] claimed for [`Core::run_turn`].
pub struct Claim {
    steer: mpsc::UnboundedReceiver<String>,
    model: watch::Receiver<Arc<Llm>>,
    mode: watch::Receiver<Option<turn::Mode>>,
    cancel: watch::Receiver<bool>,
}

/// What the latest turn of a thread left for the person: the calls held
/// for approval and the questions asked. Kept in the store, so a restart
/// does not lose them.
#[derive(Debug, Clone, Default)]
struct Waiting {
    held: Vec<turn::Held>,
    questions: Vec<turn::Asked>,
}

/// How the server process was started, for `settings/get` to tell the
/// values in effect from those in the file.
#[derive(Debug, Clone, Copy, Default)]
pub struct Started {
    /// The port it listens on.
    pub port: Option<u16>,
    /// `--sandbox on|off` given on the command line, which the file's
    /// `server.sandbox` does not override.
    pub sandbox_flag: Option<bool>,
}

/// What a thread chose to run on, as stored; an absent field follows
/// `default` in the settings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct ThreadChoice {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    summary: Option<String>,
    /// The approval mode of a `turn/start` that names none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mode: Option<String>,
}

/// A provider, model and reasoning to make a client for.
#[derive(Debug, Clone, Default)]
struct Choice {
    provider: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    summary: Option<String>,
}

impl Choice {
    /// The thread's choice over the settings' `default`. A model chosen in
    /// `default` belongs to `default`'s provider, so it is dropped when the
    /// thread chose another provider.
    fn of(thread: &ThreadChoice, default: &settings::Selection) -> Self {
        let same_provider = thread.provider.is_none() || thread.provider == default.provider;
        Self {
            provider: thread.provider.clone().or_else(|| default.provider.clone()),
            model: thread
                .model
                .clone()
                .or_else(|| default.model.clone().filter(|_| same_provider)),
            effort: thread.effort.clone().or_else(|| default.effort.clone()),
            summary: thread.summary.clone().or_else(|| default.summary.clone()),
        }
    }

    /// The choice of a helper `role` in a conversation that chose `self`.
    fn role(&self, role: &settings::Role) -> Self {
        if role.provider.is_some() {
            Self {
                provider: role.provider.clone(),
                model: role.model.clone(),
                effort: role.effort.clone(),
                summary: self.summary.clone(),
            }
        } else {
            Self {
                provider: self.provider.clone(),
                model: role.model.clone().or_else(|| self.model.clone()),
                effort: role.effort.clone().or_else(|| self.effort.clone()),
                summary: self.summary.clone(),
            }
        }
    }
}

/// Request processor.
///
/// Holds no per-connection state, so one instance serves every connection.
/// Each conversation runs on the provider and model it chose; the clients
/// are made from the settings when first needed and kept.
pub struct Core {
    provider: Provider,
    store: Option<Store>,
    turns: Mutex<HashMap<SessionId, Running>>,
    /// The credential store; `None` is [`oauth::Store::default_path`].
    credentials: Option<std::path::PathBuf>,
    /// Clients by everything that made them, so a changed provider gets a
    /// new one and an unchanged one keeps its connections and check-in.
    clients: Mutex<HashMap<String, Arc<Llm>>>,
    started: Started,
    /// Sign-ins started over the protocol.
    logins: accounts::Logins,
    /// Which account of a sign-in provider each request uses.
    pool: Arc<pool::Pool>,
}

impl Default for Core {
    fn default() -> Self {
        Self::new()
    }
}

impl Core {
    /// Core that makes its clients from the settings beside its store.
    pub fn new() -> Self {
        Self::with_provider(Provider::Settings)
    }

    /// Core without a store, so without settings: `chat` reports
    /// `provider_unconfigured`.
    pub fn without_llm() -> Self {
        Self::new()
    }

    /// Core that answers every choice naming no configured provider with
    /// `llm`, or with `llm`'s provider for another model.
    pub fn with_llm(llm: Llm) -> Self {
        Self::with_provider(Provider::Fixed(Arc::new(llm)))
    }

    fn with_provider(provider: Provider) -> Self {
        Self {
            provider,
            store: None,
            turns: Mutex::new(HashMap::new()),
            credentials: None,
            clients: Mutex::new(HashMap::new()),
            started: Started::default(),
            logins: accounts::Logins::new(accounts::LoginEndpoints::default()),
            pool: Arc::new(pool::Pool::new(None)),
        }
    }

    /// Records how the server process was started.
    pub fn with_started(mut self, started: Started) -> Self {
        self.started = started;
        self
    }

    /// Keeps threads in `store`; the settings are the `settings.json`
    /// beside it.
    pub fn with_store(mut self, store: Store) -> Self {
        self.pool = Arc::new(pool::Pool::new(Some(store.clone())));
        self.store = Some(store);
        self
    }

    /// Reads credentials from the store at `path` instead of the default.
    pub fn with_credentials(mut self, path: std::path::PathBuf) -> Self {
        self.credentials = Some(path);
        self
    }

    /// The settings file beside the database.
    pub fn settings_path(&self) -> Result<std::path::PathBuf, ErrorInfo> {
        Ok(self.store()?.files_folder().with_file_name("settings.json"))
    }

    /// The settings; the empty settings for a core without a store.
    async fn settings(&self) -> Result<settings::Settings, ErrorInfo> {
        match &self.store {
            Some(store) => {
                settings::Settings::load(&store.files_folder().with_file_name("settings.json"))
                    .await
                    .map_err(|error| ErrorInfo::new("settings_invalid", error))
            }
            None => Ok(settings::Settings::default()),
        }
    }

    /// The settings file, once its present content is known to be valid:
    /// a file that is broken already is reported as `settings_invalid`, not
    /// as an error in the change asked for.
    async fn writable_settings(&self) -> Result<std::path::PathBuf, ErrorInfo> {
        self.store()?;
        self.settings().await?;
        self.settings_path()
    }

    fn credentials_path(&self) -> Result<std::path::PathBuf, ErrorInfo> {
        match &self.credentials {
            Some(path) => Ok(path.clone()),
            None => oauth::Store::default_path()
                .map_err(|error| ErrorInfo::new("provider_config", error)),
        }
    }

    /// The client for `choice`, made once and kept.
    fn client(
        &self,
        settings: &settings::Settings,
        choice: &Choice,
    ) -> Result<Arc<Llm>, ErrorInfo> {
        let id = match (&self.provider, &choice.provider) {
            (_, Some(id)) => id,
            (Provider::Fixed(llm), None) => {
                return match choice.model.as_deref() {
                    Some(model) if model != llm.model() => llm.sibling(model),
                    _ => Ok(llm.clone()),
                }
            }
            (Provider::Settings, None) => return Err(unconfigured()),
        };
        let provider = match settings.provider(id) {
            Some(provider) => provider.clone(),
            None => {
                return Err(ErrorInfo::new(
                    "provider_config",
                    format!("unknown provider {id}"),
                ))
            }
        };
        if provider.disabled {
            return Err(ErrorInfo::new(
                "provider_disabled",
                format!("the provider {id} is turned off"),
            ));
        }
        let model = choice
            .model
            .clone()
            .unwrap_or_else(|| provider.default_model().to_string());
        let effort = choice
            .effort
            .clone()
            .or_else(|| provider.reasoning.effort.clone());
        let summary = choice
            .summary
            .clone()
            .or_else(|| provider.reasoning.summary.clone());
        // Read for every client asked for, so a key changed with `key set`
        // is used from the next request on; the cache keeps it by a digest.
        let api_key = if provider.kind.uses_api_key() {
            self.api_key(id)?
        } else {
            String::new()
        };
        let key_digest = {
            use sha2::Digest;
            let digest = sha2::Sha256::digest(api_key.as_bytes());
            digest
                .iter()
                .take(8)
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        };
        let idle = settings.server.stream_idle();
        let key = serde_json::to_string(&(
            id,
            &provider,
            &model,
            &effort,
            &summary,
            &key_digest,
            idle.as_secs(),
        ))
        .map_err(|error| ErrorInfo::new("internal", error.to_string()))?;
        if let Some(client) = self.clients_lock().get(&key) {
            return Ok(client.clone());
        }
        let reasoning = reasoning::Reasoning {
            effort: match effort.as_deref() {
                Some(text) => Some(reasoning::Effort::parse(text).ok_or_else(|| {
                    ErrorInfo::new("invalid_params", format!("unknown effort {text:?}"))
                })?),
                None => None,
            },
            summary: match summary.as_deref() {
                Some(text) => reasoning::Summary::parse(text).ok_or_else(|| {
                    ErrorInfo::new("invalid_params", format!("unknown summary {text:?}"))
                })?,
                None => reasoning::Summary::Auto,
            },
        };
        let config = LlmConfig::for_provider(
            &provider,
            &model,
            api_key,
            reasoning,
            self.credentials.clone(),
        );
        let route = Some(self.route(id, &provider));
        let client = Arc::new(
            Llm::new(config)?
                .with_codex_client_version(provider.client_version.clone())
                .with_stream_idle(idle)?
                .with_route(route)
                .with_token_url(self.logins.token_url(provider.kind)),
        );
        Ok(self.clients_lock().entry(key).or_insert(client).clone())
    }

    /// How the provider `id`'s clients choose their account or key.
    pub(crate) fn route(&self, id: &str, provider: &settings::Provider) -> pool::Route {
        let host = reqwest::Url::parse(provider.base_url())
            .ok()
            .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
            .unwrap_or_default();
        pool::Route {
            pool: Arc::clone(&self.pool),
            provider: id.to_string(),
            kind: settings::account_store(id, provider.kind),
            rules: pool::Rules::of(provider.pool.as_ref()),
            pinned: provider.active_account.clone(),
            host: format!("{id}|{host}"),
        }
    }

    fn clients_lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<Llm>>> {
        self.clients
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    /// The stored API key of the static-key provider `id`.
    fn api_key(&self, id: &str) -> Result<String, ErrorInfo> {
        let store = oauth::Store::load(self.credentials_path()?)
            .map_err(|error| ErrorInfo::new("provider_config", error))?;
        let name = settings::api_key_name(id);
        store
            .accounts(&name)
            .iter()
            .filter(|account| account.has_key)
            .find_map(|account| store.account_credential(&name, &account.id))
            .and_then(|credential| credential.api_key.clone())
            .ok_or_else(|| {
                ErrorInfo::new(
                    "provider_unconfigured",
                    format!(
                        "no API key is stored for the provider {id}; run `inocode-server key set {id}`"
                    ),
                )
            })
    }

    async fn thread_choice(&self, thread: SessionId) -> Result<ThreadChoice, ErrorInfo> {
        match self.store()?.thread_settings(thread).await? {
            Some(text) => serde_json::from_str(&text)
                .map_err(|error| ErrorInfo::new("store", format!("thread settings: {error}"))),
            None => Ok(ThreadChoice::default()),
        }
    }

    /// The conversation's client of `thread`.
    async fn thread_client(&self, thread: SessionId) -> Result<Arc<Llm>, ErrorInfo> {
        let settings = self.settings().await?;
        let choice = Choice::of(&self.thread_choice(thread).await?, &settings.default);
        self.client(&settings, &choice)
    }

    /// The client of the settings' `default`, for requests outside a thread.
    async fn default_client(&self) -> Result<Arc<Llm>, ErrorInfo> {
        let settings = self.settings().await?;
        let choice = Choice::of(&ThreadChoice::default(), &settings.default);
        self.client(&settings, &choice)
    }

    /// The client of a helper role; `None` where the conversation's own
    /// model `main` does the work.
    fn role_client(
        &self,
        settings: &settings::Settings,
        conversation: &Choice,
        role: Option<&settings::Role>,
        main: &Arc<Llm>,
    ) -> Result<Option<Arc<Llm>>, ErrorInfo> {
        let Some(role) = role else {
            return Ok(None);
        };
        let client = self.client(settings, &conversation.role(role))?;
        Ok((!Arc::ptr_eq(&client, main)).then_some(client))
    }

    /// The helper models of a turn of the conversation that chose
    /// `conversation`, around its model `main`.
    async fn helpers(
        &self,
        conversation: &Choice,
        project: Option<&str>,
        main: watch::Receiver<Arc<Llm>>,
        chosen_mode: watch::Receiver<Option<turn::Mode>>,
    ) -> Result<turn::Models, ErrorInfo> {
        let settings = self.settings().await?;
        let company = self.company(&settings, project).await?;
        let current = main.borrow().clone();
        let roles = &settings.roles;
        // Made whatever the mode, since the person may switch to `auto`
        // while the turn runs.
        let reviewer =
            self.role_client(&settings, conversation, roles.review.as_ref(), &current)?;
        let colleague = match &roles.colleague {
            Some(role) => Some(self.client(&settings, &conversation.role(role))?),
            None => None,
        };
        Ok(turn::Models {
            reviewer,
            compactor: self.role_client(
                &settings,
                conversation,
                roles.compact.as_ref(),
                &current,
            )?,
            explorer: self.role_client(
                &settings,
                conversation,
                roles.explore.as_ref(),
                &current,
            )?,
            checker: self.role_client(&settings, conversation, roles.check.as_ref(), &current)?,
            colleague,
            main,
            mode: chosen_mode,
            company,
            budget: None,
        })
    }

    /// Runs an immediate request and returns the response to send back.
    pub fn handle(&self, request: Request) -> Response {
        let Request { id, method, params } = request;
        let body = match method.as_str() {
            "ping" => ResponseBody::Result {
                result: json!({ "pong": true }),
            },
            "echo" => match params {
                Some(value) => ResponseBody::Result { result: value },
                None => ResponseBody::Error {
                    error: ErrorInfo::new("invalid_params", "echo requires params"),
                },
            },
            _ => ResponseBody::Error {
                error: ErrorInfo::new("unknown_method", "Unknown method"),
            },
        };
        Response { id: Some(id), body }
    }

    /// Starts a streaming completion for `prompt` on the default model.
    pub async fn chat(&self, prompt: &str) -> Result<ChatStream, ErrorInfo> {
        self.default_client().await?.chat(prompt).await
    }

    /// The thread store, when one is open.
    pub fn thread_store(&self) -> Option<&Store> {
        self.store.as_ref()
    }

    fn store(&self) -> Result<&Store, ErrorInfo> {
        self.store
            .as_ref()
            .ok_or_else(|| ErrorInfo::new("store_unconfigured", "no thread store is open"))
    }

    /// Creates a thread, in the project named `project` when given.
    pub async fn start_thread(&self, project: Option<&str>) -> Result<SessionId, ErrorInfo> {
        let store = self.store()?;
        let project = match project {
            Some(key) => Some(store.project(key).await?),
            None => None,
        };
        let default = self.settings().await?.default;
        let thread = store.start_session(project, "").await?;
        // The thread keeps the default of its start, so a later change of
        // the default does not move a conversation to another model.
        if default != settings::Selection::default() {
            let chosen = ThreadChoice {
                provider: default.provider,
                model: default.model,
                effort: default.effort,
                summary: default.summary,
                mode: None,
            };
            let text = serde_json::to_string(&chosen)
                .map_err(|error| ErrorInfo::new("internal", error.to_string()))?;
            store.set_thread_settings(thread, text).await?;
        }
        Ok(thread)
    }

    /// Up to `limit` messages of `thread` before `before` (the latest when
    /// `None`), oldest first.
    pub async fn read_thread(
        &self,
        thread: SessionId,
        before: Option<i64>,
        limit: usize,
    ) -> Result<Vec<StoredMessage>, ErrorInfo> {
        let store = self.store()?;
        if !store.session_exists(thread).await? {
            return Err(unknown_thread());
        }
        store.page(thread, before, limit).await
    }

    /// Sets the goal of `thread`, replacing any earlier one; the turns from
    /// now on keep going until the checker finds it achieved or impossible,
    /// or a limit stops it (docs/evidence/goal-command.md).
    pub async fn set_goal(
        &self,
        thread: SessionId,
        objective: &str,
        check: Option<&str>,
        rounds: Option<u32>,
        tokens: Option<u64>,
    ) -> Result<goal::Goal, ErrorInfo> {
        let store = self.store()?;
        if !store.session_exists(thread).await? {
            return Err(unknown_thread());
        }
        let from_seq = store
            .page(thread, None, 1)
            .await?
            .last()
            .map_or(1, |last| last.seq + 1);
        let goal = goal::Goal::new(objective, check, rounds, tokens, from_seq)?;
        let replaced = goal::load(store, thread).await?;
        goal::save(store, thread, &goal).await?;
        if let Some(replaced) = replaced.filter(goal::Goal::is_active) {
            store
                .end_goal_task(thread, replaced.from_seq, "abandoned")
                .await?;
        }
        Ok(goal)
    }

    /// The goal of `thread`, if one was ever set.
    pub async fn goal(&self, thread: SessionId) -> Result<Option<goal::Goal>, ErrorInfo> {
        let store = self.store()?;
        if !store.session_exists(thread).await? {
            return Err(unknown_thread());
        }
        goal::load(store, thread).await
    }

    /// Stops the active goal of `thread`; the running turn ends at its next
    /// check. Returns the goal as it now is.
    pub async fn stop_goal(&self, thread: SessionId) -> Result<Option<goal::Goal>, ErrorInfo> {
        let store = self.store()?;
        let Some(mut goal) = self.goal(thread).await? else {
            return Ok(None);
        };
        if goal.is_active() {
            goal.state = goal::State::Stopped;
            goal.reason = "stopped by the person".to_string();
            goal::save(store, thread, &goal).await?;
            store
                .end_goal_task(thread, goal.from_seq, "abandoned")
                .await?;
        }
        Ok(Some(goal))
    }

    /// Claims `thread` for a new turn and makes its model. One turn runs
    /// per thread; [`Self::end_turn`] releases it.
    pub async fn begin_turn(&self, thread: SessionId) -> Result<Claim, ErrorInfo> {
        if !self.store()?.session_exists(thread).await? {
            return Err(unknown_thread());
        }
        let model = self.thread_client(thread).await?;
        let mut turns = self.turns.lock().map_err(|_| poisoned_turns())?;
        if turns.contains_key(&thread) {
            return Err(ErrorInfo::new(
                "turn_running",
                "a turn is already running in this thread",
            ));
        }
        let (steer_sender, steer) = mpsc::unbounded_channel();
        let (model_sender, model) = watch::channel(model);
        let (mode_sender, mode) = watch::channel(None);
        let (cancel_sender, cancel) = watch::channel(false);
        turns.insert(
            thread,
            Running {
                steer: steer_sender,
                model: model_sender,
                mode: mode_sender,
                cancel: cancel_sender,
            },
        );
        Ok(Claim {
            steer,
            model,
            mode,
            cancel,
        })
    }

    /// Interrupts the running turn of `thread`, whichever connection started
    /// it; `false` when none runs.
    pub fn interrupt(&self, thread: SessionId) -> bool {
        self.turns
            .lock()
            .ok()
            .and_then(|turns| {
                turns
                    .get(&thread)
                    .map(|running| running.cancel.send(true).is_ok())
            })
            .unwrap_or(false)
    }

    /// Whether a turn of `thread` runs.
    pub fn turn_running(&self, thread: SessionId) -> bool {
        self.turns
            .lock()
            .is_ok_and(|turns| turns.contains_key(&thread))
    }

    pub fn end_turn(&self, thread: SessionId) {
        if let Ok(mut turns) = self.turns.lock() {
            turns.remove(&thread);
        }
    }

    /// Adds `text` to the running turn of `thread`; `false` when none runs.
    pub fn steer(&self, thread: SessionId, text: String) -> bool {
        self.turns
            .lock()
            .ok()
            .and_then(|turns| {
                turns
                    .get(&thread)
                    .map(|running| running.steer.send(text).is_ok())
            })
            .unwrap_or(false)
    }

    /// The approval mode `thread` chose for turns that name none.
    pub async fn thread_mode(&self, thread: SessionId) -> Result<Option<turn::Mode>, ErrorInfo> {
        Ok(self
            .thread_choice(thread)
            .await?
            .mode
            .as_deref()
            .and_then(turn::Mode::parse))
    }

    /// What `thread` runs on: the provider, model, effort, summary and
    /// approval mode, with what it did not choose filled in from `default`.
    pub async fn thread_settings(&self, thread: SessionId) -> Result<Value, ErrorInfo> {
        if !self.store()?.session_exists(thread).await? {
            return Err(unknown_thread());
        }
        let settings = self.settings().await?;
        let chosen = self.thread_choice(thread).await?;
        let choice = Choice::of(&chosen, &settings.default);
        let model = match self.client(&settings, &choice) {
            Ok(client) => Some(client.model().to_string()),
            Err(_) => choice.model.clone(),
        };
        Ok(json!({
            "thread_id": thread.0,
            "provider": choice.provider,
            "model": model,
            "effort": choice.effort,
            "summary": choice.summary,
            "mode": chosen.mode.unwrap_or_else(|| turn::Mode::default().as_str().to_string()),
        }))
    }

    /// Changes what `thread` runs on: the fields of `patch` among
    /// `provider`, `model`, `effort`, `summary` and `mode`, `null` returning
    /// a field to `default`. A new provider without a model gets the
    /// provider's default model. A running turn uses the new model from its
    /// next request on.
    pub async fn set_thread_settings(
        &self,
        thread: SessionId,
        patch: &Value,
    ) -> Result<Value, ErrorInfo> {
        let invalid = |message: String| ErrorInfo::new("invalid_params", message);
        let patch = patch
            .as_object()
            .ok_or_else(|| invalid("the settings are not an object".into()))?;
        if !self.store()?.session_exists(thread).await? {
            return Err(unknown_thread());
        }
        let settings = self.settings().await?;
        let mut chosen = self.thread_choice(thread).await?;
        for (name, value) in patch.iter().filter(|(name, _)| *name != "thread_id") {
            let text = match value {
                Value::Null => None,
                Value::String(text) if !text.trim().is_empty() => Some(text.trim().to_string()),
                _ => return Err(invalid(format!("\"{name}\" must be a string or null"))),
            };
            match name.as_str() {
                "provider" => {
                    if let Some(id) = &text {
                        if settings.provider(id).is_none() {
                            return Err(invalid(format!("unknown provider {id}")));
                        }
                    }
                    if !patch.contains_key("model") {
                        chosen.model = None;
                    }
                    chosen.provider = text;
                }
                "model" => chosen.model = text,
                "effort" => {
                    if let Some(effort) = &text {
                        reasoning::Effort::parse(effort)
                            .ok_or_else(|| invalid(format!("unknown effort {effort:?}")))?;
                    }
                    chosen.effort = text;
                }
                "summary" => {
                    if let Some(summary) = &text {
                        reasoning::Summary::parse(summary)
                            .ok_or_else(|| invalid(format!("unknown summary {summary:?}")))?;
                    }
                    chosen.summary = text;
                }
                "mode" => {
                    if let Some(mode) = &text {
                        turn::Mode::parse(mode).ok_or_else(|| {
                            invalid(
                                "\"mode\" must be \"ask\", \"on-request\", \"auto\" or \"full\""
                                    .into(),
                            )
                        })?;
                    }
                    chosen.mode = text;
                }
                other => return Err(invalid(format!("\"{other}\" is not a thread setting"))),
            }
        }
        // Made before the choice is kept, so a choice that cannot run (no
        // key, a turned-off provider) is refused instead of stored.
        let client = match Choice::of(&chosen, &settings.default) {
            choice if choice.provider.is_some() || matches!(self.provider, Provider::Fixed(_)) => {
                Some(self.client(&settings, &choice)?)
            }
            _ => None,
        };
        let text = serde_json::to_string(&chosen)
            .map_err(|error| ErrorInfo::new("internal", error.to_string()))?;
        self.store()?.set_thread_settings(thread, text).await?;
        if let Ok(turns) = self.turns.lock() {
            if let Some(running) = turns.get(&thread) {
                if let Some(client) = client {
                    running.model.send_replace(client);
                }
                if patch.contains_key("mode") {
                    let mode = chosen.mode.as_deref().and_then(turn::Mode::parse);
                    running.mode.send_replace(Some(mode.unwrap_or_default()));
                }
            }
        }
        self.thread_settings(thread).await
    }

    /// The providers of the settings, each with whether its credential is
    /// stored, and the `default` choice.
    pub async fn providers(&self) -> Result<Value, ErrorInfo> {
        let settings = self.settings().await?;
        let credentials = oauth::Store::load(self.credentials_path()?).ok();
        let providers: Vec<Value> = settings
            .providers
            .iter()
            .flatten()
            .map(|(id, provider)| provider_view(id, provider, credentials.as_ref()))
            .collect();
        Ok(json!({ "providers": providers, "default": settings.default }))
    }

    /// Adds the provider `{id, kind, ...}` of `params`.
    pub async fn add_provider(&self, params: &Value) -> Result<Value, ErrorInfo> {
        let (id, fields) = provider_params(params)?;
        let provider: settings::Provider = serde_json::from_value(Value::Object(fields))
            .map_err(|error| ErrorInfo::new("invalid_params", error.to_string()))?;
        let path = self.writable_settings().await?;
        settings::add_provider(&path, &id, &provider)
            .await
            .map_err(|error| ErrorInfo::new("invalid_params", error))?;
        self.provider_result(&id).await
    }

    /// Changes the fields `params` names of the provider `id`; `null`
    /// returns a field to the kind's default.
    pub async fn update_provider(&self, params: &Value) -> Result<Value, ErrorInfo> {
        let (id, fields) = provider_params(params)?;
        let path = self.writable_settings().await?;
        settings::update_provider(&path, &id, &Value::Object(fields))
            .await
            .map_err(|error| ErrorInfo::new("invalid_params", error))?;
        self.provider_result(&id).await
    }

    /// Removes the provider `id`, unless `default`, a role or a thread
    /// chooses it, and its stored API key with it.
    pub async fn remove_provider(&self, params: &Value) -> Result<Value, ErrorInfo> {
        let (id, _) = provider_params(params)?;
        let path = self.writable_settings().await?;
        let threads = self.store()?.threads_choosing(&id).await?;
        if !threads.is_empty() {
            let listed: Vec<String> = threads.iter().map(i64::to_string).collect();
            return Err(ErrorInfo::new(
                "provider_in_use",
                format!(
                    "the provider {id} is chosen by the threads {}; choose another provider \
                     there first (thread/settings)",
                    listed.join(", ")
                ),
            ));
        }
        let kind = self
            .settings()
            .await?
            .provider(&id)
            .map(|provider| provider.kind);
        match settings::remove_provider(&path, &id).await {
            Ok(()) => {
                // A provider added later under the same id must not find
                // this one's key.
                if kind.is_some_and(settings::Kind::uses_api_key) {
                    let mut store = oauth::Store::load(self.credentials_path()?)
                        .map_err(|error| ErrorInfo::new("provider_config", error))?;
                    store
                        .remove_provider(&settings::api_key_name(&id))
                        .map_err(|error| ErrorInfo::new("provider_config", error))?;
                }
                Ok(json!({ "removed": id }))
            }
            Err(settings::RemoveError::InUse(uses)) => Err(ErrorInfo::new(
                "provider_in_use",
                format!(
                    "the provider {id} is chosen in {}; choose another provider there first",
                    uses.join(", ")
                ),
            )),
            Err(settings::RemoveError::Invalid(error)) => {
                Err(ErrorInfo::new("invalid_params", error))
            }
        }
    }

    async fn provider_result(&self, id: &str) -> Result<Value, ErrorInfo> {
        let settings = self.settings().await?;
        let provider = settings
            .provider(id)
            .ok_or_else(|| ErrorInfo::new("internal", "the provider was not kept"))?;
        let credentials = oauth::Store::load(self.credentials_path()?).ok();
        Ok(json!({ "provider": provider_view(id, provider, credentials.as_ref()) }))
    }

    /// The `default`, `roles` and `server` sections as written, the server
    /// values in effect, and which written values wait for a restart.
    pub async fn get_settings(&self) -> Result<Value, ErrorInfo> {
        self.store()?;
        Ok(self.sections_view(&self.settings().await?))
    }

    /// Merges `patch` into the `default`, `roles` and `server` sections. A
    /// change of `server.sandbox` applies to the commands started from now
    /// on, unless `--sandbox` was given at the start.
    pub async fn set_settings(&self, patch: &Value) -> Result<Value, ErrorInfo> {
        let settings = settings::set_sections(&self.writable_settings().await?, patch)
            .await
            .map_err(|error| ErrorInfo::new("invalid_params", error))?;
        let sandbox_changed = patch
            .get("server")
            .and_then(|server| server.get("sandbox"))
            .is_some();
        if sandbox_changed && self.started.sandbox_flag.is_none() {
            sandbox::set_enabled(settings.server.sandbox.unwrap_or(true));
        }
        Ok(self.sections_view(&settings))
    }

    fn sections_view(&self, settings: &settings::Settings) -> Value {
        let file = &settings.server;
        let mut waiting = Vec::new();
        if let Some(port) = self.started.port {
            if file.port.unwrap_or(settings::DEFAULT_PORT) != port {
                waiting.push("port");
            }
        }
        if file
            .credential_store()
            .is_ok_and(|mode| mode != secrets::mode())
        {
            waiting.push("credentials");
        }
        json!({
            "default": settings.default,
            "roles": settings.roles,
            "server": settings.server,
            "effective": {
                "port": self.started.port,
                "sandbox": sandbox::enabled(),
                "sandbox_from_command_line": self.started.sandbox_flag.is_some(),
                "credentials": secrets::mode().as_str(),
                "stream_idle_seconds": file.stream_idle().as_secs(),
            },
            "restart_required": !waiting.is_empty(),
            "restart_for": waiting,
        })
    }

    /// The configured provider's models with their prices, benchmark scores
    /// and measured speed (docs/evidence/model-catalog.md).
    /// The models of every usable provider (or only `provider`), with
    /// prices, scores and measured speed. Hidden providers and models are
    /// left out unless `include_hidden`, then marked. A provider whose list
    /// cannot be had is given with its error instead of failing the rest.
    pub async fn model_catalog(
        &self,
        provider: Option<&str>,
        include_hidden: bool,
    ) -> Result<Value, ErrorInfo> {
        let store = self.store()?;
        let settings = self.settings().await?;
        if let Some(id) = provider {
            if settings.provider(id).is_none() {
                return Err(ErrorInfo::new(
                    "invalid_params",
                    format!("unknown provider {id}"),
                ));
            }
        }
        let mut listed = Vec::new();
        let mut sources = Vec::new();
        for (id, entry) in settings.providers.iter().flatten() {
            if entry.disabled
                || provider.is_some_and(|wanted| wanted != id)
                || (entry.hidden && !include_hidden)
            {
                continue;
            }
            let choice = Choice {
                provider: Some(id.clone()),
                ..Choice::default()
            };
            let built = async {
                let llm = self.client(&settings, &choice)?;
                let models = match &entry.models {
                    Some(models) => models.iter().map(|model| listed_model(model)).collect(),
                    None => llm.models().await?,
                };
                Ok::<_, ErrorInfo>(catalog::build(&llm, store, models).await)
            }
            .await;
            let mut shown = json!({
                "id": id,
                "kind": entry.kind.name(),
                "hidden": entry.hidden,
            });
            match built {
                Ok(catalog) => {
                    let models: Vec<Value> = catalog
                        .models
                        .iter()
                        .filter_map(|model| {
                            let hidden = entry.hidden_models.contains(&model.id);
                            if hidden && !include_hidden {
                                return None;
                            }
                            let mut value = serde_json::to_value(model).ok()?;
                            value["hidden"] = Value::Bool(hidden);
                            Some(value)
                        })
                        .collect();
                    shown["models"] = Value::from(models);
                    shown["plan"] = json!(catalog.plan);
                    shown["stale"] = Value::Bool(catalog.stale);
                    for source in catalog.sources {
                        if !sources.contains(&source) {
                            sources.push(source);
                        }
                    }
                }
                Err(error) => shown["error"] = json!(error),
            }
            listed.push(shown);
        }
        Ok(json!({ "providers": listed, "sources": sources }))
    }

    /// Hides or shows one model of a provider in the lists shown for
    /// choosing: `{provider, model, hidden}`.
    pub async fn set_model_visibility(&self, params: &Value) -> Result<Value, ErrorInfo> {
        let text = |name: &str| {
            params[name].as_str().ok_or_else(|| {
                ErrorInfo::new("invalid_params", format!("\"{name}\" must be a string"))
            })
        };
        let hidden = params["hidden"]
            .as_bool()
            .ok_or_else(|| ErrorInfo::new("invalid_params", "\"hidden\" must be true or false"))?;
        let provider = settings::set_model_hidden(
            &self.writable_settings().await?,
            text("provider")?,
            text("model")?,
            hidden,
        )
        .await
        .map_err(|error| ErrorInfo::new("invalid_params", error))?;
        Ok(json!({ "hidden_models": provider.hidden_models }))
    }

    /// Writes `patch` into the translation settings beside the database
    /// and gives them back; turns that start after it use them. When the
    /// thinking is translated with Firefox's or Chrome's models, they are
    /// fetched first, and `models` says whether that worked.
    pub async fn set_translation(&self, patch: &Value) -> Result<Value, ErrorInfo> {
        let path = self.store()?.files_folder().with_file_name("settings.json");
        let settings = translate::update(&path, patch)
            .await
            .map_err(|error| ErrorInfo::new("invalid_params", error))?;
        let mut models = Value::Null;
        if settings.thinking
            && matches!(
                settings.service,
                translate::Service::Firefox | translate::Service::Chromium
            )
        {
            // Models take seconds to fetch the first time; fetching them now
            // keeps the first turn's thinking from waiting on it.
            let language = language::default(&path).await;
            if let Some(to) = translate::target(&settings, language.as_deref()) {
                let translator =
                    translate::shared(&settings, path.with_file_name("translation-models"));
                models = match translator.prepare("en", &to).await {
                    Ok(notes) => json!({ "ready": true, "notes": notes }),
                    Err(error) => json!({ "ready": false, "error": error }),
                };
            }
        }
        Ok(json!({
            "thinking": settings.thinking,
            "service": settings.service.name(),
            "to": settings.to,
            "models": models,
        }))
    }

    /// The folder translation models are kept in, beside the database.
    fn translation_models_folder(&self) -> Result<std::path::PathBuf, ErrorInfo> {
        Ok(self
            .store()?
            .files_folder()
            .with_file_name("translation-models"))
    }

    /// The translation models kept on disk.
    pub async fn translation_models(&self) -> Result<Value, ErrorInfo> {
        let models = translate::installed(&self.translation_models_folder()?).await;
        let listed: Vec<Value> = models
            .into_iter()
            .map(|model| {
                json!({
                    "service": model.service.name(),
                    "from": model.from,
                    "to": model.to,
                    "version": model.version,
                    "bytes": model.bytes,
                    "license": model.license,
                })
            })
            .collect();
        Ok(json!({ "models": listed }))
    }

    /// Deletes the kept model `{service, from, to}` of `params`.
    pub async fn remove_translation_model(&self, params: &Value) -> Result<Value, ErrorInfo> {
        let text = |name: &str| {
            params[name].as_str().ok_or_else(|| {
                ErrorInfo::new("invalid_params", format!("\"{name}\" must be a string"))
            })
        };
        let service = translation_service(text("service")?)?;
        let removed = translate::remove(
            &self.translation_models_folder()?,
            service,
            text("from")?,
            text("to")?,
        )
        .await?;
        Ok(json!({ "removed": removed }))
    }

    /// The languages the service `{service}` of `params` translates.
    pub async fn translation_languages(&self, params: &Value) -> Result<Value, ErrorInfo> {
        let name = params["service"]
            .as_str()
            .ok_or_else(|| ErrorInfo::new("invalid_params", "\"service\" must be a string"))?;
        let settings = translate::Settings::read(
            &self.store()?.files_folder().with_file_name("settings.json"),
        )
        .await
        .map_err(|error| ErrorInfo::new("settings_invalid", error))?;
        let languages = translate::languages(
            translation_service(name)?,
            &translate::Hosts::default(),
            &settings.google_languages_key,
        )
        .await?;
        Ok(json!({
            "from": languages.from,
            "to": languages.to,
            "direct": languages.direct,
        }))
    }

    /// The requests to models of the last `days`, per model.
    pub async fn model_stats(
        &self,
        days: u64,
        utc_offset_minutes: i64,
    ) -> Result<Vec<usage_log::ModelStats>, ErrorInfo> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |now| now.as_millis() as u64);
        let since = now.saturating_sub(days * 24 * 3_600_000);
        self.store()?.model_stats(since, utc_offset_minutes).await
    }

    /// Hands the person's answer to a question of an MCP server that the
    /// running turn of `thread` waits for; `false` when none waits.
    pub fn answer(&self, thread: SessionId, elicitation: u64, answer: Value) -> bool {
        self.store()
            .map(|store| store.answer_elicitation(thread, elicitation, answer))
            .unwrap_or(false)
    }

    /// Runs the turn [`Self::begin_turn`] claimed.
    #[allow(clippy::too_many_arguments)]
    pub async fn run_turn(
        &self,
        thread: SessionId,
        prompt: String,
        reply_to: Option<i64>,
        approvals: turn::Approvals,
        claim: Claim,
        cancel: watch::Receiver<bool>,
        events: mpsc::Sender<TurnEvent>,
    ) -> Outcome {
        let failed = |error| Outcome {
            ending: turn::Ending::Failed(error),
            usage: Default::default(),
            held: Vec::new(),
            questions: Vec::new(),
            warnings: Vec::new(),
            edited: Vec::new(),
            over_budget: false,
        };
        let store = match self.store() {
            Ok(store) => store,
            Err(error) => return failed(error),
        };
        let conversation = match (self.settings().await, self.thread_choice(thread).await) {
            (Ok(settings), Ok(chosen)) => Choice::of(&chosen, &settings.default),
            (Err(error), _) | (_, Err(error)) => return failed(error),
        };
        let project = match store.session_project(thread).await {
            Ok(project) => project.map(|(_, key)| key),
            Err(error) => return failed(error),
        };
        let models = match self
            .helpers(&conversation, project.as_deref(), claim.model, claim.mode)
            .await
        {
            Ok(models) => models,
            Err(error) => return failed(error),
        };
        let prompt = turn::Prompt {
            text: prompt,
            reply_to,
        };
        // The turn stops on its connection's cancel or on `turn/interrupt`
        // from any connection.
        let (stop, stopped) = watch::channel(false);
        let mut from_connection = cancel;
        let mut from_thread = claim.cancel;
        let forward = tokio::spawn(async move {
            tokio::select! {
                () = turn::cancelled(&mut from_connection) => {}
                () = turn::cancelled(&mut from_thread) => {}
            }
            let _ = stop.send(true);
        });
        let mut outcome = pool::in_thread(
            thread,
            turn::run(
                models,
                store,
                thread,
                prompt,
                approvals,
                claim.steer,
                stopped,
                events,
            ),
        )
        .await;
        forward.abort();
        let waiting = Waiting {
            held: outcome.held.clone(),
            questions: outcome.questions.clone(),
        };
        if let Err(error) = self.set_waiting(thread, &waiting).await {
            outcome.warnings.push(format!(
                "the held calls and questions were not saved, so /approve and /answer may not \
                 find them: {}",
                error.message
            ));
        }
        outcome
    }

    async fn set_waiting(&self, thread: SessionId, waiting: &Waiting) -> Result<(), ErrorInfo> {
        let text = |value: Result<String, serde_json::Error>| {
            value.map_err(|error| ErrorInfo::new("internal", error.to_string()))
        };
        self.store()?
            .set_waiting(
                thread,
                text(serde_json::to_string(&waiting.held))?,
                text(serde_json::to_string(&waiting.questions))?,
            )
            .await
    }

    /// What the latest turn of `thread` left for the person.
    async fn waiting(&self, thread: SessionId) -> Result<Waiting, ErrorInfo> {
        let Some((held, questions)) = self.store()?.waiting(thread).await? else {
            return Ok(Waiting::default());
        };
        let parse = |error: serde_json::Error| {
            ErrorInfo::new("store", format!("the waiting calls and questions: {error}"))
        };
        Ok(Waiting {
            held: serde_json::from_str(&held).map_err(parse)?,
            questions: serde_json::from_str(&questions).map_err(parse)?,
        })
    }

    /// Logs a finished `chat` request like a turn's.
    pub async fn log_chat(&self, stream: &ChatStream) {
        let Some(store) = &self.store else {
            return;
        };
        if let Err(error) = store.log_unthreaded(&[stream.record("chat")]).await {
            eprintln!(
                "inocode: the chat request was not logged: {}",
                error.message
            );
        }
    }

    /// Lists the models of the provider `provider`, or of the provider of
    /// `thread`, or of the default one: the settings' list when it has one,
    /// else what the provider advertises. The models the settings hide are
    /// left out unless `include_hidden`.
    pub async fn models(
        &self,
        provider: Option<&str>,
        thread: Option<SessionId>,
        include_hidden: bool,
    ) -> Result<Vec<ModelInfo>, ErrorInfo> {
        let mut models = self.all_models(provider, thread).await?;
        if !include_hidden {
            let settings = self.settings().await?;
            let id = match (provider, thread) {
                (Some(id), _) => Some(id.to_string()),
                (None, Some(thread)) => {
                    Choice::of(&self.thread_choice(thread).await?, &settings.default).provider
                }
                (None, None) => settings.default.provider.clone(),
            };
            if let Some(entry) = id.as_deref().and_then(|id| settings.provider(id)) {
                models.retain(|model| !entry.hidden_models.contains(&model.id));
            }
        }
        Ok(models)
    }

    async fn all_models(
        &self,
        provider: Option<&str>,
        thread: Option<SessionId>,
    ) -> Result<Vec<ModelInfo>, ErrorInfo> {
        let settings = self.settings().await?;
        let choice = match (provider, thread) {
            (Some(id), _) => {
                if settings.provider(id).is_none() {
                    return Err(ErrorInfo::new(
                        "invalid_params",
                        format!("unknown provider {id}"),
                    ));
                }
                Choice {
                    provider: Some(id.to_string()),
                    ..Choice::default()
                }
            }
            (None, Some(thread)) => {
                Choice::of(&self.thread_choice(thread).await?, &settings.default)
            }
            (None, None) => Choice::of(&ThreadChoice::default(), &settings.default),
        };
        let listed = choice
            .provider
            .as_deref()
            .and_then(|id| settings.provider(id))
            .and_then(|provider| provider.models.as_ref());
        if let Some(models) = listed {
            return Ok(models.iter().map(|id| listed_model(id)).collect());
        }
        self.client(&settings, &choice)?.models().await
    }

    /// The default account's usage.
    pub async fn usage(&self) -> Result<Usage, ErrorInfo> {
        self.default_client().await?.usage().await
    }
}

fn unconfigured() -> ErrorInfo {
    ErrorInfo::new(
        "provider_unconfigured",
        "no LLM provider is configured; add one with providers/add, or sign in with \
         `inocode-server login` or store a key with `inocode-server key set`",
    )
}

/// The `id` of a `providers/*` request and its other fields.
fn provider_params(params: &Value) -> Result<(String, serde_json::Map<String, Value>), ErrorInfo> {
    let mut fields = params
        .as_object()
        .cloned()
        .ok_or_else(|| ErrorInfo::new("invalid_params", "the params are not an object"))?;
    match fields.remove("id") {
        Some(Value::String(id)) if !id.trim().is_empty() => Ok((id.trim().to_string(), fields)),
        _ => Err(ErrorInfo::new(
            "invalid_params",
            "requires an \"id\" string",
        )),
    }
}

/// A provider as `providers/*` shows it: its settings, the endpoint and
/// model it uses when they name none, and whether its credential is stored
/// (`null` when the credential store cannot be read).
fn provider_view(
    id: &str,
    provider: &settings::Provider,
    credentials: Option<&oauth::Store>,
) -> Value {
    let mut view = serde_json::to_value(provider).unwrap_or_else(|_| json!({}));
    let name = if provider.kind.uses_api_key() {
        settings::api_key_name(id)
    } else {
        provider.kind.auth().store_name().to_string()
    };
    if let Some(view) = view.as_object_mut() {
        view.insert("id".into(), json!(id));
        view.insert("effective_base_url".into(), json!(provider.base_url()));
        view.insert(
            "effective_default_model".into(),
            json!(provider.default_model()),
        );
        let stored = |store: &oauth::Store| match store.credential(&name) {
            Some(credential) if provider.kind.uses_api_key() => credential
                .api_key
                .as_ref()
                .is_some_and(|key| !key.trim().is_empty()),
            found => found.is_some(),
        };
        view.insert("credential".into(), json!(credentials.map(stored)));
    }
    view
}

/// A model the settings list by name, with nothing else known about it.
fn listed_model(id: &str) -> ModelInfo {
    ModelInfo {
        id: id.to_string(),
        display_name: None,
        owned_by: None,
        reasoning_levels: None,
        default_reasoning: None,
        context_window: None,
        max_output: None,
        speed_tiers: None,
        access_programs: None,
        theos_per_request: None,
    }
}

fn unknown_thread() -> ErrorInfo {
    ErrorInfo::new("unknown_thread", "no thread has this id")
}

fn poisoned_turns() -> ErrorInfo {
    ErrorInfo::new("internal", "the turn registry is unusable")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(id: u64, method: &str, params: Option<Value>) -> Request {
        Request {
            id,
            method: method.to_string(),
            params,
        }
    }

    #[test]
    fn ping_answers_with_pong_and_echoes_the_id() {
        let response = Core::without_llm().handle(request(7, "ping", None));
        assert_eq!(response.id, Some(7));
        assert!(matches!(
            response.body,
            ResponseBody::Result { result } if result == json!({ "pong": true })
        ));
    }

    #[test]
    fn echo_returns_the_sent_value() {
        let params = json!({ "text": "日本語", "numbers": [1, 2, 3] });
        let response = Core::without_llm().handle(request(1, "echo", Some(params.clone())));
        assert!(matches!(
            response.body,
            ResponseBody::Result { result } if result == params
        ));
    }

    #[test]
    fn echo_without_params_is_an_error() {
        let response = Core::without_llm().handle(request(3, "echo", None));
        assert_eq!(response.id, Some(3));
        assert!(matches!(
            response.body,
            ResponseBody::Error { error } if error.code == "invalid_params"
        ));
    }

    #[test]
    fn unknown_method_is_rejected() {
        let response = Core::without_llm().handle(request(9, "delete_everything", None));
        assert_eq!(response.id, Some(9));
        assert!(matches!(
            response.body,
            ResponseBody::Error { error }
                if error.code == "unknown_method" && error.message == "Unknown method"
        ));
    }

    #[test]
    fn chat_requires_a_prompt() {
        let missing = request(1, "chat", None);
        assert_eq!(missing.prompt().unwrap_err().code, "invalid_params");

        let empty = request(1, "chat", Some(json!({ "prompt": "   " })));
        assert_eq!(empty.prompt().unwrap_err().code, "invalid_params");

        let present = request(1, "chat", Some(json!({ "prompt": "hello" })));
        assert_eq!(present.prompt().unwrap(), "hello");
    }

    #[test]
    fn cancel_requires_a_request_id() {
        let missing = request(1, "cancel", None);
        assert_eq!(missing.cancel_target().unwrap_err().code, "invalid_params");

        let present = request(1, "cancel", Some(json!({ "request_id": 5 })));
        assert_eq!(present.cancel_target().unwrap(), 5);
    }

    #[test]
    fn events_serialize_with_their_id_and_name() {
        let started = serde_json::to_value(EventPayload::started(4)).unwrap();
        assert_eq!(started, json!({ "id": 4, "event": "started" }));

        let delta = serde_json::to_value(EventPayload::delta(4, "hi")).unwrap();
        assert_eq!(delta, json!({ "id": 4, "event": "delta", "text": "hi" }));

        let failed = serde_json::to_value(EventPayload::failed(
            4,
            ErrorInfo::new("provider_http", "boom"),
        ))
        .unwrap();
        assert_eq!(
            failed,
            json!({ "id": 4, "event": "failed", "error": { "code": "provider_http", "message": "boom" } })
        );
    }
}
