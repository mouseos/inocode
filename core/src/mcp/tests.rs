use super::*;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::oneshot;

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
            Some("server/discover") => {
                let refusal = json!({"jsonrpc": "2.0", "id": id,
                    "error": {"code": -32601, "message": "Method not found"}});
                writer
                    .write_all(format!("{refusal}\n").as_bytes())
                    .await
                    .expect("write");
                continue;
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
    let connection = Arc::new(Connection::new("fake server", client_read, client_write));

    connection.open().await.expect("open");
    assert_eq!(connection.era(), Some(Era::Legacy("2025-06-18".into())));
    let listing = connection.list_tools().await.expect("tools");
    assert_eq!(listing.ttl, None);
    let mut tools = Tools::default();
    add(
        &mut tools,
        "fake server",
        &Slot::new(connection, None, listing.clone()),
        listing.tools,
    );
    let names: Vec<&str> = tools.specs.iter().map(|spec| spec.name.as_str()).collect();
    assert_eq!(
        names,
        ["mcp__fake_server__echo_text", "mcp__fake_server__fail"]
    );
    assert_eq!(
        tools.specs[0].description,
        "Echo (tool echo.text of MCP server fake server)"
    );

    let (slot, tool) = &tools.routes["mcp__fake_server__echo_text"];
    let result = slot
        .current()
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
    let (slot, tool) = &tools.routes["mcp__fake_server__fail"];
    let result = slot
        .current()
        .request("tools/call", json!({"name": tool, "arguments": {}}))
        .await
        .expect("call");
    assert_eq!(render(&result), Err("broken".into()));

    let seen = seen.lock().expect("seen");
    let methods: Vec<&str> = seen.iter().filter_map(|m| m["method"].as_str()).collect();
    assert_eq!(
        methods,
        [
            "server/discover",
            "initialize",
            "notifications/initialized",
            "tools/list",
            "tools/list",
            "tools/call",
            "tools/call"
        ]
    );
    assert_eq!(
        seen[0]["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"],
        connection::MODERN_VERSION
    );
    assert_eq!(
        seen[1]["params"]["protocolVersion"],
        connection::LEGACY_VERSION
    );
    assert_eq!(
        seen[1]["params"]["capabilities"],
        json!({"elicitation": {"form": {}, "url": {}}})
    );
    assert!(
        seen[3]["params"].get("_meta").is_none(),
        "no per-request fields for an older server"
    );
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
    let connection = Connection::new("gone", client_read, client_write);
    drop(server_end);
    let error = connection
        .request("tools/list", json!({}))
        .await
        .unwrap_err()
        .to_string();
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
                let body: Value = serde_json::from_slice(&buffer[header_end..header_end + length])
                    .unwrap_or(Value::Null);
                seen.lock().expect("seen").push((
                    method.to_string(),
                    headers.clone(),
                    body.clone(),
                ));
                let id = body.get("id").cloned().unwrap_or(Value::Null);
                let json_answer = |result: Value, extra: &str| {
                    let text = json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string();
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
                    // As a server of an older revision refuses a request
                    // that comes before its session.
                    (_, Some("server/discover")) => {
                        let text = json!({"jsonrpc": "2.0", "id": null, "error":
                            {"code": -32000, "message": "Bad Request: No valid session ID provided"}})
                        .to_string();
                        format!(
                            "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\n\
                             content-length: {}\r\nconnection: close\r\n\r\n{text}",
                            text.len()
                        )
                    }
                    ("DELETE", _) => {
                        "HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                            .to_string()
                    }
                    (_, Some("initialize")) => {
                        let number = state.sessions.fetch_add(1, Ordering::SeqCst) + 1;
                        json_answer(
                            json!({"protocolVersion": connection::LEGACY_VERSION, "capabilities": {},
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
                    (_, Some("tools/call")) => {
                        json_answer(json!({"content": [{"type": "text", "text": "echoed"}]}), "")
                    }
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
    let settings: ServerConfig = serde_json::from_value(json!({ "url": url })).expect("settings");
    let connection = connect("web", &settings).expect("connect");
    connection.open().await.expect("open");
    connection.list_tools().await.expect("tools");

    // The server ends the first session at the call: a new one is
    // started and the call sent again.
    let echoed = connection
        .request("tools/call", json!({"name": "echo", "arguments": {}}))
        .await
        .map_err(|fault| fault.to_string())
        .and_then(|result| render(&result));
    assert_eq!(echoed, Ok("echoed".into()));
    let requests = seen.lock().expect("seen").clone();
    // After the probe, the handshake, the list and the answer to its ping:
    let later: Vec<(Option<&str>, Option<&str>)> = requests[5..]
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
    let (sender, mut questions) = mpsc::unbounded_channel::<Notice>();
    let _waiting = Waiting::register(
        &connection.shared,
        Some(crate::store::SessionId(1)),
        Some(sender),
        None,
    );
    tokio::spawn(async move {
        let Some(Notice::Question(question)) = questions.recv().await else {
            panic!("a question");
        };
        assert_eq!(question.params["message"], "Who?");
        let _ = question
            .reply
            .send(json!({"action": "accept", "content": {"name": "Bo"}}));
    });
    let greeted = connection
        .request("tools/call", json!({"name": "greet", "arguments": {}}))
        .await
        .map_err(|fault| fault.to_string())
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
    let connection = Arc::new(connect("web", &settings).expect("connect"));
    connection.open().await.expect("open");
    let listing = connection.list_tools().await.expect("tools");
    assert_eq!(listing.tools.len(), 1);
    let mut tools = Tools::default();
    add(
        &mut tools,
        "web",
        &Slot::new(connection.clone(), None, listing.clone()),
        listing.tools,
    );
    let (route, tool) = tools.routes.get("mcp__web__echo").expect("route");
    let result = route
        .current()
        .request("tools/call", json!({"name": tool, "arguments": {}}))
        .await
        .map_err(|fault| fault.to_string())
        .and_then(|result| render(&result));
    assert_eq!(result, Ok("echoed".into()));

    let mut requests = seen.lock().expect("seen").clone();
    // The probe of the stateless revision, refused as an older server does:
    let probe = requests.remove(0);
    assert_eq!(probe.2["method"], "server/discover");
    assert_eq!(probe.1["mcp-method"], "server/discover");
    assert_eq!(probe.1["mcp-protocol-version"], connection::MODERN_VERSION);
    assert!(!probe.1.contains_key("mcp-session-id"));
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
            later.then_some(connection::LEGACY_VERSION),
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
            connect("x", &settings).err().as_deref(),
            Some("give either command or url")
        );
    }
    let settings: ServerConfig =
        serde_json::from_value(json!({"url": "file:///etc/passwd"})).expect("settings");
    assert!(connect("x", &settings).is_err());
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

/// A stdio server in Python with `add` and `crash`, which ends the
/// server without answering.
const CRASHING_SERVER: &str = r#"
import json, sys
for line in sys.stdin:
    message = json.loads(line)
    if "id" not in message or "method" not in message:
        continue
    method = message["method"]
    if method == "server/discover":
        print(json.dumps({"jsonrpc": "2.0", "id": message["id"],
                          "error": {"code": -32601, "message": "Method not found"}}), flush=True)
        continue
    if method == "initialize":
        result = {"protocolVersion": "2025-11-25", "capabilities": {"tools": {}},
                  "serverInfo": {"name": "calc", "version": "1"}}
    elif method == "tools/list":
        result = {"tools": [{"name": "add", "inputSchema": {"type": "object"}},
                            {"name": "crash", "inputSchema": {"type": "object"}}]}
    elif message["params"]["name"] == "crash":
        sys.exit(1)
    else:
        a = message["params"]["arguments"]
        result = {"content": [{"type": "text", "text": str(a["a"] + a["b"])}]}
    print(json.dumps({"jsonrpc": "2.0", "id": message["id"], "result": result}), flush=True)
"#;

fn python() -> Option<&'static str> {
    ["python3", "python"].into_iter().find(|name| {
        std::process::Command::new(name)
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success())
    })
}

#[tokio::test]
async fn a_crashed_server_is_started_again_once_and_a_changed_file_is_read_again() {
    let Some(python) = python() else {
        eprintln!("skipped: no Python to run the MCP server");
        return;
    };
    let dir = tempfile::tempdir().expect("dir");
    let script = dir.path().join("calc.py");
    std::fs::write(&script, CRASHING_SERVER).expect("script");
    let config = dir.path().join("mcp.json");
    std::fs::write(
        &config,
        json!({"mcpServers": {
            "calc": {"command": python, "args": [script]},
            "broken": {"command": "inocode-no-such-command"}
        }})
        .to_string(),
    )
    .expect("config");
    let store = Store::open(dir.path().join("inocode.db"))
        .await
        .expect("store");
    let (_cancel, cancel) = tokio::sync::watch::channel(false);
    let run = |name: &'static str, arguments: &'static str| {
        let (store, cancel) = (store.clone(), cancel.clone());
        async move {
            call(&store, name, arguments, cancel, None)
                .await
                .expect("a tool")
        }
    };

    assert_eq!(
        run("mcp__calc__add", r#"{"a": 1, "b": 2}"#).await,
        Ok("3".into())
    );
    let problems = render_problems(&store).await.expect("problems");
    assert!(
        problems.contains("server broken did not start"),
        "{problems}"
    );
    let crashed = run("mcp__calc__crash", "{}").await.expect_err("crashed");
    assert!(crashed.contains("it was started again"), "{crashed}");
    assert_eq!(
        run("mcp__calc__add", r#"{"a": 2, "b": 2}"#).await,
        Ok("4".into())
    );
    let again = run("mcp__calc__crash", "{}").await.expect_err("crashed");
    assert!(again.contains("already started again once"), "{again}");

    // A changed mcp.json is read at the next use. Some file systems
    // keep times to the second.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    std::fs::write(
        &config,
        json!({"mcpServers": {"sum": {"command": python, "args": [script]}}}).to_string(),
    )
    .expect("config");
    assert_eq!(
        run("mcp__sum__add", r#"{"a": 3, "b": 4}"#).await,
        Ok("7".into())
    );
    assert!(call(&store, "mcp__calc__add", "{}", cancel.clone(), None)
        .await
        .is_none());
    assert_eq!(render_problems(&store).await, None);
}

#[tokio::test]
async fn servers_start_at_once_each_within_its_own_time() {
    let Some(python) = python() else {
        eprintln!("skipped: no Python to run the MCP server");
        return;
    };
    let dir = tempfile::tempdir().expect("dir");
    let script = dir.path().join("slow.py");
    std::fs::write(
        &script,
        format!("import time\ntime.sleep(3)\n{CRASHING_SERVER}"),
    )
    .expect("script");
    let config = dir.path().join("mcp.json");
    std::fs::write(
        &config,
        json!({"mcpServers": {
            "first": {"command": python, "args": [script]},
            "second": {"command": python, "args": [script], "startup_timeout_sec": 20},
            "third": {"command": python, "args": [script]},
            "hurried": {"command": python, "args": [script], "startup_timeout_sec": 0.5},
            "wrong": {"command": python, "args": [script], "startup_timeout_sec": -1}
        }})
        .to_string(),
    )
    .expect("config");
    let accounts = dir.path().join("auth.json");
    let hosts = crate::connectors::Hosts::default();

    let began = std::time::Instant::now();
    let tools = load(&config, &accounts, Path::new("no-plugins"), &hosts).await;
    let took = began.elapsed();

    // One after another, the three would take at least 9 seconds.
    assert!(took < Duration::from_secs(8), "{took:?}");
    let names: Vec<&str> = tools.specs.iter().map(|spec| spec.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "mcp__first__add",
            "mcp__first__crash",
            "mcp__second__add",
            "mcp__second__crash",
            "mcp__third__add",
            "mcp__third__crash"
        ]
    );
    let problems = tools.problems.join("\n");
    assert!(
        problems.contains("server hurried did not start: did not start within 500ms"),
        "{problems}"
    );
    assert!(
        problems.contains(
            "server wrong did not start: startup_timeout_sec must be a number of seconds above 0, not -1"
        ),
        "{problems}"
    );
}

#[test]
fn a_question_goes_to_the_one_thread_whose_calls_wait() {
    let shared = Shared::default();
    let (sender, mut questions) = mpsc::unbounded_channel();
    let first = Waiting::register(
        &shared,
        Some(crate::store::SessionId(1)),
        Some(sender),
        None,
    );
    let mut asked = connection::forward(&shared, json!({"message": "one"}));
    let Ok(Notice::Question(question)) = questions.try_recv() else {
        panic!("the thread is asked");
    };
    assert_eq!(question.params["message"], "one");
    drop(question);

    let other = Waiting::register(&shared, Some(crate::store::SessionId(2)), None, None);
    let mut refused = connection::forward(&shared, json!({"message": "two"}));
    assert_eq!(refused.try_recv(), Ok(json!({"action": "cancel"})));
    assert!(questions.try_recv().is_err(), "no thread is asked");
    drop((first, other));
    assert!(shared.waiters.lock().expect("waiters").is_empty());
    let mut nobody = connection::forward(&shared, json!({}));
    assert_eq!(nobody.try_recv(), Ok(json!({"action": "cancel"})));
    let _ = asked.try_recv();
}

#[tokio::test]
async fn missing_and_broken_servers_give_no_tools() {
    let dir = tempfile::tempdir().expect("dir");
    let config = dir.path().join("mcp.json");
    let accounts = dir.path().join("auth.json");
    let hosts = crate::connectors::Hosts::default();
    assert!(load(&config, &accounts, Path::new("no-plugins"), &hosts)
        .await
        .specs
        .is_empty());
    std::fs::write(
        &config,
        r#"{"mcpServers": {"none": {"command": "inocode-no-such-command"}}}"#,
    )
    .expect("write");
    assert!(load(&config, &accounts, Path::new("no-plugins"), &hosts)
        .await
        .specs
        .is_empty());
}

/// A request line and its headers with lowercase names.
type Listed = (String, HashMap<String, String>);

/// Answers every request with `body` and keeps each request's first
/// line and headers.
async fn list_server(body: Value, seen: Arc<Mutex<Vec<Listed>>>) -> String {
    answering_server(seen, move |_, _, _| (200, body.clone())).await
}

/// Answers each request with what `answer` makes of its first line,
/// headers and body, and keeps each request's first line and headers.
async fn answering_server(
    seen: Arc<Mutex<Vec<Listed>>>,
    answer: impl Fn(&str, &HashMap<String, String>, &str) -> (u16, Value) + Send + 'static,
) -> String {
    use tokio::io::AsyncReadExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("address");
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let mut buffer = Vec::new();
            let mut chunk = [0u8; 4096];
            let end = loop {
                if let Some(at) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                    break at;
                }
                let read = stream.read(&mut chunk).await.unwrap_or(0);
                if read == 0 {
                    break buffer.len();
                }
                buffer.extend_from_slice(&chunk[..read]);
            };
            let head = String::from_utf8_lossy(&buffer[..end]).to_string();
            let mut lines = head.split("\r\n");
            let first = lines.next().unwrap_or("").to_string();
            let headers: HashMap<String, String> = lines
                .filter_map(|line| line.split_once(':'))
                .map(|(name, value)| (name.to_lowercase(), value.trim().to_string()))
                .collect();
            let length: usize = headers
                .get("content-length")
                .and_then(|value| value.parse().ok())
                .unwrap_or(0);
            let mut content = buffer[(end + 4).min(buffer.len())..].to_vec();
            while content.len() < length {
                let read = stream.read(&mut chunk).await.unwrap_or(0);
                if read == 0 {
                    break;
                }
                content.extend_from_slice(&chunk[..read]);
            }
            let (status, reply) = answer(&first, &headers, &String::from_utf8_lossy(&content));
            seen.lock().expect("seen").push((first, headers));
            let text = reply.to_string();
            let reply = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
                 connection: close\r\n\r\n{text}",
                text.len()
            );
            let _ = stream.write_all(reply.as_bytes()).await;
            let _ = stream.shutdown().await;
        }
    });
    format!("http://{address}")
}

