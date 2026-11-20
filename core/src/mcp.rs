//! The MCP client: the servers listed in `mcp.json` beside the thread store
//! are started over stdio, and their tools are given to the model as
//! `mcp__<server>__<tool>`. Design and sources: docs/evidence/mcp-client.md.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::oneshot;

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
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
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

/// Runs an MCP tool; `None` when `name` is not one.
pub(crate) async fn call(
    store: &Store,
    name: &str,
    arguments: &str,
    mut cancel: tokio::sync::watch::Receiver<bool>,
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
    let request = connection.request("tools/call", json!({"name": tool, "arguments": arguments}));
    Some(tokio::select! {
        answer = request => answer.and_then(|result| render(&result)),
        _ = cancel.wait_for(|cancelled| *cancelled) => Err("cancelled".into()),
    })
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
            let connection = Arc::new(spawn(&settings)?);
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

fn spawn(settings: &ServerConfig) -> Result<Connection, String> {
    let mut child = tokio::process::Command::new(&settings.command)
        .args(&settings.args)
        .envs(&settings.env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| format!("cannot run {}: {error}", settings.command))?;
    let stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let mut connection = Connection::new(stdout, stdin);
    connection.child = Some(child);
    Ok(connection)
}

type Writer = Arc<tokio::sync::Mutex<Box<dyn AsyncWrite + Send + Unpin>>>;
/// Requests waiting for their answer; `None` once the server has closed.
type Pending = Arc<Mutex<Option<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>>;

/// One server, spoken to with JSON-RPC messages, one per line.
pub(crate) struct Connection {
    writer: Writer,
    pending: Pending,
    next_id: AtomicU64,
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
        tokio::spawn(read(reader, writer.clone(), pending.clone()));
        Self {
            writer,
            pending,
            next_id: AtomicU64::new(1),
            child: None,
        }
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        match self.pending.lock().expect("pending lock").as_mut() {
            Some(pending) => pending.insert(id, sender),
            None => return Err("the MCP server has stopped".into()),
        };
        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        if let Err(error) = send(&self.writer, &message).await {
            if let Some(pending) = self.pending.lock().expect("pending lock").as_mut() {
                pending.remove(&id);
            }
            return Err(error);
        }
        receiver
            .await
            .unwrap_or_else(|_| Err("the MCP server has stopped".into()))
    }

    /// The handshake, then every page of the server's tools.
    async fn list_tools(&self) -> Result<Vec<Value>, String> {
        let answer = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
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
        send(
            &self.writer,
            &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        )
        .await?;
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
        if let Some(child) = self.child.as_mut() {
            let _ = child.start_kill();
        }
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
async fn read(reader: impl AsyncRead + Unpin, writer: Writer, pending: Pending) {
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let id = message.get("id").cloned();
        match (message.get("method").and_then(Value::as_str), id) {
            (Some(method), Some(id)) => {
                let answer = match method {
                    "ping" => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
                    _ => json!({"jsonrpc": "2.0", "id": id,
                        "error": {"code": -32601, "message": "Method not found"}}),
                };
                let _ = send(&writer, &answer).await;
            }
            (Some(_), None) => {}
            (None, Some(id)) => {
                let Some(id) = id.as_u64() else { continue };
                let sender = pending
                    .lock()
                    .expect("pending lock")
                    .as_mut()
                    .and_then(|pending| pending.remove(&id));
                let answer = match message.get("error") {
                    Some(error) => Err(format!(
                        "MCP error {}: {}",
                        error.get("code").unwrap_or(&Value::Null),
                        error.get("message").and_then(Value::as_str).unwrap_or("")
                    )),
                    None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                };
                if let Some(sender) = sender {
                    let _ = sender.send(answer);
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
        let waiting = connection
            .pending
            .lock()
            .expect("pending")
            .as_ref()
            .map(HashMap::len);
        assert!(matches!(waiting, None | Some(0)), "{waiting:?}");
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
