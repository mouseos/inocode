//! The MCP client: the servers listed in `mcp.json` beside the thread store
//! are started over stdio or reached over HTTP, and their tools are given to
//! the model as `mcp__<server>__<tool>`. Servers of the stateless revision
//! (2026-07-28) and of the revisions with an `initialize` handshake are both
//! spoken to. Design and sources: docs/evidence/mcp-client.md.

mod auth;
mod connection;
mod features;
mod http;
#[cfg(test)]
mod tests;

#[cfg(test)]
pub(crate) use features::PromptArgument;
pub(crate) use features::{
    get_prompt, prompt_commands, resource_tool, PromptCommand, LIST_RESOURCES, LIST_TEMPLATES,
    READ_RESOURCE,
};
pub use features::{request, subscribe, ResourceUpdates};

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
    NO_PERSON, SAMPLING_REFUSED, STOPPED, URL_ELICITATION_REQUIRED,
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
    /// How an HTTP server is signed in to with OAuth, when it asks for it.
    #[serde(default)]
    oauth: Option<Box<auth::Settings>>,
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
    /// The sign-in store an HTTP server's OAuth sign-in is kept in.
    #[serde(skip)]
    sign_ins: Option<PathBuf>,
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
    /// The server's prompts, resources and resource templates as last
    /// listed, by the method that lists them.
    kept: Mutex<HashMap<&'static str, Kept>>,
}

/// A list kept from an earlier request: while the time the server said it
/// stays fresh is not over, or, when it said none, while the server says it
/// tells of changes; and only until it tells of one.
struct Kept {
    items: Vec<Value>,
    ttl: Option<Duration>,
    at: Instant,
    /// The count of the server's words of change when it was listed.
    changes: u64,
}

impl Slot {
    fn new(connection: Arc<Connection>, source: Option<Source>, listing: Listing) -> Arc<Self> {
        Arc::new(Self {
            server: connection.shared.server.clone(),
            connection: Mutex::new(connection),
            source,
            restarted: AtomicBool::new(false),
            listing: Mutex::new((listing, Instant::now())),
            kept: Mutex::new(HashMap::new()),
        })
    }

    /// Every item of the list `method` (`prompts/list`,
    /// `resources/list` or `resources/templates/list`), under `key` in
    /// its pages; listed again unless a kept list is still fresh.
    async fn list(&self, method: &'static str, key: &str) -> Result<Vec<Value>, Fault> {
        let connection = self.current();
        let (counter, pointer) = match method {
            "prompts/list" => (&connection.shared.prompts_changed, "/prompts/listChanged"),
            _ => (
                &connection.shared.resources_changed,
                "/resources/listChanged",
            ),
        };
        let changes = counter.load(Ordering::SeqCst);
        if let Some(kept) = self.kept.lock().expect("kept lock").get(method) {
            let fresh = match kept.ttl {
                Some(ttl) => kept.at.elapsed() < ttl,
                None => connection.offers(pointer),
            };
            if fresh && kept.changes == changes {
                return Ok(kept.items.clone());
            }
        }
        let (items, ttl) = connection.list_all(method, key).await?;
        self.kept.lock().expect("kept lock").insert(
            method,
            Kept {
                items: items.clone(),
                ttl,
                at: Instant::now(),
                changes,
            },
        );
        Ok(items)
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
        self.kept.lock().expect("kept lock").clear();
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

    /// The server named `server` that started.
    fn served(&self, server: &str) -> Option<&Served> {
        self.servers.iter().find(|served| served.name == server)
    }

    /// The tools of the servers, and those that read their resources when
    /// a server offers resources; these are never left to the search.
    fn offered(&self, messages: &[crate::conversation::Message]) -> Vec<ToolSpec> {
        let mut offered = self.tools_offered(messages);
        if self
            .servers
            .iter()
            .any(|served| served.slot.current().offers("/resources"))
        {
            offered.extend(features::resource_specs());
        }
        offered
    }

    fn tools_offered(&self, messages: &[crate::conversation::Message]) -> Vec<ToolSpec> {
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
/// the turn's events, for its thread; with the thread's project folder,
/// given to the server as the roots, and the model that answers the
/// server's requests to run it.
pub(crate) struct Asking<'a> {
    pub session: crate::store::SessionId,
    pub events: &'a mpsc::Sender<crate::turn::TurnEvent>,
    pub folder: Option<&'a Path>,
    pub sampling: Option<Sampling<'a>>,
}

/// The thread's current model, and the turn's usage its requests count in.
#[derive(Clone, Copy)]
pub(crate) struct Sampling<'a> {
    pub model: &'a crate::Llm,
    pub usage: &'a Mutex<crate::turn::TurnUsage>,
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
        method: "tools/call",
        name: tool,
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

/// One request on a connection that may need more input or become a task:
/// a call of a tool, the reading of a resource, the getting of a prompt or
/// a completion.
#[derive(Clone, Copy)]
struct Call<'a> {
    store: &'a Store,
    connection: &'a Arc<Connection>,
    method: &'a str,
    /// The tool or prompt name, or the resource URI, the request is about.
    name: &'a str,
    asking: Option<&'a Asking<'a>>,
}

impl Call<'_> {
    /// Calls the tool with `arguments`, its result as text for the model.
    async fn run(
        &self,
        arguments: &Value,
        cancel: &tokio::sync::watch::Receiver<bool>,
    ) -> Result<String, Fault> {
        let params = json!({"name": self.name, "arguments": arguments});
        let result = self.result(params, cancel).await?;
        render(&result).map_err(Fault::Other)
    }

