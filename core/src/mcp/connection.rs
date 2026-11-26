//! One MCP server spoken to with JSON-RPC: which protocol era it speaks, how
//! requests are framed for that era, and what the server sends on its own.
//! Design and sources: docs/evidence/mcp-client.md.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use reqwest::header::HeaderMap;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

use super::http::{self, Http, ParamHeader};

/// The stateless revision: no handshake; every request carries its version,
/// the client's identity and capabilities in `_meta`.
pub(super) const MODERN_VERSION: &str = "2026-07-28";
/// The version `initialize` asks for.
pub(super) const LEGACY_VERSION: &str = "2025-11-25";
/// Versions with an `initialize` handshake whose `initialize`, `tools/list`
/// and `tools/call` are the ones used here.
pub(super) const LEGACY_VERSIONS: [&str; 4] =
    ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
/// The tasks extension: a server may answer a call with a task to poll.
pub(super) const TASKS: &str = "io.modelcontextprotocol/tasks";
/// How long a stdio server may leave `server/discover` unanswered before
/// `initialize` is sent beside it: a legacy server may answer an unknown
/// request with nothing at all, and a slow one has not read it yet.
const PROBE_WAIT: Duration = Duration::from_secs(5);
/// How long a message that only ends something may take; it is not waited
/// for longer.
pub(super) const END_TIMEOUT: Duration = Duration::from_secs(5);

/// The error codes the 2026-07-28 revision defines; only a modern server
/// sends them.
pub(super) const HEADER_MISMATCH: i64 = -32020;
const MISSING_CAPABILITY: i64 = -32021;
const UNSUPPORTED_VERSION: i64 = -32022;
/// 2025-11-25: the call needs the person to visit a URL first.
pub(super) const URL_ELICITATION_REQUIRED: i64 = -32042;

/// The error of a request to a server that had stopped before it was sent.
pub(super) const NOT_RUNNING: &str = "the MCP server is not running";
/// The error of a request whose server stopped before it answered.
pub(super) const STOPPED: &str = "the MCP server has stopped";
/// The error of a request whose session the server ended (HTTP 404).
const SESSION_ENDED: &str = "the MCP server ended the session";

/// A JSON-RPC error a server answered with.
#[derive(Debug, Clone)]
pub(super) struct RpcError {
    pub(super) code: i64,
    pub(super) message: String,
    pub(super) data: Value,
}

impl RpcError {
    pub(super) fn of(error: &Value) -> Box<Self> {
        Box::new(Self {
            code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
            message: error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            data: error.get("data").cloned().unwrap_or(Value::Null),
        })
    }

    /// Whether only a server of the stateless revision answers with it.
    pub(super) fn modern(&self) -> bool {
        matches!(
            self.code,
            HEADER_MISMATCH | MISSING_CAPABILITY | UNSUPPORTED_VERSION
        )
    }
}

/// Why a request got no result.
#[derive(Debug)]
pub(super) enum Fault {
    Rpc(Box<RpcError>),
    /// An HTTP status other than a success, with the JSON-RPC error its body
    /// carried, if any.
    Status {
        status: reqwest::StatusCode,
        rpc: Option<Box<RpcError>>,
        body: String,
    },
    NotRunning,
    Stopped,
    SessionEnded,
    /// A response stream of the stateless revision closed before its answer:
    /// the request is lost and is sent again as a new one.
    Broken,
    Other(String),
}

impl Fault {
    pub(super) fn rpc(&self) -> Option<&RpcError> {
        match self {
            Self::Rpc(rpc) => Some(&**rpc),
            Self::Status { rpc, .. } => rpc.as_deref(),
            _ => None,
        }
    }
}

impl fmt::Display for Fault {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rpc(rpc) => {
                write!(formatter, "MCP error {}: {}", rpc.code, rpc.message)?;
                if rpc.code == MISSING_CAPABILITY {
                    let required = rpc.data.get("requiredCapabilities").unwrap_or(&Value::Null);
                    write!(
                        formatter,
                        " (it requires the client capabilities {required})"
                    )?;
                }
                Ok(())
            }
            Self::Status { status, body, .. } => {
                write!(formatter, "the MCP server answered {status}: {body}")
            }
            Self::NotRunning => formatter.write_str(NOT_RUNNING),
            Self::Stopped => formatter.write_str(STOPPED),
            Self::SessionEnded => formatter.write_str(SESSION_ENDED),
            Self::Broken => {
                formatter.write_str("the MCP server closed the stream before answering")
            }
            Self::Other(text) => formatter.write_str(text),
        }
    }
}

/// The protocol a server was found to speak, with its version.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Era {
    Modern(String),
    Legacy(String),
}

/// A question of the server for the person, and where its answer goes.
pub(crate) struct Elicitation {
    pub(super) params: Value,
    pub(super) reply: oneshot::Sender<Value>,
}

/// A server's request to run the model, and where the result or the
/// JSON-RPC error (code and message) goes.
pub(crate) struct Sampled {
    pub(super) params: Value,
    pub(super) reply: oneshot::Sender<Result<Value, (i64, String)>>,
}

/// What a server sends while a call waits.
pub(super) enum Notice {
    Question(Elicitation),
    /// `sampling/createMessage` of an older server.
    Sampling(Sampled),
    /// `notifications/progress` for the call's token.
    Progress(Value),
    /// `notifications/message`.
    Log(Value),
}

/// A call waiting on a connection: the thread it is made for, and where the
/// server's questions, progress and logs go when they may be shown.
pub(super) struct Waiter {
    call: u64,
    session: Option<crate::store::SessionId>,
    notices: Option<mpsc::UnboundedSender<Notice>>,
}

