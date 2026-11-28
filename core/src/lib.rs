//! Request handling for InoCode.
//!
//! A [`Request`] is decoded from a JSON frame payload and answered either with
//! a [`Response`] (immediate methods such as `ping` and `echo`) or, for `chat`,
//! with a stream of [`EventPayload`] events. The core knows nothing about TCP,
//! framing, or connections, so the server owns all transport concerns.

pub mod accounts;
mod agent_tools;
mod ask;
pub mod attach;
mod audio;
mod camera;
#[cfg(windows)]
mod camera_mode;
pub mod catalog;
pub mod claude_cli;
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
pub mod files;
mod find;
mod git;
pub mod goal;
pub mod hidden_input;
mod hooks;
mod language;
mod llm;
mod mcp;
pub mod oauth;
mod outline;
mod pdf;
mod picture;
mod plan_mode;
mod plugins;
pub mod pool;
mod promise;
mod queue;
mod read;
pub mod reasoning;
pub mod remote_password;
mod review;
pub mod rules;
mod rules_import;
pub mod sandbox;
pub mod secrets;
pub mod settings;
mod shell;
mod worktree;
pub use shell::{complete_utf8, JobInfo, JobLog, JobWatch};
mod snapshot;
mod sse;
pub mod store;
mod store_groups;
mod store_pool;
mod store_threads;
mod title;
mod transcribe;
pub mod translate;
pub mod turn;
mod usage;
pub mod usage_log;
mod web_search;

