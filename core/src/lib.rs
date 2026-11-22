//! Request handling for InoCode.
//!
//! A [`Request`] is decoded from a JSON frame payload and answered either with
//! a [`Response`] (immediate methods such as `ping` and `echo`) or, for `chat`,
//! with a stream of [`EventPayload`] events. The core knows nothing about TCP,
//! framing, or connections, so the server owns all transport concerns.

mod agent_tools;
mod ask;
mod camera;
#[cfg(windows)]
mod camera_mode;
pub mod catalog;
mod colleague;
mod compaction;
mod connectors;
pub mod conversation;
pub mod deepai;
mod edit;
mod explore;
mod fetch;
mod find;
pub mod goal;
mod language;
mod llm;
mod mcp;
pub mod oauth;
mod outline;
mod picture;
mod plugins;
mod read;
pub mod reasoning;
mod review;
pub mod rules;
mod rules_import;
pub mod sandbox;
mod shell;
mod sse;
pub mod store;
pub mod turn;
mod usage;
pub mod usage_log;
mod web_search;

pub use usage::{Usage, UsageWindow};

pub use llm::{
    ChatStream, JsonOutput, Llm, LlmConfig, ModelInfo, ProviderKind, RequestRecord, TokenUsage,
    DEFAULT_BASE_URL, DEFAULT_MAX_TOKENS, DEFAULT_MODEL,
};

use std::collections::HashMap;
use std::sync::Mutex;

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

    pub fn elicitation(id: u64, elicitation: Value) -> Self {
        Self {
            elicitation: Some(elicitation),
            ..Self::new(id, "elicitation")
        }
    }

    pub fn message(id: u64, seq: i64, message: &conversation::Message) -> Self {
        Self {
            message: Some(json!({ "seq": seq, "message": message })),
            ..Self::new(id, "message")
        }
    }

    /// The end of a turn that was neither interrupted nor failed.
    pub fn turn_completed(
        id: u64,
        reason: &'static str,
        usage: turn::TurnUsage,
        held: Vec<turn::Held>,
        questions: Vec<turn::Asked>,
    ) -> Self {
        Self {
            reason: Some(reason),
            usage: Some(usage),
            held: Some(held),
            questions: Some(questions),
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
        }
    }
}

/// The provider a [`Core`] answers `chat` with.
enum Provider {
    /// No provider configured: `chat` reports `provider_unconfigured`.
    Unconfigured,
    /// Configuration was attempted and failed; the error is reported as is.
    Unavailable(ErrorInfo),
    Ready(Box<Llm>),
}

/// Request processor.
///
/// Holds no per-connection state, so one instance serves every connection. The
/// immediate methods (`ping`, `echo`) exercise the request path; `chat` streams
/// a completion from the configured provider.
pub struct Core {
    provider: Provider,
    store: Option<Store>,
    /// Where steering input goes, for each thread with a running turn.
    turns: Mutex<HashMap<SessionId, mpsc::UnboundedSender<String>>>,
}

impl Core {
    /// Core without a provider: `chat` reports `provider_unconfigured`.
    pub fn without_llm() -> Self {
        Self::with_provider(Provider::Unconfigured)
    }

    pub fn with_llm(llm: Llm) -> Self {
        Self::with_provider(Provider::Ready(Box::new(llm)))
    }

    fn with_provider(provider: Provider) -> Self {
        Self {
            provider,
            store: None,
            turns: Mutex::new(HashMap::new()),
        }
    }

    /// Keeps threads in `store`.
    pub fn with_store(mut self, store: Store) -> Self {
        self.store = Some(store);
        self
    }

    /// Core configured from the environment, or from the recorded failure.
    pub fn from_env() -> Self {
        match Llm::from_env() {
            Ok(llm) => Self::with_llm(llm),
            Err(error) => Self::with_provider(Provider::Unavailable(error)),
        }
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

    /// Starts a streaming completion for `prompt`.
    pub async fn chat(&self, prompt: &str) -> Result<ChatStream, ErrorInfo> {
        match &self.provider {
            Provider::Ready(llm) => llm.chat(prompt).await,
            Provider::Unavailable(error) => Err(error.clone()),
            Provider::Unconfigured => Err(ErrorInfo::new(
                "provider_unconfigured",
                "no LLM provider is configured",
            )),
        }
    }

    fn llm(&self) -> Result<&Llm, ErrorInfo> {
        match &self.provider {
            Provider::Ready(llm) => Ok(llm.as_ref()),
            Provider::Unavailable(error) => Err(error.clone()),
            Provider::Unconfigured => Err(ErrorInfo::new(
                "provider_unconfigured",
                "no LLM provider is configured",
            )),
        }
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
        store.start_session(project, "").await
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
        goal::save(store, thread, &goal).await?;
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
        }
        Ok(Some(goal))
    }

    /// Claims `thread` for a new turn and returns where its steering input
    /// arrives. One turn runs per thread; [`Self::end_turn`] releases it.
    pub async fn begin_turn(
        &self,
        thread: SessionId,
    ) -> Result<mpsc::UnboundedReceiver<String>, ErrorInfo> {
        self.llm()?;
        if !self.store()?.session_exists(thread).await? {
            return Err(unknown_thread());
        }
        let mut turns = self.turns.lock().map_err(|_| poisoned_turns())?;
        if turns.contains_key(&thread) {
            return Err(ErrorInfo::new(
                "turn_running",
                "a turn is already running in this thread",
            ));
        }
        let (sender, receiver) = mpsc::unbounded_channel();
        turns.insert(thread, sender);
        Ok(receiver)
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
            .and_then(|turns| turns.get(&thread).map(|sender| sender.send(text).is_ok()))
            .unwrap_or(false)
    }

    /// The configured provider's models with their prices, benchmark scores
    /// and measured speed (docs/evidence/model-catalog.md).
    pub async fn model_catalog(&self) -> Result<catalog::Catalog, ErrorInfo> {
        let llm = self.llm()?;
        let store = self.store()?;
        let models = llm.models().await?;
        Ok(catalog::build(llm, store, models).await)
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
        steer: mpsc::UnboundedReceiver<String>,
        cancel: watch::Receiver<bool>,
        events: mpsc::Sender<TurnEvent>,
    ) -> Outcome {
        match (self.llm(), self.store()) {
            (Ok(llm), Ok(store)) => {
                let prompt = turn::Prompt {
                    text: prompt,
                    reply_to,
                };
                turn::run(llm, store, thread, prompt, approvals, steer, cancel, events).await
            }
            (Err(error), _) | (_, Err(error)) => Outcome {
                ending: turn::Ending::Failed(error),
                usage: Default::default(),
                held: Vec::new(),
                questions: Vec::new(),
            },
        }
    }

    /// Lists the models the configured provider advertises.
    pub async fn models(&self) -> Result<Vec<ModelInfo>, ErrorInfo> {
        match &self.provider {
            Provider::Ready(llm) => llm.models().await,
            Provider::Unavailable(error) => Err(error.clone()),
            Provider::Unconfigured => Err(ErrorInfo::new(
                "provider_unconfigured",
                "no LLM provider is configured",
            )),
        }
    }

    /// The configured account's usage.
    pub async fn usage(&self) -> Result<Usage, ErrorInfo> {
        match &self.provider {
            Provider::Ready(llm) => llm.usage().await,
            Provider::Unavailable(error) => Err(error.clone()),
            Provider::Unconfigured => Err(ErrorInfo::new(
                "provider_unconfigured",
                "no LLM provider is configured",
            )),
        }
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