/// What a connection's readers share with the calls waiting on it.
#[derive(Default)]
pub(super) struct Shared {
    /// The server's name in `mcp.json`, shown with its questions and logs.
    pub(super) server: String,
    pub(super) waiters: Mutex<Vec<Waiter>>,
    /// Where each waiting call's progress goes, by its progress token.
    progress: Mutex<HashMap<String, mpsc::UnboundedSender<Notice>>>,
    /// Raised when the server says its tools changed; they are listed again
    /// before the next model request.
    pub(super) tools_changed: AtomicBool,
    /// Counts the server's words that its prompts, or its resources, changed;
    /// a list kept from an earlier count is listed again.
    pub(super) prompts_changed: AtomicU64,
    pub(super) resources_changed: AtomicU64,
    /// Who waits for the updates of which resource.
    watchers: Mutex<Vec<Watcher>>,
    /// URL questions put to the person, by their `elicitationId`: the
    /// server's word that one is complete answers it.
    completions: Mutex<HashMap<String, oneshot::Sender<()>>>,
    /// The project folder of the thread that last made a request here: the
    /// root an older server is given.
    folder: Mutex<Option<PathBuf>>,
    /// Whether an older server asked for the roots, and so is told when
    /// they change.
    roots_asked: AtomicBool,
}

/// One who waits for the updates of the resource `uri`.
struct Watcher {
    id: u64,
    uri: String,
    updates: mpsc::UnboundedSender<String>,
}

impl Shared {
    pub(super) fn new(server: &str) -> Arc<Self> {
        Arc::new(Self {
            server: server.to_string(),
            ..Self::default()
        })
    }

    /// Where a notice that names no call goes: the call that waits, when
    /// every waiting call is for one and the same thread; otherwise nowhere,
    /// so nothing reaches another thread's person.
    fn one_thread(&self) -> Option<mpsc::UnboundedSender<Notice>> {
        let waiting = self.waiters.lock().expect("waiters lock");
        let session = waiting.first().and_then(|waiter| waiter.session);
        let one = session.is_some() && waiting.iter().all(|waiter| waiter.session == session);
        waiting
            .first()
            .filter(|_| one)
            .and_then(|waiter| waiter.notices.clone())
    }

    /// Handles a notification of the server.
    pub(super) fn notice(&self, message: &Value) {
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        match message.get("method").and_then(Value::as_str) {
            Some("notifications/progress") => {
                let Some(token) = params.get("progressToken").map(token_key) else {
                    return;
                };
                let target = self
                    .progress
                    .lock()
                    .expect("progress lock")
                    .get(&token)
                    .cloned();
                if let Some(target) = target {
                    let _ = target.send(Notice::Progress(params));
                }
            }
            Some("notifications/message") => {
                eprintln!(
                    "MCP server {} [{}] {}{}",
                    self.server,
                    params.get("level").and_then(Value::as_str).unwrap_or("?"),
                    params
                        .get("logger")
                        .and_then(Value::as_str)
                        .map(|logger| format!("{logger}: "))
                        .unwrap_or_default(),
                    params.get("data").unwrap_or(&Value::Null)
                );
                if let Some(target) = self.one_thread() {
                    let _ = target.send(Notice::Log(params));
                }
            }
            Some("notifications/tools/list_changed") => {
                self.tools_changed.store(true, Ordering::SeqCst);
            }
            Some("notifications/prompts/list_changed") => {
                self.prompts_changed.fetch_add(1, Ordering::SeqCst);
            }
            Some("notifications/resources/list_changed") => {
                self.resources_changed.fetch_add(1, Ordering::SeqCst);
            }
            Some("notifications/resources/updated") => {
                let Some(uri) = params.get("uri").and_then(Value::as_str) else {
                    return;
                };
                for watcher in self.watchers.lock().expect("watchers lock").iter() {
                    if watcher.uri == uri {
                        let _ = watcher.updates.send(uri.to_string());
                    }
                }
            }
            Some("notifications/elicitation/complete") => {
                let id = params.get("elicitationId").and_then(Value::as_str);
                let waiting = id.and_then(|id| {
                    self.completions
                        .lock()
                        .expect("completions lock")
                        .remove(id)
                });
                // Unknown and already completed ids are ignored.
                if let Some(waiting) = waiting {
                    let _ = waiting.send(());
                }
            }
            _ => {}
        }
    }

    /// Where the server's word that the URL question `id` is complete
    /// arrives; given up with [`Self::forget_completion`].
    pub(super) fn await_completion(&self, id: &str) -> oneshot::Receiver<()> {
        let (sender, receiver) = oneshot::channel();
        self.completions
            .lock()
            .expect("completions lock")
            .insert(id.to_string(), sender);
        receiver
    }

    pub(super) fn forget_completion(&self, id: &str) {
        self.completions
            .lock()
            .expect("completions lock")
            .remove(id);
    }

    /// Notes `folder` as the folder of the thread now using the server;
    /// whether that changed the roots of an older server that asked for
    /// them.
    fn use_folder(&self, folder: &Path) -> bool {
        let mut known = self.folder.lock().expect("folder lock");
        if known.as_deref() == Some(folder) {
            return false;
        }
        *known = Some(folder.to_path_buf());
        self.roots_asked.load(Ordering::SeqCst)
    }

    /// The URIs watched, each once, in order.
    fn watched(&self) -> Vec<String> {
        let mut uris: Vec<String> = self
            .watchers
            .lock()
            .expect("watchers lock")
            .iter()
            .map(|watcher| watcher.uri.clone())
            .collect();
        uris.sort();
        uris.dedup();
        uris
    }
}