pub use mcp::ResourceUpdates;
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

    /// What a `turn/start` request allows: `mode` (`ask`, `on-request`,
    /// `auto` or `full`; `on-request` when absent).
    pub fn approvals(&self) -> Result<turn::Approvals, ErrorInfo> {
        let mode = match self.params.as_ref().and_then(|params| params.get("mode")) {
            None | Some(Value::Null) => turn::Mode::default(),
            Some(value) => value.as_str().and_then(turn::Mode::parse).ok_or_else(|| {
                ErrorInfo::new(
                    "invalid_params",
                    "\"mode\" must be \"ask\", \"on-request\", \"auto\" or \"full\"",
                )
            })?,
        };
        Ok(turn::Approvals { mode })
    }

    /// Whether a `turn/start` request names its `mode`; one that does not
    /// takes the mode its thread chose.
    pub fn names_mode(&self) -> bool {
        self.params
            .as_ref()
            .and_then(|params| params.get("mode"))
            .is_some_and(|mode| !mode.is_null())
    }

    /// Plan mode as a `turn/start` sets it for its thread (`plan`, a
    /// boolean); `None` when absent.
    pub fn plan(&self) -> Result<Option<bool>, ErrorInfo> {
        match self.params.as_ref().and_then(|params| params.get("plan")) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Bool(on)) => Ok(Some(*on)),
            Some(_) => Err(ErrorInfo::new(
                "invalid_params",
                "\"plan\" must be true or false",
            )),
        }
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

    /// The files a `turn/start` sends with its prompt: `attachments` and
    /// `mentions`, both optional.
    pub fn attached(&self) -> Result<attach::Requested, ErrorInfo> {
        attach::Requested::parse(self.params.as_ref())
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
        // A form's answer needs its content; a URL visited, a request to
        // run the model approved or a call approved once is accepted
        // without one. Which the question is, the store checks.
        let answer = match (action, params.and_then(|params| params.get("content"))) {
            ("accept", None | Some(Value::Null)) => json!({ "action": "accept" }),
            ("accept", Some(content)) if content.is_object() => {
                json!({ "action": "accept", "content": content })
            }
            ("accept", Some(_)) => {
                return Err(ErrorInfo::new(
                    "invalid_params",
                    "\"content\" must be an object",
                ))
            }
            // The feedback on a declined plan.
            (action, Some(content)) if content.is_object() => {
                json!({ "action": action, "content": content })
            }
            (action, _) => json!({ "action": action }),
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
    /// A question for the person during a turn (`elicitation`), or the id
    /// of one that is settled (`answered`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elicitation: Option<Value>,
    /// An MCP server's progress or log message during a tool call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp: Option<Value>,
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
    /// The thread's new or proposed title.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The thread a `thread/titles/subscribe` or `thread/status/subscribe`
    /// event is about.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<i64>,
    /// Where a thread now stands.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<ThreadStatus>,
    /// The first of the messages a rewind deleted, or the thread a new
    /// thread was forked from (`thread_created`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<i64>,
    /// The messages queued for the next turns of a thread.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queue: Option<Value>,
    /// What changed in the project's files during a turn.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff: Option<Value>,
    /// Whether plan mode is now on (`plan_mode`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<bool>,
    /// One request's usage, the turn's totals and the context (`usage`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<Value>,
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

    /// The question `elicitation` is settled, so other screens can close it.
    pub fn answered(id: u64, elicitation: u64) -> Self {
        Self {
            elicitation: Some(json!(elicitation)),
            ..Self::new(id, "answered")
        }
    }

    /// How far an MCP tool call has come (`mcp_progress`) or a log message
    /// of its server (`mcp_log`).
    pub fn mcp(id: u64, event: &'static str, mcp: Value) -> Self {
        Self {
            mcp: Some(mcp),
            ..Self::new(id, event)
        }
    }

    /// The thread's new title (`title_updated`), or a title the model
    /// proposes for it (`title_proposed`).
    pub fn title(id: u64, event: &'static str, title: String) -> Self {
        Self {
            title: Some(title),
            ..Self::new(id, event)
        }
    }

    /// `thread`'s new title, for `thread/titles/subscribe`.
    pub fn thread_title(id: u64, thread: SessionId, title: String) -> Self {
        Self {
            thread_id: Some(thread.0),
            ..Self::title(id, "title_updated", title)
        }
    }

    /// Where a thread now stands (`status`), with the thread named for
    /// `thread/status/subscribe`.
    pub fn thread_status(id: u64, thread: Option<SessionId>, status: ThreadStatus) -> Self {
        Self {
            thread_id: thread.map(|thread| thread.0),
            status: Some(status),
            ..Self::new(id, "status")
        }
    }

    /// The end of a turn, for `thread/subscribe` (`turn_ended`): `reason`
    /// is `completed`, `length`, `cancelled` or `failed`.
    pub fn turn_ended(id: u64, end: TurnEnd) -> Self {
        Self {
            reason: Some(end.ending),
            usage: Some(end.usage),
            questions: Some(end.questions),
            warnings: (!end.warnings.is_empty()).then_some(end.warnings),
            error: end.error,
            ..Self::new(id, "turn_ended")
        }
    }

    /// The messages queued for the next turns of the watched thread
    /// (`queue`).
    /// The older part of the thread is being summarized
    /// (`compaction_started`), or was (`compaction_ended`, with the
    /// summary's `seq` when one was stored).
    pub fn compaction(id: u64, ended: Option<Option<i64>>) -> Self {
        match ended {
            None => Self::new(id, "compaction_started"),
            Some(seq) => Self {
                result: Some(json!({ "seq": seq })),
                ..Self::new(id, "compaction_ended")
            },
        }
    }

    /// A step of a helper answering a call (`helper`).
    pub fn helper(id: u64, step: &turn::HelperStep) -> Self {
        Self {
            result: serde_json::to_value(step).ok(),
            ..Self::new(id, "helper")
        }
    }

    /// A call handed its task to a member running in `thread`
    /// (`delegated`).
    pub fn delegated(id: u64, call_id: &str, thread: SessionId, member: &str) -> Self {
        Self {
            result: Some(json!({ "call_id": call_id, "thread_id": thread.0, "member": member })),
            ..Self::new(id, "delegated")
        }
    }

    /// What a hook says to the person (`hook`).
    pub fn hook(id: u64, event: &str, text: &str) -> Self {
        Self {
            result: Some(json!({ "event": event, "text": text })),
            ..Self::new(id, "hook")
        }
    }

    /// The person's command started as job `job` (`shell_started`).
    pub fn shell_started(id: u64, job: u64) -> Self {
        Self {
            result: Some(json!({ "job_id": job })),
            ..Self::new(id, "shell_started")
        }
    }

    /// The watched thread's commands (`jobs`).
    pub fn jobs(id: u64, jobs: &[shell::JobInfo]) -> Self {
        Self {
            result: Some(json!({ "jobs": jobs })),
            ..Self::new(id, "jobs")
        }
    }

    pub fn queue(id: u64, queue: &[store::Queued]) -> Self {
        Self {
            queue: Some(json!(queue)),
            ..Self::new(id, "queue")
        }
    }

    /// The messages of the watched thread from `from` on were deleted
    /// (`rewound`).
    pub fn rewound(id: u64, from: i64) -> Self {
        Self {
            from: Some(from),
            ..Self::new(id, "rewound")
        }
    }

    /// What changed in the project's files since the turn began (`diff`).
    pub fn diff(id: u64, diff: Value) -> Self {
        Self {
            diff: Some(diff),
            ..Self::new(id, "diff")
        }
    }

    /// Plan mode of the thread turned on or off during its turn
    /// (`plan_mode`).
    pub fn plan_mode(id: u64, on: bool) -> Self {
        Self {
            plan: Some(on),
            ..Self::new(id, "plan_mode")
        }
    }

    /// A change to the list of threads, of `thread` when the subscription
    /// is not of that thread alone: `thread_created` (with `from` for a
    /// fork), `thread_deleted`, `thread_archived`, `thread_unarchived`, or
    /// `thread_renamed` (with `title`).
    pub fn thread_change(id: u64, thread: Option<SessionId>, change: &ThreadChange) -> Self {
        let event = Self {
            thread_id: thread.map(|thread| thread.0),
            ..Self::new(id, change.event())
        };
        match change {
            ThreadChange::Created { from } => Self {
                from: from.map(|from| from.0),
                ..event
            },
            ThreadChange::Renamed(title) => Self {
                title: Some(title.clone()),
                ..event
            },
            _ => event,
        }
    }

    /// An event of `kind` that carries nothing but its name.
    pub fn named(id: u64, kind: &'static str) -> Self {
        Self::new(id, kind)
    }

    /// What is left of a running turn's task by the estimate.
    pub fn estimate(id: u64, estimate: &estimate::Estimate) -> Self {
        Self {
            estimate: serde_json::to_value(estimate).ok(),
            ..Self::new(id, "estimate")
        }
    }

    /// A model request of a running turn ended.
    pub fn usage(id: u64, report: &turn::UsageReport) -> Self {
        Self {
            tokens: serde_json::to_value(report).ok(),
            ..Self::new(id, "usage")
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
            mcp: None,
            questions: None,
            result: None,
            warnings: None,
            estimate: None,
            title: None,
            thread_id: None,
            status: None,
            from: None,
            queue: None,
            diff: None,
            plan: None,
            tokens: None,
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
/// chosen for its thread goes, how any connection interrupts it, and,
/// once it runs, where its events go.
struct Running {
    steer: mpsc::UnboundedSender<String>,
    model: watch::Sender<Arc<Llm>>,
    mode: watch::Sender<Option<turn::Mode>>,
    cancel: watch::Sender<bool>,
    events: Option<mpsc::Sender<TurnEvent>>,
    /// Whether the person has questions to answer once the turn ends: as
    /// before the turn until the turn leaves its own.
    waiting: bool,
}

/// How many [`Activity`] items a subscriber may fall behind before it
/// misses some.
pub(crate) const ACTIVITY_KEPT: usize = 4096;

/// Where a thread stands, for `thread/list` and the subscriptions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadStatus {
    /// No turn runs and nothing waits for the person.
    Idle,
    /// A turn (or a compaction) runs.
    Running,
    /// A running turn waits for the person to answer a question (an
    /// approval prompt or an MCP server's), or no turn runs and the latest
    /// left questions to answer.
    Waiting,
}

/// What happens in a thread, as `thread/subscribe` relays it.
#[derive(Debug, Clone)]
pub enum Activity {
    /// A turn began; its events follow.
    TurnStarted,
    /// An event of the running turn, as the connection that started it
    /// receives it.
    Event(TurnEvent),
    /// The turn ended.
    TurnEnded(TurnEnd),
    /// The messages from the one numbered `from` on were deleted.
    Rewound(i64),
    /// The thread's status changed.
    Status(ThreadStatus),
    /// The messages queued for the next turns, as they now are.
    Queue(Vec<store::Queued>),
    /// The thread was created, deleted, archived or unarchived.
    Changed(ThreadChange),
    /// The thread's commands, as they now are: sent when one starts or ends.
    Jobs(Vec<shell::JobInfo>),
}

/// A change to the list of threads, for the subscriptions
/// (docs/evidence/thread-operations.md).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThreadChange {
    /// A new thread; `from` is the thread it was forked from.
    Created {
        from: Option<SessionId>,
    },
    Deleted,
    Archived,
    Unarchived,
    /// The thread's new title, sent by the store whenever it gives one.
    Renamed(String),
}

impl ThreadChange {
    fn event(&self) -> &'static str {
        match self {
            Self::Created { .. } => "thread_created",
            Self::Deleted => "thread_deleted",
            Self::Archived => "thread_archived",
            Self::Unarchived => "thread_unarchived",
            Self::Renamed(_) => "thread_renamed",
        }
    }
}

/// How a turn ended, for those watching its thread.
#[derive(Debug, Clone)]
pub struct TurnEnd {
    /// `completed`, `length`, `cancelled` or `failed`.
    pub ending: &'static str,
    pub usage: turn::TurnUsage,
    pub questions: Vec<turn::Asked>,
    pub warnings: Vec<String>,
    pub error: Option<ErrorInfo>,
}

impl TurnEnd {
    pub(crate) fn of(outcome: &Outcome) -> Self {
        let (ending, error) = match &outcome.ending {
            turn::Ending::Interrupted => ("cancelled", None),
            turn::Ending::Failed(error) => ("failed", Some(error.clone())),
            ending => (ending.as_str(), None),
        };
        Self {
            ending,
            usage: outcome.usage.clone(),
            questions: outcome.questions.clone(),
            warnings: outcome.warnings.clone(),
            error,
        }
    }
}

/// What [`Core::begin_turn`] claimed for [`Core::run_turn`].
pub struct Claim {
    steer: mpsc::UnboundedReceiver<String>,
    model: watch::Receiver<Arc<Llm>>,
    mode: watch::Receiver<Option<turn::Mode>>,
    cancel: watch::Receiver<bool>,
}

/// What the latest turn of a thread left for the person: the questions
/// asked. Kept in the store, so a restart does not lose them.
#[derive(Debug, Clone, Default)]
struct Waiting {
    questions: Vec<turn::Asked>,
}

impl Waiting {
    fn is_empty(&self) -> bool {
        self.questions.is_empty()
    }
}

/// How the server process was started, for `settings/get` to tell the
/// values in effect from those in the file.
#[derive(Debug, Clone, Default)]
pub struct Started {
    /// The port it listens on.
    pub port: Option<u16>,
    /// `--sandbox on|off` given on the command line, which the file's
    /// `server.sandbox` does not override.
    pub sandbox_flag: Option<bool>,
    /// The port remote access listens on; `None` when it is off.
    pub remote_port: Option<u16>,
    /// The hash of the remote password in effect.
    pub remote_password_hash: Option<String>,
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
    /// Plan mode (docs/evidence/plan-mode.md); the field
    /// [`plan_mode`] reads and writes.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    plan: bool,
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
    /// How many times each thread's choices were written, changed only
    /// while `turns` is held: a turn compares it with the count when it was
    /// asked for, so a choice made while it starts is not lost.
    choices_written: Mutex<HashMap<SessionId, u64>>,
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
    /// What happens in every thread, for the subscriptions.
    activity: tokio::sync::broadcast::Sender<(SessionId, Activity)>,
    /// Held while a queue changes or its first message is sent, so no
    /// message is lost or sent twice.
    queue_lock: tokio::sync::Mutex<()>,
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
            choices_written: Mutex::new(HashMap::new()),
            credentials: None,
            clients: Mutex::new(HashMap::new()),
            started: Started::default(),
            logins: accounts::Logins::new(accounts::LoginEndpoints::default()),
            pool: Arc::new(pool::Pool::new(None)),
            activity: tokio::sync::broadcast::channel(ACTIVITY_KEPT).0,
            queue_lock: tokio::sync::Mutex::new(()),
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
        self.activity = store.activity();
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
            title: None,
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
                plan: false,
            };
            let text = serde_json::to_string(&chosen)
                .map_err(|error| ErrorInfo::new("internal", error.to_string()))?;
            store.set_thread_settings(thread, text).await?;
        }
        let _ = self.activity.send((
            thread,
            Activity::Changed(ThreadChange::Created { from: None }),
        ));
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
        let asked = self.choices_seen(thread);
        self.begin_turn_asked(thread, asked).await
    }

    /// How many times `thread`'s choices were written so far; given to
    /// [`Self::begin_turn_asked`] when a turn is asked for.
    pub fn choices_seen(&self, thread: SessionId) -> u64 {
        let _turns = self.turns.lock();
        self.choices_written
            .lock()
            .map(|written| written.get(&thread).copied().unwrap_or(0))
            .unwrap_or(0)
    }

    /// [`Self::begin_turn`] for a turn asked for when `thread`'s choices had
    /// been written `asked` times: choices written since reach the turn.
    pub async fn begin_turn_asked(
        &self,
        thread: SessionId,
        asked: u64,
    ) -> Result<Claim, ErrorInfo> {
        if !self.store()?.session_exists(thread).await? {
            return Err(unknown_thread());
        }
        if self.store()?.is_archived(thread).await? {
            return Err(archived_thread());
        }
        let model = self.thread_client(thread).await?;
        let waiting = self
            .waiting(thread)
            .await
            .is_ok_and(|waiting| !waiting.is_empty());
        // The guard is not held across the awaits below.
        let (steer, model, mode, cancel, changed) = {
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
                    events: None,
                    waiting,
                },
            );
            // Sent while `turns` is held, so the statuses of a thread go out in
            // the order its turns begin and end.
            let _ = self
                .activity
                .send((thread, Activity::Status(ThreadStatus::Running)));
            let changed = self
                .choices_written
                .lock()
                .map(|written| written.get(&thread).copied().unwrap_or(0) != asked)
                .unwrap_or(false);
            (steer, model, mode, cancel, changed)
        };
        if changed {
            // Written before the turn was registered: read back what was
            // kept and hand it to the turn as a change.
            let client = self.thread_client(thread).await;
            let mode = self.thread_mode(thread).await;
            if let Ok(turns) = self.turns.lock() {
                if let Some(running) = turns.get(&thread) {
                    if let Ok(client) = client {
                        running.model.send_replace(client);
                    }
                    if let Ok(mode) = mode {
                        running.mode.send_replace(Some(mode.unwrap_or_default()));
                    }
                }
            }
        }
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
            if let Some(running) = turns.remove(&thread) {
                let status = if running.waiting {
                    ThreadStatus::Waiting
                } else {
                    ThreadStatus::Idle
                };
                let _ = self.activity.send((thread, Activity::Status(status)));
            }
        }
    }

    /// `thread/fork`: a new thread with `thread`'s messages up to `cut`,
    /// and its plan, standing instructions and choice of model. Refused
    /// while a turn runs, whose messages are not all stored yet.
    pub async fn fork_thread(
        &self,
        thread: SessionId,
        cut: ForkCut,
    ) -> Result<SessionId, ErrorInfo> {
        let store = self.store()?;
        if !store.session_exists(thread).await? {
            return Err(unknown_thread());
        }
        if self.turn_running(thread) {
            return Err(ErrorInfo::new(
                "turn_running",
                "a turn is running in this thread; fork it when the turn ends",
            ));
        }
        let through = match cut {
            ForkCut::Whole => i64::MAX,
            ForkCut::Before(seq) | ForkCut::Through(seq) => {
                let messages = store.since(thread, i64::MIN, usize::MAX >> 1).await?;
                fork_end(&messages, cut, seq)?
            }
        };
        let fork = store.fork_session_through(thread, through).await?;
        self.pool.note_lineage(fork, thread).await;
        let _ = self.activity.send((
            fork,
            Activity::Changed(ThreadChange::Created { from: Some(thread) }),
        ));
        Ok(fork)
    }

    /// Copies the files of `thread`'s project before a turn; nothing for a
    /// thread without a project.
    async fn take_snapshot(&self, thread: SessionId) -> Result<Option<TurnCopy>, ErrorInfo> {
        let store = self.store()?;
        let Some((project, folder)) = store.session_project(thread).await? else {
            return Ok(None);
        };
        let (copies, folder) = (self.snapshots(project), std::path::PathBuf::from(folder));
        let tree = snapshot::take(&copies, &folder).await?;
        let seq = store.add_snapshot(thread, tree.clone()).await?;
        Ok(Some(TurnCopy {
            copies,
            folder,
            tree,
            seq,
        }))
    }

    /// `thread/diff`: what changed in the project's files during the turn
    /// the message numbered `seq` came in (the latest turn when `None`):
    /// from the copy taken before it to the copy taken before the next
    /// turn, or to the files as they are now when no turn followed.
    pub async fn thread_diff(
        &self,
        thread: SessionId,
        seq: Option<i64>,
    ) -> Result<Value, ErrorInfo> {
        let store = self.store()?;
        if !store.session_exists(thread).await? {
            return Err(unknown_thread());
        }
        let no_copy = || {
            ErrorInfo::new(
                "no_snapshot",
                "no copy of the project's files was taken before this turn",
            )
        };
        let (project, folder) = store.session_project(thread).await?.ok_or_else(no_copy)?;
        let (at, tree, next) = store
            .snapshot_span(thread, seq)
            .await?
            .ok_or_else(no_copy)?;
        let (copies, folder) = (self.snapshots(project), std::path::PathBuf::from(folder));
        let diff = match next {
            Some(next) => snapshot::diff_trees(&copies, &folder, &tree, &next).await?,
            None => snapshot::diff(&copies, &folder, &tree).await?,
        };
        let mut value = diff_value(at, diff);
        value["thread_id"] = json!(thread.0);
        Ok(value)
    }

    /// Where the copies of `project`'s files are kept.
    fn snapshots(&self, project: store::ProjectId) -> std::path::PathBuf {
        self.store
            .as_ref()
            .map(|store| store.files_folder())
            .unwrap_or_default()
            .join("snapshots")
            .join(format!("project-{}", project.0))
    }

    /// Puts the files of `thread`'s project back as they were before the
    /// turn the message numbered `seq` came in; how many files added since
    /// were deleted.
    async fn restore_files(&self, thread: SessionId, seq: i64) -> Result<usize, ErrorInfo> {
        let store = self.store()?;
        let no_copy = || {
            ErrorInfo::new(
                "no_snapshot",
                format!("no copy of the project's files was taken before message {seq}"),
            )
        };
        let (project, folder) = store.session_project(thread).await?.ok_or_else(no_copy)?;
        let tree = store
            .snapshot_before(thread, seq)
            .await?
            .ok_or_else(no_copy)?;
        snapshot::restore(
            &self.snapshots(project),
            std::path::Path::new(&folder),
            &tree,
        )
        .await
    }

    /// `thread/rewind`: deletes the person's message numbered `seq` and
    /// everything after it, with the quote of a reply that came with it,
    /// and returns the message's text to be written again. The questions
    /// go, and so do the standing instructions quoted from
    /// what was deleted. With `restore` naming the files, the project's
    /// files are first put back as they were before that message's turn;
    /// with [`Restore::Files`] the messages stay. Refused while a turn runs.
    pub async fn rewind_thread(
        &self,
        thread: SessionId,
        seq: i64,
        restore: Restore,
    ) -> Result<Value, ErrorInfo> {
        let store = self.store()?;
        if !store.session_exists(thread).await? {
            return Err(unknown_thread());
        }
        if self.turn_running(thread) {
            return Err(ErrorInfo::new(
                "turn_running",
                "a turn is running in this thread; rewind it when the turn ends",
            ));
        }
        let messages = store.since(thread, i64::MIN, usize::MAX >> 1).await?;
        let kept = fork_end(&messages, ForkCut::Before(seq), seq)?;
        let from = messages
            .iter()
            .map(|stored| stored.seq)
            .find(|&stored| stored > kept)
            .unwrap_or(seq);
        let text = messages
            .iter()
            .find(|stored| stored.seq == seq)
            .and_then(|stored| match &stored.message.body {
                conversation::Body::User { parts, .. } => Some(parts.join("\n")),
                _ => None,
            })
            .unwrap_or_default();
        let deleted_files = match restore {
            Restore::Conversation => None,
            Restore::Files | Restore::Both => Some(self.restore_files(thread, seq).await?),
        };
        if restore == Restore::Files {
            return Ok(json!({ "thread_id": thread.0, "files_restored": true,
                              "files_deleted": deleted_files, "removed": 0, "text": text }));
        }
        let removed = store.rewind_session(thread, from).await?;
        if let Ok(turns) = self.turns.lock() {
            if !turns.contains_key(&thread) {
                let _ = self.activity.send((thread, Activity::Rewound(from)));
                let _ = self
                    .activity
                    .send((thread, Activity::Status(ThreadStatus::Idle)));
            }
        }
        let mut result =
            json!({ "thread_id": thread.0, "from": from, "removed": removed, "text": text });
        if let Some(deleted) = deleted_files {
            result["files_restored"] = json!(true);
            result["files_deleted"] = json!(deleted);
        }
        Ok(result)
    }

    /// Where `thread` stands now.
    pub async fn thread_status(&self, thread: SessionId) -> Result<ThreadStatus, ErrorInfo> {
        if !self.store()?.session_exists(thread).await? {
            return Err(unknown_thread());
        }
        if self.turn_running(thread) {
            return Ok(if self.store()?.elicitations(thread).is_empty() {
                ThreadStatus::Running
            } else {
                ThreadStatus::Waiting
            });
        }
        Ok(if self.waiting(thread).await?.is_empty() {
            ThreadStatus::Idle
        } else {
            ThreadStatus::Waiting
        })
    }

    /// `thread/subscribe` and `thread/status/subscribe`: what happens in
    /// every thread from now on, until the value returned is dropped.
    pub fn activity(&self) -> tokio::sync::broadcast::Receiver<(SessionId, Activity)> {
        self.activity.subscribe()
    }

    /// Tells the running turn of `thread`, if one runs, that the thread's
    /// title is now `title`.
    pub(crate) async fn announce_title(&self, thread: SessionId, title: &str) {
        let events = self.turns.lock().ok().and_then(|turns| {
            turns
                .get(&thread)
                .and_then(|running| running.events.clone())
        });
        if let Some(events) = events {
            let _ = events.send(TurnEvent::Title(title.to_string())).await;
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
            "plan": chosen.plan,
        }))
    }

    /// Changes what `thread` runs on: the fields of `patch` among
    /// `provider`, `model`, `effort`, `summary` and `mode`, `null` returning
    /// a field to `default`, and `plan` (`true`, `false` or `null` for off). A new provider without a model gets the
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
            if name == "plan" {
                chosen.plan = match value {
                    Value::Bool(on) => *on,
                    Value::Null => false,
                    _ => return Err(invalid("\"plan\" must be true, false or null".into())),
                };
                continue;
            }
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
            if let Ok(mut written) = self.choices_written.lock() {
                *written.entry(thread).or_default() += 1;
            }
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
        let mut view = self.sections_view(&self.settings().await?);
        view["claude_cli_channels"] = json!(claude_cli::channels().await);
        Ok(view)
    }

    /// Merges `patch` into the `default`, `roles` and `server` sections. A
    /// change of `server.sandbox` applies to the commands started from now
    /// on, unless `--sandbox` was given at the start.
    pub async fn set_settings(&self, patch: &Value) -> Result<Value, ErrorInfo> {
        if let Some(channel) = patch
            .pointer("/server/claude_cli_channel")
            .and_then(Value::as_str)
        {
            if let Some(channels) = claude_cli::channels().await {
                if !channels.contains_key(channel) {
                    let mut names: Vec<&str> = channels.keys().map(String::as_str).collect();
                    names.sort_unstable();
                    return Err(ErrorInfo::new(
                        "invalid_params",
                        format!(
                            "npm has no channel {channel} of Claude Code; it has {}",
                            names.join(", ")
                        ),
                    ));
                }
            }
        }
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
            let remote_port = file.remote_address().map(|address| address.port());
            if remote_port.is_some() != self.started.remote_port.is_some() {
                waiting.push("remote");
            } else if remote_port != self.started.remote_port {
                waiting.push("remote_port");
            }
            if file.remote_password_hash != self.started.remote_password_hash {
                waiting.push("remote_password");
            }
        }
        if file
            .credential_store()
            .is_ok_and(|mode| mode != secrets::mode())
        {
            waiting.push("credentials");
        }
        // The password's hash is never shown, only whether there is one.
        let mut server = serde_json::to_value(&settings.server).unwrap_or_else(|_| json!({}));
        if let Some(fields) = server.as_object_mut() {
            fields.remove("remote_password_hash");
            fields.insert(
                "remote_password_set".into(),
                json!(file.remote_password_hash.is_some()),
            );
        }
        json!({
            "default": settings.default,
            "roles": settings.roles,
            "server": server,
            "effective": {
                "remote": self.started.remote_port.is_some(),
                "remote_port": self.started.remote_port,
                "remote_password_set": self.started.remote_password_hash.is_some(),
                "port": self.started.port,
                "sandbox": sandbox::enabled(),
                "sandbox_from_command_line": self.started.sandbox_flag.is_some(),
                "credentials": secrets::mode().as_str(),
                "stream_idle_seconds": file.stream_idle().as_secs(),
                "claude_cli_channel": file
                    .claude_cli_channel
                    .as_deref()
                    .unwrap_or(claude_cli::DEFAULT_CHANNEL),
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

    /// Hands the person's answer to a question (an MCP server's, or an
    /// approval prompt) that the running turn of `thread` waits for;
    /// `false` when none waits.
    pub fn answer(&self, thread: SessionId, elicitation: u64, answer: Value) -> bool {
        self.store()
            .map(|store| store.answer_elicitation(thread, elicitation, answer))
            .unwrap_or(false)
    }

    /// The questions the running turn of `thread` waits on, oldest first,
    /// as the `elicitation` events showed them.
    pub fn elicitations(&self, thread: SessionId) -> Vec<Value> {
        self.store()
            .map(|store| {
                store
                    .elicitations(thread)
                    .into_iter()
                    .map(|(_, question)| question)
                    .filter(|question| !question.is_null())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// `mcp/resources/list`, `mcp/resources/templates/list`,
    /// `mcp/resources/read` and `mcp/complete`: what the MCP servers offer
    /// beside their tools (docs/evidence/mcp-client.md).
    pub async fn mcp_request(&self, method: &str, params: &Value) -> Result<Value, ErrorInfo> {
        mcp::request(self.store()?, method, params).await
    }

    /// `thread/titles/subscribe`: the titles threads are given from now on,
    /// as `(thread, title)`, until the value returned is dropped.
    pub fn title_changes(
        &self,
    ) -> Result<tokio::sync::broadcast::Receiver<(SessionId, String)>, ErrorInfo> {
        Ok(self.store()?.title_changes())
    }

    /// `mcp/resources/subscribe`: the updates of one resource of one MCP
    /// server, until the value returned is dropped.
    pub async fn mcp_subscribe(&self, params: &Value) -> Result<ResourceUpdates, ErrorInfo> {
        mcp::subscribe(self.store()?, params).await
    }

    /// What the person sends to start a turn of `thread`: `text`, the
    /// message it replies to, and the files of `requested`, read now from
    /// the data sent or the thread's project folder. Refused with
    /// `invalid_params`, before anything is stored, when a file cannot be
    /// read or the thread's model is known not to take images
    /// (docs/evidence/input-attachments.md, docs/evidence/file-mentions.md).
    pub async fn turn_prompt(
        &self,
        thread: SessionId,
        text: String,
        reply_to: Option<i64>,
        requested: attach::Requested,
    ) -> Result<turn::Prompt, ErrorInfo> {
        let mut prompt = turn::Prompt {
            text,
            reply_to,
            attached: Default::default(),
            shell: None,
        };
        if requested.is_empty() {
            return Ok(prompt);
        }
        let store = self.store()?;
        if !store.session_exists(thread).await? {
            return Err(unknown_thread());
        }
        let folder = store
            .session_project(thread)
            .await?
            .map(|(_, key)| std::path::PathBuf::from(key));
        let attached = attach::gather(requested, folder).await?;
        if let Some(image) = attached.first_image() {
            let model = self.thread_client(thread).await?;
            let listed = catalog::kept(&store.files_folder().join("catalog"), "models-dev");
            if listed.and_then(|listed| catalog::takes_images(&listed, model.model()))
                == Some(false)
            {
                return Err(ErrorInfo::new(
                    "invalid_params",
                    format!(
                        "the model {} of this thread does not take images, so {image} cannot \
                         be shown to it; choose a model that reads images, or send the message \
                         without it",
                        model.model()
                    ),
                ));
            }
        }
        prompt.attached = attached;
        Ok(prompt)
    }

    /// `files/search`: up to `limit` files of the project folder of
    /// `thread` whose paths match `query` fuzzily, best first, leaving out
    /// what `.gitignore` leaves out.
    /// Runs `event`'s hooks for `thread` working in `folder`, outside a
    /// turn; gives what they say to the person (docs/evidence/hooks.md).
    pub(crate) async fn run_hooks(
        &self,
        thread: SessionId,
        folder: Option<&std::path::Path>,
        event: &str,
        target: Option<&str>,
        input: Value,
    ) -> Vec<String> {
        let Ok(store) = self.store() else {
            return Vec::new();
        };
        let hooks = hooks::Hooks::load(store, thread.0, folder);
        if !hooks.has(event) {
            return Vec::new();
        }
        hooks.run(event, target, input).await.messages
    }

    /// `hooks/list {thread_id?}`: the settings' hooks, and the hooks of the
    /// thread's project with whether they are trusted as they are.
    pub async fn hooks(&self, thread: Option<SessionId>) -> Result<Value, ErrorInfo> {
        let store = self.store()?;
        let folder = match thread {
            Some(thread) => self.thread_folder(thread).await?,
            None => None,
        };
        Ok(hooks::listing(store, folder.as_deref()))
    }

    /// `hooks/trust {thread_id}`: trusts the hooks of the thread's project
    /// as they are now; `false` when it has none.
    pub async fn trust_hooks(&self, thread: SessionId) -> Result<Value, ErrorInfo> {
        let store = self.store()?;
        let folder = self
            .thread_folder(thread)
            .await?
            .ok_or_else(|| ErrorInfo::new("invalid_params", "this thread has no project folder"))?;
        let trusted = hooks::trust(store, &folder)
            .map_err(|message| ErrorInfo::new("hooks_failed", message))?;
        Ok(json!({ "trusted": trusted, "project": folder.to_string_lossy() }))
    }

    async fn thread_folder(
        &self,
        thread: SessionId,
    ) -> Result<Option<std::path::PathBuf>, ErrorInfo> {
        let store = self.store()?;
        if !store.session_exists(thread).await? {
            return Err(unknown_thread());
        }
        Ok(store
            .session_project(thread)
            .await?
            .map(|(_, key)| std::path::PathBuf::from(key)))
    }

    /// Moves `thread` to the project of `folder`, an existing folder named by
    /// its absolute path; its next turn works there. Refused while a turn
    /// runs (docs/evidence/thread-folders.md).
    pub async fn move_thread(&self, thread: SessionId, folder: &str) -> Result<Value, ErrorInfo> {
        let store = self.store()?;
        if !store.session_exists(thread).await? {
            return Err(unknown_thread());
        }
        self.refuse_while_running(thread)?;
        let path = std::path::Path::new(folder);
        if !path.is_absolute() || !path.is_dir() {
            return Err(ErrorInfo::new(
                "invalid_params",
                format!("{folder} is not an existing folder named by its absolute path"),
            ));
        }
        let before = self.thread_folder(thread).await?;
        let project = store.project(folder).await?;
        store.set_session_project(thread, project).await?;
        let said = self
            .run_hooks(
                thread,
                Some(path),
                "CwdChanged",
                None,
                json!({ "old_cwd": before.map(|folder| folder.to_string_lossy().into_owned()),
                        "new_cwd": folder }),
            )
            .await;
        Ok(json!({ "thread_id": thread.0, "project": folder, "hook_messages": said }))
    }

    fn refuse_while_running(&self, thread: SessionId) -> Result<(), ErrorInfo> {
        if self.turn_running(thread) {
            return Err(ErrorInfo::new(
                "turn_running",
                "a turn is running in this thread; change its folder when the turn ends",
            ));
        }
        Ok(())
    }

    /// Gives `thread` a git worktree of its project's repository, on a new
    /// branch, and moves the thread there. The worktree is removed with the
    /// thread when it has nothing uncommitted (docs/evidence/thread-folders.md).
    pub async fn add_worktree(
        &self,
        thread: SessionId,
        branch: Option<&str>,
        base: Option<&str>,
    ) -> Result<Value, ErrorInfo> {
        let store = self.store()?;
        if !store.session_exists(thread).await? {
            return Err(unknown_thread());
        }
        self.refuse_while_running(thread)?;
        let repo = store
            .session_project(thread)
            .await?
            .map(|(_, key)| std::path::PathBuf::from(key))
            .filter(|folder| folder.is_dir())
            .ok_or_else(|| {
                ErrorInfo::new(
                    "invalid_params",
                    "this thread has no project folder to make a worktree of",
                )
            })?;
        let target = worktree::place(&store.files_folder(), thread.0);
        if target.exists() {
            return Err(ErrorInfo::new(
                "already_exists",
                format!("this thread already has a worktree in {}", target.display()),
            ));
        }
        let branch = branch.map_or_else(|| worktree::default_branch(thread.0), str::to_string);
        worktree::add(&repo, &target, &branch, base).await?;
        let folder = target.to_string_lossy().into_owned();
        let project = store.project(&folder).await?;
        store.set_session_project(thread, project).await?;
        let mut said = self
            .run_hooks(
                thread,
                Some(&target),
                "WorktreeCreate",
                None,
                json!({ "worktree_path": folder, "branch": branch,
                        "repository": repo.to_string_lossy() }),
            )
            .await;
        said.extend(
            self.run_hooks(
                thread,
                Some(&target),
                "CwdChanged",
                None,
                json!({ "old_cwd": repo.to_string_lossy(), "new_cwd": folder }),
            )
            .await,
        );
        Ok(json!({
            "thread_id": thread.0, "project": folder, "branch": branch,
            "from": repo.to_string_lossy(), "hook_messages": said,
        }))
    }

    /// `groups/*`, `projects/group` and `thread/group`: the groups that hold
    /// projects and threads without a project, nested to any depth
    /// (docs/evidence/project-groups.md).
    pub async fn groups(&self) -> Result<Value, ErrorInfo> {
        let (groups, projects) = self.store()?.groups().await?;
        Ok(json!({ "groups": groups, "projects": projects }))
    }

    pub async fn create_group(&self, name: &str, parent: Option<i64>) -> Result<Value, ErrorInfo> {
        let id = self.store()?.create_group(name, parent).await?;
        Ok(json!({ "group_id": id }))
    }

    pub async fn rename_group(&self, id: i64, name: &str) -> Result<Value, ErrorInfo> {
        self.store()?.rename_group(id, name).await?;
        Ok(json!({ "group_id": id, "name": name.trim() }))
    }

    pub async fn move_group(&self, id: i64, parent: Option<i64>) -> Result<Value, ErrorInfo> {
        self.store()?.move_group(id, parent).await?;
        Ok(json!({ "group_id": id, "parent_id": parent }))
    }

    pub async fn delete_group(&self, id: i64) -> Result<Value, ErrorInfo> {
        self.store()?.delete_group(id).await?;
        Ok(json!({ "deleted": id }))
    }

    pub async fn group_project(
        &self,
        project: &str,
        group: Option<i64>,
    ) -> Result<Value, ErrorInfo> {
        self.store()?.group_project(project, group).await?;
        Ok(json!({ "project": project, "group_id": group }))
    }

    pub async fn group_thread(
        &self,
        thread: SessionId,
        group: Option<i64>,
    ) -> Result<Value, ErrorInfo> {
        self.store()?.group_thread(thread, group).await?;
        Ok(json!({ "thread_id": thread.0, "group_id": group }))
    }

    /// Every installed plugin, on or off (docs/evidence/plugin-management.md).
    pub async fn plugins(&self) -> Result<Value, ErrorInfo> {
        let root = plugins::folder(self.store()?);
        let plugins = tokio::task::spawn_blocking(move || plugins::listing(&root))
            .await
            .map_err(|error| ErrorInfo::new("internal", error.to_string()))?;
        Ok(json!({ "plugins": plugins }))
    }

    pub async fn search_plugins(&self, query: &str) -> Result<Value, ErrorInfo> {
        let found = plugins::search(self.store()?, &json!({ "query": query }).to_string())
            .await
            .map_err(|error| ErrorInfo::new("plugins", error))?;
        Ok(serde_json::from_str(&found).unwrap_or_else(|_| json!({ "text": found })))
    }

    /// Installs `name@catalog` as the person asked, without the approval the
    /// model's `plugin_install` waits for.
    pub async fn install_plugin(&self, plugin: &str) -> Result<Value, ErrorInfo> {
        let text = plugins::install(self.store()?, &json!({ "plugin": plugin }).to_string())
            .await
            .map_err(|error| ErrorInfo::new("plugins", error))?;
        Ok(json!({ "text": text }))
    }

    pub async fn remove_plugin(&self, plugin: &str) -> Result<Value, ErrorInfo> {
        plugins::remove(self.store()?, plugin)
            .await
            .map_err(|error| ErrorInfo::new("plugins", error))?;
        Ok(json!({ "removed": plugin }))
    }

    /// Turns `name@catalog` on or off; `changed` is `false` when it already
    /// was.
    pub async fn enable_plugin(&self, plugin: &str, enabled: bool) -> Result<Value, ErrorInfo> {
        let changed = plugins::set_enabled(self.store()?, plugin, enabled)
            .await
            .map_err(|error| ErrorInfo::new("plugins", error))?;
        Ok(json!({ "plugin": plugin, "enabled": enabled, "changed": changed }))
    }

    /// Answers a side question about `thread` with its model, sending the
    /// answer's text on `deltas` as it comes; nothing is kept in the thread
    /// (docs/evidence/side-question.md).
    pub async fn aside(
        &self,
        thread: SessionId,
        question: &str,
        deltas: tokio::sync::mpsc::Sender<String>,
        cancel: watch::Receiver<bool>,
    ) -> Result<String, ErrorInfo> {
        let store = self.store()?;
        if !store.session_exists(thread).await? {
            return Err(unknown_thread());
        }
        let llm = self.thread_client(thread).await?;
        turn::aside(&llm, store, thread, question, deltas, cancel).await
    }

    /// The job log folder, once `thread` is known to exist and to have job
    /// `id` when one is named (docs/evidence/background-jobs.md).
    async fn job_place(
        &self,
        thread: SessionId,
        id: Option<u64>,
    ) -> Result<std::path::PathBuf, ErrorInfo> {
        let store = self.store()?;
        if !store.session_exists(thread).await? {
            return Err(unknown_thread());
        }
        let logs = store.files_folder().join("logs");
        if let Some(id) = id {
            if !shell::list(&logs, thread).iter().any(|job| job.id == id) {
                return Err(ErrorInfo::new(
                    "unknown_job",
                    format!("no job {id} in this thread"),
                ));
            }
        }
        Ok(logs)
    }

    /// The commands the thread's turns started, oldest first.
    pub async fn jobs(&self, thread: SessionId) -> Result<Vec<JobInfo>, ErrorInfo> {
        let logs = self.job_place(thread, None).await?;
        Ok(shell::list(&logs, thread))
    }

    pub async fn job_log(
        &self,
        thread: SessionId,
        id: u64,
        offset: u64,
        limit: u64,
    ) -> Result<JobLog, ErrorInfo> {
        let logs = self.job_place(thread, Some(id)).await?;
        tokio::task::spawn_blocking(move || shell::read_log(&logs, thread, id, offset, limit))
            .await
            .map_err(|error| ErrorInfo::new("internal", error.to_string()))?
            .map_err(|message| ErrorInfo::new("job_failed", message))
    }

    /// Types `input` and then `keys` (named as the `job` tool names them)
    /// into a running job; returns how many bytes were sent.
    pub async fn write_job(
        &self,
        thread: SessionId,
        id: u64,
        input: Option<&str>,
        keys: &[String],
    ) -> Result<usize, ErrorInfo> {
        let logs = self.job_place(thread, Some(id)).await?;
        let bytes = shell::typed(input, keys)
            .map_err(|message| ErrorInfo::new("invalid_params", message))?;
        shell::write_to(&logs, thread, id, &bytes)
            .map_err(|message| ErrorInfo::new("job_failed", message))?;
        Ok(bytes.len())
    }

    /// Stops a job; `false` when it had already ended.
    pub async fn stop_job(&self, thread: SessionId, id: u64) -> Result<bool, ErrorInfo> {
        let logs = self.job_place(thread, Some(id)).await?;
        shell::stop_job(&logs, thread, id).map_err(|message| ErrorInfo::new("job_failed", message))
    }

    pub async fn watch_job(&self, thread: SessionId, id: u64) -> Result<JobWatch, ErrorInfo> {
        let logs = self.job_place(thread, Some(id)).await?;
        shell::watch_job(&logs, thread, id).map_err(|message| ErrorInfo::new("job_failed", message))
    }

    pub async fn search_files(
        &self,
        thread: SessionId,
        query: String,
        limit: usize,
    ) -> Result<Value, ErrorInfo> {
        let store = self.store()?;
        if !store.session_exists(thread).await? {
            return Err(unknown_thread());
        }
        let folder = store
            .session_project(thread)
            .await?
            .map(|(_, key)| std::path::PathBuf::from(key))
            .ok_or_else(|| {
                ErrorInfo::new(
                    "invalid_params",
                    "this thread has no project folder to search",
                )
            })?;
        let found = tokio::task::spawn_blocking(move || find::fuzzy_paths(&folder, &query, limit))
            .await
            .map_err(|error| ErrorInfo::new("internal", error.to_string()))?;
        let files: Vec<Value> = found
            .into_iter()
            .map(|(path, score)| json!({ "path": path, "score": score }))
            .collect();
        Ok(json!({ "files": files }))
    }

    /// Runs the turn [`Self::begin_turn`] claimed. Its events go to
    /// `events` and to those watching the thread.
    pub async fn run_turn(
        &self,
        thread: SessionId,
        prompt: turn::Prompt,
        approvals: turn::Approvals,
        claim: Claim,
        cancel: watch::Receiver<bool>,
        events: mpsc::Sender<TurnEvent>,
    ) -> Outcome {
        let _ = self.activity.send((thread, Activity::TurnStarted));
        let outcome = self
            .run_claimed(thread, prompt, approvals, claim, cancel, events)
            .await;
        let _ = self
            .activity
            .send((thread, Activity::TurnEnded(TurnEnd::of(&outcome))));
        outcome
    }

    async fn run_claimed(
        &self,
        thread: SessionId,
        prompt: turn::Prompt,
        approvals: turn::Approvals,
        claim: Claim,
        cancel: watch::Receiver<bool>,
        events: mpsc::Sender<TurnEvent>,
    ) -> Outcome {
        let failed = |error| Outcome {
            ending: turn::Ending::Failed(error),
            usage: Default::default(),
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
        let mut models = match self
            .helpers(&conversation, project.as_deref(), claim.model, claim.mode)
            .await
        {
            Ok(models) => models,
            Err(error) => return failed(error),
        };
        models.title = match self.settings().await {
            Ok(settings) => Some(title::Titling {
                listed: conversation
                    .provider
                    .as_deref()
                    .and_then(|id| settings.provider(id))
                    .and_then(|provider| provider.models.clone()),
            }),
            Err(error) => return failed(error),
        };
        // Every event passes here on its way to `events`, so those watching
        // the thread see it too, in the same order. The thread waits while
        // a question to the person is open: `waiting` goes out before the
        // first question and `running` after the last is settled, all
        // between the statuses `begin_turn` and `end_turn` send.
        let (teed, mut teeing) = mpsc::channel(1);
        let activity = self.activity.clone();
        let changed = Arc::new(tokio::sync::Notify::new());
        let files_may_change = Arc::clone(&changed);
        let tee = tokio::spawn(async move {
            let mut open = std::collections::HashSet::new();
            while let Some(event) = teeing.recv().await {
                if let TurnEvent::Message { message, .. } = &event {
                    if let conversation::Body::ToolResult { name, .. } = &message.body {
                        if !leaves_files(name) {
                            files_may_change.notify_one();
                        }
                    }
                }
                let status = match &event {
                    TurnEvent::Elicitation(question) => question["id"]
                        .as_u64()
                        .filter(|&id| open.insert(id) && open.len() == 1)
                        .map(|_| ThreadStatus::Waiting),
                    _ => None,
                };
                if let Some(status) = status {
                    let _ = activity.send((thread, Activity::Status(status)));
                }
                let _ = activity.send((thread, Activity::Event(Clone::clone(&event))));
                if let TurnEvent::Answered(id) = &event {
                    if open.remove(id) && open.is_empty() {
                        let _ = activity.send((thread, Activity::Status(ThreadStatus::Running)));
                    }
                }
                let _ = events.send(event).await;
            }
        });
        if let Ok(mut turns) = self.turns.lock() {
            if let Some(running) = turns.get_mut(&thread) {
                running.events = Some(teed.clone());
            }
        }
        let (snapshot, copy) = match self.take_snapshot(thread).await {
            Ok(copy) => (None, copy),
            Err(error) => (Some(error), None),
        };
        // The diff is made beside the turn, so the model does not wait for
        // it; the last one is made once the turn's calls are done.
        let (finish, finished) = tokio::sync::oneshot::channel();
        let differ =
            copy.map(|copy| tokio::spawn(send_diffs(copy, changed, finished, teed.clone())));
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
                teed,
            ),
        )
        .await;
        forward.abort();
        let _ = finish.send(());
        if let Some(differ) = differ {
            let _ = differ.await;
        }
        let waiting = Waiting {
            questions: outcome.questions.clone(),
        };
        if let Ok(mut turns) = self.turns.lock() {
            if let Some(running) = turns.get_mut(&thread) {
                running.events = None;
                running.waiting = !waiting.is_empty();
            }
        }
        // Every event is passed on before the turn's end is reported.
        let _ = tee.await;
        if let Some(error) = snapshot {
            outcome.warnings.push(format!(
                "the project's files were not copied before this turn, so they cannot be put \
                 back to how they were before it: {}",
                error.message
            ));
        }
        if let Err(error) = self.set_waiting(thread, &waiting).await {
            outcome.warnings.push(format!(
                "the questions were not saved, so /answer may not find them: {}",
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
            .set_waiting(thread, text(serde_json::to_string(&waiting.questions))?)
            .await
    }

    /// What the latest turn of `thread` left for the person.
    async fn waiting(&self, thread: SessionId) -> Result<Waiting, ErrorInfo> {
        let Some(questions) = self.store()?.waiting(thread).await? else {
            return Ok(Waiting::default());
        };
        let parse = |error: serde_json::Error| {
            ErrorInfo::new("store", format!("the waiting questions: {error}"))
        };
        Ok(Waiting {
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

/// What `thread/rewind` puts back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Restore {
    /// The messages only.
    Conversation,
    /// The project's files only; the messages stay.
    Files,
    /// The files, then the messages.
    Both,
}

/// Where `thread/fork` cuts a thread, by the number of a message of the
/// person.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkCut {
    /// Every message.
    Whole,
    /// The messages before the person's message, so that it can be written
    /// again.
    Before(i64),
    /// The person's message and what followed it until the person's next
    /// message.
    Through(i64),
}

/// The number of the last message a fork at `cut` keeps of `messages`; the
/// quote of a reply that came with a message left out is left out too.
fn fork_end(messages: &[StoredMessage], cut: ForkCut, seq: i64) -> Result<i64, ErrorInfo> {
    use conversation::{Body, Speaker};
    let by_person = |stored: &StoredMessage| {
        matches!(
            &stored.message.body,
            Body::User {
                from: Speaker::Person,
                ..
            }
        )
    };
    let at = messages
        .iter()
        .position(|stored| stored.seq == seq && by_person(stored))
        .ok_or_else(|| {
            ErrorInfo::new(
                "invalid_params",
                format!("message {seq} is not a message of the person in this thread"),
            )
        })?;
    let mut end = match cut {
        ForkCut::Before(_) => at,
        _ => messages[at + 1..]
            .iter()
            .position(by_person)
            .map_or(messages.len(), |next| at + 1 + next),
    };
    while end > 0
        && matches!(
            &messages[end - 1].message.body,
            Body::User { from: Speaker::Harness(kind), .. } if kind == turn::REPLY_KIND
        )
    {
        end -= 1;
    }
    Ok(end
        .checked_sub(1)
        .map_or(i64::MIN, |last| messages[last].seq))
}

/// The copy of a project's files taken before a turn: where the copies
/// are kept, the project's folder, the copy's tree, and the number of the
/// turn's first message.
struct TurnCopy {
    copies: std::path::PathBuf,
    folder: std::path::PathBuf,
    tree: String,
    seq: i64,
}

/// Whether the tool `name` is known to leave the project's files as they
/// are. After a call to any other tool the turn's diff is made again, so an
/// MCP tool, a shell command or input to a job always counts as a change
/// (docs/evidence/turn-diff.md).
fn leaves_files(name: &str) -> bool {
    matches!(
        name,
        read::READ
            | find::FIND
            | explore::EXPLORE
            | fetch::FETCH
            | web_search::SEARCH
            | mcp::SEARCH
            | plugins::SEARCH
            | agent_tools::PLAN
            | agent_tools::RECALL
            | agent_tools::LESSON
            | agent_tools::METHOD
            | agent_tools::ISSUE
            | ask::ASK
            | title::TITLE
            | promise::PROMISE
    )
}

/// A diff as `diff` events and `thread/diff` give it.
fn diff_value(seq: i64, diff: snapshot::Diff) -> Value {
    json!({ "seq": seq, "files": diff.files, "text": diff.text, "truncated": diff.truncated })
}

/// Sends the turn's diff as a [`TurnEvent::Diff`] each time `changed` is
/// notified and once more when `finished` fires, whenever it differs from
/// the last one sent; a diff with no file is not sent while none was. A
/// diff that cannot be made is skipped.
async fn send_diffs(
    copy: TurnCopy,
    changed: Arc<tokio::sync::Notify>,
    mut finished: tokio::sync::oneshot::Receiver<()>,
    events: mpsc::Sender<TurnEvent>,
) {
    let mut sent: Option<snapshot::Diff> = None;
    loop {
        let last = tokio::select! {
            biased;
            _ = &mut finished => true,
            () = changed.notified() => false,
        };
        if let Ok(diff) = snapshot::diff(&copy.copies, &copy.folder, &copy.tree).await {
            let same = sent
                .as_ref()
                .map_or(diff.files.is_empty(), |sent| *sent == diff);
            if !same {
                let _ = events
                    .send(TurnEvent::Diff(diff_value(copy.seq, diff.clone())))
                    .await;
                sent = Some(diff);
            }
        }
        if last {
            return;
        }
    }
}

fn unknown_thread() -> ErrorInfo {
    ErrorInfo::new("unknown_thread", "no thread has this id")
}

pub(crate) fn archived_thread() -> ErrorInfo {
    ErrorInfo::new(
        "thread_archived",
        "this thread is archived; unarchive it with thread/unarchive to go on",
    )
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
