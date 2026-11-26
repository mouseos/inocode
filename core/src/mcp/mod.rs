//! The MCP client: the servers listed in `mcp.json` beside the thread store
//! are started over stdio or reached over HTTP, and their tools are given to
//! the model as `mcp__<server>__<tool>`. Servers of the stateless revision
//! (2026-07-28) and of the revisions with an `initialize` handshake are both
//! spoken to. Design and sources: docs/evidence/mcp-client.md.

mod connection;
mod http;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::conversation::ToolSpec;
use crate::store::Store;
use connection::{
    Connection, Era, Fault, Listing, Notice, RpcError, Shared, Transport, Waiting, HEADER_MISMATCH,
    STOPPED, URL_ELICITATION_REQUIRED,
};

pub(crate) const PREFIX: &str = "mcp__";
/// How long a server may take to start and list its tools unless its
/// settings say otherwise (Codex's `DEFAULT_STARTUP_TIMEOUT` and opencode's
/// startup timeout are 30 seconds too).
const START_TIMEOUT: Duration = Duration::from_secs(30);
/// From this many MCP tools on, none is given to the model up front; it finds
/// the ones it needs with `SEARCH` (Codex's `DIRECT_MCP_TOOL_EXPOSURE_THRESHOLD`
/// in `codex-rs/core/src/mcp_tool_exposure.rs`).
const DEFER_AT: usize = 100;
/// The tool that searches the MCP tools not given up front.
pub(crate) const SEARCH: &str = "tool_search";
/// How many tools a search returns unless the model asks for another number
/// (Codex's `TOOL_SEARCH_DEFAULT_LIMIT`).
const SEARCH_LIMIT: usize = 8;
/// Providers accept tool names of at most 64 characters.
const NAME_CHARS: usize = 64;
/// The log levels of the protocol (RFC 5424's severities), lowest first.
const LOG_LEVELS: [&str; 8] = [
    "debug",
    "info",
    "notice",
    "warning",
    "error",
    "critical",
    "alert",
    "emergency",
];
/// How many times one call is sent again with more input (or after the
/// person visited a URL) before it is given up; a server may ask again and
/// again.
const MAX_ROUNDS: usize = 20;
/// How long a task's state is polled after when the server suggests no time.
const TASK_POLL: Duration = Duration::from_secs(1);

#[derive(Deserialize, Default, Clone)]
struct Config {
    #[serde(rename = "mcpServers", default)]
    servers: BTreeMap<String, ServerConfig>,
    /// The accounts' connectors and ChatGPT apps by name or id; each one
    /// the account has added is used unless it is turned off here.
    #[serde(default)]
    connectors: HashMap<String, ConnectorSetting>,
    /// The variables of the environment that the installed plugins'
    /// settings may use as `${NAME}`; the others are left empty.
    #[serde(rename = "pluginVariables", default)]
    plugin_variables: Vec<String>,
}

#[derive(Deserialize, Clone)]
struct ConnectorSetting {
    #[serde(default = "turned_on")]
    enabled: bool,
}

fn turned_on() -> bool {
    true
}

impl Config {
    /// Whether no name in `names` is turned off.
    fn uses(&self, names: &[&str]) -> bool {
        names.iter().all(|name| {
            self.connectors
                .get(*name)
                .is_none_or(|setting| setting.enabled)
        })
    }
}

#[derive(Deserialize, Clone)]
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
    /// How many seconds the server may take to start and list its tools;
    /// `START_TIMEOUT` when missing. Codex's name for it.
    #[serde(default)]
    startup_timeout_sec: Option<f64>,
    /// The level from which the server's log messages are asked for
    /// (`debug` … `emergency`); none are asked for when missing.
    #[serde(default)]
    log_level: Option<String>,
    /// Where a started server runs: an installed plugin's folder.
    #[serde(skip)]
    folder: Option<PathBuf>,
}

/// The tools of every server that started, in the order of the file.
#[derive(Default)]
pub(crate) struct Tools {
    specs: Vec<ToolSpec>,
    routes: HashMap<String, (Arc<Slot>, String)>,
    /// What went wrong when the servers started: servers that did not
    /// start, tools left out, variables left empty.
    problems: Vec<String>,
    /// When `mcp.json` was last changed, as it was when these were loaded.
    stamp: Option<SystemTime>,
    /// The servers that started, in the order their tools are given.
    servers: Vec<Served>,
    /// The connectors turned on or off, for a connector's tools listed again.
    config: Config,
    /// The problems of the start, kept when tools are listed again.
    started: Vec<String>,
}

/// A server that started, and whether it is an account's connector.
#[derive(Clone)]
struct Served {
    name: String,
    slot: Arc<Slot>,
    connector: bool,
}

/// One server's connection, and how to start it again.
pub(crate) struct Slot {
    server: String,
    connection: Mutex<Arc<Connection>>,
    /// `None` for a connection that cannot be made again.
    source: Option<Source>,
    restarted: AtomicBool,
    /// The server's tools as last listed, and when.
    listing: Mutex<(Listing, Instant)>,
}

impl Slot {
    fn new(connection: Arc<Connection>, source: Option<Source>, listing: Listing) -> Arc<Self> {
        Arc::new(Self {
            server: connection.shared.server.clone(),
            connection: Mutex::new(connection),
            source,
            restarted: AtomicBool::new(false),
            listing: Mutex::new((listing, Instant::now())),
        })
    }