/// The roots of the client, as `roots/list` answers: the project folder as
/// a `file://` URI, or none without one.
pub(super) fn roots(folder: Option<&Path>) -> Value {
    let roots: Vec<Value> = folder
        .and_then(|folder| {
            let uri = reqwest::Url::from_file_path(folder).ok()?;
            let name = folder
                .file_name()
                .map(|name| name.to_string_lossy().into_owned());
            Some(match name {
                Some(name) => json!({"uri": uri.as_str(), "name": name}),
                None => json!({"uri": uri.as_str()}),
            })
        })
        .into_iter()
        .collect();
    json!({"roots": roots})
}

/// A progress token as a key: a string as it is, a number as its digits.
fn token_key(token: &Value) -> String {
    token
        .as_str()
        .map_or_else(|| token.to_string(), str::to_string)
}

/// A call's place among the waiting calls, given up when the call ends.
pub(super) struct Waiting<'a> {
    shared: &'a Shared,
    call: u64,
    token: Option<String>,
}

impl<'a> Waiting<'a> {
    pub(super) fn register(
        shared: &'a Shared,
        session: Option<crate::store::SessionId>,
        notices: Option<mpsc::UnboundedSender<Notice>>,
        token: Option<String>,
    ) -> Self {
        static CALLS: AtomicU64 = AtomicU64::new(0);
        let call = CALLS.fetch_add(1, Ordering::Relaxed);
        if let (Some(token), Some(notices)) = (&token, &notices) {
            shared
                .progress
                .lock()
                .expect("progress lock")
                .insert(token.clone(), notices.clone());
        }
        shared.waiters.lock().expect("waiters lock").push(Waiter {
            call,
            session,
            notices,
        });
        Self {
            shared,
            call,
            token,
        }
    }
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        if let Ok(mut waiting) = self.shared.waiters.lock() {
            waiting.retain(|waiter| waiter.call != self.call);
        }
        if let (Some(token), Ok(mut progress)) = (&self.token, self.shared.progress.lock()) {
            progress.remove(token);
        }
    }
}

/// Hands a server's `elicitation/create` to the call that waits on the
/// connection. A question names no call, so it is put to the person only
/// when every waiting call is for one and the same thread; otherwise, or
/// when no call waits, it is answered with `cancel`.
pub(super) fn forward(shared: &Shared, params: Value) -> oneshot::Receiver<Value> {
    let (reply, answer) = oneshot::channel();
    match shared.one_thread() {
        Some(notices) => {
            let _ = notices.send(Notice::Question(Elicitation { params, reply }));
        }
        None => {
            let _ = reply.send(json!({"action": "cancel"}));
        }
    }
    answer
}

/// The answer to a request a legacy server makes: `ping` is answered,
/// questions and requests to run the model go to the person of the call
/// that waits, the roots are the folder of the thread that last used the
/// server, and the rest is refused.
pub(super) async fn answer_request(
    shared: &Shared,
    method: &str,
    id: Value,
    params: Value,
) -> Value {
    let failed = |code: i64, message: &str| json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}});
    match method {
        "elicitation/create" => {
            let mode = params.get("mode").and_then(Value::as_str).unwrap_or("form");
            if !matches!(mode, "form" | "url") {
                return failed(
                    -32602,
                    &format!("the elicitation mode {mode:?} was not declared"),
                );
            }
            let result = forward(shared, params)
                .await
                .unwrap_or_else(|_| json!({"action": "cancel"}));
            json!({"jsonrpc": "2.0", "id": id, "result": result})
        }
        "sampling/createMessage" => {
            let (reply, answer) = oneshot::channel();
            match shared.one_thread() {
                Some(notices) => {
                    let _ = notices.send(Notice::Sampling(Sampled { params, reply }));
                }
                None => {
                    let _ = reply.send(Err((SAMPLING_REFUSED, NO_PERSON.into())));
                }
            }
            match answer.await {
                Ok(Ok(result)) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
                Ok(Err((code, message))) => failed(code, &message),
                Err(_) => failed(SAMPLING_REFUSED, NO_PERSON),
            }
        }
        "roots/list" => {
            shared.roots_asked.store(true, Ordering::SeqCst);
            let folder = shared.folder.lock().expect("folder lock").clone();
            json!({"jsonrpc": "2.0", "id": id, "result": roots(folder.as_deref())})
        }
        "ping" => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
        _ => failed(-32601, "Method not found"),
    }
}

/// The code of a refused request to run the model (2025-11-25: the user
/// rejected it).
pub(super) const SAMPLING_REFUSED: i64 = -1;
pub(super) const NO_PERSON: &str =
    "no person can approve the request now (no call of a thread waits on this server)";

/// The result of an answer, or its error.
pub(super) fn answer_of(message: &Value) -> Result<Value, Fault> {
    match message.get("error") {
        Some(error) => Err(Fault::Rpc(RpcError::of(error))),
        None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
    }
}

fn client_info() -> Value {
    json!({"name": "inocode", "version": env!("CARGO_PKG_VERSION")})
}

/// The client features declared to every server: questions in both modes,
/// roots, and running the model, with the request's tools too. A server of
/// the stateless revision may also make tasks; an older one is told when
/// the roots change.
fn client_capabilities(modern: bool) -> Value {
    match modern {
        true => json!({
            "elicitation": {"form": {}, "url": {}},
            "roots": {},
            "sampling": {"tools": {}},
            "extensions": {TASKS: {}},
        }),
        false => json!({
            "elicitation": {"form": {}, "url": {}},
            "roots": {"listChanged": true},
            "sampling": {"tools": {}},
        }),
    }
}