fn signed_in(path: &Path, provider: &str, access: &str, account: Option<&str>) {
    let mut store = crate::oauth::Store::load(path.to_path_buf()).expect("store");
    store
        .save(
            provider,
            crate::oauth::Credential {
                access: access.into(),
                refresh: "r".into(),
                expires_at_ms: Some(u64::MAX),
                account_id: account.map(str::to_string),
                email: None,
                api_key: None,
                oauth_access_token: None,
            },
        )
        .expect("save");
}

#[tokio::test]
async fn signed_in_accounts_bring_their_connectors() {
    let dir = tempfile::tempdir().expect("dir");
    let accounts = dir.path().join("auth.json");
    let listed = Arc::new(Mutex::new(Vec::new()));
    let claude_api = list_server(
        json!({"data": [{"id": "srv1", "display_name": "Notion"}], "next_page": null}),
        listed.clone(),
    )
    .await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mcp = http_server(seen.clone()).await;
    let base = mcp.trim_end_matches("/mcp").to_string();
    let hosts = crate::connectors::Hosts {
        claude_api,
        claude_proxy: base.clone(),
        chatgpt: base,
        claude_token: None,
    };
    let config = dir.path().join("mcp.json");

    // Without a sign-in nothing is listed or reached.
    assert!(load(&config, &accounts, Path::new("no-plugins"), &hosts)
        .await
        .specs
        .is_empty());
    assert!(listed.lock().expect("listed").is_empty());

    signed_in(&accounts, "claude-oauth", "claude-token", None);
    signed_in(&accounts, "gpt-oauth", "gpt-token", Some("acct-1"));
    let tools = load(&config, &accounts, Path::new("no-plugins"), &hosts).await;
    let names: Vec<&str> = tools.specs.iter().map(|spec| spec.name.as_str()).collect();
    assert_eq!(
        names,
        ["mcp__claude_ai_Notion__echo", "mcp__codex_apps__echo"]
    );

    let listed = listed.lock().expect("listed").clone();
    assert!(
        listed[0].0.starts_with("GET /v1/mcp_servers?limit=1000 "),
        "{listed:?}"
    );
    assert_eq!(listed[0].1["authorization"], "Bearer claude-token");
    assert_eq!(listed[0].1["anthropic-beta"], "mcp-servers-2025-12-04");

    let seen = seen.lock().expect("seen").clone();
    let claude: Vec<_> = seen
        .iter()
        .filter(|(_, headers, _)| {
            headers.get("authorization").map(String::as_str) == Some("Bearer claude-token")
        })
        .collect();
    assert!(!claude.is_empty());
    assert!(claude.iter().all(
        |(_, headers, _)| headers.contains_key("x-mcp-client-session-id")
            && headers["user-agent"].starts_with("claude-cli/")
    ));
    let chatgpt: Vec<_> = seen
        .iter()
        .filter(|(_, headers, _)| {
            headers.get("authorization").map(String::as_str) == Some("Bearer gpt-token")
        })
        .collect();
    assert!(!chatgpt.is_empty());
    assert!(chatgpt
        .iter()
        .all(|(_, headers, _)| headers["chatgpt-account-id"] == "acct-1"));
    assert_eq!(claude.len() + chatgpt.len(), seen.len());
}