    fn current(&self) -> Arc<Connection> {
        self.connection.lock().expect("slot lock").clone()
    }

    /// Starts a server this client runs once more, after it stopped; only
    /// once for each load of the servers, so a server that keeps crashing
    /// is not started again and again. The era found before is spoken.
    async fn restart(&self) -> Result<Arc<Connection>, String> {
        if self.restarted.swap(true, Ordering::SeqCst) {
            return Err("it was already started again once".into());
        }
        let source = self.source.as_ref().ok_or("it cannot be started again")?;
        let (connection, listing) = start(&self.server, source, self.current().era()).await?;
        *self.listing.lock().expect("listing lock") = (listing, Instant::now());
        // The tools given to the model are made again from the new list.
        connection
            .shared
            .tools_changed
            .store(true, Ordering::SeqCst);
        *self.connection.lock().expect("slot lock") = connection.clone();
        Ok(connection)
    }

    /// Whether the tools are to be listed again: the server said they
    /// changed, or the time it said the list stays fresh is over.
    fn stale(&self) -> bool {
        let changed = self
            .current()
            .shared
            .tools_changed
            .swap(false, Ordering::SeqCst);
        let (listing, received) = &*self.listing.lock().expect("listing lock");
        changed || listing.ttl.is_some_and(|ttl| received.elapsed() >= ttl)
    }

    /// Lists the server's tools again; the old list stays when that fails.
    async fn relist(&self) {
        let timeout = self
            .source
            .as_ref()
            .and_then(|source| source.start_timeout().ok())
            .unwrap_or(START_TIMEOUT);
        match tokio::time::timeout(timeout, self.current().list_tools()).await {
            Ok(Ok(listing)) => {
                *self.listing.lock().expect("listing lock") = (listing, Instant::now());
            }
            Ok(Err(fault)) => eprintln!(
                "MCP: the tools of server {} could not be listed again: {fault}",
                self.server
            ),
            Err(_) => eprintln!(
                "MCP: the tools of server {} were not listed again within {timeout:?}",
                self.server
            ),
        }
    }
}

/// When `config` was last changed; `None` when it is missing.
fn modified(config: &Path) -> Option<SystemTime> {
    std::fs::metadata(config)
        .and_then(|meta| meta.modified())
        .ok()
}

/// The servers in `mcp.json` beside the thread store and those of the
/// installed plugins, started once and kept with the store, so the tool
/// list stays the same from request to request. They start anew after a
/// plugin is installed or `mcp.json` changed.
async fn loaded(store: &Store) -> Arc<Tools> {
    let config: PathBuf = store.files_folder().with_file_name("mcp.json");
    let accounts = crate::connectors::store_beside(store);
    let plugins = crate::plugins::folder(store);
    let stamp = modified(&config);
    let mut kept = store.mcp().lock().await;
    if let Some(tools) = kept.as_ref().filter(|tools| tools.stamp == stamp) {
        return tools.clone();
    }
    // The servers of the old list stop once the calls still using them end.
    *kept = None;
    let mut tools = load(
        &config,
        &accounts,
        &plugins,
        &crate::connectors::Hosts::default(),
    )
    .await;
    tools.stamp = stamp;
    let tools = Arc::new(tools);
    *kept = Some(tools.clone());
    tools
}

/// The loaded tools, with the tools of each server that said they changed,
/// or whose list is no longer fresh, listed again. Only done before a model
/// request, so the list stays the same within one.
async fn refreshed(store: &Store) -> Arc<Tools> {
    let tools = loaded(store).await;
    let stale: Vec<&Served> = tools
        .servers
        .iter()
        .filter(|served| served.slot.stale())
        .collect();
    if stale.is_empty() {
        return tools;
    }
    futures_util::future::join_all(stale.iter().map(|served| served.slot.relist())).await;
    let mut rebuilt = Tools {
        stamp: tools.stamp,
        servers: tools.servers.clone(),
        config: tools.config.clone(),
        started: tools.started.clone(),
        ..Tools::default()
    };
    rebuilt.build();
    let rebuilt = Arc::new(rebuilt);
    let mut kept = store.mcp().lock().await;
    if kept.as_ref().is_some_and(|kept| Arc::ptr_eq(kept, &tools)) {
        *kept = Some(rebuilt.clone());
    }
    rebuilt
}

/// What went wrong when the MCP servers started, for the model; `None`
/// when nothing did or they have not started.
pub(crate) async fn render_problems(store: &Store) -> Option<String> {
    let kept = store.mcp().lock().await;
    let problems = &kept.as_ref()?.problems;
    (!problems.is_empty()).then(|| format!("MCP servers:\n- {}", problems.join("\n- ")))
}

/// The MCP tools to offer the model in a request after `messages`: all of
/// them when there are fewer than `DEFER_AT`, otherwise `SEARCH` and the
/// tools an earlier search in `messages` found.
pub(crate) async fn specs(
    store: &Store,
    messages: &[crate::conversation::Message],
) -> Vec<ToolSpec> {
    refreshed(store).await.offered(messages)
}

