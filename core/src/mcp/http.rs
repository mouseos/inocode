//! MCP over HTTP: Streamable HTTP of every revision, and the deprecated
//! HTTP+SSE transport of 2024-11-05. Design and sources:
//! docs/evidence/mcp-client.md.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;
use tokio::sync::oneshot;

use super::connection::{answer_of, answer_request, Fault, Pending, RpcError, Shared, END_TIMEOUT};
use crate::sse::SseParser;

const SESSION_HEADER: &str = "MCP-Session-Id";
const VERSION_HEADER: &str = "MCP-Protocol-Version";
/// Where a modern message names its version.
const VERSION_POINTER: &str = "/params/_meta/io.modelcontextprotocol~1protocolVersion";
/// Bytes of one JSON answer read at most, as for the providers' answers.
const MAX_HTTP_BODY: usize = 16 * 1024 * 1024;
/// Bytes of a refusal read at most, and characters of it shown.
const MAX_REFUSAL: usize = 64 * 1024;
const SHOWN_REFUSAL: usize = 1024;
/// How long a stream that broke waits before it is opened again, unless the
/// server said otherwise with `retry`; doubled up to `MAX_BACKOFF` while the
/// streams opened bring nothing.
const BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// A server reached over HTTP: every message is one POST, answered with
/// JSON or with a stream of server-sent events; or, for a server of the
/// HTTP+SSE transport, posted to the endpoint its stream named and answered
/// on that stream.
pub(super) struct Http {
    client: reqwest::Client,
    url: reqwest::Url,
    headers: HeaderMap,
    /// The account every request signs in with, and its sign-in store.
    account: Option<(crate::connectors::Account, PathBuf)>,
    /// The session a legacy server gave at initialization, sent with every
    /// later legacy request.
    session: Mutex<Option<String>>,
    /// The legacy version agreed at initialization, sent with every later
    /// legacy request.
    version: Mutex<Option<String>>,
    shared: Arc<Shared>,
    /// The HTTP+SSE transport, once the server proved to use it.
    link: Mutex<Option<Arc<Link>>>,
    /// Streams read in the background; stopped with the connection.
    tasks: Mutex<Vec<tokio::task::AbortHandle>>,
    /// The legacy GET stream, opened anew with each session.
    listener: Mutex<Option<tokio::task::AbortHandle>>,
}

/// The HTTP+SSE transport: where messages are posted, and the requests
/// whose answers its stream has yet to bring.
struct Link {
    endpoint: reqwest::Url,
    pending: Pending,
}