/// Lists the real connectors of the accounts signed in at
/// `~/.inocode/auth.json`: claude.ai's list (one request) and the tools
/// of ChatGPT's apps (its handshake and one list). Connects to no
/// claude.ai connector and calls no tool. Run by hand, with the
/// person's approval.
#[test]
fn only_apps_the_person_added_are_used_unless_turned_off() {
    let config: Config = serde_json::from_value(json!({"connectors": {
        "Pets": {"enabled": false},
        "connector_gh": {"enabled": false},
        "Notion": {"enabled": true},
    }}))
    .expect("config");
    let linked = |name: &str, id: &str, app: &str, link: &str| {
        json!({"name": name, "_meta": {
            "connector_id": id,
            "connector_name": app,
            "_codex_apps": {"resource_uri": format!("/{id}/{link}/{name}")},
        }})
    };
    let tool = |name: &str, id: &str, app: &str| linked(name, id, app, "link_69dced7");
    let listed = vec![
        linked(
            "sites_create",
            "connector_sites",
            "Sites",
            "implicit_link::connector_sites",
        ),
        linked(
            "linear_list",
            "connector_linear",
            "Linear",
            "synthetic_link::connector_linear",
        ),
        tool("pets_feed", "connector_pets", "Pets"),
        tool("github_list", "connector_gh", "GitHub"),
        tool("notion_search", "connector_notion", "Notion"),
        tool("gmail_read", "connector_gmail", "Gmail"),
    ];
    let kept: Vec<String> = by_app(&config, "codex_apps", listed)
        .iter()
        .map(|tool| tool["name"].as_str().expect("name").to_string())
        .collect();
    assert_eq!(kept, ["notion_search", "gmail_read"]);

    // A claude.ai connector is one app, named by the server.
    let plain = vec![json!({"name": "search"})];
    assert_eq!(by_app(&config, "claude.ai Slack", plain.clone()).len(), 1);
    assert!(config.uses(&["claude.ai Slack", "Slack"]));
    assert!(!config.uses(&["claude.ai Pets", "Pets"]));
}