/// The server's tools as listed once.
#[derive(Clone, Default)]
pub(super) struct Listing {
    pub(super) tools: Vec<Value>,
    /// How long the server said the list stays fresh; `None` when it did
    /// not say, and the list is kept until it says the tools changed.
    pub(super) ttl: Option<Duration>,
    /// Tools left out, and why.
    pub(super) rejected: Vec<String>,
}

type Writer = Arc<tokio::sync::Mutex<Box<dyn AsyncWrite + Send + Unpin>>>;
/// Requests waiting for their answer; `None` once the server has closed.
pub(super) type Pending = Arc<Mutex<Option<HashMap<u64, oneshot::Sender<Result<Value, Fault>>>>>>;

/// One server, spoken to with JSON-RPC messages.
pub(crate) struct Connection {
    next_id: AtomicU64,
    pub(super) transport: Transport,
    pub(super) shared: Arc<Shared>,
    /// The era found when the server ran before, so a server started again
    /// is not probed again; `None` probes.
    pub(super) known: Option<Era>,
    /// The level from which the server's log messages are asked for.
    pub(super) log_level: Option<String>,
    era: Mutex<Option<Era>>,
    /// The `x-mcp-header` parameters of each tool, by the tool's name.
    param_headers: Mutex<HashMap<String, Vec<ParamHeader>>>,
    /// What the server said it offers, from `server/discover` or
    /// `initialize`.
    capabilities: Mutex<Value>,
    /// The open `subscriptions/listen` of a modern server.
    listening: Mutex<Option<Listening>>,
}

/// How an open subscription is ended: on stdio by its request id, over
/// HTTP by closing its stream.
enum Listening {
    Pipe(u64),
    Http(tokio::task::AbortHandle),
}

/// A wait for the updates of one resource; ended when dropped.
pub(crate) struct Subscription {
    connection: Arc<Connection>,
    id: u64,
    pub(super) updates: mpsc::UnboundedReceiver<String>,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        let connection = self.connection.clone();
        let Some(uri) = connection.unwatch(self.id) else {
            return;
        };
        if connection.modern().is_some() {
            connection.listen();
            return;
        }
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let unsubscribe = connection.request("resources/unsubscribe", json!({"uri": uri}));
                let _ = tokio::time::timeout(END_TIMEOUT, unsubscribe).await;
            });
        }
    }
}

pub(super) enum Transport {
    Pipe(Pipe),
    Http(Arc<Http>),
}

/// A server this client started, spoken to one message per line.
pub(super) struct Pipe {
    writer: Writer,
    pub(super) pending: Pending,
    /// Ended when the connection is dropped.
    pub(super) child: Option<Box<tokio::process::Child>>,
}

/// What a `server/discover` answer says of the server.
enum Probed {
    Modern(String, Value),
    Legacy,
    /// The server stopped: an older server that takes nothing before
    /// `initialize` may end at an unknown request.
    Stopped,
    Failed(Fault),
}

fn probed(answer: Result<Value, Fault>) -> Probed {
    match answer {
        Ok(result) => match result.get("supportedVersions").and_then(Value::as_array) {
            Some(versions) => chosen(versions, result.clone()),
            None => Probed::Legacy,
        },
        Err(Fault::Stopped | Fault::NotRunning) => Probed::Stopped,
        Err(fault @ (Fault::Other(_) | Fault::Broken)) => Probed::Failed(fault),
        Err(fault) => match fault.rpc() {
            Some(rpc) if rpc.code == UNSUPPORTED_VERSION => {
                let supported = rpc
                    .data
                    .get("supported")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                chosen(&supported, Value::Null)
            }
            Some(rpc) if rpc.modern() => Probed::Failed(fault),
            _ => Probed::Legacy,
        },
    }
}

/// The version to speak among those a modern server supports.
fn chosen(versions: &[Value], discovered: Value) -> Probed {
    let names: Vec<&str> = versions.iter().filter_map(Value::as_str).collect();
    if names.contains(&MODERN_VERSION) {
        Probed::Modern(MODERN_VERSION.to_string(), discovered)
    } else if names.iter().any(|name| LEGACY_VERSIONS.contains(name)) {
        Probed::Legacy
    } else {
        Probed::Failed(Fault::Other(format!(
            "the MCP server supports only the protocol versions {names:?}"
        )))
    }
}

/// Whether the server says it may send word of changes: of its tool,
/// prompt or resource lists, or of the resources subscribed to.
fn tells_changes(capabilities: &Value) -> bool {
    [
        "/tools/listChanged",
        "/prompts/listChanged",
        "/resources/listChanged",
        "/resources/subscribe",
    ]
    .iter()
    .any(|pointer| capabilities.pointer(pointer) == Some(&json!(true)))
}

impl Connection {
    pub(super) fn new(
        server: &str,
        reader: impl AsyncRead + Send + Unpin + 'static,
        writer: impl AsyncWrite + Send + Unpin + 'static,
    ) -> Self {
        let writer: Writer = Arc::new(tokio::sync::Mutex::new(Box::new(writer)));
        let pending: Pending = Arc::new(Mutex::new(Some(HashMap::new())));
        let shared = Shared::new(server);
        tokio::spawn(read(
            reader,
            writer.clone(),
            pending.clone(),
            shared.clone(),
        ));
        Self::with(
            Transport::Pipe(Pipe {
                writer,
                pending,
                child: None,
            }),
            shared,
        )
    }

    pub(super) fn with(transport: Transport, shared: Arc<Shared>) -> Self {
        Self {
            next_id: AtomicU64::new(1),
            transport,
            shared,
            known: None,
            log_level: None,
            era: Mutex::new(None),
            param_headers: Mutex::new(HashMap::new()),
            capabilities: Mutex::new(Value::Null),
            listening: Mutex::new(None),
        }
    }