impl Tools {
    /// The specs, routes and problems made anew from each server's tools as
    /// last listed.
    fn build(&mut self) {
        self.specs.clear();
        self.routes.clear();
        self.problems = self.started.clone();
        for served in self.servers.clone() {
            let listing = served.slot.listing.lock().expect("listing lock").0.clone();
            self.problems.extend(listing.rejected);
            let listed = match served.connector {
                true => by_app(&self.config, &served.name, listing.tools),
                false => listing.tools,
            };
            add(self, &served.name, &served.slot, listed);
        }
    }

    fn offered(&self, messages: &[crate::conversation::Message]) -> Vec<ToolSpec> {
        if self.specs.len() < DEFER_AT {
            return self.specs.clone();
        }
        let mut found = std::collections::HashSet::new();
        for message in messages {
            let crate::conversation::Body::ToolResult {
                name,
                content,
                is_error: false,
                ..
            } = &message.body
            else {
                continue;
            };
            if name != SEARCH {
                continue;
            }
            let Ok(result) = serde_json::from_str::<Value>(content) else {
                continue;
            };
            for tool in result["tools"].as_array().into_iter().flatten() {
                if let Some(name) = tool["name"].as_str() {
                    found.insert(name.to_string());
                }
            }
        }
        let mut offered = vec![self.search_spec()];
        offered.extend(
            self.specs
                .iter()
                .filter(|spec| found.contains(&spec.name))
                .cloned(),
        );
        offered
    }

    fn search_spec(&self) -> ToolSpec {
        let mut servers: Vec<&str> = self
            .routes
            .values()
            .map(|(slot, _)| slot.server.as_str())
            .collect();
        servers.sort_unstable();
        servers.dedup();
        ToolSpec {
            name: SEARCH.to_string(),
            description: format!(
                "Searches the tools of these MCP servers, which are not given to you up \
                 front: {}. The tools found can be called from your next step on.",
                servers.join(", ")
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Words describing the tool needed."},
                    "limit": {"type": "integer", "description": format!("How many tools to return; {SEARCH_LIMIT} if left out.")},
                },
                "required": ["query"],
                "additionalProperties": false,
            }),
        }
    }

    /// The tools that best match `query` by BM25 over each tool's name,
    /// description and argument names, as JSON naming each one.
    fn search(&self, arguments: &str) -> Result<String, String> {
        #[derive(Deserialize)]
        struct Arguments {
            query: String,
            limit: Option<usize>,
        }
        let arguments: Arguments = serde_json::from_str(arguments)
            .map_err(|error| format!("invalid arguments: {error}"))?;
        let query = words(&arguments.query);
        if query.is_empty() {
            return Err("query must not be empty".into());
        }
        let limit = arguments.limit.unwrap_or(SEARCH_LIMIT);
        if limit == 0 {
            return Err("limit must be greater than zero".into());
        }
        let documents: Vec<Vec<String>> = self.specs.iter().map(search_words).collect();
        let tools: Vec<Value> = bm25(&documents, &query)
            .into_iter()
            .take(limit)
            .map(|index| {
                let spec = &self.specs[index];
                json!({"name": spec.name, "description": spec.description})
            })
            .collect();
        Ok(json!({"tools": tools}).to_string())
    }
}

/// Lowercased runs of letters and digits.
pub(crate) fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// The words a search matches a tool on: its name, its description and the
/// names and descriptions of its arguments (as Codex's
/// `default_tool_search_text`).
fn search_words(spec: &ToolSpec) -> Vec<String> {
    fn schema(value: &Value, text: &mut String) {
        if let Some(description) = value.get("description").and_then(Value::as_str) {
            text.push(' ');
            text.push_str(description);
        }
        if let Some(properties) = value.get("properties").and_then(Value::as_object) {
            for (name, property) in properties {
                text.push(' ');
                text.push_str(name);
                schema(property, text);
            }
        }
        if let Some(items) = value.get("items") {
            schema(items, text);
        }
    }
    let mut text = format!("{} {}", spec.name, spec.description);
    schema(&spec.parameters, &mut text);
    words(&text)
}

/// The indexes of `documents` that share a word with `query`, best first,
/// ranked by Okapi BM25 (k1 = 1.2, b = 0.75; Robertson and Zaragoza 2009).
pub(crate) fn bm25(documents: &[Vec<String>], query: &[String]) -> Vec<usize> {
    const K1: f64 = 1.2;
    const B: f64 = 0.75;
    let count = documents.len() as f64;
    let average = documents.iter().map(Vec::len).sum::<usize>() as f64 / count.max(1.0);
    let mut terms: Vec<&String> = query.iter().collect();
    terms.sort_unstable();
    terms.dedup();
    let mut scored: Vec<(usize, f64)> = documents
        .iter()
        .enumerate()
        .filter_map(|(index, document)| {
            let mut score = 0.0;
            for term in &terms {
                let frequency = document.iter().filter(|word| word == term).count() as f64;
                if frequency == 0.0 {
                    continue;
                }
                let holding = documents
                    .iter()
                    .filter(|other| other.contains(term))
                    .count() as f64;
                let idf = ((count - holding + 0.5) / (holding + 0.5) + 1.0).ln();
                let length = document.len() as f64 / average.max(1.0);
                score += idf * frequency * (K1 + 1.0) / (frequency + K1 * (1.0 - B + B * length));
            }
            (score > 0.0).then_some((index, score))
        })
        .collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    scored.into_iter().map(|(index, _)| index).collect()
}