impl Http {
    pub(super) fn new(
        url: &str,
        headers: &HashMap<String, String>,
        account: Option<(crate::connectors::Account, PathBuf)>,
        shared: Arc<Shared>,
    ) -> Result<Self, String> {
        let url =
            reqwest::Url::parse(url).map_err(|error| format!("invalid url {url}: {error}"))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(format!("{url} is not an http or https url"));
        }
        let mut map = HeaderMap::new();
        for (name, value) in headers {
            let header = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| format!("invalid header name {name}"))?;
            let value = HeaderValue::from_str(value)
                .map_err(|_| format!("invalid value for header {name}"))?;
            map.insert(header, value);
        }
        Ok(Self {
            client: reqwest::Client::new(),
            url,
            headers: map,
            account,
            session: Mutex::new(None),
            version: Mutex::new(None),
            shared,
            link: Mutex::new(None),
            tasks: Mutex::new(Vec::new()),
            listener: Mutex::new(None),
        })
    }

    /// The headers signing a request in, when the server is an account's.
    async fn signed(&self) -> Result<HeaderMap, Fault> {
        match &self.account {
            Some((account, store)) => account.headers(store).await.map_err(Fault::Other),
            None => Ok(HeaderMap::new()),
        }
    }

    fn link(&self) -> Option<Arc<Link>> {
        self.link.lock().expect("link lock").clone()
    }

    /// Keeps the version a legacy server agreed to, for the headers of the
    /// later requests over Streamable HTTP.
    pub(super) fn agreed(&self, version: &str) {
        if self.link().is_none() {
            *self.version.lock().expect("version lock") = Some(version.to_string());
        }
    }

    /// Posts one message; the response once its status is a success. A
    /// modern message carries its version, method and name as headers too;
    /// a legacy one the session and the agreed version.
    pub(super) async fn post(
        &self,
        message: &Value,
        extra: &HeaderMap,
    ) -> Result<reqwest::Response, Fault> {
        let link = self.link();
        let target = link
            .as_ref()
            .map_or_else(|| self.url.clone(), |link| link.endpoint.clone());
        let mut request = self
            .client
            .post(target)
            .headers(self.headers.clone())
            .headers(self.signed().await?)
            .header(
                reqwest::header::ACCEPT,
                "application/json, text/event-stream",
            )
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .headers(extra.clone())
            .body(message.to_string());
        let modern = message.pointer(VERSION_POINTER).and_then(Value::as_str);
        let mut session = None;
        if let Some(version) = modern {
            let method = message.get("method").and_then(Value::as_str).unwrap_or("");
            request = request
                .header(VERSION_HEADER, version)
                .header("Mcp-Method", method);
            if let Some(name) = mcp_name(message) {
                request = request.header("Mcp-Name", header_text(name));
            }
        } else if link.is_none() {
            session = self.session.lock().expect("session lock").clone();
            if let Some(session) = &session {
                request = request.header(SESSION_HEADER, session);
            }
            if let Some(version) = self.version.lock().expect("version lock").clone() {
                request = request.header(VERSION_HEADER, version);
            }
        }
        let response = request
            .send()
            .await
            .map_err(|error| Fault::Other(format!("cannot reach the MCP server: {error}")))?;
        let status = response.status();
        if !status.is_success() {
            if status == reqwest::StatusCode::NOT_FOUND && session.is_some() {
                *self.session.lock().expect("session lock") = None;
                *self.version.lock().expect("version lock") = None;
                return Err(Fault::SessionEnded);
            }
            return Err(refusal(response).await);
        }
        if modern.is_none() && link.is_none() {
            if let Some(session) = response
                .headers()
                .get(SESSION_HEADER)
                .and_then(|value| value.to_str().ok())
            {
                *self.session.lock().expect("session lock") = Some(session.to_string());
            }
        }
        Ok(response)
    }

    pub(super) async fn request(
        &self,
        id: u64,
        message: &Value,
        extra: &HeaderMap,
    ) -> Result<Value, Fault> {
        if let Some(link) = self.link() {
            return self.request_by_link(&link, id, message, extra).await;
        }
        let mut response = self.post(message, extra).await?;
        if is_stream(&response) {
            let modern = message.pointer(VERSION_POINTER).is_some();
            return self.answer_in_events(id, response, modern).await;
        }
        let body = crate::llm::read_capped(&mut response, MAX_HTTP_BODY)
            .await
            .map_err(|error| Fault::Other(format!("cannot read the MCP server's answer: {error}")))?
            .ok_or_else(|| Fault::Other("the MCP server's answer is too large".into()))?;
        let message: Value = serde_json::from_slice(&body).map_err(|error| {
            Fault::Other(format!("the MCP server's answer is not JSON: {error}"))
        })?;
        if message.get("id").and_then(Value::as_u64) != Some(id) {
            return Err(Fault::Other(
                "the MCP server answered another request".into(),
            ));
        }
        answer_of(&message)
    }

    async fn request_by_link(
        &self,
        link: &Link,
        id: u64,
        message: &Value,
        extra: &HeaderMap,
    ) -> Result<Value, Fault> {
        let (sender, receiver) = oneshot::channel();
        match link.pending.lock().expect("pending lock").as_mut() {
            Some(pending) => pending.insert(id, sender),
            None => return Err(Fault::NotRunning),
        };
        if let Err(fault) = self.post(message, extra).await {
            if let Some(pending) = link.pending.lock().expect("pending lock").as_mut() {
                pending.remove(&id);
            }
            return Err(fault);
        }
        receiver.await.unwrap_or(Err(Fault::Stopped))
    }

    /// Handles one message of a stream: the answer to `id` is returned; the
    /// server's requests are answered with a POST each, and its
    /// notifications handled.
    async fn take(&self, message: &Value, id: Option<u64>) -> Option<Result<Value, Fault>> {
        match (
            message.get("method").and_then(Value::as_str),
            message.get("id"),
        ) {
            (Some(method), Some(request)) => {
                let params = message.get("params").cloned().unwrap_or(Value::Null);
                let answer = answer_request(&self.shared, method, request.clone(), params).await;
                let _ = self.post(&answer, &HeaderMap::new()).await;
                None
            }
            (Some(_), None) => {
                self.shared.notice(message);
                None
            }
            (None, Some(answered)) if id.is_some() && answered.as_u64() == id => {
                Some(answer_of(message))
            }
            _ => None,
        }
    }

    /// Reads the server's events until the answer to request `id`. A legacy
    /// stream that breaks first is resumed from its last event id with a GET,
    /// after the time the server asked for; a modern one cannot be.
    async fn answer_in_events(
        &self,
        id: u64,
        mut response: reqwest::Response,
        modern: bool,
    ) -> Result<Value, Fault> {
        let mut parser = SseParser::new();
        let mut last: Option<String> = None;
        let mut ended = false;
        loop {
            while let Some(item) = parser.next_item() {
                let data = item.map_err(|error| Fault::Other(error.message))?;
                let Ok(message) = serde_json::from_str::<Value>(&data) else {
                    continue;
                };
                if let Some(answer) = self.take(&message, Some(id)).await {
                    return answer;
                }
            }
            if let Some(id) = parser.last_id().filter(|id| !id.is_empty()) {
                last = Some(id.to_string());
            }
            if ended {
                if modern {
                    return Err(Fault::Broken);
                }
                let Some(resume) = &last else {
                    return Err(Fault::Other(
                        "the MCP server closed the stream before answering".into(),
                    ));
                };
                let wait = parser.retry().map_or(BACKOFF, Duration::from_millis);
                tokio::time::sleep(wait).await;
                response = self.get_stream(Some(resume)).await?;
                parser = SseParser::new();
                ended = false;
                continue;
            }
            match response.chunk().await {
                Ok(Some(chunk)) => parser.push(&chunk),
                Ok(None) => {
                    parser.end();
                    ended = true;
                }
                Err(_) if !modern && last.is_some() => {
                    parser.end();
                    ended = true;
                }
                Err(error) => {
                    return Err(match modern {
                        true => Fault::Broken,
                        false => {
                            Fault::Other(format!("cannot read the MCP server's answer: {error}"))
                        }
                    })
                }
            }
        }
    }

    /// A GET stream of a legacy session, resumed after `last` when given.
    async fn get_stream(&self, last: Option<&str>) -> Result<reqwest::Response, Fault> {
        let mut request = self
            .client
            .get(self.url.clone())
            .headers(self.headers.clone())
            .headers(self.signed().await?)
            .header(reqwest::header::ACCEPT, "text/event-stream");
        if let Some(session) = self.session.lock().expect("session lock").clone() {
            request = request.header(SESSION_HEADER, session);
        }
        if let Some(version) = self.version.lock().expect("version lock").clone() {
            request = request.header(VERSION_HEADER, version);
        }
        if let Some(last) = last {
            request = request.header("Last-Event-ID", last);
        }
        let response = request
            .send()
            .await
            .map_err(|error| Fault::Other(format!("cannot reach the MCP server: {error}")))?;
        if !response.status().is_success() {
            return Err(refusal(response).await);
        }
        if !is_stream(&response) {
            return Err(Fault::Other(
                "the MCP server's GET answer is not an event stream".into(),
            ));
        }
        Ok(response)
    }

    /// Opens the GET stream of a legacy session, where the server tells its
    /// tool list changed, and keeps it open: resumed from its last event id
    /// when it breaks, until the server refuses it (405: it offers none).
    pub(super) fn listen_legacy(self: &Arc<Self>) {
        let http = self.clone();
        let task = tokio::spawn(async move {
            let mut last: Option<String> = None;
            let mut backoff = BACKOFF;
            loop {
                let mut retry = None;
                match http.get_stream(last.as_deref()).await {
                    Ok(mut response) => {
                        let mut parser = SseParser::new();
                        let mut heard = false;
                        loop {
                            while let Some(Ok(data)) = parser.next_item() {
                                heard = true;
                                if let Ok(message) = serde_json::from_str::<Value>(&data) {
                                    http.take(&message, None).await;
                                }
                            }
                            match response.chunk().await {
                                Ok(Some(chunk)) => parser.push(&chunk),
                                _ => break,
                            }
                        }
                        if let Some(id) = parser.last_id().filter(|id| !id.is_empty()) {
                            last = Some(id.to_string());
                        }
                        retry = parser.retry();
                        if heard {
                            backoff = BACKOFF;
                        }
                    }
                    Err(Fault::Other(_)) => {}
                    Err(_) => return,
                }
                tokio::time::sleep(retry.map_or(backoff, Duration::from_millis)).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        });
        let old = self
            .listener
            .lock()
            .expect("listener lock")
            .replace(task.abort_handle());
        if let Some(old) = old {
            old.abort();
        }
    }

    /// Keeps the `subscriptions/listen` request `message` open: its stream
    /// tells the tool list changed. A stream that breaks is opened again,
    /// and the tools listed again since a change may have been missed; the
    /// server's answer ends the subscription.
    pub(super) fn listen_modern(self: &Arc<Self>, message: Value) {
        let http = self.clone();
        let id = message.get("id").and_then(Value::as_u64);
        let task = tokio::spawn(async move {
            let mut backoff = BACKOFF;
            loop {
                match http.post(&message, &HeaderMap::new()).await {
                    Ok(mut response) if is_stream(&response) => {
                        let mut parser = SseParser::new();
                        loop {
                            while let Some(Ok(data)) = parser.next_item() {
                                let Ok(message) = serde_json::from_str::<Value>(&data) else {
                                    continue;
                                };
                                if http.take(&message, id).await.is_some() {
                                    return;
                                }
                                backoff = BACKOFF;
                            }
                            match response.chunk().await {
                                Ok(Some(chunk)) => parser.push(&chunk),
                                _ => break,
                            }
                        }
                    }
                    Ok(_) => return,
                    Err(Fault::Other(_)) => {}
                    Err(_) => return,
                }
                http.shared
                    .tools_changed
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        });
        self.tasks
            .lock()
            .expect("tasks lock")
            .push(task.abort_handle());
    }

    /// Opens the deprecated HTTP+SSE transport: a GET stream whose first
    /// event names the endpoint messages are posted to, on the server's own
    /// origin, and which brings the answers.
    pub(super) async fn open_sse(self: &Arc<Self>) -> Result<(), Fault> {
        let mut response = self
            .client
            .get(self.url.clone())
            .headers(self.headers.clone())
            .headers(self.signed().await?)
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .send()
            .await
            .map_err(|error| Fault::Other(format!("cannot reach the MCP server: {error}")))?;
        if !response.status().is_success() {
            return Err(refusal(response).await);
        }
        if !is_stream(&response) {
            return Err(Fault::Other("its GET answer is not an event stream".into()));
        }
        let mut parser = SseParser::new();
        let endpoint = 'found: loop {
            while let Some(item) = parser.next_event() {
                let (kind, data) = item.map_err(|error| Fault::Other(error.message))?;
                if kind == "endpoint" {
                    break 'found data;
                }
            }
            match response.chunk().await {
                Ok(Some(chunk)) => parser.push(&chunk),
                _ => {
                    return Err(Fault::Other(
                        "its event stream ended before naming its endpoint".into(),
                    ))
                }
            }
        };
        let endpoint = self
            .url
            .join(endpoint.trim())
            .map_err(|error| Fault::Other(format!("its endpoint is not a URL: {error}")))?;
        // The configured headers, such as a token, go to the server's origin only.
        if endpoint.origin() != self.url.origin() {
            return Err(Fault::Other(format!(
                "its endpoint {endpoint} is on another origin"
            )));
        }
        let link = Arc::new(Link {
            endpoint,
            pending: Arc::new(Mutex::new(Some(HashMap::new()))),
        });
        *self.link.lock().expect("link lock") = Some(link.clone());
        *self.session.lock().expect("session lock") = None;
        *self.version.lock().expect("version lock") = None;
        let task = tokio::spawn(read_link(self.clone(), response, parser, link));
        self.tasks
            .lock()
            .expect("tasks lock")
            .push(task.abort_handle());
        Ok(())
    }

    /// Stops the streams read in the background and ends the session, if
    /// the server gave one, without waiting.
    pub(super) fn end(&self) {
        for task in self.tasks.lock().expect("tasks lock").drain(..) {
            task.abort();
        }
        if let Some(listener) = self.listener.lock().expect("listener lock").take() {
            listener.abort();
        }
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
        let account = self.account.clone();
        runtime.spawn(async move {
            let request = match account {
                Some((account, store)) => match account.headers(&store).await {
                    Ok(headers) => request.headers(headers),
                    Err(_) => return,
                },
                None => request,
            };
            let _ = request.send().await;
        });
    }
}

