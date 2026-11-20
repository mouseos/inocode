//! The MCP client: the servers listed in `mcp.json` beside the thread store
//! are started over stdio or reached over Streamable HTTP, and their tools
//! are given to the model as `mcp__<server>__<tool>`. Design and sources:
//! docs/evidence/mcp-client.md.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

use crate::conversation::ToolSpec;
use crate::store::Store;

pub(crate) const PREFIX: &str = "mcp__";
const PROTOCOL_VERSION: &str = "2025-11-25";
/// Versions whose `initialize`, `tools/list` and `tools/call` are the ones
/// used here.
const SUPPORTED_VERSIONS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
/// How long a server may take to start and list its tools.
const START_TIMEOUT: Duration = Duration::from_secs(30);
/// Providers accept tool names of at most 64 characters.
const NAME_CHARS: usize = 64;

#[derive(Deserialize)]
struct Config {
    #[serde(rename = "mcpServers", default)]
    servers: BTreeMap<String, ServerConfig>,
}

#[derive(Deserialize)]
struct ServerConfig {
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
    /// The MCP endpoint of a server reached over Streamable HTTP.
    #[serde(default)]
    url: Option<String>,
    /// Sent with every HTTP request, such as an `Authorization` header.
    #[serde(default)]
    headers: HashMap<String, String>,
}

/// The tools of every server that started, in the order of the file.
#[derive(Default)]
pub(crate) struct Tools {
    specs: Vec<ToolSpec>,
    routes: HashMap<String, (Arc<Connection>, String)>,
}

/// The servers in `mcp.json` beside the thread store, started once and kept
/// with the store, so the tool list stays the same from request to request.
async fn loaded(store: &Store) -> Arc<Tools> {
    let config: PathBuf = store.files_folder().with_file_name("mcp.json");
    store
        .mcp()
        .get_or_init(|| async { Arc::new(load(&config).await) })
        .await
        .clone()
}

/// The MCP tools to offer the model.
pub(crate) async fn specs(store: &Store) -> Vec<ToolSpec> {
    loaded(store).await.specs.clone()
}

/// Where a call's server questions for the person go: the turn's events,
/// for its thread.
pub(crate) struct Asking<'a> {
    pub session: crate::store::SessionId,
    pub events: &'a mpsc::Sender<crate::turn::TurnEvent>,
}

/// How long a server's question waits for the person before it is answered
/// with `cancel`.
const ANSWER_WAIT: Duration = Duration::from_secs(600);

/// Runs an MCP tool; `None` when `name` is not one. With `asking`, the
/// server's questions during the call (`elicitation/create`) are put to the
/// person and the call waits for the answer.
pub(crate) async fn call(
    store: &Store,
    name: &str,
    arguments: &str,
    mut cancel: tokio::sync::watch::Receiver<bool>,
    asking: Option<Asking<'_>>,
) -> Option<Result<String, String>> {
    if !name.starts_with(PREFIX) {
        return None;
    }
    let tools = loaded(store).await;
    let (connection, tool) = tools.routes.get(name)?;
    let arguments: Value = if arguments.trim().is_empty() {
        json!({})
    } else {
        match serde_json::from_str(arguments) {
            Ok(value) => value,
            Err(error) => return Some(Err(format!("invalid arguments: {error}"))),
        }
    };
    let (sender, mut questions) = mpsc::unbounded_channel();
    if asking.is_some() {
        *connection.elicit.lock().expect("elicit lock") = Some(sender);
    }
    let request = connection.request("tools/call", json!({"name": tool, "arguments": arguments}));
    tokio::pin!(request);
    let interrupted = cancel.clone();
    let answer = loop {
        tokio::select! {
            answer = &mut request => break answer.and_then(|result| render(&result)),
            Some(question) = questions.recv() => {
                let answer = match &asking {
                    Some(asking) => {
                        ask_person(store, &connection.server, asking, question.params, interrupted.clone())
                            .await
                    }
                    None => json!({"action": "cancel"}),
                };
                let _ = question.reply.send(answer);
            }
            () = crate::turn::cancelled(&mut cancel) => break Err("cancelled".into()),
        }
    };
    *connection.elicit.lock().expect("elicit lock") = None;
    Some(answer)
}

/// Puts a server's question to the person and waits for the answer, for
/// `ANSWER_WAIT` at most and until the turn is interrupted; the result for
/// the server. Only form questions are asked; others are declined, since
/// this client declares only forms.
async fn ask_person(
    store: &Store,
    server: &str,
    asking: &Asking<'_>,
    params: Value,
    mut cancel: tokio::sync::watch::Receiver<bool>,
) -> Value {
    let cancelled = json!({"action": "cancel"});
    if params
        .get("mode")
        .and_then(Value::as_str)
        .is_some_and(|mode| mode != "form")
    {
        return json!({"action": "decline"});
    }
    let (id, answer) = store.wait_elicitation(asking.session);
    let schema = params
        .get("requestedSchema")
        .cloned()
        .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
    let question = json!({
        "id": id,
        "server": server,
        "message": params.get("message").and_then(Value::as_str).unwrap_or(""),
        "requestedSchema": schema,
        "questions": questions_of(&schema),
    });
    if asking
        .events
        .send(crate::turn::TurnEvent::Elicitation(question))
        .await
        .is_err()
    {
        store.forget_elicitation(id);
        return cancelled;
    }
    let result = tokio::select! {
        answer = answer => answer.unwrap_or_else(|_| cancelled.clone()),
        () = tokio::time::sleep(ANSWER_WAIT) => cancelled.clone(),
        () = crate::turn::cancelled(&mut cancel) => cancelled.clone(),
    };
    store.forget_elicitation(id);
    result
}