/// Where a call's server questions, progress and logs for the person go:
/// the turn's events, for its thread.
pub(crate) struct Asking<'a> {
    pub session: crate::store::SessionId,
    pub events: &'a mpsc::Sender<crate::turn::TurnEvent>,
}

/// How long a server's question waits for the person before it is answered
/// with `cancel`.
const ANSWER_WAIT: Duration = Duration::from_secs(600);

/// Runs an MCP tool; `None` when `name` is not one. With `asking`, the
/// server's questions during the call are put to the person and the call
/// waits for the answer, and its progress and logs are shown.
pub(crate) async fn call(
    store: &Store,
    name: &str,
    arguments: &str,
    cancel: tokio::sync::watch::Receiver<bool>,
    asking: Option<Asking<'_>>,
) -> Option<Result<String, String>> {
    if name == SEARCH {
        return Some(loaded(store).await.search(arguments));
    }
    if !name.starts_with(PREFIX) {
        return None;
    }
    let tools = loaded(store).await;
    let (slot, tool) = tools.routes.get(name)?;
    let arguments: Value = if arguments.trim().is_empty() {
        json!({})
    } else {
        match serde_json::from_str(arguments) {
            Ok(value) => value,
            Err(error) => return Some(Err(format!("invalid arguments: {error}"))),
        }
    };
    let connection = slot.current();
    let call = Call {
        store,
        connection: &connection,
        tool,
        asking: asking.as_ref(),
    };
    let answer = call.run(&arguments, &cancel).await;
    let stopped = match &answer {
        Err(Fault::NotRunning) => true,
        Err(Fault::Stopped) => false,
        _ => return Some(answer.map_err(|fault| fault.to_string())),
    };
    if !connection.is_pipe() {
        return Some(answer.map_err(|fault| fault.to_string()));
    }
    Some(match slot.restart().await {
        // The call never reached the stopped server, so it is sent again.
        Ok(connection) if stopped => Call {
            connection: &connection,
            ..call
        }
        .run(&arguments, &cancel)
        .await
        .map_err(|fault| fault.to_string()),
        Ok(_) => Err(format!(
            "{STOPPED} during the call, which may have been partly done; it was started \
             again, so the call can be made again if needed"
        )),
        Err(error) => Err(format!(
            "{}; it could not be started again: {error}",
            answer
                .err()
                .map(|fault| fault.to_string())
                .unwrap_or_default()
        )),
    })
}

/// A progress token unique among the calls of this process.
fn progress_token() -> String {
    static TOKENS: AtomicU64 = AtomicU64::new(1);
    format!("inocode-{}", TOKENS.fetch_add(1, Ordering::Relaxed))
}

/// One call of a tool on a connection.
#[derive(Clone, Copy)]
struct Call<'a> {
    store: &'a Store,
    connection: &'a Arc<Connection>,
    tool: &'a str,
    asking: Option<&'a Asking<'a>>,
}