/// Reads the HTTP+SSE stream: answers go to their requests, the server's
/// requests are answered by POST, its notifications handled. When the
/// stream ends, what still waits fails.
async fn read_link(
    http: Arc<Http>,
    mut response: reqwest::Response,
    mut parser: SseParser,
    link: Arc<Link>,
) {
    loop {
        while let Some(item) = parser.next_event() {
            let Ok((_, data)) = item else { continue };
            let Ok(message) = serde_json::from_str::<Value>(&data) else {
                continue;
            };
            match (
                message.get("method").and_then(Value::as_str),
                message.get("id"),
            ) {
                (None, Some(id)) => {
                    let Some(id) = id.as_u64() else { continue };
                    let sender = link
                        .pending
                        .lock()
                        .expect("pending lock")
                        .as_mut()
                        .and_then(|pending| pending.remove(&id));
                    if let Some(sender) = sender {
                        let _ = sender.send(answer_of(&message));
                    }
                }
                (Some(_), Some(_)) => {
                    // A question waits for the person; the answers behind it
                    // are read meanwhile.
                    let http = http.clone();
                    tokio::spawn(async move {
                        http.take(&message, None).await;
                    });
                }
                (Some(_), None) => http.shared.notice(&message),
                (None, None) => {}
            }
        }
        match response.chunk().await {
            Ok(Some(chunk)) => parser.push(&chunk),
            _ => break,
        }
    }
    let waiting = link.pending.lock().expect("pending lock").take();
    for (_, sender) in waiting.into_iter().flatten() {
        let _ = sender.send(Err(Fault::Stopped));
    }
}