/// A server's requested schema as the questions of `ask`, so the person's
/// screen shows both the same way. Each question keeps the property's name;
/// the answer's `content` is keyed by those names.
fn questions_of(schema: &Value) -> Value {
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|names| names.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let options = |values: Option<&Value>, titled: Option<&Value>| -> Vec<Value> {
        if let Some(titled) = titled.and_then(Value::as_array) {
            return titled
                .iter()
                .map(|option| {
                    let value = option.get("const").cloned().unwrap_or(Value::Null);
                    let label = option
                        .get("title")
                        .cloned()
                        .unwrap_or_else(|| value.clone());
                    json!({"label": label, "value": value})
                })
                .collect();
        }
        values
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .map(|value| json!({"label": value, "value": value}))
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut questions = Vec::new();
    for (name, property) in schema
        .get("properties")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        let text = property
            .get("title")
            .or_else(|| property.get("description"))
            .and_then(Value::as_str)
            .unwrap_or(name);
        let mut question = json!({
            "name": name,
            "question": text,
            "required": required.contains(&name.as_str()),
        });
        for key in ["description", "default", "minimum", "maximum", "format"] {
            if let Some(value) = property.get(key) {
                question[key] = value.clone();
            }
        }
        let kind = match property.get("type").and_then(Value::as_str) {
            Some("boolean") => "yes_no",
            Some("number" | "integer") => "number",
            Some("array") => {
                let items = property.get("items");
                question["options"] = Value::Array(options(
                    items.and_then(|items| items.get("enum")),
                    items.and_then(|items| items.get("anyOf")),
                ));
                "choose_many"
            }
            _ if property.get("enum").is_some() || property.get("oneOf").is_some() => {
                question["options"] =
                    Value::Array(options(property.get("enum"), property.get("oneOf")));
                "choose_one"
            }
            _ => match property.get("format").and_then(Value::as_str) {
                Some("date" | "date-time") => "date",
                _ => "text",
            },
        };
        question["kind"] = json!(kind);
        questions.push(question);
    }
    Value::Array(questions)
}

async fn load(config: &Path) -> Tools {
    let mut tools = Tools::default();
    let text = match std::fs::read_to_string(config) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return tools,
        Err(error) => {
            eprintln!("MCP: cannot read {}: {error}", config.display());
            return tools;
        }
    };
    let config: Config = match serde_json::from_str(&text) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("MCP: {} is not valid: {error}", config.display());
            return tools;
        }
    };
    for (server, settings) in config.servers {
        let started = tokio::time::timeout(START_TIMEOUT, async {
            let mut connection = connect(&settings)?;
            connection.server = server.clone();
            let connection = Arc::new(connection);
            let listed = connection.list_tools().await?;
            Ok::<_, String>((connection, listed))
        })
        .await
        .unwrap_or_else(|_| Err(format!("did not start within {START_TIMEOUT:?}")));
        match started {
            Ok((connection, listed)) => add(&mut tools, &server, &connection, listed),
            Err(error) => eprintln!("MCP: server {server} is not available: {error}"),
        }
    }
    tools
}

/// Adds a server's tools under names the providers accept; a tool whose
/// name is already taken is left out.
fn add(tools: &mut Tools, server: &str, connection: &Arc<Connection>, listed: Vec<Value>) {
    for tool in listed {
        let Some(original) = tool.get("name").and_then(Value::as_str) else {
            continue;
        };
        let name = tool_name(server, original);
        if tools.routes.contains_key(&name) {
            eprintln!("MCP: tool {original} of {server} is left out: {name} is taken");
            continue;
        }
        let description = tool
            .get("description")
            .or_else(|| tool.get("title"))
            .and_then(Value::as_str)
            .unwrap_or("");
        tools.specs.push(ToolSpec {
            name: name.clone(),
            description: format!("{description} (tool {original} of MCP server {server})")
                .trim_start()
                .to_string(),
            parameters: tool
                .get("inputSchema")
                .filter(|schema| schema.is_object())
                .cloned()
                .unwrap_or_else(|| json!({"type": "object"})),
        });
        tools
            .routes
            .insert(name, (connection.clone(), original.to_string()));
    }
}

/// `mcp__<server>__<tool>` with characters other than letters, digits, `_`
/// and `-` made `_`, cut to `NAME_CHARS`.
fn tool_name(server: &str, tool: &str) -> String {
    format!("{PREFIX}{server}__{tool}")
        .chars()
        .map(
            |c| match c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                true => c,
                false => '_',
            },
        )
        .take(NAME_CHARS)
        .collect()
}

/// A tool result as text for the model; an error when the server says the
/// tool failed. Content the model cannot be shown here is named instead.
fn render(result: &Value) -> Result<String, String> {
    let text_of = |value: &Value, key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let mut parts: Vec<String> = Vec::new();
    for item in result
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let kind = text_of(item, "type");
        parts.push(match kind.as_str() {
            "text" => text_of(item, "text"),
            "image" | "audio" => {
                format!("({kind} of type {} not shown)", text_of(item, "mimeType"))
            }
            "resource_link" => format!(
                "(resource {}: {})",
                text_of(item, "name"),
                text_of(item, "uri")
            ),
            "resource" => {
                let resource = item.get("resource").unwrap_or(&Value::Null);
                match resource.get("text").and_then(Value::as_str) {
                    Some(text) => text.to_string(),
                    None => format!("(resource {} not shown)", text_of(resource, "uri")),
                }
            }
            _ => format!("(content of type {kind} not shown)"),
        });
    }
    if parts.is_empty() {
        if let Some(structured) = result.get("structuredContent") {
            parts.push(structured.to_string());
        }
    }
    let text = parts.join("\n");
    match result.get("isError").and_then(Value::as_bool) {
        Some(true) => Err(text),
        _ => Ok(text),
    }
}