impl Call<'_> {
    /// Calls the tool: input the server asks for is gathered and the call
    /// sent again with it; a task the server makes of the call is polled
    /// until it ends; the call stops when the turn is interrupted, and the
    /// server is told so.
    async fn run(
        &self,
        arguments: &Value,
        cancel: &tokio::sync::watch::Receiver<bool>,
    ) -> Result<String, Fault> {
        let connection = self.connection;
        let (sender, mut notices) = mpsc::unbounded_channel();
        let token = self.asking.map(|_| progress_token());
        let _waiting = Waiting::register(
            &connection.shared,
            self.asking.map(|asking| asking.session),
            self.asking.map(|_| sender),
            token.clone(),
        );
        let mut cancel = cancel.clone();
        let mut retry = serde_json::Map::new();
        let (mut rounds, mut sent_again, mut listed_again) = (0, false, false);
        loop {
            let mut params = json!({"name": self.tool, "arguments": arguments});
            if let Some(token) = &token {
                params["_meta"] = json!({"progressToken": token});
            }
            for (key, value) in std::mem::take(&mut retry) {
                params[key] = value;
            }
            let id = connection.next_id();
            let headers = connection.call_headers(self.tool, arguments);
            let request = connection.exchange(id, "tools/call", params, &headers);
            let Some(answer) = self.wait(request, &mut notices, &mut cancel).await else {
                connection.cancel(id).await;
                return Err(Fault::Other("cancelled".into()));
            };
            let result = match answer {
                Ok(result) => result,
                Err(Fault::Broken) if !sent_again => {
                    sent_again = true;
                    continue;
                }
                // The tool's headers may have changed: they are listed again.
                Err(fault)
                    if !listed_again
                        && fault.rpc().is_some_and(|rpc| rpc.code == HEADER_MISMATCH) =>
                {
                    listed_again = true;
                    connection.list_tools().await?;
                    connection
                        .shared
                        .tools_changed
                        .store(true, Ordering::SeqCst);
                    continue;
                }
                Err(fault) => {
                    let visits = fault
                        .rpc()
                        .filter(|rpc| rpc.code == URL_ELICITATION_REQUIRED)
                        .map(|rpc| rpc.data.get("elicitations").cloned());
                    let Some(Some(Value::Array(visits))) = visits else {
                        return Err(fault);
                    };
                    rounds += 1;
                    if rounds > MAX_ROUNDS || !self.visited(visits, &cancel).await {
                        return Err(fault);
                    }
                    continue;
                }
            };
            match result.get("resultType").and_then(Value::as_str) {
                None | Some("complete") => return render(&result).map_err(Fault::Other),
                Some("input_required") => {
                    rounds += 1;
                    if rounds > MAX_ROUNDS {
                        return Err(Fault::Other(format!(
                            "the MCP server still asked for input after {MAX_ROUNDS} rounds"
                        )));
                    }
                    if let Some(requests) = result.get("inputRequests").and_then(Value::as_object) {
                        let responses = self.inputs(requests, &cancel).await?;
                        retry.insert("inputResponses".into(), Value::Object(responses));
                    }
                    if let Some(state) = result.get("requestState") {
                        retry.insert("requestState".into(), state.clone());
                    }
                }
                Some("task") => return self.task(result, &mut notices, &mut cancel).await,
                Some(other) => {
                    return Err(Fault::Other(format!(
                        "the MCP server answered with the unknown resultType {other:?}"
                    )))
                }
            }
        }
    }

    /// Waits for `request`, putting the server's questions to the person
    /// and showing its progress and logs meanwhile; `None` when the turn is
    /// interrupted first.
    async fn wait<T>(
        &self,
        request: impl std::future::Future<Output = T>,
        notices: &mut mpsc::UnboundedReceiver<Notice>,
        cancel: &mut tokio::sync::watch::Receiver<bool>,
    ) -> Option<T> {
        tokio::pin!(request);
        let interrupted = cancel.clone();
        loop {
            // What the server sent before its answer is shown before the
            // answer is taken.
            tokio::select! {
                biased;
                Some(notice) = notices.recv() => match notice {
                    Notice::Question(question) => {
                        let answer = self.question(question.params, &interrupted).await;
                        let _ = question.reply.send(answer);
                    }
                    Notice::Progress(params) => self.show(crate::turn::TurnEvent::McpProgress(json!({
                        "server": self.connection.shared.server,
                        "tool": self.tool,
                        "progress": params.get("progress"),
                        "total": params.get("total"),
                        "message": params.get("message"),
                    }))),
                    Notice::Log(params) => self.show(crate::turn::TurnEvent::McpLog(json!({
                        "server": self.connection.shared.server,
                        "level": params.get("level"),
                        "logger": params.get("logger"),
                        "data": params.get("data"),
                    }))),
                },
                answer = &mut request => return Some(answer),
                () = crate::turn::cancelled(cancel) => return None,
            }
        }
    }

    /// Shows an event of the call; dropped when the turn's events are full,
    /// so a server's flood of progress cannot hold the call up.
    fn show(&self, event: crate::turn::TurnEvent) {
        if let Some(asking) = self.asking {
            let _ = asking.events.try_send(event);
        }
    }

    /// The person's answer to a question of the server; `cancel` when
    /// nobody can be asked.
    async fn question(&self, params: Value, cancel: &tokio::sync::watch::Receiver<bool>) -> Value {
        match self.asking {
            Some(asking) => {
                ask_person(
                    self.store,
                    &self.connection.shared.server,
                    asking,
                    params,
                    cancel.clone(),
                )
                .await
            }
            None => json!({"action": "cancel"}),
        }
    }

    /// The answers to a server's input requests, keyed as asked. Only
    /// questions (`elicitation/create`) are answered; this client declares
    /// no other input, so asking for one fails the call.
    async fn inputs(
        &self,
        requests: &serde_json::Map<String, Value>,
        cancel: &tokio::sync::watch::Receiver<bool>,
    ) -> Result<serde_json::Map<String, Value>, Fault> {
        for request in requests.values() {
            let method = request.get("method").and_then(Value::as_str).unwrap_or("");
            if method != "elicitation/create" {
                return Err(Fault::Other(format!(
                    "the MCP server asked for {method}, which inocode does not offer (it did \
                     not declare that capability)"
                )));
            }
        }
        let mut responses = serde_json::Map::new();
        for (key, request) in requests {
            let params = request.get("params").cloned().unwrap_or(Value::Null);
            responses.insert(key.clone(), self.question(params, cancel).await);
        }
        Ok(responses)
    }

    /// Puts the URLs a 2025-11-25 server needs the person to visit to the
    /// person; whether every one was accepted.
    async fn visited(
        &self,
        visits: Vec<Value>,
        cancel: &tokio::sync::watch::Receiver<bool>,
    ) -> bool {
        for visit in visits {
            let mut params = visit;
            params["mode"] = json!("url");
            let answer = self.question(params, cancel).await;
            if answer.get("action").and_then(Value::as_str) != Some("accept") {
                return false;
            }
        }
        true
    }

    /// Polls the task the server made of the call until it ends, answering
    /// the input it asks for on the way; an interrupted turn cancels it.
    async fn task(
        &self,
        created: Value,
        notices: &mut mpsc::UnboundedReceiver<Notice>,
        cancel: &mut tokio::sync::watch::Receiver<bool>,
    ) -> Result<String, Fault> {
        let connection = self.connection;
        let id = created
            .get("taskId")
            .and_then(Value::as_str)
            .ok_or_else(|| Fault::Other("the MCP server made a task without a taskId".into()))?
            .to_string();
        let mut task = created;
        let mut answered: HashSet<String> = HashSet::new();
        let mut shown: Option<String> = None;
        loop {
            let message = task
                .get("statusMessage")
                .and_then(Value::as_str)
                .map(str::to_string);
            if message.is_some() && message != shown {
                self.show(crate::turn::TurnEvent::McpProgress(json!({
                    "server": connection.shared.server,
                    "tool": self.tool,
                    "status": task.get("status"),
                    "message": message,
                })));
                shown = message.clone();
            }
            match task.get("status").and_then(Value::as_str) {
                Some("completed") => {
                    return render(task.get("result").unwrap_or(&Value::Null)).map_err(Fault::Other)
                }
                Some("failed") => {
                    return Err(match task.get("error") {
                        Some(error) => Fault::Rpc(RpcError::of(error)),
                        None => Fault::Other("the MCP server's task failed".into()),
                    })
                }
                Some("cancelled") => {
                    return Err(Fault::Other(format!(
                        "the MCP server cancelled the task{}",
                        message.map(|text| format!(": {text}")).unwrap_or_default()
                    )))
                }
                Some("input_required") => {
                    let open: serde_json::Map<String, Value> = task
                        .get("inputRequests")
                        .and_then(Value::as_object)
                        .into_iter()
                        .flatten()
                        .filter(|(key, _)| !answered.contains(*key))
                        .map(|(key, request)| (key.clone(), request.clone()))
                        .collect();
                    if !open.is_empty() {
                        let interrupted = cancel.clone();
                        let responses = self.inputs(&open, &interrupted).await?;
                        answered.extend(open.keys().cloned());
                        let update = connection.request(
                            "tasks/update",
                            json!({"taskId": id, "inputResponses": responses}),
                        );
                        match self.wait(update, notices, cancel).await {
                            Some(answer) => {
                                answer?;
                            }
                            None => return self.cancel_task(&id).await,
                        }
                    }
                }
                _ => {}
            }
            let poll = task
                .get("pollIntervalMs")
                .and_then(Value::as_u64)
                .map_or(TASK_POLL, Duration::from_millis);
            tokio::select! {
                () = tokio::time::sleep(poll) => {}
                () = crate::turn::cancelled(cancel) => return self.cancel_task(&id).await,
            }
            let get = connection.request("tasks/get", json!({"taskId": id}));
            task = match self.wait(get, notices, cancel).await {
                Some(answer) => answer?,
                None => return self.cancel_task(&id).await,
            };
        }
    }

    /// Asks the server to cancel task `id`, without waiting long; the call
    /// ends as cancelled.
    async fn cancel_task(&self, id: &str) -> Result<String, Fault> {
        let _ = tokio::time::timeout(
            connection::END_TIMEOUT,
            self.connection
                .request("tasks/cancel", json!({"taskId": id})),
        )
        .await;
        Err(Fault::Other("cancelled".into()))
    }
}