    /// Whether the server's capabilities hold `pointer` (such as
    /// `/resources`) as something other than `false` or `null`.
    pub(crate) fn offers(&self, pointer: &str) -> bool {
        let capabilities = self.capabilities.lock().expect("capabilities lock");
        capabilities
            .pointer(pointer)
            .is_some_and(|value| !value.is_null() && value != &json!(false))
    }

    /// Tells an older server that asked for the roots that they changed,
    /// when `folder`, the project folder of the thread now using it, is not
    /// the one it was given; without waiting long.
    pub(super) async fn use_folder(&self, folder: &Path) {
        if !self.shared.use_folder(folder) || self.modern().is_some() {
            return;
        }
        let _ = tokio::time::timeout(
            END_TIMEOUT,
            self.notify("notifications/roots/list_changed", Value::Null),
        )
        .await;
    }

    /// Waits for the updates of the resource `uri`: a modern server is
    /// subscribed to with `subscriptions/listen`, an older one with
    /// `resources/subscribe`.
    pub(super) async fn subscribe(self: &Arc<Self>, uri: &str) -> Result<Subscription, Fault> {
        if !self.offers("/resources/subscribe") {
            return Err(Fault::Other(
                "the MCP server does not offer subscriptions to its resources".into(),
            ));
        }
        static WATCHERS: AtomicU64 = AtomicU64::new(1);
        let id = WATCHERS.fetch_add(1, Ordering::Relaxed);
        let (sender, updates) = mpsc::unbounded_channel();
        let first = {
            let mut watchers = self.shared.watchers.lock().expect("watchers lock");
            let first = !watchers.iter().any(|watcher| watcher.uri == uri);
            watchers.push(Watcher {
                id,
                uri: uri.to_string(),
                updates: sender,
            });
            first
        };
        if first {
            let subscribed = match self.modern() {
                Some(_) => {
                    self.listen();
                    Ok(())
                }
                None => self
                    .request("resources/subscribe", json!({"uri": uri}))
                    .await
                    .map(drop),
            };
            if let Err(fault) = subscribed {
                self.unwatch(id);
                return Err(fault);
            }
        }
        Ok(Subscription {
            connection: self.clone(),
            id,
            updates,
        })
    }

    /// Removes the watcher `id`; the URI it watched when no one else
    /// watches it now.
    fn unwatch(&self, id: u64) -> Option<String> {
        let mut watchers = self.shared.watchers.lock().expect("watchers lock");
        let index = watchers.iter().position(|watcher| watcher.id == id)?;
        let uri = watchers.remove(index).uri;
        (!watchers.iter().any(|watcher| watcher.uri == uri)).then_some(uri)
    }

    pub(super) fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    pub(super) fn era(&self) -> Option<Era> {
        self.era.lock().expect("era lock").clone()
    }

    fn modern(&self) -> Option<String> {
        match self.era() {
            Some(Era::Modern(version)) => Some(version),
            _ => None,
        }
    }

    pub(super) fn is_pipe(&self) -> bool {
        matches!(self.transport, Transport::Pipe(_))
    }

    /// `params` with the per-request fields of the stateless revision in its
    /// `_meta`.
    fn with_meta(&self, params: Value, version: &str) -> Value {
        let mut params = match params {
            Value::Object(params) => params,
            _ => serde_json::Map::new(),
        };
        let meta = params.entry("_meta").or_insert_with(|| json!({}));
        if !meta.is_object() {
            *meta = json!({});
        }
        meta["io.modelcontextprotocol/protocolVersion"] = json!(version);
        meta["io.modelcontextprotocol/clientInfo"] = client_info();
        meta["io.modelcontextprotocol/clientCapabilities"] = client_capabilities(true);
        if let Some(level) = &self.log_level {
            meta["io.modelcontextprotocol/logLevel"] = json!(level);
        }
        Value::Object(params)
    }

    /// Sends request `id` with `params` as they are and waits for its answer.
    async fn transmit(
        &self,
        id: u64,
        method: &str,
        params: Value,
        extra: &HeaderMap,
    ) -> Result<Value, Fault> {
        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        match &self.transport {
            Transport::Pipe(pipe) => pipe_request(&pipe.writer, &pipe.pending, id, &message).await,
            Transport::Http(http) => http.request(id, &message, extra).await,
        }
    }

    /// Sends request `id`, framed for the era the server speaks, with
    /// `extra` headers over HTTP.
    pub(super) async fn exchange(
        &self,
        id: u64,
        method: &str,
        params: Value,
        extra: &HeaderMap,
    ) -> Result<Value, Fault> {
        let params = self.exchange_params(params);
        self.transmit(id, method, params, extra).await
    }

    /// Sends a request. A session the server ended is started anew and the
    /// request sent once more; so is a request whose stream broke.
    pub(super) async fn request(&self, method: &str, params: Value) -> Result<Value, Fault> {
        let (mut renewed, mut sent_again) = (false, false);
        loop {
            let id = self.next_id();
            match self
                .exchange(id, method, params.clone(), &HeaderMap::new())
                .await
            {
                Err(Fault::SessionEnded) if !renewed => {
                    renewed = true;
                    self.initialize().await?;
                }
                Err(Fault::Broken) if !sent_again => sent_again = true,
                answer => return answer,
            }
        }
    }

    /// Sends a notification; none goes to a modern server over HTTP, where
    /// the revision defines none.
    pub(super) async fn notify(&self, method: &str, params: Value) -> Result<(), Fault> {
        let mut message = json!({"jsonrpc": "2.0", "method": method});
        if !params.is_null() {
            message["params"] = params;
        }
        match &self.transport {
            Transport::Pipe(pipe) => send(&pipe.writer, &message).await.map_err(Fault::Other),
            Transport::Http(http) => http.post(&message, &HeaderMap::new()).await.map(drop),
        }
    }