fn is_stream(response: &reqwest::Response) -> bool {
    response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|kind| kind.starts_with("text/event-stream"))
}

/// A response whose status is not a success, with the JSON-RPC error its
/// body carries.
async fn refusal(mut response: reqwest::Response) -> Fault {
    let status = response.status();
    let body = crate::llm::read_capped(&mut response, MAX_REFUSAL)
        .await
        .ok()
        .flatten()
        .map(|body| String::from_utf8_lossy(&body).trim().to_string())
        .unwrap_or_default();
    let rpc = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|value| value.get("error").map(RpcError::of));
    Fault::Status {
        status,
        rpc,
        body: body.chars().take(SHOWN_REFUSAL).collect(),
    }
}

/// The value of `Mcp-Name`: the name of the tool or prompt, or the URI of
/// the resource, a request is about.
fn mcp_name(message: &Value) -> Option<&str> {
    let key = match message.get("method").and_then(Value::as_str)? {
        "tools/call" | "prompts/get" => "name",
        "resources/read" => "uri",
        _ => return None,
    };
    message.pointer(&format!("/params/{key}"))?.as_str()
}

/// A header value as the transport encodes it: as it is when it is plain
/// visible ASCII without surrounding blanks, otherwise Base64 between
/// `=?base64?` and `?=`.
pub(super) fn header_text(text: &str) -> String {
    let plain = text.chars().all(|c| c == '\t' || (' '..='~').contains(&c))
        && !text.starts_with([' ', '\t'])
        && !text.ends_with([' ', '\t'])
        && !(text.starts_with("=?base64?") && text.ends_with("?="));
    if plain {
        text.to_string()
    } else {
        format!(
            "=?base64?{}?=",
            base64::engine::general_purpose::STANDARD.encode(text)
        )
    }
}