    /// What the request is about, as the person's events name it.
    fn about(&self) -> serde_json::Map<String, Value> {
        let mut about = serde_json::Map::new();
        about.insert("server".into(), json!(self.connection.shared.server));
        match self.method {
            "tools/call" => about.insert("tool".into(), json!(self.name)),
            method => {
                about.insert("method".into(), json!(method));
                about.insert("name".into(), json!(self.name))
            }
        };
        about
    }

    /// Sends the request with `params`: input the server asks for is
    /// gathered and the request sent again with it; a task the server makes
    /// of it is polled until it ends; it stops when the turn is
    /// interrupted, and the server is told so. The complete result.
    async fn result(
        &self,
        base: Value,
        cancel: &tokio::sync::watch::Receiver<bool>,
    ) -> Result<Value, Fault> {
        let connection = self.connection;
        let (sender, mut notices) = mpsc::unbounded_channel();
        let token = self.asking.map(|_| progress_token());
        let _waiting = Waiting::register(
            &connection.shared,
            self.asking.map(|asking| asking.session),
            self.asking.map(|_| sender),
            token.clone(),
        );
        if let Some(folder) = self.asking.and_then(|asking| asking.folder) {
            connection.use_folder(folder).await;
        }
        let tool = self.method == "tools/call";
        let arguments = base.get("arguments").cloned().unwrap_or(Value::Null);
        let mut cancel = cancel.clone();
        let mut retry = serde_json::Map::new();
        let (mut rounds, mut sent_again, mut listed_again) = (0, false, false);
        loop {
            let mut params = base.clone();
            if let Some(token) = &token {
                params["_meta"] = json!({"progressToken": token});
            }
            for (key, value) in std::mem::take(&mut retry) {
                params[key] = value;
            }
            let id = connection.next_id();
            let headers = match tool {
                true => connection.call_headers(self.name, &arguments),
                false => reqwest::header::HeaderMap::new(),
            };
            let request = connection.exchange(id, self.method, params, &headers);
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
                    if tool
                        && !listed_again
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
                None | Some("complete") => return Ok(result),
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
                    Notice::Sampling(sampled) => {
                        let answer = self.sample(&sampled.params, &interrupted).await;
                        let _ = sampled.reply.send(answer);
                    }
                    Notice::Progress(params) => {
                        let mut shown = self.about();
                        for key in ["progress", "total", "message"] {
                            shown.insert(key.into(), params.get(key).cloned().unwrap_or(Value::Null));
                        }
                        self.show(crate::turn::TurnEvent::McpProgress(Value::Object(shown)))
                    }
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
    /// nobody can be asked. A URL question with an `elicitationId` is
    /// answered `accept` for the person when the server says first that
    /// what the page was for is complete.
    async fn question(&self, params: Value, cancel: &tokio::sync::watch::Receiver<bool>) -> Value {
        let Some(asking) = self.asking else {
            return json!({"action": "cancel"});
        };
        let shared = &self.connection.shared;
        let id = params
            .get("elicitationId")
            .and_then(Value::as_str)
            .filter(|_| params.get("mode").and_then(Value::as_str) == Some("url"))
            .map(str::to_string);
        let completed = id.as_deref().map(|id| shared.await_completion(id));
        let answer = ask_person(
            self.store,
            &shared.server,
            asking,
            params,
            cancel.clone(),
            completed,
        )
        .await;
        if let Some(id) = &id {
            shared.forget_completion(id);
        }
        answer
    }

    /// The answers to a server's input requests, keyed as asked: questions
    /// go to the person, the roots are the thread's project folder, and a
    /// request to run the model is put to the person first. A request this
    /// client did not declare, or one refused, fails the call: an input
    /// response has no form for an error.
    async fn inputs(
        &self,
        requests: &serde_json::Map<String, Value>,
        cancel: &tokio::sync::watch::Receiver<bool>,
    ) -> Result<serde_json::Map<String, Value>, Fault> {
        for request in requests.values() {
            let method = request.get("method").and_then(Value::as_str).unwrap_or("");
            if !matches!(
                method,
                "elicitation/create" | "roots/list" | "sampling/createMessage"
            ) {
                return Err(Fault::Other(format!(
                    "the MCP server asked for {method}, which inocode does not offer (it did \
                     not declare that capability)"
                )));
            }
        }
        let mut responses = serde_json::Map::new();
        for (key, request) in requests {
            let params = request.get("params").cloned().unwrap_or(Value::Null);
            let response = match request.get("method").and_then(Value::as_str) {
                Some("roots/list") => {
                    connection::roots(self.asking.and_then(|asking| asking.folder))
                }
                Some("sampling/createMessage") => {
                    self.sample(&params, cancel).await.map_err(|(_, message)| {
                        Fault::Other(format!(
                            "the MCP server's request to run the model was not answered: \
                             {message}"
                        ))
                    })?
                }
                _ => self.question(params, cancel).await,
            };
            responses.insert(key.clone(), response);
        }
        Ok(responses)
    }

    /// Runs the thread's model for the server's `sampling/createMessage`,
    /// once the person approved it; the result, or the JSON-RPC error
    /// (code and message) of an older server. The model may call the
    /// request's own `tools` as its `toolChoice` says, and the calls are
    /// returned for the server to run; `includeContext` other than `none` is
    /// deprecated and taken as none; `temperature`, `stopSequences`,
    /// `metadata` and `modelPreferences` are not used, the thread's own
    /// model answering.
    async fn sample(
        &self,
        params: &Value,
        cancel: &tokio::sync::watch::Receiver<bool>,
    ) -> Result<Value, (i64, String)> {
        let invalid = |message: &str| (-32602, message.to_string());
        let asking = self
            .asking
            .ok_or_else(|| (SAMPLING_REFUSED, NO_PERSON.to_string()))?;
        let sampling = asking.sampling.ok_or_else(|| {
            (
                SAMPLING_REFUSED,
                "no model can answer here (only during a turn of a thread)".to_string(),
            )
        })?;
        let tools = sampling_tools(params.get("tools")).map_err(|e| invalid(&e))?;
        let choice = match params.get("toolChoice").map(|choice| choice.get("mode")) {
            None | Some(None) => None,
            Some(Some(mode)) => Some(match mode.as_str() {
                Some("auto") => crate::llm::ToolChoice::Auto,
                Some("required") => crate::llm::ToolChoice::Required,
                Some("none") => crate::llm::ToolChoice::None,
                _ => return Err(invalid("toolChoice.mode must be auto, required or none")),
            }),
        };
        if tools.is_empty() && choice == Some(crate::llm::ToolChoice::Required) {
            return Err(invalid("toolChoice required needs tools"));
        }
        let choice = choice.filter(|_| !tools.is_empty());
        let max_tokens = params
            .get("maxTokens")
            .and_then(Value::as_u64)
            .filter(|max| *max > 0)
            .ok_or_else(|| invalid("maxTokens must be a number above 0"))?;
        let model = sampling.model;
        let messages = sampling_messages(params.get("messages"), model).map_err(|e| invalid(&e))?;
        let system = params
            .get("systemPrompt")
            .and_then(Value::as_str)
            .map(str::to_string);
        let server = &self.connection.shared.server;
        let question = json!({
            "server": server,
            "mode": "sampling",
            "message": format!(
                "The MCP server {server} asks to run the model {} on these messages.",
                model.model()
            ),
            "model": model.model(),
            "messages": params.get("messages"),
            "systemPrompt": system,
            "maxTokens": max_tokens,
            "tools": tools.iter().map(|tool| &tool.name).collect::<Vec<_>>(),
            "toolChoice": params.pointer("/toolChoice/mode"),
        });
        let answer = put_to_person(self.store, asking, question, cancel.clone(), None).await;
        if answer.get("action").and_then(Value::as_str) != Some("accept") {
            return Err((SAMPLING_REFUSED, "User rejected sampling request".into()));
        }
        let conversation = crate::conversation::Conversation { system, messages };
        let mut cancel = cancel.clone();
        let failed = |message: String| (-32603, message);
        let stopped = || failed("stopped with the turn".into());
        let started = tokio::select! {
            started = model.converse_capped(&conversation, &tools, choice, max_tokens) => started,
            () = crate::turn::cancelled(&mut cancel) => return Err(stopped()),
        };
        let mut stream = started.map_err(|error| failed(error.message))?;
        let ended = loop {
            tokio::select! {
                next = stream.next() => match next {
                    Ok(Some(_)) => {}
                    Ok(None) => break Ok(()),
                    Err(error) => break Err(failed(error.message)),
                },
                () = crate::turn::cancelled(&mut cancel) => break Err(stopped()),
            }
        };
        sampling
            .usage
            .lock()
            .expect("usage lock")
            .add_stream(&stream, "mcp_sampling");
        ended?;
        let crate::conversation::Body::Assistant { stop, parts, .. } = stream.assistant() else {
            unreachable!("a stream assembles an assistant turn");
        };
        let mut text = String::new();
        let mut calls = Vec::new();
        for part in parts {
            match part {
                crate::conversation::AssistantPart::Text { text: part } => text.push_str(&part),
                crate::conversation::AssistantPart::ToolCall {
                    call_id,
                    name,
                    arguments,
                    ..
                } => calls.push(json!({
                    "type": "tool_use",
                    "id": call_id,
                    "name": name,
                    "input": serde_json::from_str::<Value>(&arguments)
                        .ok()
                        .filter(Value::is_object)
                        .unwrap_or_else(|| json!({})),
                })),
                crate::conversation::AssistantPart::Reasoning { .. } => {}
            }
        }
        // A provider that does not hold to `tool_choice` is caught here.
        match choice {
            Some(crate::llm::ToolChoice::None) if !calls.is_empty() => {
                return Err(failed(
                    "the model called a tool though toolChoice was none".into(),
                ))
            }
            Some(crate::llm::ToolChoice::Required) if calls.is_empty() => {
                return Err(failed(
                    "the model answered without calling a tool though toolChoice was required"
                        .into(),
                ))
            }
            _ => {}
        }
        let stop = match stop {
            _ if !calls.is_empty() => "toolUse",
            crate::conversation::StopReason::Length => "maxTokens",
            _ => "endTurn",
        };
        let content = match calls.is_empty() {
            true => json!({"type": "text", "text": text}),
            false => {
                let mut blocks = Vec::new();
                if !text.is_empty() {
                    blocks.push(json!({"type": "text", "text": text}));
                }
                blocks.extend(calls);
                match blocks.len() {
                    1 => blocks.remove(0),
                    _ => Value::Array(blocks),
                }
            }
        };
        Ok(json!({
            "role": "assistant",
            "content": content,
            "model": model.model(),
            "stopReason": stop,
        }))
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
    ) -> Result<Value, Fault> {
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
                let mut about = self.about();
                about.insert(
                    "status".into(),
                    task.get("status").cloned().unwrap_or(Value::Null),
                );
                about.insert("message".into(), json!(message));
                self.show(crate::turn::TurnEvent::McpProgress(Value::Object(about)));
                shown = message.clone();
            }
            match task.get("status").and_then(Value::as_str) {
                Some("completed") => return Ok(task.get("result").cloned().unwrap_or(Value::Null)),
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
    async fn cancel_task(&self, id: &str) -> Result<Value, Fault> {
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
/// is shown for the person to visit, and accepting it says nothing more;
/// `completed` brings the server's word that the visit is done.
async fn ask_person(
    store: &Store,
    server: &str,
    asking: &Asking<'_>,
    params: Value,
    cancel: tokio::sync::watch::Receiver<bool>,
    completed: Option<tokio::sync::oneshot::Receiver<()>>,
) -> Value {
    let mode = params.get("mode").and_then(Value::as_str).unwrap_or("form");
    let message = params.get("message").and_then(Value::as_str).unwrap_or("");
    let question = match mode {
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
            let mut question =
                json!({"server": server, "mode": "url", "message": message, "url": url.as_str()});
            if let Some(id) = params.get("elicitationId").and_then(Value::as_str) {
                question["elicitationId"] = json!(id);
            }
            question
        }
        _ => return json!({"action": "decline"}),
    };
    let result = put_to_person(store, asking, question, cancel, completed).await;
    match mode {
        "url" if result.get("action").and_then(Value::as_str) == Some("accept") => {
            json!({"action": "accept"})
        }
        _ => result,
    }
}

/// Shows `question` to the person as the turn's `elicitation` event and
/// waits for the answer given with `turn/answer`: for `ANSWER_WAIT` at
/// most and until the turn is interrupted (then `cancel`), or until
/// `completed` says the question is settled (then `accept`). Its
/// settling is reported as the turn's `Answered` event.
async fn put_to_person(
    store: &Store,
    asking: &Asking<'_>,
    mut question: Value,
    mut cancel: tokio::sync::watch::Receiver<bool>,
    completed: Option<tokio::sync::oneshot::Receiver<()>>,
) -> Value {
    let cancelled = json!({"action": "cancel"});
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
    let completed = async {
        let done = match completed {
            Some(completed) => completed.await.is_ok(),
            None => false,
        };
        if !done {
            std::future::pending::<()>().await;
        }
    };
    let result = tokio::select! {
        answer = answer => answer.unwrap_or_else(|_| cancelled.clone()),
        () = completed => json!({"action": "accept"}),
        () = tokio::time::sleep(ANSWER_WAIT) => cancelled.clone(),
        () = crate::turn::cancelled(&mut cancel) => cancelled.clone(),
    };
    store.forget_elicitation(id);
    let _ = asking
        .events
        .send(crate::turn::TurnEvent::Answered(id))
        .await;
    result
}

/// The `tools` of a `sampling/createMessage`, as the model is offered them.
fn sampling_tools(tools: Option<&Value>) -> Result<Vec<ToolSpec>, String> {
    let Some(tools) = tools.filter(|tools| !tools.is_null()) else {
        return Ok(Vec::new());
    };
    let tools = tools.as_array().ok_or("tools must be a list")?;
    tools
        .iter()
        .map(|tool| {
            let name = tool
                .get("name")
                .and_then(Value::as_str)
                .ok_or("each tool needs a name")?;
            Ok(ToolSpec {
                name: name.to_string(),
                description: tool
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                parameters: tool
                    .get("inputSchema")
                    .filter(|schema| schema.is_object())
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object"})),
            })
        })
        .collect()
}

/// The messages of a `sampling/createMessage` as a conversation for
/// `model`: text as it is; images and audio, which a person's message here
/// cannot carry, named with their type and size; the model's tool uses as
/// its calls, and a user message of tool results as their answers. Every
/// tool use must be answered by the next message, which holds only
/// results.
fn sampling_messages(
    messages: Option<&Value>,
    model: &crate::Llm,
) -> Result<Vec<crate::conversation::Message>, String> {
    use crate::conversation::{AssistantPart, Body, Message, Speaker, StopReason};
    let messages = messages
        .and_then(Value::as_array)
        .filter(|messages| !messages.is_empty())
        .ok_or("messages must be a list of at least one message")?;
    let mut conversation = Vec::new();
    // The tool uses the next message must answer, by id, with their names.
    let mut open: Vec<(String, String)> = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        let blocks = match message.get("content") {
            Some(Value::Array(blocks)) => blocks.clone(),
            Some(block) => vec![block.clone()],
            None => Vec::new(),
        };
        let kind_of = |block: &Value| {
            block
                .get("type")
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        let results = blocks
            .iter()
            .filter(|block| kind_of(block).as_deref() == Some("tool_result"))
            .count();
        let role = message.get("role").and_then(Value::as_str);
        if results > 0 && (results != blocks.len() || role != Some("user")) {
            return Err("tool results must make up a user message on their own".into());
        }
        if !open.is_empty() && results == 0 {
            return Err("every tool use must be answered by the next message".into());
        }
        if results > 0 {
            for (at, block) in blocks.iter().enumerate() {
                let id = block
                    .get("toolUseId")
                    .and_then(Value::as_str)
                    .ok_or("a tool result needs its toolUseId")?;
                let Some(found) = open.iter().position(|(open, _)| open == id) else {
                    return Err(format!("the tool result for {id} answers no tool use"));
                };
                let (_, name) = open.remove(found);
                conversation.push(Message {
                    id: format!("sampling-{index}-{at}"),
                    body: Body::ToolResult {
                        call_id: id.to_string(),
                        name,
                        content: result_text(block),
                        is_error: block.get("isError").and_then(Value::as_bool) == Some(true),
                        images: Vec::new(),
                    },
                });
            }
            if !open.is_empty() {
                return Err("every tool use must be answered by the next message".into());
            }
            continue;
        }
        let mut texts = Vec::new();
        let mut calls = Vec::new();
        for block in &blocks {
            let field = |key: &str| block.get(key).and_then(Value::as_str).unwrap_or("");
            match field("type") {
                "text" => texts.push(field("text").to_string()),
                kind @ ("image" | "audio") => texts.push(format!(
                    "({kind} of type {}, {} bytes, not shown)",
                    field("mimeType"),
                    decoded_len(field("data"))
                )),
                "tool_use" if role == Some("assistant") => {
                    let (id, name) = (field("id"), field("name"));
                    if id.is_empty() || name.is_empty() {
                        return Err("a tool use needs its id and name".into());
                    }
                    open.push((id.to_string(), name.to_string()));
                    calls.push(AssistantPart::ToolCall {
                        call_id: id.to_string(),
                        name: name.to_string(),
                        arguments: block
                            .get("input")
                            .cloned()
                            .unwrap_or_else(|| json!({}))
                            .to_string(),
                        replay: None,
                    });
                }
                kind => return Err(format!("content of type {kind:?} is not taken in sampling")),
            }
        }
        let body = match role {
            Some("user") => Body::User {
                from: Speaker::Person,
                parts: texts,
            },
            Some("assistant") => {
                let mut parts = Vec::new();
                if !texts.is_empty() {
                    parts.push(AssistantPart::Text {
                        text: texts.join("\n"),
                    });
                }
                let stop = match calls.is_empty() {
                    true => StopReason::Completed,
                    false => StopReason::ToolUse,
                };
                parts.extend(calls);
                Body::Assistant {
                    origin: model.origin(),
                    stop,
                    parts,
                }
            }
            _ => return Err("each message's role must be user or assistant".into()),
        };
        conversation.push(Message {
            id: format!("sampling-{index}"),
            body,
        });
    }
    if !open.is_empty() {
        return Err("every tool use must be answered by the next message".into());
    }
    Ok(conversation)
}

/// A tool result of a sampling message as text for the model: its text
/// blocks, other blocks named, or its structured content.
fn result_text(block: &Value) -> String {
    let parts: Vec<String> = block
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|item| {
            let field = |key: &str| item.get(key).and_then(Value::as_str).unwrap_or("");
            match field("type") {
                "text" => field("text").to_string(),
                kind => format!("(content of type {kind} not shown)"),
            }
        })
        .collect();
    match (parts.is_empty(), block.get("structuredContent")) {
        (true, Some(structured)) => structured.to_string(),
        _ => parts.join("\n"),
    }
}

/// How many bytes the Base64 text `data` holds.
fn decoded_len(data: &str) -> usize {
    let data = data.trim_end_matches('=');
    data.len() * 3 / 4
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

/// The servers `mcp.json` names and those of the installed plugins, with
/// the plugin each comes from.
fn configured_servers(store: &Store) -> (Config, Vec<(String, ServerConfig, Option<String>)>) {
    let mut config = configured(&store.files_folder().with_file_name("mcp.json"));
    let mut servers: Vec<(String, ServerConfig, Option<String>)> =
        std::mem::take(&mut config.servers)
            .into_iter()
            .map(|(name, settings)| (name, settings, None))
            .collect();
    let installed = crate::plugins::Installed::all(&crate::plugins::folder(store));
    for plugin in &installed {
        for server in plugin.mcp_servers(&config.plugin_variables, ambiguous(&installed, plugin)) {
            if let Ok(settings) = serde_json::from_value::<ServerConfig>(server.settings) {
                servers.push((server.name, settings, Some(plugin.name().to_string())));
            }
        }
    }
    (config, servers)
}

/// The servers `mcp.json` names and those of the installed plugins, with
/// how each is reached and, for one reached over HTTP, its sign-in state;
/// and the tools of the servers already started. Nothing is started:
/// servers start at a turn's first use.
pub(crate) async fn listed(store: &Store) -> Value {
    let (config, configured) = configured_servers(store);
    let kept = store.mcp().lock().await;
    let tools: Option<Vec<String>> = kept
        .as_ref()
        .map(|tools| tools.specs.iter().map(|spec| spec.name.clone()).collect());
    let problems: Option<Vec<String>> = kept.as_ref().map(|tools| tools.problems.clone());
    let reached: HashSet<String> = kept
        .as_ref()
        .map(|tools| {
            tools
                .servers
                .iter()
                .map(|served| served.name.clone())
                .collect()
        })
        .unwrap_or_default();
    drop(kept);
    let sign_ins = crate::connectors::store_beside(store);
    let servers: Vec<Value> = configured
        .into_iter()
        .map(|(name, settings, plugin)| {
            let sign_in = settings.url.as_deref().map(|url| {
                match settings
                    .headers
                    .keys()
                    .any(|name| name.eq_ignore_ascii_case("authorization"))
                {
                    true => "headers",
                    false => auth::status(&sign_ins, url, reached.contains(&name)),
                }
            });
            json!({
                "name": name,
                "command": settings.command,
                "args": settings.args,
                "url": settings.url,
                "plugin": plugin,
                "sign_in": sign_in,
            })
        })
        .collect();
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

/// `mcp/login/start {server}`, `mcp/login/status {login_id}`,
/// `mcp/login/cancel {login_id}` and `mcp/logout {server}`: the person's
/// OAuth sign-ins to the HTTP servers; `None` for other methods.
pub(crate) async fn sign_in_request(
    store: &Store,
    method: &str,
    params: &Value,
) -> Option<Result<Value, crate::ErrorInfo>> {
    use crate::ErrorInfo;
    let text = |key: &str| {
        params
            .get(key)
            .and_then(Value::as_str)
            .ok_or_else(|| ErrorInfo::new("invalid_params", format!("requires a \"{key}\" string")))
    };
    let failed = |message: String| ErrorInfo::new("mcp_error", message);
    let target = |server: &str| -> Result<auth::Target, ErrorInfo> {
        let (_, servers) = configured_servers(store);
        let (name, settings, _) = servers
            .into_iter()
            .find(|(name, _, _)| name == server)
            .ok_or_else(|| {
                ErrorInfo::new("invalid_params", format!("no MCP server is named {server}"))
            })?;
        let url = settings.url.ok_or_else(|| {
            ErrorInfo::new(
                "invalid_params",
                format!("server {server} is not reached over HTTP; it takes no sign-in"),
            )
        })?;
        Ok(auth::Target {
            server: name,
            url,
            headers: settings.headers,
            settings: settings.oauth.map(|oauth| *oauth).unwrap_or_default(),
        })
    };
    let sign_ins = crate::connectors::store_beside(store);
    Some(match method {
        "mcp/login/start" => {
            let target = match text("server").and_then(target) {
                Ok(target) => target,
                Err(error) => return Some(Err(error)),
            };
            let store = store.clone();
            // The servers start anew, with the new sign-in.
            auth::login_start(&sign_ins, target, async move { store.forget_mcp().await })
                .await
                .map_err(failed)
        }
        "mcp/login/status" => {
            text("login_id").and_then(|id| auth::login_status(id).map_err(failed))
        }
        "mcp/login/cancel" => {
            text("login_id").and_then(|id| auth::login_cancel(id).map_err(failed))
        }
        "mcp/logout" => {
            let target = match text("server").and_then(target) {
                Ok(target) => target,
                Err(error) => return Some(Err(error)),
            };
            match auth::logout(&sign_ins, &target.url) {
                Ok(removed) => {
                    store.forget_mcp().await;
                    Ok(json!({"server": target.server, "removed": removed}))
                }
                Err(error) => Err(failed(error)),
            }
        }
        _ => return None,
    })
}

/// The servers of `config`, of the plugins installed under `plugins` and the
/// connectors of the accounts signed in at `accounts`, started.
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
        .map(|(name, mut settings)| {
            settings.sign_ins = Some(accounts.to_path_buf());
            (name, Source::Configured(settings))
        })
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
                    settings.sign_ins = Some(accounts.to_path_buf());
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
            let mut http = http::Http::new(url, &settings.headers, None, shared.clone())?;
            // A server signed in to with a header of mcp.json gets no token.
            let by_header = settings
                .headers
                .keys()
                .any(|name| name.eq_ignore_ascii_case("authorization"));
            if let (Some(store), false) = (&settings.sign_ins, by_header) {
                let url = reqwest::Url::parse(url)
                    .map_err(|error| format!("invalid url {url}: {error}"))?;
                http = http.signed_by(auth::Auth::new(server, url, store.clone()));
            }
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