    /// Tells the server that request `id` is no longer waited for. Over
    /// HTTP the stateless revision takes the closed stream for that.
    pub(super) async fn cancel(&self, id: u64) {
        if !self.is_pipe() && self.modern().is_some() {
            return;
        }
        let params = json!({"requestId": id, "reason": "The person interrupted the turn."});
        let _ =
            tokio::time::timeout(END_TIMEOUT, self.notify("notifications/cancelled", params)).await;
    }

    /// Finds the era the server speaks and opens the connection for it: a
    /// known era is spoken at once; otherwise `server/discover` probes, and
    /// a server that does not answer as a modern one gets `initialize`.
    pub(super) async fn open(&self) -> Result<(), Fault> {
        match self.known.clone() {
            Some(Era::Legacy(_)) => self.open_legacy().await,
            Some(Era::Modern(version)) => {
                let id = self.next_id();
                let params = self.with_meta(json!({}), &version);
                let discovered = self
                    .transmit(id, "server/discover", params, &HeaderMap::new())
                    .await?;
                self.become_modern(version, &discovered);
                Ok(())
            }
            None => self.probe().await,
        }
    }

    async fn probe(&self) -> Result<(), Fault> {
        let id = self.next_id();
        let params = self.with_meta(json!({}), MODERN_VERSION);
        let none = HeaderMap::new();
        let discover = self.transmit(id, "server/discover", params, &none);
        let probed = match &self.transport {
            Transport::Http(_) => probed(discover.await),
            Transport::Pipe(_) => {
                tokio::pin!(discover);
                match tokio::time::timeout(PROBE_WAIT, &mut discover).await {
                    Ok(answer) => probed(answer),
                    Err(_) => return self.race(discover).await,
                }
            }
        };
        self.settle(probed).await
    }

    /// `initialize` beside an unanswered `server/discover`: whichever answer
    /// tells the era first decides it. A slow modern server answers the
    /// probe first (stdio is read in order); a legacy server that ignores
    /// unknown requests answers only `initialize`.
    async fn race(
        &self,
        mut discover: Pin<&mut impl Future<Output = Result<Value, Fault>>>,
    ) -> Result<(), Fault> {
        let id = self.next_id();
        let none = HeaderMap::new();
        let initialize = self.transmit(id, "initialize", initialize_params(), &none);
        tokio::pin!(initialize);
        let mut discovered_legacy = false;
        let mut refused: Option<Fault> = None;
        loop {
            tokio::select! {
                answer = &mut discover, if !discovered_legacy => match probed(answer) {
                    Probed::Legacy => {
                        discovered_legacy = true;
                        if let Some(fault) = refused.take() {
                            return Err(fault);
                        }
                    }
                    other => return self.settle(other).await,
                },
                answer = &mut initialize, if refused.is_none() => match answer {
                    Ok(result) => return self.finish_initialize(result).await,
                    Err(fault) if discovered_legacy => return Err(fault),
                    Err(fault) => refused = Some(fault),
                },
            }
        }
    }

    async fn settle(&self, probed: Probed) -> Result<(), Fault> {
        match probed {
            Probed::Modern(version, discovered) => {
                self.become_modern(version, &discovered);
                Ok(())
            }
            Probed::Legacy => self.open_legacy().await,
            Probed::Stopped => Err(Fault::Stopped),
            Probed::Failed(fault) => Err(fault),
        }
    }

    fn become_modern(&self, version: String, discovered: &Value) {
        *self.era.lock().expect("era lock") = Some(Era::Modern(version));
        *self.capabilities.lock().expect("capabilities lock") = discovered
            .get("capabilities")
            .cloned()
            .unwrap_or(Value::Null);
        self.listen();
    }

    /// What `subscriptions/listen` asks for: word of the list changes the
    /// server says it sends, and the updates of the resources watched.
    fn filter(&self) -> serde_json::Map<String, Value> {
        let mut filter = serde_json::Map::new();
        for (pointer, field) in [
            ("/tools/listChanged", "toolsListChanged"),
            ("/prompts/listChanged", "promptsListChanged"),
            ("/resources/listChanged", "resourcesListChanged"),
        ] {
            if self.offers(pointer) {
                filter.insert(field.into(), json!(true));
            }
        }
        let watched = self.shared.watched();
        if self.offers("/resources/subscribe") && !watched.is_empty() {
            filter.insert("resourceSubscriptions".into(), json!(watched));
        }
        filter
    }

    /// Opens `subscriptions/listen` anew for what [`Self::filter`] asks for,
    /// ending the subscription opened before; none is opened when there is
    /// nothing to ask for.
    fn listen(&self) {
        let filter = self.filter();
        let old = self.listening.lock().expect("listening lock").take();
        match old {
            Some(Listening::Http(task)) => task.abort(),
            Some(Listening::Pipe(old)) => {
                if let Transport::Pipe(pipe) = &self.transport {
                    if let Some(pending) = pipe.pending.lock().expect("pending lock").as_mut() {
                        pending.remove(&old);
                    }
                    let writer = pipe.writer.clone();
                    let cancelled = json!({"jsonrpc": "2.0", "method": "notifications/cancelled",
                        "params": {"requestId": old}});
                    tokio::spawn(async move {
                        let _ = send(&writer, &cancelled).await;
                    });
                }
            }
            None => {}
        }
        if filter.is_empty() {
            return;
        }
        let id = self.next_id();
        let params = self.exchange_params(json!({"notifications": filter}));
        let message = json!({"jsonrpc": "2.0", "id": id, "method": "subscriptions/listen",
                             "params": params});
        let listening = match &self.transport {
            Transport::Pipe(pipe) => {
                let (writer, pending) = (pipe.writer.clone(), pipe.pending.clone());
                // The answer only ends the subscription; notifications for
                // it arrive with the others.
                tokio::spawn(async move {
                    let _ = pipe_request(&writer, &pending, id, &message).await;
                });
                Listening::Pipe(id)
            }
            Transport::Http(http) => Listening::Http(http.listen_modern(message)),
        };
        *self.listening.lock().expect("listening lock") = Some(listening);
    }