/// A tool parameter mirrored into an `Mcp-Param-<name>` header.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct ParamHeader {
    name: String,
    /// The chain of `properties` keys from the schema's root to it.
    path: Vec<String>,
}

impl ParamHeader {
    /// The header for a call with `arguments`; none when the parameter has
    /// no value of its type there.
    pub(super) fn header(&self, arguments: &Value) -> Option<(HeaderName, HeaderValue)> {
        let mut value = arguments;
        for key in &self.path {
            value = value.get(key)?;
        }
        let text = match value {
            Value::String(text) => text.clone(),
            Value::Bool(flag) => flag.to_string(),
            Value::Number(number) if number.is_i64() || number.is_u64() => number.to_string(),
            _ => return None,
        };
        let name = HeaderName::from_bytes(format!("Mcp-Param-{}", self.name).as_bytes()).ok()?;
        let value = HeaderValue::from_str(&header_text(&text)).ok()?;
        Some((name, value))
    }
}

/// The `x-mcp-header` parameters of a tool's input schema, or why the tool
/// breaks the transport's rules for them.
pub(super) fn param_headers(schema: &Value) -> Result<Vec<ParamHeader>, String> {
    let mut found = Vec::new();
    scan(schema, Some(Vec::new()), &mut found)?;
    let mut names = HashSet::new();
    for param in &found {
        if !names.insert(param.name.to_ascii_lowercase()) {
            return Err(format!("x-mcp-header {} is given twice", param.name));
        }
    }
    Ok(found)
}