fn connect(settings: &ServerConfig) -> Result<Connection, String> {
    match (&settings.command, &settings.url) {
        (Some(command), None) => spawn(command, settings),
        (None, Some(url)) => {
            let elicit = Elicit::default();
            let http = Http::new(url, &settings.headers, elicit.clone())?;
            Ok(Connection::with(Transport::Http(http), elicit))
        }
        _ => Err("give either command or url".into()),
    }
}

fn spawn(command: &str, settings: &ServerConfig) -> Result<Connection, String> {
    let mut child = tokio::process::Command::new(command)
        .args(&settings.args)
        .envs(&settings.env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| format!("cannot run {command}: {error}"))?;
    let stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let mut connection = Connection::new(stdout, stdin);
    if let Transport::Pipe(pipe) = &mut connection.transport {
        pipe.child = Some(child);
    }
    Ok(connection)
}

type Writer = Arc<tokio::sync::Mutex<Box<dyn AsyncWrite + Send + Unpin>>>;
/// Requests waiting for their answer; `None` once the server has closed.
type Pending = Arc<Mutex<Option<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>>;

/// One server, spoken to with JSON-RPC messages.
pub(crate) struct Connection {
    next_id: AtomicU64,
    transport: Transport,
    /// Where the server's questions go while a call waits for its answer.
    elicit: Elicit,
    /// The server's name in `mcp.json`, shown with its questions.
    server: String,
}

/// A question of the server for the person, and where its answer goes.
pub(crate) struct Elicitation {
    params: Value,
    reply: oneshot::Sender<Value>,
}

type Elicit = Arc<Mutex<Option<mpsc::UnboundedSender<Elicitation>>>>;

/// Hands a server's `elicitation/create` to the call that waits on the
/// connection; it is answered with `cancel` when no call waits.
fn forward(elicit: &Elicit, params: Value) -> oneshot::Receiver<Value> {
    let (reply, answer) = oneshot::channel();
    let waiting = elicit.lock().expect("elicit lock").clone();
    match waiting {
        Some(waiting) => {
            let _ = waiting.send(Elicitation { params, reply });
        }
        None => {
            let _ = reply.send(json!({"action": "cancel"}));
        }
    }
    answer
}

/// The result for a server's question once its answer arrives.
async fn answered(answer: oneshot::Receiver<Value>) -> Value {
    answer.await.unwrap_or_else(|_| json!({"action": "cancel"}))
}

enum Transport {
    Pipe(Pipe),
    Http(Http),
}

/// A server this client started, spoken to one message per line.
struct Pipe {
    writer: Writer,
    pending: Pending,
    /// Ended when the connection is dropped.
    child: Option<tokio::process::Child>,
}

impl Connection {
    fn new(
        reader: impl AsyncRead + Send + Unpin + 'static,
        writer: impl AsyncWrite + Send + Unpin + 'static,
    ) -> Self {
        let writer: Writer = Arc::new(tokio::sync::Mutex::new(Box::new(writer)));
        let pending: Pending = Arc::new(Mutex::new(Some(HashMap::new())));
        let elicit = Elicit::default();
        tokio::spawn(read(
            reader,
            writer.clone(),
            pending.clone(),
            elicit.clone(),
        ));
        Self::with(
            Transport::Pipe(Pipe {
                writer,
                pending,
                child: None,
            }),
            elicit,
        )
    }

    fn with(transport: Transport, elicit: Elicit) -> Self {
        Self {
            next_id: AtomicU64::new(1),
            transport,
            elicit,
            server: String::new(),
        }
    }

    /// Sends a request; over HTTP, a session the server ended is started
    /// anew and the request sent once more, as the transport requires.
    async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        match self.send(method, params.clone()).await {
            Err(error) if error == SESSION_ENDED => {
                self.initialize().await?;
                self.send(method, params).await
            }
            answer => answer,
        }
    }

    async fn send(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        match &self.transport {
            Transport::Pipe(pipe) => pipe.request(id, &message).await,
            Transport::Http(http) => http.request(id, &message).await,
        }
    }

    /// The handshake: `initialize`, the version check, and
    /// `notifications/initialized`.
    async fn initialize(&self) -> Result<(), String> {
        let answer = self
            .send(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {"elicitation": {"form": {}}},
                    "clientInfo": {"name": "inocode", "version": env!("CARGO_PKG_VERSION")}
                }),
            )
            .await?;
        let version = answer
            .get("protocolVersion")
            .and_then(Value::as_str)
            .unwrap_or("");
        if !SUPPORTED_VERSIONS.contains(&version) {
            return Err(format!("unsupported protocol version {version:?}"));
        }
        if let Transport::Http(http) = &self.transport {
            *http.version.lock().expect("version lock") = Some(version.to_string());
        }
        self.notify("notifications/initialized").await
    }

    async fn notify(&self, method: &str) -> Result<(), String> {
        let message = json!({"jsonrpc": "2.0", "method": method});
        match &self.transport {
            Transport::Pipe(pipe) => send(&pipe.writer, &message).await,
            Transport::Http(http) => http.post(&message).await.map(drop),
        }
    }

    /// The handshake, then every page of the server's tools.
    async fn list_tools(&self) -> Result<Vec<Value>, String> {
        self.initialize().await?;
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = match &cursor {
                Some(cursor) => json!({"cursor": cursor}),
                None => json!({}),
            };
            let page = self.request("tools/list", params).await?;
            tools.extend(
                page.get("tools")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default(),
            );
            match page.get("nextCursor").and_then(Value::as_str) {
                Some(next) => cursor = Some(next.to_string()),
                None => return Ok(tools),
            }
        }
    }
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