    fn exchange_params(&self, params: Value) -> Value {
        match self.modern() {
            Some(version) => self.with_meta(params, &version),
            None => params,
        }
    }

    /// `initialize`, falling back to the deprecated HTTP+SSE transport when
    /// an HTTP server refuses the POST the way only such a server does.
    async fn open_legacy(&self) -> Result<(), Fault> {
        match self.initialize().await {
            Err(fault) if self.may_use_sse(&fault) => {
                let Transport::Http(http) = &self.transport else {
                    return Err(fault);
                };
                if let Err(sse) = http.open_sse().await {
                    return Err(Fault::Other(format!(
                        "{fault}; nor does it offer the HTTP+SSE transport: {sse}"
                    )));
                }
                self.initialize().await
            }
            answer => answer,
        }
    }

    fn may_use_sse(&self, fault: &Fault) -> bool {
        let Fault::Status { status, rpc, .. } = fault else {
            return false;
        };
        !self.is_pipe()
            && matches!(status.as_u16(), 400 | 404 | 405)
            && !rpc.as_deref().is_some_and(RpcError::modern)
    }

    /// The handshake: `initialize`, the version check, and
    /// `notifications/initialized`.
    pub(super) async fn initialize(&self) -> Result<(), Fault> {
        let id = self.next_id();
        let result = self
            .transmit(id, "initialize", initialize_params(), &HeaderMap::new())
            .await?;
        self.finish_initialize(result).await
    }

    async fn finish_initialize(&self, answer: Value) -> Result<(), Fault> {
        let version = answer
            .get("protocolVersion")
            .and_then(Value::as_str)
            .unwrap_or("");
        if !LEGACY_VERSIONS.contains(&version) {
            return Err(Fault::Other(format!(
                "unsupported protocol version {version:?}"
            )));
        }
        *self.era.lock().expect("era lock") = Some(Era::Legacy(version.to_string()));
        if let Transport::Http(http) = &self.transport {
            http.agreed(version);
        }
        self.notify("notifications/initialized", Value::Null)
            .await?;
        let capabilities = answer.get("capabilities").cloned().unwrap_or(Value::Null);
        *self.capabilities.lock().expect("capabilities lock") = capabilities.clone();
        if let (Some(level), Some(_)) = (&self.log_level, capabilities.get("logging")) {
            let id = self.next_id();
            let set = self
                .exchange(
                    id,
                    "logging/setLevel",
                    json!({"level": level}),
                    &HeaderMap::new(),
                )
                .await;
            if let Err(fault) = set {
                eprintln!(
                    "MCP: server {} did not take the log level {level}: {fault}",
                    self.shared.server
                );
            }
        }
        if let Transport::Http(http) = &self.transport {
            if tells_changes(&capabilities) {
                http.listen_legacy();
            }
        }
        Ok(())
    }

    /// Every page of the server's tools. Over Streamable HTTP in the
    /// stateless revision, a tool whose `x-mcp-header` parameters break the
    /// rules is left out, as the transport requires.
    pub(super) async fn list_tools(&self) -> Result<Listing, Fault> {
        let (tools, ttl) = self.list_all("tools/list", "tools").await?;
        let mut listing = Listing {
            tools,
            ttl,
            rejected: Vec::new(),
        };
        let mut headers = HashMap::new();
        if self.headers_apply() {
            let server = &self.shared.server;
            let mut rejected = Vec::new();
            listing.tools.retain(|tool| {
                let name = tool.get("name").and_then(Value::as_str).unwrap_or("");
                match http::param_headers(tool.get("inputSchema").unwrap_or(&Value::Null)) {
                    Ok(params) => {
                        headers.insert(name.to_string(), params);
                        true
                    }
                    Err(reason) => {
                        eprintln!("MCP: tool {name} of server {server} is left out: {reason}");
                        rejected.push(format!(
                            "tool {name} of server {server} is left out: {reason}"
                        ));
                        false
                    }
                }
            });
            listing.rejected = rejected;
        }
        *self.param_headers.lock().expect("headers lock") = headers;
        Ok(listing)
    }

    /// Every page of the list `method` answers under `key`, and the
    /// shortest time a page said it stays fresh.
    pub(super) async fn list_all(
        &self,
        method: &str,
        key: &str,
    ) -> Result<(Vec<Value>, Option<Duration>), Fault> {
        let mut items = Vec::new();
        let mut fresh: Option<Duration> = None;
        let mut cursor: Option<String> = None;
        loop {
            let params = match &cursor {
                Some(cursor) => json!({"cursor": cursor}),
                None => json!({}),
            };
            let page = self.request(method, params).await?;
            let ttl = page
                .get("ttlMs")
                .and_then(Value::as_f64)
                .map(|ms| Duration::from_millis(ms.max(0.0) as u64));
            if let Some(ttl) = ttl {
                fresh = Some(fresh.map_or(ttl, |known| known.min(ttl)));
            }
            items.extend(
                page.get(key)
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default(),
            );
            match page.get("nextCursor").and_then(Value::as_str) {
                Some(next) => cursor = Some(next.to_string()),
                None => break,
            }
        }
        Ok((items, fresh))
    }