#[test]
fn many_tools_are_found_by_search_and_offered_from_then_on() {
    use crate::conversation::{Body, Message};
    let spec = |name: &str, description: &str| ToolSpec {
        name: name.to_string(),
        description: description.to_string(),
        parameters: json!({"type": "object", "properties": {"page_id": {"type": "string"}}}),
    };
    let mut tools = Tools::default();
    tools
        .specs
        .push(spec("mcp__notion__fetch", "Fetch a Notion page"));
    assert_eq!(
        tools.offered(&[]).len(),
        1,
        "a few tools are all given up front"
    );

    tools
        .specs
        .push(spec("mcp__gmail__send", "Send an email message"));
    tools.specs.push(spec(
        "mcp__calendar__list_events",
        "List the events of a calendar",
    ));
    for index in 0..DEFER_AT {
        tools.specs.push(spec(
            &format!("mcp__github__tool_{index}"),
            "Work with a GitHub repository",
        ));
    }
    let offered: Vec<String> = tools.offered(&[]).into_iter().map(|t| t.name).collect();
    assert_eq!(offered, [SEARCH]);

    let found = tools
        .search(r#"{"query": "send an email"}"#)
        .expect("search");
    let names: Vec<String> = serde_json::from_str::<Value>(&found).expect("json")["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .map(|tool| tool["name"].as_str().expect("name").to_string())
        .collect();
    assert_eq!(names[0], "mcp__gmail__send");
    assert!(names.len() <= SEARCH_LIMIT);
    let one = tools
        .search(r#"{"query": "calendar events", "limit": 1}"#)
        .expect("search");
    assert!(one.contains("mcp__calendar__list_events"));
    assert_eq!(
        serde_json::from_str::<Value>(&one).expect("json")["tools"]
            .as_array()
            .expect("tools")
            .len(),
        1
    );
    assert!(tools.search(r#"{"query": "  "}"#).is_err());

    let answer = |content: String, is_error: bool| Message {
        id: "r".to_string(),
        body: Body::ToolResult {
            call_id: "c".to_string(),
            name: SEARCH.to_string(),
            content,
            is_error,
            images: Vec::new(),
        },
    };
    let offered: Vec<String> = tools
        .offered(&[answer(one, false)])
        .into_iter()
        .map(|t| t.name)
        .collect();
    assert_eq!(offered, [SEARCH, "mcp__calendar__list_events"]);
    let failed = tools.offered(&[answer(found, true)]);
    assert_eq!(failed.len(), 1, "a failed search finds nothing");
}

#[tokio::test]
async fn a_sign_in_missing_the_connector_scope_is_refreshed_once() {
    let dir = tempfile::tempdir().expect("dir");
    let accounts = dir.path().join("auth.json");
    signed_in(&accounts, "claude-oauth", "old-token", None);
    let refreshes = Arc::new(Mutex::new(Vec::new()));
    let token = answering_server(refreshes.clone(), |_, _, body| {
        let body: Value = serde_json::from_str(body).expect("json");
        assert_eq!(body["grant_type"], "refresh_token");
        assert!(body["scope"]
            .as_str()
            .is_some_and(|scope| scope.split(' ').any(|s| s == "user:mcp_servers")));
        (
            200,
            json!({"access_token": "new-token", "expires_in": 3600}),
        )
    })
    .await;
    let listed = Arc::new(Mutex::new(Vec::new()));
    let claude_api = answering_server(listed.clone(), |_, headers, _| {
        if headers["authorization"] == "Bearer new-token" {
            (200, json!({"data": [], "next_page": null}))
        } else {
            (
                403,
                json!({"error": {"message":
                    "OAuth token does not meet scope requirement user:mcp_servers"}}),
            )
        }
    })
    .await;
    let hosts = crate::connectors::Hosts {
        claude_api,
        claude_proxy: "http://127.0.0.1:9".into(),
        chatgpt: "http://127.0.0.1:9".into(),
        claude_token: Some(format!("{token}/v1/oauth/token")),
    };
    assert!(crate::connectors::servers(&accounts, &hosts)
        .await
        .is_empty());
    assert_eq!(refreshes.lock().expect("refreshes").len(), 1);
    let listed = listed.lock().expect("listed").clone();
    assert_eq!(listed.len(), 2, "{listed:?}");
    assert_eq!(listed[1].1["authorization"], "Bearer new-token");
    let stored = crate::oauth::Store::load(accounts).expect("store");
    assert_eq!(
        stored.credential("claude-oauth").expect("stored").access,
        "new-token"
    );
}

#[tokio::test]
#[ignore = "reaches the real accounts"]
async fn lists_the_real_connectors() {
    let accounts = crate::oauth::Store::default_path().expect("store");
    let servers = crate::connectors::servers(&accounts, &crate::connectors::Hosts::default()).await;
    for server in &servers {
        println!("server: {} ({:?})", server.name, server.account);
    }
    // INOCODE_LIVE_ACCOUNT=claude or chatgpt picks whose servers are reached.
    let account = match std::env::var("INOCODE_LIVE_ACCOUNT").as_deref() {
        Ok("claude") => crate::connectors::Account::Claude,
        _ => crate::connectors::Account::ChatGpt,
    };
    for server in servers
        .into_iter()
        .filter(|server| server.account == account)
    {
        let name = server.name.clone();
        let connection = Source::Connector(server, accounts.clone())
            .connect(&name)
            .expect("connect");
        if let Err(fault) = connection.open().await {
            println!("{name}: {fault}");
            continue;
        }
        match connection.list_tools().await.map(|listing| listing.tools) {
            Ok(tools) => {
                println!("{name} tools: {}", tools.len());
                for tool in tools.iter().take(15) {
                    println!("  {}", tool["name"].as_str().unwrap_or("?"));
                }
                // INOCODE_LIVE_CALL="<server name>:<tool>" calls that tool
                // with no arguments; pick one that only reads.
                let wanted = std::env::var("INOCODE_LIVE_CALL").unwrap_or_default();
                if let Some((server, tool)) = wanted.split_once(':') {
                    if name.ends_with(server) {
                        let result = connection
                            .request("tools/call", json!({"name": tool, "arguments": {}}))
                            .await;
                        let shown = match result.map(|value| render(&value)) {
                            Ok(Ok(text)) => format!("ok, {} chars", text.chars().count()),
                            Ok(Err(text)) => format!("tool error: {text}"),
                            Err(error) => format!("request failed: {error}"),
                        };
                        println!("{name} {tool}: {shown}");
                    }
                }
            }
            Err(error) => println!("{name}: {error}"),
        }
    }
}

/// Writes one message to a fake stdio server's client.
async fn say<W: AsyncWrite + Unpin>(writer: &tokio::sync::Mutex<W>, message: Value) {
    writer
        .lock()
        .await
        .write_all(format!("{message}\n").as_bytes())
        .await
        .expect("write");
}

/// A stdio server of the stateless revision only. It refuses `initialize`
/// and every request without the per-request fields, lists its tools as
/// fresh for no time, opens a subscription and says its tools changed, and
/// has tools that ask for a form or a visit, report progress and a log,
/// run as a task that asks for approval, ask for sampling, or never answer.
async fn modern_server(
    reader: impl AsyncRead + Unpin,
    writer: impl AsyncWrite + Unpin,
    seen: Arc<Mutex<Vec<Value>>>,
) {
    let writer = tokio::sync::Mutex::new(writer);
    let mut lines = BufReader::new(reader).lines();
    let mut updated = false;
    while let Ok(Some(line)) = lines.next_line().await {
        let message: Value = serde_json::from_str(&line).expect("json");
        seen.lock().expect("seen").push(message.clone());
        let (Some(method), Some(id)) = (message["method"].as_str(), message.get("id").cloned())
        else {
            continue;
        };
        let params = &message["params"];
        let meta = &params["_meta"];
        if meta["io.modelcontextprotocol/protocolVersion"] != connection::MODERN_VERSION {
            say(
                &writer,
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32602,
                "message": "this server speaks only 2026-07-28"}}),
            )
            .await;
            continue;
        }
        let complete = |mut result: Value| {
            result["resultType"] = json!("complete");
            result
        };
        let ask = |requests: Value, state: Option<&str>| {
            let mut result = json!({"resultType": "input_required", "inputRequests": requests});
            if let Some(state) = state {
                result["requestState"] = json!(state);
            }
            result
        };
        let text = |text: String| complete(json!({"content": [{"type": "text", "text": text}]}));
        let responses = &params["inputResponses"];
        let result = match method {
            "server/discover" => complete(json!({
                "supportedVersions": [connection::MODERN_VERSION],
                "capabilities": {"tools": {"listChanged": true}, "logging": {},
                                 "extensions": {connection::TASKS: {}}},
                "_meta": {"io.modelcontextprotocol/serverInfo": {"name": "modern", "version": "1"}},
                "ttlMs": 60000, "cacheScope": "public"})),
            "subscriptions/listen" => {
                let tag = json!({"io.modelcontextprotocol/subscriptionId": id});
                say(
                    &writer,
                    json!({"jsonrpc": "2.0",
                    "method": "notifications/subscriptions/acknowledged",
                    "params": {"_meta": tag, "notifications": params["notifications"]}}),
                )
                .await;
                say(
                    &writer,
                    json!({"jsonrpc": "2.0",
                    "method": "notifications/tools/list_changed", "params": {"_meta": tag}}),
                )
                .await;
                continue;
            }
            "tools/list" => complete(json!({"tools": [
                {"name": "greet", "inputSchema": {"type": "object"}},
                {"name": "visit", "inputSchema": {"type": "object"}}],
                "ttlMs": 0, "cacheScope": "public"})),
            "tools/call" => match params["name"].as_str().unwrap_or("") {
                "greet" if responses["who"].is_object() => {
                    assert_eq!(params["requestState"], "s1");
                    let token = meta["progressToken"].clone();
                    say(
                        &writer,
                        json!({"jsonrpc": "2.0", "method": "notifications/progress",
                        "params": {"progressToken": token, "progress": 1, "total": 2,
                                   "message": "greeting"}}),
                    )
                    .await;
                    say(
                        &writer,
                        json!({"jsonrpc": "2.0", "method": "notifications/message",
                        "params": {"level": "info", "logger": "greeter", "data": "greeted"}}),
                    )
                    .await;
                    let name = responses["who"]["content"]["name"].as_str().unwrap_or("?");
                    text(format!("hello {name}"))
                }
                "greet" => ask(
                    json!({"who": {"method": "elicitation/create", "params": {
                        "mode": "form", "message": "Who?", "requestedSchema": {
                            "type": "object", "properties": {"name": {"type": "string"}}}}}}),
                    Some("s1"),
                ),
                "visit" if responses["page"].is_object() => {
                    text(format!("visited: {}", responses["page"]))
                }
                "visit" => ask(
                    json!({"page": {"method": "elicitation/create", "params": {
                        "mode": "url", "message": "Open the page",
                        "url": "https://example.com/set?step=1"}}}),
                    None,
                ),
                "chore" => json!({"resultType": "task", "taskId": "t1", "status": "working",
                    "createdAt": "2026-11-26T00:00:00Z", "lastUpdatedAt": "2026-11-26T00:00:00Z",
                    "ttlMs": null, "pollIntervalMs": 10}),
                "sample" => ask(
                    json!({"llm": {"method": "sampling/createMessage", "params": {
                        "messages": [], "maxTokens": 10}}}),
                    None,
                ),
                _ => continue,
            },
            "tasks/get" if updated => complete(json!({"taskId": "t1", "status": "completed",
                "result": {"content": [{"type": "text", "text": "chore done"}]}})),
            "tasks/get" => complete(json!({"taskId": "t1", "status": "input_required",
                "statusMessage": "waiting for approval", "pollIntervalMs": 10,
                "inputRequests": {"ok": {"method": "elicitation/create", "params": {
                    "mode": "form", "message": "Go on?", "requestedSchema": {
                        "type": "object", "properties": {"go": {"type": "boolean"}}}}}}})),
            "tasks/update" => {
                updated = true;
                complete(json!({}))
            }
            _ => complete(json!({})),
        };
        say(
            &writer,
            json!({"jsonrpc": "2.0", "id": id, "result": result}),
        )
        .await;
    }
}

/// A connection to `modern_server` that asks for log messages from `info`.
fn modern_connection(seen: Arc<Mutex<Vec<Value>>>) -> Arc<Connection> {
    let (client_end, server_end) = tokio::io::duplex(1 << 16);
    let (client_read, client_write) = tokio::io::split(client_end);
    let (server_read, server_write) = tokio::io::split(server_end);
    tokio::spawn(modern_server(server_read, server_write, seen));
    let mut connection = Connection::new("modern", client_read, client_write);
    connection.log_level = Some("info".into());
    Arc::new(connection)
}

#[tokio::test]
async fn a_server_of_the_new_revision_is_spoken_to_without_a_handshake() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let connection = modern_connection(seen.clone());
    connection.open().await.expect("open");
    assert_eq!(
        connection.era(),
        Some(Era::Modern(connection::MODERN_VERSION.into()))
    );
    let listing = connection.list_tools().await.expect("tools");
    assert_eq!(listing.ttl, Some(Duration::ZERO));
    assert_eq!(listing.tools.len(), 2);

    // The subscription brings the server's word that its tools changed.
    for _ in 0..100 {
        if connection.shared.tools_changed.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let slot = Slot::new(connection.clone(), None, listing);
    assert!(slot.stale(), "the server said its tools changed");
    // A list fresh for no time is listed again at every use.
    assert!(slot.stale(), "the list is fresh for no time");

    let seen = seen.lock().expect("seen").clone();
    let requests: Vec<&Value> = seen.iter().filter(|m| m.get("id").is_some()).collect();
    let mut methods: Vec<&str> = requests
        .iter()
        .filter_map(|m| m["method"].as_str())
        .collect();
    // The subscription is opened beside the list.
    methods[1..].sort_unstable();
    assert_eq!(
        methods,
        ["server/discover", "subscriptions/listen", "tools/list"]
    );
    for request in &requests {
        let meta = &request["params"]["_meta"];
        assert_eq!(
            meta["io.modelcontextprotocol/protocolVersion"],
            connection::MODERN_VERSION
        );
        assert_eq!(
            meta["io.modelcontextprotocol/clientInfo"]["name"],
            "inocode"
        );
        assert_eq!(
            meta["io.modelcontextprotocol/clientCapabilities"],
            json!({"elicitation": {"form": {}, "url": {}},
                   "extensions": {connection::TASKS: {}}})
        );
        assert_eq!(meta["io.modelcontextprotocol/logLevel"], "info");
    }
    let listen = requests
        .iter()
        .find(|m| m["method"] == "subscriptions/listen")
        .expect("listen");
    assert_eq!(
        listen["params"]["notifications"],
        json!({"toolsListChanged": true})
    );
}

/// What a call showed the person, as `(kind, value)`.
type Shown = Arc<Mutex<Vec<(String, Value)>>>;

/// Answers the questions of the turn's events as a person would: a name,
/// a visit, an approval; and keeps what was shown.
fn person(
    store: Store,
    session: crate::store::SessionId,
    mut events: mpsc::Receiver<crate::turn::TurnEvent>,
) -> Shown {
    let shown: Shown = Arc::default();
    let kept = shown.clone();
    tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            let (kind, value) = match event {
                crate::turn::TurnEvent::Elicitation(question) => {
                    let answer = match question["message"].as_str() {
                        Some("Who?") => json!({"action": "accept", "content": {"name": "Bo"}}),
                        Some("Go on?") => json!({"action": "accept", "content": {"go": true}}),
                        _ => json!({"action": "accept", "content": {}}),
                    };
                    let id = question["id"].as_u64().expect("id");
                    assert!(store.answer_elicitation(session, id, answer));
                    ("question", question)
                }
                crate::turn::TurnEvent::McpProgress(progress) => ("progress", progress),
                crate::turn::TurnEvent::McpLog(log) => ("log", log),
                _ => continue,
            };
            kept.lock().expect("shown").push((kind.to_string(), value));
        }
    });
    shown
}