impl Pipe {
    async fn request(&self, id: u64, message: &Value) -> Result<Value, String> {
        let (sender, receiver) = oneshot::channel();
        match self.pending.lock().expect("pending lock").as_mut() {
            Some(pending) => pending.insert(id, sender),
            None => return Err("the MCP server has stopped".into()),
        };
        if let Err(error) = send(&self.writer, message).await {
            if let Some(pending) = self.pending.lock().expect("pending lock").as_mut() {
                pending.remove(&id);
            }
            return Err(error);
        }
        receiver
            .await
            .unwrap_or_else(|_| Err("the MCP server has stopped".into()))
    }
}

const SESSION_HEADER: &str = "MCP-Session-Id";
/// The error of a request whose session the server ended (HTTP 404).
const SESSION_ENDED: &str = "the MCP server ended the session";
const VERSION_HEADER: &str = "MCP-Protocol-Version";
/// Bytes of one JSON answer read at most, as for the providers' answers.
const MAX_HTTP_BODY: usize = 16 * 1024 * 1024;
/// How long ending a session may take; it is not waited for.
const END_TIMEOUT: Duration = Duration::from_secs(5);

/// A server reached over Streamable HTTP: every message is one POST to its
/// endpoint, answered with JSON or with a stream of server-sent events.
struct Http {
    client: reqwest::Client,
    url: reqwest::Url,
    headers: reqwest::header::HeaderMap,
    /// The session the server gave at initialization, sent with every later
    /// request.
    session: Mutex<Option<String>>,
    /// The protocol version agreed at initialization, sent with every later
    /// request.
    version: Mutex<Option<String>>,
    elicit: Elicit,
}

impl Http {
    fn new(url: &str, headers: &HashMap<String, String>, elicit: Elicit) -> Result<Self, String> {
        let url =
            reqwest::Url::parse(url).map_err(|error| format!("invalid url {url}: {error}"))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(format!("{url} is not an http or https url"));
        }
        let mut map = reqwest::header::HeaderMap::new();
        for (name, value) in headers {
            let header = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| format!("invalid header name {name}"))?;
            let value = reqwest::header::HeaderValue::from_str(value)
                .map_err(|_| format!("invalid value for header {name}"))?;
            map.insert(header, value);
        }
        Ok(Self {
            client: reqwest::Client::new(),
            url,
            headers: map,
            session: Mutex::new(None),
            version: Mutex::new(None),
            elicit,
        })
    }

    /// Posts one message; the response once its status is a success.
    async fn post(&self, message: &Value) -> Result<reqwest::Response, String> {
        let mut request = self
            .client
            .post(self.url.clone())
            .headers(self.headers.clone())
            .header(
                reqwest::header::ACCEPT,
                "application/json, text/event-stream",
            )
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(message.to_string());
        let session = self.session.lock().expect("session lock").clone();
        if let Some(session) = &session {
            request = request.header(SESSION_HEADER, session);
        }
        if let Some(version) = self.version.lock().expect("version lock").clone() {
            request = request.header(VERSION_HEADER, version);
        }
        let mut response = request
            .send()
            .await
            .map_err(|error| format!("cannot reach the MCP server: {error}"))?;
        let status = response.status();
        if !status.is_success() {
            if status == reqwest::StatusCode::NOT_FOUND && session.is_some() {
                *self.session.lock().expect("session lock") = None;
                *self.version.lock().expect("version lock") = None;
                return Err(SESSION_ENDED.into());
            }
            let body = crate::llm::read_capped(&mut response, 1024)
                .await
                .ok()
                .flatten()
                .map(|body| String::from_utf8_lossy(&body).trim().to_string())
                .unwrap_or_default();
            return Err(format!("the MCP server answered {status}: {body}"));
        }
        if let Some(session) = response
            .headers()
            .get(SESSION_HEADER)
            .and_then(|value| value.to_str().ok())
        {
            *self.session.lock().expect("session lock") = Some(session.to_string());
        }
        Ok(response)
    }

    async fn request(&self, id: u64, message: &Value) -> Result<Value, String> {
        let mut response = self.post(message).await?;
        let events = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|kind| kind.starts_with("text/event-stream"));
        if events {
            return self.answer_in_events(id, response).await;
        }
        let body = crate::llm::read_capped(&mut response, MAX_HTTP_BODY)
            .await
            .map_err(|error| format!("cannot read the MCP server's answer: {error}"))?
            .ok_or("the MCP server's answer is too large")?;
        let message: Value = serde_json::from_slice(&body)
            .map_err(|error| format!("the MCP server's answer is not JSON: {error}"))?;
        if message.get("id").and_then(Value::as_u64) != Some(id) {
            return Err("the MCP server answered another request".into());
        }
        answer_of(&message)
    }

    /// Reads the server's events until the answer to request `id`; the
    /// server's own requests met on the way are answered with a POST each.
    async fn answer_in_events(
        &self,
        id: u64,
        mut response: reqwest::Response,
    ) -> Result<Value, String> {
        let mut parser = crate::sse::SseParser::new();
        let mut ended = false;
        loop {
            while let Some(item) = parser.next_item() {
                let data = item.map_err(|error| error.message)?;
                let Ok(message) = serde_json::from_str::<Value>(&data) else {
                    continue;
                };
                match (
                    message.get("method").and_then(Value::as_str),
                    message.get("id"),
                ) {
                    (Some("elicitation/create"), Some(request)) => {
                        let params = message.get("params").cloned().unwrap_or(Value::Null);
                        let result = answered(forward(&self.elicit, params)).await;
                        let answer = json!({"jsonrpc": "2.0", "id": request, "result": result});
                        let _ = self.post(&answer).await;
                    }
                    (Some(method), Some(request)) => {
                        let _ = self.post(&reply(method, request.clone())).await;
                    }
                    (None, Some(answered)) if answered.as_u64() == Some(id) => {
                        return answer_of(&message);
                    }
                    _ => {}
                }
            }
            if ended {
                return Err("the MCP server closed the stream before answering".into());
            }
            match response.chunk().await {
                Ok(Some(chunk)) => parser.push(&chunk),
                Ok(None) => {
                    parser.end();
                    ended = true;
                }
                Err(error) => return Err(format!("cannot read the MCP server's answer: {error}")),
            }
        }
    }

    /// Ends the session, if the server gave one, without waiting.
    fn end(&self) {
        let Some(session) = self.session.lock().expect("session lock").take() else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let request = self
            .client
            .delete(self.url.clone())
            .headers(self.headers.clone())
            .header(SESSION_HEADER, session)
            .timeout(END_TIMEOUT);
        runtime.spawn(async move {
            let _ = request.send().await;
        });
    }
}