/// Puts a server's question to the person and waits for the answer, for
/// `ANSWER_WAIT` at most and until the turn is interrupted; the result for
/// the server. A form is asked as its questions; a URL (http or https only)
/// is shown for the person to visit, and accepting it says nothing more.
async fn ask_person(
    store: &Store,
    server: &str,
    asking: &Asking<'_>,
    params: Value,
    mut cancel: tokio::sync::watch::Receiver<bool>,
) -> Value {
    let cancelled = json!({"action": "cancel"});
    let mode = params.get("mode").and_then(Value::as_str).unwrap_or("form");
    let message = params.get("message").and_then(Value::as_str).unwrap_or("");
    let mut question = match mode {
        "form" => {
            let schema = params
                .get("requestedSchema")
                .cloned()
                .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
            json!({
                "server": server,
                "mode": "form",
                "message": message,
                "requestedSchema": schema,
                "questions": questions_of(&schema),
            })
        }
        "url" => {
            let url = params
                .get("url")
                .and_then(Value::as_str)
                .and_then(|url| reqwest::Url::parse(url).ok())
                .filter(|url| matches!(url.scheme(), "http" | "https"));
            let Some(url) = url else {
                return json!({"action": "decline"});
            };
            json!({"server": server, "mode": "url", "message": message, "url": url.as_str()})
        }
        _ => return json!({"action": "decline"}),
    };
    let (id, answer) = store.wait_elicitation(asking.session);
    question["id"] = json!(id);
    store.describe_elicitation(id, question.clone());
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
    match mode {
        "url" if result.get("action").and_then(Value::as_str) == Some("accept") => {
            json!({"action": "accept"})
        }
        _ => result,
    }
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

/// `mcp.json`; empty when it is missing or not valid.
fn configured(config: &Path) -> Config {
    let text = match std::fs::read_to_string(config) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Config::default(),
        Err(error) => {
            eprintln!("MCP: cannot read {}: {error}", config.display());
            return Config::default();
        }
    };
    serde_json::from_str::<Config>(&text).unwrap_or_else(|error| {
        eprintln!("MCP: {} is not valid: {error}", config.display());
        Config::default()
    })
}

/// How a server is reached: as `mcp.json` says, or as an account's
/// connector.
enum Source {
    Configured(ServerConfig),
    Connector(crate::connectors::Server, PathBuf),
}