#[tokio::test]
async fn input_progress_logs_and_tasks_of_a_new_server_reach_the_person() {
    let dir = tempfile::tempdir().expect("dir");
    let store = Store::open(dir.path().join("inocode.db"))
        .await
        .expect("store");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let connection = modern_connection(seen.clone());
    connection.open().await.expect("open");
    let session = crate::store::SessionId(7);
    let (events, received) = mpsc::channel(64);
    let shown = person(store.clone(), session, received);
    let asking = Asking {
        session,
        events: &events,
    };
    let call = |tool: &'static str| Call {
        store: &store,
        connection: &connection,
        tool,
        asking: Some(&asking),
    };
    let (_stop, cancel) = tokio::sync::watch::channel(false);
    let run = |tool: &'static str| {
        let cancel = cancel.clone();
        async move {
            call(tool)
                .run(&json!({}), &cancel)
                .await
                .map_err(|fault| fault.to_string())
        }
    };

    assert_eq!(run("greet").await, Ok("hello Bo".into()));
    assert_eq!(
        run("visit").await,
        Ok(r#"visited: {"action":"accept"}"#.into())
    );
    assert_eq!(run("chore").await, Ok("chore done".into()));
    let refused = run("sample").await.expect_err("sampling is not offered");
    assert!(refused.contains("sampling/createMessage"), "{refused}");

    // An interrupted call is given up, and the server told which one.
    let (stop, interrupted) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let _ = stop.send(true);
    });
    let hung = call("hang").run(&json!({}), &interrupted).await;
    assert_eq!(
        hung.map_err(|fault| fault.to_string()),
        Err("cancelled".into())
    );
    tokio::time::sleep(Duration::from_millis(100)).await;

    let seen = seen.lock().expect("seen").clone();
    let calls: Vec<&Value> = seen
        .iter()
        .filter(|m| m["method"] == "tools/call")
        .collect();
    let greets: Vec<&&Value> = calls
        .iter()
        .filter(|m| m["params"]["name"] == "greet")
        .collect();
    assert_eq!(greets.len(), 2);
    assert_ne!(
        greets[0]["id"], greets[1]["id"],
        "the retry is a request of its own"
    );
    assert_eq!(
        greets[1]["params"]["inputResponses"],
        json!({"who": {"action": "accept", "content": {"name": "Bo"}}})
    );
    assert_eq!(greets[1]["params"]["arguments"], json!({}));
    assert!(greets[0]["params"].get("requestState").is_none());
    let update = seen
        .iter()
        .find(|m| m["method"] == "tasks/update")
        .expect("the task's input");
    assert_eq!(
        update["params"]["inputResponses"],
        json!({"ok": {"action": "accept", "content": {"go": true}}})
    );
    let hang = calls
        .iter()
        .find(|m| m["params"]["name"] == "hang")
        .expect("the call that hung");
    let cancelled = seen
        .iter()
        .find(|m| m["method"] == "notifications/cancelled")
        .expect("the server is told");
    assert_eq!(cancelled["params"]["requestId"], hang["id"]);

    drop(events);
    let shown = shown.lock().expect("shown").clone();
    let questions: Vec<&Value> = shown
        .iter()
        .filter(|(kind, _)| kind == "question")
        .map(|(_, question)| question)
        .collect();
    assert_eq!(
        questions.len(),
        3,
        "the task's question is asked once: {questions:?}"
    );
    assert_eq!(questions[0]["server"], "modern");
    assert_eq!(questions[0]["mode"], "form");
    assert_eq!(
        questions[1],
        &json!({"id": questions[1]["id"], "server": "modern", "mode": "url",
                "message": "Open the page", "url": "https://example.com/set?step=1"})
    );
    assert!(shown.contains(&(
        "progress".into(),
        json!({"server": "modern", "tool": "greet", "progress": 1, "total": 2,
               "message": "greeting"})
    )));
    assert!(shown.contains(&(
        "log".into(),
        json!({"server": "modern", "level": "info", "logger": "greeter", "data": "greeted"})
    )));
    assert!(shown.iter().any(|(kind, value)| kind == "progress"
        && value["message"] == "waiting for approval"
        && value["status"] == "input_required"));
}