/// The result of an answer, or its error as text.
fn answer_of(message: &Value) -> Result<Value, String> {
    match message.get("error") {
        Some(error) => Err(format!(
            "MCP error {}: {}",
            error.get("code").unwrap_or(&Value::Null),
            error.get("message").and_then(Value::as_str).unwrap_or("")
        )),
        None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
    }
}

/// The answer to a request from the server: `ping` is answered and the
/// other requests are refused, since this client offers none of the client
/// features.
fn reply(method: &str, id: Value) -> Value {
    match method {
        "ping" => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
        _ => json!({"jsonrpc": "2.0", "id": id,
            "error": {"code": -32601, "message": "Method not found"}}),
    }
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

/// Hands answers to their requests, answers `ping`, refuses the other
/// requests a server may make (this client offers none of the client
/// features), and fails what is still waiting when the server closes.
async fn read(reader: impl AsyncRead + Unpin, writer: Writer, pending: Pending, elicit: Elicit) {
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let id = message.get("id").cloned();
        match (message.get("method").and_then(Value::as_str), id) {
            (Some("elicitation/create"), Some(id)) => {
                let params = message.get("params").cloned().unwrap_or(Value::Null);
                let answer = forward(&elicit, params);
                let writer = writer.clone();
                tokio::spawn(async move {
                    let result = answered(answer).await;
                    let _ = send(
                        &writer,
                        &json!({"jsonrpc": "2.0", "id": id, "result": result}),
                    )
                    .await;
                });
            }
            (Some(method), Some(id)) => {
                let _ = send(&writer, &reply(method, id)).await;
            }
            (Some(_), None) => {}
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
        let _ = sender.send(Err("the MCP server has stopped".into()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A server with two pages of tools, one of which fails; it pings the
    /// client before answering a call.
    async fn fake_server(
        reader: impl AsyncRead + Unpin,
        mut writer: impl AsyncWrite + Unpin,
        seen: Arc<Mutex<Vec<Value>>>,
    ) {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let message: Value = serde_json::from_str(&line).expect("json");
            seen.lock().expect("seen").push(message.clone());
            let id = message.get("id").cloned().unwrap_or(Value::Null);
            let result = match message.get("method").and_then(Value::as_str) {
                Some("initialize") => json!({"protocolVersion": "2025-06-18",
                    "capabilities": {"tools": {}}, "serverInfo": {"name": "fake", "version": "1"}}),
                Some("tools/list") if message["params"].get("cursor").is_none() => json!({
                    "tools": [{"name": "echo.text", "description": "Echo",
                        "inputSchema": {"type": "object"}}],
                    "nextCursor": "2"}),
                Some("tools/list") => {
                    json!({"tools": [{"name": "fail", "inputSchema": {"type": "object"}}]})
                }
                Some("tools/call") => {
                    let ping = json!({"jsonrpc": "2.0", "id": "p1", "method": "ping"});
                    writer
                        .write_all(format!("{ping}\n").as_bytes())
                        .await
                        .expect("ping");
                    let pong = lines.next_line().await.expect("line").expect("pong");
                    seen.lock()
                        .expect("seen")
                        .push(serde_json::from_str(&pong).expect("json"));
                    match message["params"]["name"].as_str() {
                        Some("fail") => {
                            json!({"content": [{"type": "text", "text": "broken"}], "isError": true})
                        }
                        _ => json!({"content": [
                            {"type": "text", "text": message["params"]["arguments"]["say"]},
                            {"type": "image", "data": "AAAA", "mimeType": "image/png"}]}),
                    }
                }
                _ => continue,
            };
            let answer = json!({"jsonrpc": "2.0", "id": id, "result": result});
            writer
                .write_all(format!("{answer}\n").as_bytes())
                .await
                .expect("write");
        }
    }

    #[tokio::test]
    async fn tools_are_listed_by_page_and_called() {
        let (client_end, server_end) = tokio::io::duplex(1 << 16);
        let (client_read, client_write) = tokio::io::split(client_end);
        let (server_read, server_write) = tokio::io::split(server_end);
        let seen = Arc::new(Mutex::new(Vec::new()));
        tokio::spawn(fake_server(server_read, server_write, seen.clone()));
        let connection = Arc::new(Connection::new(client_read, client_write));

        let listed = connection.list_tools().await.expect("tools");
        let mut tools = Tools::default();
        add(&mut tools, "fake server", &connection, listed);
        let names: Vec<&str> = tools.specs.iter().map(|spec| spec.name.as_str()).collect();
        assert_eq!(
            names,
            ["mcp__fake_server__echo_text", "mcp__fake_server__fail"]
        );
        assert_eq!(
            tools.specs[0].description,
            "Echo (tool echo.text of MCP server fake server)"
        );

        let (connection, tool) = &tools.routes["mcp__fake_server__echo_text"];
        let result = connection
            .request(
                "tools/call",
                json!({"name": tool, "arguments": {"say": "hi"}}),
            )
            .await
            .expect("call");
        assert_eq!(
            render(&result),
            Ok("hi\n(image of type image/png not shown)".into())
        );
        let (connection, tool) = &tools.routes["mcp__fake_server__fail"];
        let result = connection
            .request("tools/call", json!({"name": tool, "arguments": {}}))
            .await
            .expect("call");
        assert_eq!(render(&result), Err("broken".into()));

        let seen = seen.lock().expect("seen");
        let methods: Vec<&str> = seen.iter().filter_map(|m| m["method"].as_str()).collect();
        assert_eq!(
            methods,
            [
                "initialize",
                "notifications/initialized",
                "tools/list",
                "tools/list",
                "tools/call",
                "tools/call"
            ]
        );
        assert_eq!(seen[0]["params"]["protocolVersion"], PROTOCOL_VERSION);
        assert!(
            seen.iter()
                .any(|m| m["id"] == "p1" && m["result"] == json!({})),
            "ping answered"
        );
    }

    #[tokio::test]
    async fn a_server_that_stops_fails_what_waits() {
        let (client_end, server_end) = tokio::io::duplex(1 << 10);
        let (client_read, client_write) = tokio::io::split(client_end);
        let connection = Connection::new(client_read, client_write);
        drop(server_end);
        let error = connection
            .request("tools/list", json!({}))
            .await
            .unwrap_err();
        assert!(error.contains("MCP server"), "{error}");
        let Transport::Pipe(pipe) = &connection.transport else {
            unreachable!("a connection made with new is a pipe");
        };
        let waiting = pipe
            .pending
            .lock()
            .expect("pending")
            .as_ref()
            .map(HashMap::len);
        assert!(matches!(waiting, None | Some(0)), "{waiting:?}");
    }

    /// One HTTP request as the fake server saw it: method, headers with
    /// lowercase names, and body.
    type Seen = (String, HashMap<String, String>, Value);

    /// A Streamable HTTP server: it gives a session at initialization,
    /// answers `tools/list` with events that first ping the client, and
    /// `tools/call` with JSON.
    /// What the fake HTTP server keeps between requests.
    #[derive(Default)]
    struct HttpState {
        /// Sessions given so far; each initialization gives `s-<n>`.
        sessions: AtomicU64,
        /// Whether a tool call in the first session ends it (404).
        expire_first: bool,
        /// Where the answer to the question of `greet` goes.
        waiting: Mutex<Option<oneshot::Sender<Value>>>,
    }

    async fn http_server(seen: Arc<Mutex<Vec<Seen>>>) -> String {
        http_server_with(seen, Arc::new(HttpState::default())).await
    }

    async fn http_server_with(seen: Arc<Mutex<Vec<Seen>>>, state: Arc<HttpState>) -> String {
        use tokio::io::AsyncReadExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("address");
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let seen = seen.clone();
                let state = state.clone();
                tokio::spawn(async move {
                    let mut buffer = Vec::new();
                    let mut chunk = [0u8; 4096];
                    let header_end = loop {
                        if let Some(end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                            break end + 4;
                        }
                        let read = stream.read(&mut chunk).await.expect("read");
                        assert!(read > 0, "the request ended early");
                        buffer.extend_from_slice(&chunk[..read]);
                    };
                    let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
                    let mut lines = head.split("\r\n");
                    let method = lines.next().unwrap_or("").split(' ').next().unwrap_or("");
                    let headers: HashMap<String, String> = lines
                        .filter_map(|line| line.split_once(':'))
                        .map(|(name, value)| (name.to_lowercase(), value.trim().to_string()))
                        .collect();
                    let length: usize = headers
                        .get("content-length")
                        .and_then(|length| length.parse().ok())
                        .unwrap_or(0);
                    while buffer.len() < header_end + length {
                        let read = stream.read(&mut chunk).await.expect("read");
                        assert!(read > 0, "the body ended early");
                        buffer.extend_from_slice(&chunk[..read]);
                    }
                    let body: Value =
                        serde_json::from_slice(&buffer[header_end..header_end + length])
                            .unwrap_or(Value::Null);
                    seen.lock().expect("seen").push((
                        method.to_string(),
                        headers.clone(),
                        body.clone(),
                    ));
                    let id = body.get("id").cloned().unwrap_or(Value::Null);
                    let json_answer = |result: Value, extra: &str| {
                        let text =
                            json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string();
                        format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n{extra}\
                             content-length: {}\r\nconnection: close\r\n\r\n{text}",
                            text.len()
                        )
                    };
                    let accepted =
                        "HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
                    let session = headers.get("mcp-session-id").cloned();
                    let tool = body
                        .get("params")
                        .and_then(|params| params.get("name"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let reply = match (method, body.get("method").and_then(Value::as_str)) {
                        ("DELETE", _) => {
                            "HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                                .to_string()
                        }
                        (_, Some("initialize")) => {
                            let number = state.sessions.fetch_add(1, Ordering::SeqCst) + 1;
                            json_answer(
                                json!({"protocolVersion": PROTOCOL_VERSION, "capabilities": {},
                                       "serverInfo": {"name": "fake", "version": "1"}}),
                                &format!("mcp-session-id: s-{number}\r\n"),
                            )
                        }
                        (_, Some("tools/call"))
                            if state.expire_first && session.as_deref() == Some("s-1") =>
                        {
                            "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                                .to_string()
                        }
                        (_, Some("tools/call")) if tool == "greet" => {
                            // The question goes on the call's own stream; its
                            // answer comes as a POST of its own.
                            let (sender, answer) = oneshot::channel();
                            *state.waiting.lock().expect("waiting") = Some(sender);
                            let question = json!({"jsonrpc": "2.0", "id": "e1",
                                "method": "elicitation/create",
                                "params": {"message": "Who?", "requestedSchema": {
                                    "type": "object",
                                    "properties": {"name": {"type": "string"}}}}});
                            let head = format!(
                                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                                 connection: close\r\n\r\ndata: {question}\n\n"
                            );
                            let _ = stream.write_all(head.as_bytes()).await;
                            let answer = answer.await.unwrap_or(Value::Null);
                            let name = answer["content"]["name"].as_str().unwrap_or("nobody");
                            let result = json!({"jsonrpc": "2.0", "id": id, "result":
                                {"content": [{"type": "text", "text": format!("hello {name}")}]}});
                            let _ = stream
                                .write_all(format!("data: {result}\n\n").as_bytes())
                                .await;
                            let _ = stream.shutdown().await;
                            return;
                        }
                        (_, None) if body.get("id") == Some(&json!("e1")) => {
                            if let Some(waiting) = state.waiting.lock().expect("waiting").take() {
                                let _ = waiting.send(body["result"].clone());
                            }
                            accepted.to_string()
                        }
                        (_, Some("tools/list")) => {
                            let ping = json!({"jsonrpc": "2.0", "id": "p1", "method": "ping"});
                            let answer = json!({"jsonrpc": "2.0", "id": id, "result":
                                {"tools": [{"name": "echo", "inputSchema": {"type": "object"}}]}});
                            format!(
                                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                                 connection: close\r\n\r\nid: 1\ndata:\n\n\
                                 data: {ping}\n\ndata: {answer}\n\n"
                            )
                        }
                        (_, Some("tools/call")) => json_answer(
                            json!({"content": [{"type": "text", "text": "echoed"}]}),
                            "",
                        ),
                        _ => accepted.to_string(),
                    };
                    let _ = stream.write_all(reply.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        format!("http://{address}/mcp")
    }

    #[tokio::test]
    async fn an_ended_http_session_is_started_anew_and_questions_are_answered() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let state = Arc::new(HttpState {
            expire_first: true,
            ..HttpState::default()
        });
        let url = http_server_with(seen.clone(), state).await;
        let settings: ServerConfig =
            serde_json::from_value(json!({ "url": url })).expect("settings");
        let connection = connect(&settings).expect("connect");
        connection.list_tools().await.expect("tools");

        // The server ends the first session at the call: a new one is
        // started and the call sent again.
        let echoed = connection
            .request("tools/call", json!({"name": "echo", "arguments": {}}))
            .await
            .and_then(|result| render(&result));
        assert_eq!(echoed, Ok("echoed".into()));
        let requests = seen.lock().expect("seen").clone();
        // After the handshake, the list and the answer to its ping:
        let later: Vec<(Option<&str>, Option<&str>)> = requests[4..]
            .iter()
            .map(|(_, headers, body)| {
                (
                    body.get("method").and_then(Value::as_str),
                    headers.get("mcp-session-id").map(String::as_str),
                )
            })
            .collect();
        assert_eq!(
            later,
            [
                (Some("tools/call"), Some("s-1")),
                (Some("initialize"), None),
                (Some("notifications/initialized"), Some("s-2")),
                (Some("tools/call"), Some("s-2")),
            ]
        );

        // A question on the call's event stream reaches whoever waits on
        // the connection, and the answer goes back as a POST.
        let (sender, mut questions) = mpsc::unbounded_channel::<Elicitation>();
        *connection.elicit.lock().expect("elicit") = Some(sender);
        tokio::spawn(async move {
            let question = questions.recv().await.expect("a question");
            assert_eq!(question.params["message"], "Who?");
            let _ = question
                .reply
                .send(json!({"action": "accept", "content": {"name": "Bo"}}));
        });
        let greeted = connection
            .request("tools/call", json!({"name": "greet", "arguments": {}}))
            .await
            .and_then(|result| render(&result));
        assert_eq!(greeted, Ok("hello Bo".into()));
    }

    #[tokio::test]
    async fn a_server_over_http_is_listed_called_and_its_session_kept() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let url = http_server(seen.clone()).await;
        let settings: ServerConfig = serde_json::from_value(json!({
            "url": url, "headers": {"Authorization": "Bearer t"}
        }))
        .expect("settings");
        let connection = Arc::new(connect(&settings).expect("connect"));
        let listed = connection.list_tools().await.expect("tools");
        assert_eq!(listed.len(), 1);
        let mut tools = Tools::default();
        add(&mut tools, "web", &connection, listed);
        let (route, tool) = tools.routes.get("mcp__web__echo").expect("route");
        let result = route
            .request("tools/call", json!({"name": tool, "arguments": {}}))
            .await
            .and_then(|result| render(&result));
        assert_eq!(result, Ok("echoed".into()));

        let requests = seen.lock().expect("seen").clone();
        let methods: Vec<&str> = requests
            .iter()
            .map(|(_, _, body)| {
                body.get("method")
                    .and_then(Value::as_str)
                    .unwrap_or("(answer)")
            })
            .collect();
        assert_eq!(
            methods,
            [
                "initialize",
                "notifications/initialized",
                "tools/list",
                "(answer)",
                "tools/call"
            ]
        );
        // The ping met in the event stream is answered with a POST of its own.
        assert_eq!(
            requests[3].2,
            json!({"jsonrpc": "2.0", "id": "p1", "result": {}})
        );
        for (index, (method, headers, _)) in requests.iter().enumerate() {
            assert_eq!(method, "POST");
            assert_eq!(
                headers.get("authorization").map(String::as_str),
                Some("Bearer t")
            );
            assert_eq!(
                headers.get("accept").map(String::as_str),
                Some("application/json, text/event-stream")
            );
            let later = index > 0;
            assert_eq!(headers.get("mcp-session-id").is_some(), later, "{index}");
            assert_eq!(
                headers.get("mcp-protocol-version").map(String::as_str),
                later.then_some(PROTOCOL_VERSION),
                "{index}"
            );
        }

        // Dropping the last copy ends the session.
        drop(tools);
        drop(connection);
        for _ in 0..100 {
            if seen
                .lock()
                .expect("seen")
                .iter()
                .any(|(method, _, _)| method == "DELETE")
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the session was not ended");
    }

    #[test]
    fn a_server_needs_either_a_command_or_a_url() {
        for config in [
            json!({}),
            json!({"command": "x", "url": "http://localhost/mcp"}),
        ] {
            let settings: ServerConfig = serde_json::from_value(config).expect("settings");
            assert_eq!(
                connect(&settings).err().as_deref(),
                Some("give either command or url")
            );
        }
        let settings: ServerConfig =
            serde_json::from_value(json!({"url": "file:///etc/passwd"})).expect("settings");
        assert!(connect(&settings).is_err());
    }

    #[test]
    fn a_requested_schema_becomes_the_questions_of_ask() {
        let schema = json!({
            "type": "object",
            "properties": {
                "color": {"type": "string", "title": "Color", "enum": ["Red", "Blue"]},
                "hex": {"type": "string", "oneOf": [{"const": "#f00", "title": "Red"}]},
                "tags": {"type": "array", "items": {"enum": ["a", "b"]}, "default": ["a"]},
                "ok": {"type": "boolean", "description": "Go on?"},
                "count": {"type": "integer", "minimum": 1, "maximum": 8},
                "when": {"type": "string", "format": "date"},
                "mail": {"type": "string", "format": "email"}
            },
            "required": ["color"]
        });
        let questions = questions_of(&schema);
        let by_name = |name: &str| {
            questions
                .as_array()
                .unwrap()
                .iter()
                .find(|question| question["name"] == name)
                .cloned()
                .unwrap()
        };
        assert_eq!(
            by_name("color"),
            json!({"name": "color", "question": "Color", "required": true, "kind": "choose_one",
                   "options": [{"label": "Red", "value": "Red"}, {"label": "Blue", "value": "Blue"}]})
        );
        assert_eq!(
            by_name("hex")["options"],
            json!([{"label": "Red", "value": "#f00"}])
        );
        assert_eq!(by_name("tags")["kind"], "choose_many");
        assert_eq!(by_name("tags")["default"], json!(["a"]));
        assert_eq!(by_name("ok")["kind"], "yes_no");
        assert_eq!(by_name("ok")["question"], "Go on?");
        assert_eq!(by_name("count")["kind"], "number");
        assert_eq!(by_name("count")["maximum"], 8);
        assert_eq!(by_name("when")["kind"], "date");
        assert_eq!(by_name("mail")["kind"], "text");
        assert_eq!(by_name("mail")["format"], "email");
        assert_eq!(by_name("mail")["required"], false);
    }

    #[test]
    fn names_are_made_acceptable_and_results_rendered() {
        assert_eq!(tool_name("git hub", "get.issue"), "mcp__git_hub__get_issue");
        assert_eq!(tool_name("s", &"x".repeat(100)).chars().count(), NAME_CHARS);
        assert_eq!(
            render(&json!({"content": [
                {"type": "resource_link", "uri": "file:///a", "name": "a"},
                {"type": "resource", "resource": {"uri": "file:///b", "text": "B"}},
                {"type": "resource", "resource": {"uri": "file:///c", "blob": "AA"}}]})),
            Ok("(resource a: file:///a)\nB\n(resource file:///c not shown)".into())
        );
        assert_eq!(
            render(&json!({"content": [], "structuredContent": {"n": 1}})),
            Ok("{\"n\":1}".into())
        );
    }

    #[tokio::test]
    async fn missing_and_broken_servers_give_no_tools() {
        let dir = tempfile::tempdir().expect("dir");
        let config = dir.path().join("mcp.json");
        assert!(load(&config).await.specs.is_empty());
        std::fs::write(
            &config,
            r#"{"mcpServers": {"none": {"command": "inocode-no-such-command"}}}"#,
        )
        .expect("write");
        assert!(load(&config).await.specs.is_empty());
    }
}