impl Source {
    /// How long the server may take to start and list its tools.
    fn start_timeout(&self) -> Result<Duration, String> {
        match self {
            Self::Configured(ServerConfig {
                startup_timeout_sec: Some(seconds),
                ..
            }) => Duration::try_from_secs_f64(*seconds)
                .ok()
                .filter(|timeout| !timeout.is_zero())
                .ok_or_else(|| {
                    format!(
                        "startup_timeout_sec must be a number of seconds above 0, not {seconds}"
                    )
                }),
            _ => Ok(START_TIMEOUT),
        }
    }

    /// The level from which the server's log messages are asked for; none
    /// unless the settings name one.
    fn log_level(&self) -> Result<Option<String>, String> {
        match self {
            Self::Configured(ServerConfig {
                log_level: Some(level),
                ..
            }) => match LOG_LEVELS.contains(&level.as_str()) {
                true => Ok(Some(level.clone())),
                false => Err(format!(
                    "log_level must be one of {}, not {level:?}",
                    LOG_LEVELS.join(", ")
                )),
            },
            _ => Ok(None),
        }
    }

    fn connect(&self, server: &str) -> Result<Connection, String> {
        match self {
            Self::Configured(settings) => connect(server, settings),
            Self::Connector(connector, store) => {
                let shared = Shared::new(server);
                let headers = connector.headers.iter().cloned().collect();
                let http = http::Http::new(
                    &connector.url,
                    &headers,
                    Some((connector.account, store.clone())),
                    shared.clone(),
                )?;
                Ok(Connection::with(Transport::Http(Arc::new(http)), shared))
            }
        }
    }
}

/// The servers of `config`, of the plugins installed under `plugins` and the
/// connectors of the accounts signed in at `accounts`, started.
/// The servers `mcp.json` names and those of the installed plugins, with
/// how each is reached, and the tools of the servers already started.
/// Nothing is started: servers start at a turn's first use.
pub(crate) async fn listed(store: &Store) -> Value {
    let mut config = configured(&store.files_folder().with_file_name("mcp.json"));
    let shown = |name: String, settings: &ServerConfig, plugin: Option<&str>| {
        json!({
            "name": name,
            "command": settings.command,
            "args": settings.args,
            "url": settings.url,
            "plugin": plugin,
        })
    };
    let mut servers: Vec<Value> = std::mem::take(&mut config.servers)
        .into_iter()
        .map(|(name, settings)| shown(name, &settings, None))
        .collect();
    let installed = crate::plugins::Installed::all(&crate::plugins::folder(store));
    for plugin in &installed {
        for server in plugin.mcp_servers(&config.plugin_variables, ambiguous(&installed, plugin)) {
            if let Ok(settings) = serde_json::from_value::<ServerConfig>(server.settings) {
                servers.push(shown(server.name, &settings, Some(plugin.name())));
            }
        }
    }
    let kept = store.mcp().lock().await;
    let tools: Option<Vec<String>> = kept
        .as_ref()
        .map(|tools| tools.specs.iter().map(|spec| spec.name.clone()).collect());
    let problems: Option<Vec<String>> = kept.as_ref().map(|tools| tools.problems.clone());
    drop(kept);
    let connectors: Value = config
        .connectors
        .iter()
        .map(|(name, setting)| (name.clone(), json!(setting.enabled)))
        .collect::<serde_json::Map<_, _>>()
        .into();
    json!({
        "servers": servers,
        "started": tools.is_some(),
        "tools": tools,
        "problems": problems,
        "connectors": connectors,
    })
}