    fn headers_apply(&self) -> bool {
        !self.is_pipe() && self.modern().is_some()
    }

    /// The `Mcp-Param-*` headers of a call of `tool` over HTTP.
    pub(super) fn call_headers(&self, tool: &str, arguments: &Value) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if !self.headers_apply() {
            return headers;
        }
        let known = self.param_headers.lock().expect("headers lock");
        for param in known.get(tool).into_iter().flatten() {
            if let Some((name, value)) = param.header(arguments) {
                headers.insert(name, value);
            }
        }
        headers
    }
}

fn initialize_params() -> Value {
    json!({
        "protocolVersion": LEGACY_VERSION,
        "capabilities": client_capabilities(false),
        "clientInfo": client_info(),
    })
}

impl Drop for Connection {
    fn drop(&mut self) {
        match &mut self.transport {
            Transport::Pipe(pipe) => {
                if let Some(child) = pipe.child.as_mut() {
                    let _ = child.start_kill();
                }
            }
            Transport::Http(http) => http.end(),
        }
    }
}

async fn pipe_request(
    writer: &Writer,
    pending: &Pending,
    id: u64,
    message: &Value,
) -> Result<Value, Fault> {
    let (sender, receiver) = oneshot::channel();
    match pending.lock().expect("pending lock").as_mut() {
        Some(pending) => pending.insert(id, sender),
        None => return Err(Fault::NotRunning),
    };
    if let Err(error) = send(writer, message).await {
        if let Some(pending) = pending.lock().expect("pending lock").as_mut() {
            pending.remove(&id);
        }
        return Err(Fault::Other(error));
    }
    receiver.await.unwrap_or(Err(Fault::Stopped))
}

async fn send(writer: &Writer, message: &Value) -> Result<(), String> {
    let mut line = message.to_string();
    line.push('\n');
    let mut writer = writer.lock().await;
    writer
        .write_all(line.as_bytes())
        .await
        .and(writer.flush().await)
        .map_err(|error| format!("cannot write to the MCP server: {error}"))
}

/// Hands answers to their requests, answers the server's requests, handles
/// its notifications, and fails what is still waiting when it closes.
async fn read(
    reader: impl AsyncRead + Unpin,
    writer: Writer,
    pending: Pending,
    shared: Arc<Shared>,
) {
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let id = message.get("id").cloned();
        match (message.get("method").and_then(Value::as_str), id) {
            (Some(method), Some(id)) => {
                let (method, writer, shared) = (method.to_string(), writer.clone(), shared.clone());
                let params = message.get("params").cloned().unwrap_or(Value::Null);
                tokio::spawn(async move {
                    let answer = answer_request(&shared, &method, id, params).await;
                    let _ = send(&writer, &answer).await;
                });
            }
            (Some(_), None) => shared.notice(&message),
            (None, Some(id)) => {
                let Some(id) = id.as_u64() else { continue };
                let sender = pending
                    .lock()
                    .expect("pending lock")
                    .as_mut()
                    .and_then(|pending| pending.remove(&id));
                if let Some(sender) = sender {
                    let _ = sender.send(answer_of(&message));
                }
            }
            (None, None) => {}
        }
    }
    let waiting = pending.lock().expect("pending lock").take();
    for (_, sender) in waiting.into_iter().flatten() {
        let _ = sender.send(Err(Fault::Stopped));
    }
    // No update can come any more: the waits for updates end.
    shared.watchers.lock().expect("watchers lock").clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refused(status: u16, code: i64, data: Value) -> Result<Value, Fault> {
        Err(Fault::Status {
            status: reqwest::StatusCode::from_u16(status).expect("status"),
            rpc: Some(Box::new(RpcError {
                code,
                message: String::new(),
                data,
            })),
            body: String::new(),
        })
    }

    #[test]
    fn the_answer_to_the_probe_tells_the_era() {
        let modern = probed(Ok(
            json!({"supportedVersions": ["2026-07-28", "2025-11-25"]}),
        ));
        assert!(matches!(modern, Probed::Modern(version, _) if version == MODERN_VERSION));
        assert!(matches!(probed(Ok(json!({}))), Probed::Legacy));
        let older = refused(
            400,
            UNSUPPORTED_VERSION,
            json!({"supported": ["2025-06-18"]}),
        );
        assert!(matches!(probed(older), Probed::Legacy));
        let newer = refused(
            400,
            UNSUPPORTED_VERSION,
            json!({"supported": ["2099-01-01"]}),
        );
        assert!(matches!(probed(newer), Probed::Failed(_)));
        let unknown = refused(404, -32601, Value::Null);
        assert!(matches!(probed(unknown), Probed::Legacy));
        let sessionless = Err(Fault::Status {
            status: reqwest::StatusCode::BAD_REQUEST,
            rpc: None,
            body: String::new(),
        });
        assert!(matches!(probed(sessionless), Probed::Legacy));
        let mismatch = refused(400, HEADER_MISMATCH, Value::Null);
        assert!(matches!(probed(mismatch), Probed::Failed(_)));
        let rpc = Err(Fault::Rpc(RpcError::of(
            &json!({"code": -32602, "message": "x"}),
        )));
        assert!(matches!(probed(rpc), Probed::Legacy));
        assert!(matches!(probed(Err(Fault::Stopped)), Probed::Stopped));
        let unreachable = Err(Fault::Other("cannot reach".into()));
        assert!(matches!(probed(unreachable), Probed::Failed(_)));
    }
}