/// A stdio server of the 2024-11-05 revision that, as some older servers
/// do, ends at any request before `initialize`.
const ENDS_BEFORE_INITIALIZE: &str = r#"
import json, sys
first = True
for line in sys.stdin:
    message = json.loads(line)
    if first and message.get("method") != "initialize":
        sys.exit(1)
    first = False
    if "id" not in message or "method" not in message:
        continue
    method = message["method"]
    if method == "initialize":
        result = {"protocolVersion": "2024-11-05", "capabilities": {"tools": {}},
                  "serverInfo": {"name": "old", "version": "1"}}
    elif method == "tools/list":
        result = {"tools": [{"name": "add", "inputSchema": {"type": "object"}}]}
    else:
        a = message["params"]["arguments"]
        result = {"content": [{"type": "text", "text": str(a["a"] + a["b"])}]}
    print(json.dumps({"jsonrpc": "2.0", "id": message["id"], "result": result}), flush=True)
"#;

/// A stdio server of the stateless revision only: it opens a subscription
/// and, once `grow` is called, has a second tool and says so on it.
const GROWING_SERVER: &str = r#"
import json, sys
listen = None
grown = False
def say(message):
    print(json.dumps(message), flush=True)
for line in sys.stdin:
    message = json.loads(line)
    if "id" not in message or "method" not in message:
        continue
    meta = message.get("params", {}).get("_meta", {})
    if meta.get("io.modelcontextprotocol/protocolVersion") != "2026-07-28":
        say({"jsonrpc": "2.0", "id": message["id"], "error": {"code": -32601,
             "message": "this server speaks only 2026-07-28"}})
        continue
    method = message["method"]
    if method == "server/discover":
        result = {"supportedVersions": ["2026-07-28"],
                  "capabilities": {"tools": {"listChanged": True}}, "ttlMs": 0,
                  "cacheScope": "public"}
    elif method == "subscriptions/listen":
        listen = message["id"]
        say({"jsonrpc": "2.0", "method": "notifications/subscriptions/acknowledged",
             "params": {"_meta": {"io.modelcontextprotocol/subscriptionId": listen},
                        "notifications": {"toolsListChanged": True}}})
        continue
    elif method == "tools/list":
        tools = [{"name": "grow", "inputSchema": {"type": "object"}}]
        if grown:
            tools.append({"name": "sprout", "inputSchema": {"type": "object"}})
        result = {"tools": tools, "ttlMs": 3600000, "cacheScope": "public"}
    elif method == "tools/call":
        grown = True
        say({"jsonrpc": "2.0", "id": message["id"], "result": {"resultType": "complete",
             "content": [{"type": "text", "text": "grown"}]}})
        if listen is not None:
            say({"jsonrpc": "2.0", "method": "notifications/tools/list_changed",
                 "params": {"_meta": {"io.modelcontextprotocol/subscriptionId": listen}}})
        continue
    else:
        result = {}
    result["resultType"] = "complete"
    say({"jsonrpc": "2.0", "id": message["id"], "result": result})
"#;

#[tokio::test]
async fn new_and_old_servers_start_and_changed_tools_show_at_the_next_request() {
    let Some(python) = python() else {
        eprintln!("skipped: no Python to run the MCP servers");
        return;
    };
    let dir = tempfile::tempdir().expect("dir");
    let (old, new) = (dir.path().join("old.py"), dir.path().join("new.py"));
    std::fs::write(&old, ENDS_BEFORE_INITIALIZE).expect("script");
    std::fs::write(&new, GROWING_SERVER).expect("script");
    std::fs::write(
        dir.path().join("mcp.json"),
        json!({"mcpServers": {
            "new": {"command": python, "args": [new]},
            "old": {"command": python, "args": [old]},
            "loud": {"command": python, "args": [new], "log_level": "everything"}
        }})
        .to_string(),
    )
    .expect("config");
    let store = Store::open(dir.path().join("inocode.db"))
        .await
        .expect("store");
    let names =
        |specs: Vec<ToolSpec>| -> Vec<String> { specs.into_iter().map(|spec| spec.name).collect() };

    assert_eq!(
        names(specs(&store, &[]).await),
        ["mcp__new__grow", "mcp__old__add"]
    );
    let problems = render_problems(&store).await.expect("problems");
    assert!(
        problems.contains("server loud did not start: log_level must be one of"),
        "{problems}"
    );
    let (_cancel, cancel) = tokio::sync::watch::channel(false);
    assert_eq!(
        call(
            &store,
            "mcp__old__add",
            r#"{"a": 1, "b": 2}"#,
            cancel.clone(),
            None
        )
        .await,
        Some(Ok("3".into()))
    );
    assert_eq!(
        call(&store, "mcp__new__grow", "{}", cancel.clone(), None).await,
        Some(Ok("grown".into()))
    );
    let mut offered = Vec::new();
    for _ in 0..100 {
        offered = names(specs(&store, &[]).await);
        if offered.len() == 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert_eq!(
        offered,
        ["mcp__new__grow", "mcp__new__sprout", "mcp__old__add"]
    );
    assert_eq!(
        call(&store, "mcp__new__sprout", "{}", cancel, None).await,
        Some(Ok("grown".into()))
    );
}

/// A stdio server that answers `server/discover` with nothing, as some
/// older servers do with an unknown request, or one of the stateless
/// revision that reads its first message only after `delay`.
async fn late_server(
    reader: impl AsyncRead + Unpin,
    writer: impl AsyncWrite + Unpin,
    modern: bool,
    delay: Duration,
) {
    tokio::time::sleep(delay).await;
    let writer = tokio::sync::Mutex::new(writer);
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let message: Value = serde_json::from_str(&line).expect("json");
        let (Some(method), Some(id)) = (message["method"].as_str(), message.get("id").cloned())
        else {
            continue;
        };
        let answer = match (method, modern) {
            ("server/discover", false) => continue,
            ("server/discover", true) => json!({"jsonrpc": "2.0", "id": id, "result": {
                "resultType": "complete", "supportedVersions": [connection::MODERN_VERSION],
                "capabilities": {"tools": {}}, "ttlMs": 0, "cacheScope": "public"}}),
            ("initialize", false) => json!({"jsonrpc": "2.0", "id": id, "result": {
                "protocolVersion": "2025-03-26", "capabilities": {"tools": {}},
                "serverInfo": {"name": "quiet", "version": "1"}}}),
            ("initialize", true) => json!({"jsonrpc": "2.0", "id": id, "error": {
                "code": -32601, "message": "this server speaks only 2026-07-28"}}),
            _ => json!({"jsonrpc": "2.0", "id": id, "result": {"tools": []}}),
        };
        say(&writer, answer).await;
    }
}