/// Collects the `x-mcp-header` annotations under `schema`. `path` is the
/// chain of `properties` keys that led here, `None` once the way passed
/// through anything else.
fn scan(
    schema: &Value,
    path: Option<Vec<String>>,
    found: &mut Vec<ParamHeader>,
) -> Result<(), String> {
    match schema {
        Value::Array(items) => {
            for item in items {
                scan(item, None, found)?;
            }
        }
        Value::Object(map) => {
            if let Some(name) = map.get("x-mcp-header") {
                let path = path.as_ref().filter(|path| !path.is_empty()).ok_or(
                    "an x-mcp-header is not on a parameter reached through properties alone",
                )?;
                let name = name.as_str().ok_or("an x-mcp-header is not a string")?;
                if name.is_empty() || !name.bytes().all(token_byte) {
                    return Err(format!("x-mcp-header {name:?} is not a header name"));
                }
                if !primitive(map.get("type")) {
                    return Err(format!(
                        "x-mcp-header {name} is on a parameter that is not a string, integer or boolean"
                    ));
                }
                found.push(ParamHeader {
                    name: name.to_string(),
                    path: path.clone(),
                });
            }
            for (key, value) in map {
                match key.as_str() {
                    "properties" => {
                        for (property, schema) in value.as_object().into_iter().flatten() {
                            let next = path.as_ref().map(|path| {
                                let mut next = path.clone();
                                next.push(property.clone());
                                next
                            });
                            scan(schema, next, found)?;
                        }
                    }
                    // These hold values, not schemas.
                    "x-mcp-header" | "const" | "enum" | "default" | "examples" => {}
                    _ => scan(value, None, found)?,
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// A byte of an HTTP token (RFC 9110 `tchar`).
fn token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}

/// Whether a schema's `type` is one a header may carry: string, integer or
/// boolean (with null beside it).
fn primitive(kind: Option<&Value>) -> bool {
    let allowed = |kind: &str| matches!(kind, "string" | "integer" | "boolean");
    match kind {
        Some(Value::String(kind)) => allowed(kind),
        Some(Value::Array(kinds)) => {
            let names: Vec<&str> = kinds.iter().filter_map(Value::as_str).collect();
            names.len() == kinds.len()
                && names.iter().any(|kind| allowed(kind))
                && names.iter().all(|kind| allowed(kind) || *kind == "null")
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn header_values_are_sent_plain_or_in_base64() {
        assert_eq!(header_text("us-west1"), "us-west1");
        assert_eq!(
            header_text("Hello, 世界"),
            "=?base64?SGVsbG8sIOS4lueVjA==?="
        );
        assert_eq!(header_text(" padded "), "=?base64?IHBhZGRlZCA=?=");
        assert_eq!(header_text("line1\nline2"), "=?base64?bGluZTEKbGluZTI=?=");
        assert_eq!(
            header_text("=?base64?literal?="),
            "=?base64?PT9iYXNlNjQ/bGl0ZXJhbD89?="
        );
    }

    #[test]
    fn parameters_named_for_headers_follow_the_rules() {
        let nested = json!({"type": "object", "properties": {
            "where": {"type": "object", "properties": {
                "region": {"type": "string", "x-mcp-header": "Region"}}},
            "count": {"type": ["integer", "null"], "x-mcp-header": "Count"},
            "note": {"type": "string", "default": {"x-mcp-header": "data, not a schema"}}}});
        let params = param_headers(&nested).expect("valid");
        let headers: Vec<(String, String)> = params
            .iter()
            .filter_map(|param| param.header(&json!({"where": {"region": "eu"}, "count": 3})))
            .map(|(name, value)| (name.to_string(), value.to_str().unwrap().to_string()))
            .collect();
        assert_eq!(
            headers,
            [
                ("mcp-param-region".to_string(), "eu".to_string()),
                ("mcp-param-count".to_string(), "3".to_string())
            ]
        );
        assert_eq!(params[0].header(&json!({"where": {}})), None);

        for (schema, reason) in [
            (
                json!({"properties": {"n": {"type": "number", "x-mcp-header": "N"}}}),
                "not a string",
            ),
            (
                json!({"properties": {"a": {"type": "string", "x-mcp-header": ""}}}),
                "header name",
            ),
            (
                json!({"properties": {"a": {"type": "string", "x-mcp-header": "A B"}}}),
                "header name",
            ),
            (
                json!({"properties": {"a": {"type": "string", "x-mcp-header": "Id"},
                                      "b": {"type": "string", "x-mcp-header": "id"}}}),
                "twice",
            ),
            (
                json!({"properties": {"list": {"type": "array",
                    "items": {"type": "string", "x-mcp-header": "Item"}}}}),
                "properties alone",
            ),
            (
                json!({"anyOf": [{"properties": {"a": {"type": "string", "x-mcp-header": "A"}}}]}),
                "properties alone",
            ),
            (
                json!({"type": "object", "x-mcp-header": "Root"}),
                "properties alone",
            ),
        ] {
            let error = param_headers(&schema).expect_err("invalid");
            assert!(error.contains(reason), "{error}");
        }
    }
}