async fn load(
    config: &Path,
    accounts: &Path,
    plugins: &Path,
    hosts: &crate::connectors::Hosts,
) -> Tools {
    let mut tools = Tools::default();
    let mut config = configured(config);
    let mut sources: Vec<(String, Source)> = std::mem::take(&mut config.servers)
        .into_iter()
        .map(|(name, settings)| (name, Source::Configured(settings)))
        .collect();
    let installed = crate::plugins::Installed::all(plugins);
    for plugin in &installed {
        let qualified = ambiguous(&installed, plugin);
        for server in plugin.mcp_servers(&config.plugin_variables, qualified) {
            let name = server.name;
            if !server.unfilled.is_empty() {
                tools.problems.push(format!(
                    "server {name}: {} left empty (only variables listed in pluginVariables \
                     of mcp.json are filled, and only when they are set)",
                    server
                        .unfilled
                        .iter()
                        .map(|variable| format!("${{{variable}}}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            match serde_json::from_value::<ServerConfig>(server.settings) {
                Ok(mut settings) => {
                    settings.folder = Some(plugin.root().to_path_buf());
                    sources.push((name, Source::Configured(settings)));
                }
                Err(error) => tools
                    .problems
                    .push(format!("server {name} is not valid: {error}")),
            }
        }
    }
    for server in crate::connectors::servers(accounts, hosts).await {
        let short = server
            .name
            .strip_prefix("claude.ai ")
            .unwrap_or(&server.name);
        if server.account == crate::connectors::Account::Claude
            && !config.uses(&[&server.name, short])
        {
            continue;
        }
        sources.push((
            server.name.clone(),
            Source::Connector(server, accounts.to_path_buf()),
        ));
    }
    // All servers start at once, so a slow one holds the first request up
    // only as long as itself; their tools are added in the order above.
    let started =
        futures_util::future::join_all(sources.into_iter().map(|(server, source)| async {
            let started = start(&server, &source, None).await;
            (server, source, started)
        }))
        .await;
    for (server, source, started) in started {
        let connector = matches!(source, Source::Connector(..));
        match started {
            Ok((connection, listing)) => tools.servers.push(Served {
                name: server,
                slot: Slot::new(connection, Some(source), listing),
                connector,
            }),
            Err(error) => {
                eprintln!("MCP: server {server} is not available: {error}");
                tools
                    .problems
                    .push(format!("server {server} did not start: {error}"));
            }
        }
    }
    tools.started = std::mem::take(&mut tools.problems);
    tools.config = config;
    tools.build();
    tools
}

/// Starts `server`, finds the era it speaks (`known` when it ran before)
/// and lists its tools, within its start timeout. A stdio server that
/// stops at the probe is started again and spoken to with `initialize`:
/// some older servers end at any request before it.
async fn start(
    server: &str,
    source: &Source,
    known: Option<Era>,
) -> Result<(Arc<Connection>, Listing), String> {
    let timeout = source.start_timeout()?;
    let log_level = source.log_level()?;
    tokio::time::timeout(timeout, async {
        let mut known = known;
        loop {
            let mut connection = source.connect(server)?;
            connection.known = known.clone();
            connection.log_level = log_level.clone();
            let connection = Arc::new(connection);
            match connection.open().await {
                Err(Fault::Stopped) if known.is_none() && connection.is_pipe() => {
                    known = Some(Era::Legacy(String::new()));
                    continue;
                }
                Err(fault) => return Err(fault.to_string()),
                Ok(()) => {}
            }
            let listing = connection
                .list_tools()
                .await
                .map_err(|fault| fault.to_string())?;
            return Ok((connection, listing));
        }
    })
    .await
    .unwrap_or_else(|_| Err(format!("did not start within {timeout:?}")))
}
/// Whether another installed plugin has `plugin`'s name, so that its
/// servers' names carry its catalog.
fn ambiguous(installed: &[crate::plugins::Installed], plugin: &crate::plugins::Installed) -> bool {
    installed
        .iter()
        .filter(|other| other.name() == plugin.name())
        .count()
        > 1
}

/// Whether the person added this tool's app themselves. ChatGPT's apps list
/// also carries apps OpenAI attaches on its own (`implicit_link`,
/// `synthetic_link`); their tools' `resource_uri` is
/// `/<connector>/<link>/<tool>`, and only a link the person made starts with
/// `link_`. A tool without that path is a claude.ai connector's, which the
/// person added.
fn added_by_person(tool: &Value) -> bool {
    let Some(uri) = tool
        .pointer("/_meta/_codex_apps/resource_uri")
        .and_then(Value::as_str)
    else {
        return true;
    };
    uri.split('/')
        .nth(2)
        .is_some_and(|link| link.starts_with("link_"))
}

/// A connector's tools without the apps the person did not add and the apps
/// turned off in `config`. ChatGPT's apps are one server whose tools name
/// their app in `_meta`; a claude.ai connector is one app.
fn by_app(config: &Config, server: &str, listed: Vec<Value>) -> Vec<Value> {
    listed
        .into_iter()
        .filter(added_by_person)
        .filter(|tool| {
            let field = |key: &str| {
                tool.pointer(&format!("/_meta/{key}"))
                    .and_then(Value::as_str)
            };
            let id = field("connector_id").unwrap_or(server);
            let name = field("connector_name").unwrap_or(id);
            config.uses(&[id, name])
        })
        .collect()
}

/// Adds a server's tools under names the providers accept; a tool whose
/// name is already taken is left out, and that is told.
fn add(tools: &mut Tools, server: &str, slot: &Arc<Slot>, listed: Vec<Value>) {
    for tool in listed {
        let Some(original) = tool.get("name").and_then(Value::as_str) else {
            continue;
        };
        let name = tool_name(server, original);
        if tools.routes.contains_key(&name) {
            tools.problems.push(format!(
                "tool {original} of server {server} is left out: the name {name} is taken"
            ));
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
            .insert(name, (slot.clone(), original.to_string()));
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

fn connect(server: &str, settings: &ServerConfig) -> Result<Connection, String> {
    match (&settings.command, &settings.url) {
        (Some(command), None) => spawn(server, command, settings),
        (None, Some(url)) => {
            let shared = Shared::new(server);
            let http = http::Http::new(url, &settings.headers, None, shared.clone())?;
            Ok(Connection::with(Transport::Http(Arc::new(http)), shared))
        }
        _ => Err("give either command or url".into()),
    }
}

fn spawn(server: &str, command: &str, settings: &ServerConfig) -> Result<Connection, String> {
    let mut command_line = tokio::process::Command::new(command);
    if let Some(folder) = &settings.folder {
        command_line.current_dir(folder);
    }
    let mut child = command_line
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
    let mut connection = Connection::new(server, stdout, stdin);
    if let Transport::Pipe(pipe) = &mut connection.transport {
        pipe.child = Some(Box::new(child));
    }
    Ok(connection)
}