#[tokio::test]
async fn a_probe_left_unanswered_is_raced_with_initialize() {
    let open = |modern: bool, delay: Duration| async move {
        let (client_end, server_end) = tokio::io::duplex(1 << 16);
        let (client_read, client_write) = tokio::io::split(client_end);
        let (server_read, server_write) = tokio::io::split(server_end);
        tokio::spawn(late_server(server_read, server_write, modern, delay));
        let connection = Connection::new("late", client_read, client_write);
        connection.open().await.map(|()| connection.era())
    };
    let (quiet, slow) = tokio::join!(
        open(false, Duration::ZERO),
        open(true, Duration::from_secs(6))
    );
    assert_eq!(
        quiet.expect("quiet"),
        Some(Era::Legacy("2025-03-26".into()))
    );
    assert_eq!(
        slow.expect("slow"),
        Some(Era::Modern(connection::MODERN_VERSION.into()))
    );
}

/// One HTTP request a fake server got; header names are lowercase.
#[derive(Clone, Debug)]
struct Got {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: Value,
}

/// What a fake server answers: a whole response, or a head and then the
/// chunks of a stream until their sender is dropped.
enum Reply {
    Whole(String),
    Stream(mpsc::UnboundedReceiver<String>),
}

fn empty(status: &str) -> Reply {
    Reply::Whole(format!(
        "HTTP/1.1 {status}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
    ))
}

fn json_reply(status: &str, extra: &str, value: Value) -> Reply {
    let text = value.to_string();
    Reply::Whole(format!(
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\n{extra}content-length: {}\r\n\
         connection: close\r\n\r\n{text}",
        text.len()
    ))
}

/// A stream of the given events, ended after them.
fn events(chunks: &[String]) -> Reply {
    let (sender, receiver) = mpsc::unbounded_channel();
    for chunk in chunks {
        let _ = sender.send(chunk.clone());
    }
    Reply::Stream(receiver)
}

/// An HTTP server that answers each request on its own connection with
/// what `handle` makes of it, and keeps every request.
async fn serve(
    seen: Arc<Mutex<Vec<Got>>>,
    handle: impl Fn(&Got) -> Reply + Send + Sync + 'static,
) -> String {
    use tokio::io::AsyncReadExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("address");
    let handle = Arc::new(handle);
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let (seen, handle) = (seen.clone(), handle.clone());
            tokio::spawn(async move {
                let mut buffer = Vec::new();
                let mut chunk = [0u8; 4096];
                let end = loop {
                    if let Some(at) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                        break at + 4;
                    }
                    let read = stream.read(&mut chunk).await.unwrap_or(0);
                    if read == 0 {
                        return;
                    }
                    buffer.extend_from_slice(&chunk[..read]);
                };
                let head = String::from_utf8_lossy(&buffer[..end]).to_string();
                let mut lines = head.split("\r\n");
                let mut first = lines.next().unwrap_or("").split(' ');
                let method = first.next().unwrap_or("").to_string();
                let path = first.next().unwrap_or("").to_string();
                let headers: HashMap<String, String> = lines
                    .filter_map(|line| line.split_once(':'))
                    .map(|(name, value)| (name.to_lowercase(), value.trim().to_string()))
                    .collect();
                let length: usize = headers
                    .get("content-length")
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(0);
                while buffer.len() < end + length {
                    let read = stream.read(&mut chunk).await.unwrap_or(0);
                    if read == 0 {
                        return;
                    }
                    buffer.extend_from_slice(&chunk[..read]);
                }
                let body =
                    serde_json::from_slice(&buffer[end..end + length]).unwrap_or(Value::Null);
                let got = Got {
                    method,
                    path,
                    headers,
                    body,
                };
                seen.lock().expect("seen").push(got.clone());
                match handle(&got) {
                    Reply::Whole(text) => {
                        let _ = stream.write_all(text.as_bytes()).await;
                    }
                    Reply::Stream(mut chunks) => {
                        let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                                    connection: close\r\n\r\n";
                        let _ = stream.write_all(head.as_bytes()).await;
                        while let Some(chunk) = chunks.recv().await {
                            if stream.write_all(chunk.as_bytes()).await.is_err() {
                                return;
                            }
                        }
                    }
                }
                let _ = stream.shutdown().await;
            });
        }
    });
    format!("http://{address}/mcp")
}

/// A message as one SSE event.
fn event(message: &Value) -> String {
    format!("event: message\ndata: {message}\n\n")
}

#[tokio::test]
async fn a_server_of_the_old_http_and_sse_transport_is_reached() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let stream: Arc<Mutex<Option<mpsc::UnboundedSender<String>>>> = Arc::default();
    let kept = stream.clone();
    let url = serve(seen.clone(), move |got| {
        match (got.method.as_str(), got.path.as_str()) {
            ("POST", "/mcp") => empty("405 Method Not Allowed"),
            ("GET", "/mcp") => {
                let (sender, receiver) = mpsc::unbounded_channel();
                let _ = sender.send("event: endpoint\ndata: /messages?sessionId=abc\n\n".into());
                *kept.lock().expect("stream") = Some(sender);
                Reply::Stream(receiver)
            }
            ("POST", "/messages?sessionId=abc") => {
                let id = got.body["id"].clone();
                let result = match got.body["method"].as_str() {
                    Some("initialize") => json!({"protocolVersion": "2024-11-05",
                    "capabilities": {"tools": {}}, "serverInfo": {"name": "sse", "version": "1"}}),
                    Some("tools/list") => {
                        json!({"tools": [{"name": "echo", "inputSchema": {"type": "object"}}]})
                    }
                    Some("tools/call") => {
                        json!({"content": [{"type": "text", "text": "sse echoed"}]})
                    }
                    _ => return empty("202 Accepted"),
                };
                let answer = json!({"jsonrpc": "2.0", "id": id, "result": result});
                if let Some(stream) = kept.lock().expect("stream").as_ref() {
                    let _ = stream.send(event(&answer));
                }
                empty("202 Accepted")
            }
            _ => empty("404 Not Found"),
        }
    })
    .await;
    let settings: ServerConfig = serde_json::from_value(json!({"url": url})).expect("settings");
    let connection = connect("sse", &settings).expect("connect");
    connection.open().await.expect("open");
    assert_eq!(connection.era(), Some(Era::Legacy("2024-11-05".into())));
    let listing = connection.list_tools().await.expect("tools");
    assert_eq!(listing.tools.len(), 1);
    let echoed = connection
        .request("tools/call", json!({"name": "echo", "arguments": {}}))
        .await
        .map_err(|fault| fault.to_string())
        .and_then(|result| render(&result));
    assert_eq!(echoed, Ok("sse echoed".into()));

    let seen = seen.lock().expect("seen").clone();
    let order: Vec<(String, String, Option<String>)> = seen
        .iter()
        .map(|got| {
            (
                got.method.clone(),
                got.path.clone(),
                got.body["method"].as_str().map(str::to_string),
            )
        })
        .collect();
    let posted = |method: &str| {
        (
            "POST".to_string(),
            "/messages?sessionId=abc".to_string(),
            Some(method.to_string()),
        )
    };
    assert_eq!(
        order,
        [
            ("POST".into(), "/mcp".into(), Some("server/discover".into())),
            ("POST".into(), "/mcp".into(), Some("initialize".into())),
            ("GET".into(), "/mcp".into(), None),
            posted("initialize"),
            posted("notifications/initialized"),
            posted("tools/list"),
            posted("tools/call"),
        ]
    );
    drop(stream);
}

#[tokio::test]
async fn an_older_http_stream_is_resumed_and_its_get_stream_tells_of_changes() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let listed: Arc<Mutex<Value>> = Arc::default();
    let kept = listed.clone();
    let url = serve(seen.clone(), move |got| {
        let changed = json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"});
        match (got.method.as_str(), got.body["method"].as_str()) {
            ("POST", Some("server/discover")) => json_reply(
                "400 Bad Request",
                "",
                json!({"jsonrpc": "2.0", "id": null,
                       "error": {"code": -32600, "message": "no session"}}),
            ),
            ("POST", Some("initialize")) => json_reply(
                "200 OK",
                "mcp-session-id: s-1\r\n",
                json!({"jsonrpc": "2.0", "id": got.body["id"], "result": {
                    "protocolVersion": "2025-11-25",
                    "capabilities": {"tools": {"listChanged": true}},
                    "serverInfo": {"name": "resume", "version": "1"}}}),
            ),
            // The stream of the list breaks after its first event.
            ("POST", Some("tools/list")) => {
                *kept.lock().expect("listed") = got.body["id"].clone();
                events(&["id: e1\ndata:\n\nretry: 10\n\n".into()])
            }
            ("GET", _) if got.headers.get("last-event-id").map(String::as_str) == Some("e1") => {
                let answer = json!({"jsonrpc": "2.0", "id": *kept.lock().expect("listed"),
                    "result": {"tools": [{"name": "echo", "inputSchema": {"type": "object"}}]}});
                events(&[format!("id: e2\ndata: {answer}\n\n")])
            }
            ("GET", _) => events(&[format!("retry: 60000\nid: g1\ndata: {changed}\n\n")]),
            ("DELETE", _) => empty("200 OK"),
            _ => empty("202 Accepted"),
        }
    })
    .await;
    let settings: ServerConfig = serde_json::from_value(json!({"url": url})).expect("settings");
    let connection = connect("resume", &settings).expect("connect");
    connection.open().await.expect("open");
    let listing = connection.list_tools().await.expect("tools");
    assert_eq!(listing.tools[0]["name"], "echo");
    for _ in 0..100 {
        if connection.shared.tools_changed.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        connection.shared.tools_changed.load(Ordering::SeqCst),
        "the GET stream said the tools changed"
    );
    let seen = seen.lock().expect("seen").clone();
    let resumed = seen
        .iter()
        .find(|got| got.headers.contains_key("last-event-id"))
        .expect("the list's stream was resumed");
    assert_eq!(resumed.method, "GET");
    assert_eq!(resumed.headers["last-event-id"], "e1");
    assert_eq!(resumed.headers["mcp-session-id"], "s-1");
    assert_eq!(
        resumed.headers["mcp-protocol-version"],
        connection::LEGACY_VERSION
    );
    assert!(seen.iter().any(|got| got.method == "GET"
        && !got.headers.contains_key("last-event-id")
        && got.headers["accept"] == "text/event-stream"));
}

#[tokio::test]
async fn a_new_http_server_gets_its_headers_and_bad_tools_are_left_out() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let url = serve(seen.clone(), |got| {
        let id = got.body["id"].clone();
        let version = &got.body["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"];
        if got.headers.get("mcp-protocol-version").map(String::as_str) != version.as_str()
            || got.headers.get("mcp-method").map(String::as_str) != got.body["method"].as_str()
        {
            return json_reply(
                "400 Bad Request",
                "",
                json!({"jsonrpc": "2.0", "id": id,
                       "error": {"code": -32020, "message": "Header mismatch"}}),
            );
        }
        let result = match got.body["method"].as_str() {
            Some("server/discover") => json!({"resultType": "complete",
                "supportedVersions": [connection::MODERN_VERSION],
                "capabilities": {"tools": {}}, "ttlMs": 1000, "cacheScope": "public"}),
            Some("tools/list") => json!({"resultType": "complete", "tools": [
                {"name": "query", "inputSchema": {"type": "object", "properties": {
                    "region": {"type": "string", "x-mcp-header": "Region"},
                    "q": {"type": "string"}}}},
                {"name": "bad", "inputSchema": {"type": "object", "properties": {
                    "n": {"type": "number", "x-mcp-header": "N"}}}},
                {"name": "名前", "inputSchema": {"type": "object"}}],
                "ttlMs": 1000, "cacheScope": "public"}),
            Some("tools/call") => {
                let header = |name: &str| got.headers.get(name).cloned().unwrap_or_default();
                let text = format!(
                    "region={} name={}",
                    header("mcp-param-region"),
                    header("mcp-name")
                );
                let answer = json!({"jsonrpc": "2.0", "id": id, "result": {
                    "resultType": "complete", "content": [{"type": "text", "text": text}]}});
                return events(&[event(&answer)]);
            }
            _ => json!({"resultType": "complete"}),
        };
        json_reply(
            "200 OK",
            "",
            json!({"jsonrpc": "2.0", "id": id, "result": result}),
        )
    })
    .await;
    let settings: ServerConfig = serde_json::from_value(json!({"url": url})).expect("settings");
    let connection = connect("cloud", &settings).expect("connect");
    connection.open().await.expect("open");
    assert_eq!(
        connection.era(),
        Some(Era::Modern(connection::MODERN_VERSION.into()))
    );
    let listing = connection.list_tools().await.expect("tools");
    let names: Vec<&str> = listing
        .tools
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert_eq!(names, ["query", "名前"]);
    assert_eq!(listing.ttl, Some(Duration::from_secs(1)));
    assert!(
        listing.rejected[0].contains("tool bad of server cloud is left out"),
        "{:?}",
        listing.rejected
    );
    let call = |tool: &str, arguments: Value| {
        let connection = &connection;
        let tool = tool.to_string();
        async move {
            let headers = connection.call_headers(&tool, &arguments);
            connection
                .exchange(
                    connection.next_id(),
                    "tools/call",
                    json!({"name": tool, "arguments": arguments}),
                    &headers,
                )
                .await
                .map_err(|fault| fault.to_string())
                .and_then(|result| render(&result))
        }
    };
    assert_eq!(
        call("query", json!({"region": "us-west1", "q": "x"})).await,
        Ok("region=us-west1 name=query".into())
    );
    assert_eq!(
        call("query", json!({"q": "x"})).await,
        Ok("region= name=query".into())
    );
    assert_eq!(
        call("名前", json!({})).await,
        Ok("region= name==?base64?5ZCN5YmN?=".into())
    );
    let seen = seen.lock().expect("seen").clone();
    assert!(seen
        .iter()
        .all(|got| !got.headers.contains_key("mcp-session-id")));
}
