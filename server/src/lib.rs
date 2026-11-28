//! Standalone TCP server for InoCode.
//!
//! The server owns the transport: it accepts connections, decodes JSON request
//! frames, and dispatches them to [`inocode_core::Core`]. Immediate methods
//! (`ping`, `echo`, `cancel`, `turn/steer`) are answered with a response
//! frame; `chat` and `turn/start` run in their own task and report progress
//! as `started`, `delta`, `message` (turns only), `completed`, `failed`, and
//! `cancelled` events. `cancel` also interrupts a turn, and `turn/interrupt`
//! interrupts a thread's running turn from any connection.
//!
//! Every connection first proves it knows the token of this server's start
//! with an `auth` request; a connection that does not is answered
//! `unauthorized` and closed (docs/evidence/server-auth.md).
//!
//! [`run_remote`] serves the same requests and events over WebSocket for
//! remote control, one frame per text message (docs/evidence/remote-access.md).
//!
//! Each connection has one writer task fed by a bounded queue, so frames from
//! concurrent requests never interleave and a client that stops reading cannot
//! grow the queue without limit.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use inocode::{FrameReader, FrameType, FrameWriter, ProtocolError, DEFAULT_MAX_PAYLOAD_LEN};
use inocode_core::turn::{Ending, TurnEvent};
use inocode_core::{Core, ErrorInfo, EventPayload, Request, Response, ResponseBody};
use serde_json::{json, Value};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch, Mutex};
use tokio_tungstenite::tungstenite::error::{CapacityError, Error as WsError};
use tokio_tungstenite::tungstenite::handshake::server::{
    ErrorResponse, Request as HandshakeRequest, Response as HandshakeResponse,
};
use tokio_tungstenite::tungstenite::http::header::ORIGIN;
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

/// Frames buffered per connection before a slow client blocks its senders.
const SEND_QUEUE_CAPACITY: usize = 64;

/// Messages `thread/read` returns when the request names no `limit`, and the
/// most it accepts.
const READ_DEFAULT_LIMIT: u64 = 100;
const READ_MAX_LIMIT: u64 = 1000;
const SEARCH_DEFAULT_LIMIT: u64 = 20;
const SEARCH_MAX_LIMIT: u64 = 100;

/// Pause after an accept error that is not about a single connection.
const ACCEPT_ERROR_PAUSE: std::time::Duration = std::time::Duration::from_secs(1);

/// How long a new connection may take to send its `auth` request.
const AUTH_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// How long the frames queued before a connection is closed for a protocol
/// error may take to be written.
const CLOSE_FLUSH: std::time::Duration = std::time::Duration::from_secs(2);

struct Outgoing {
    stream_id: u64,
    payload: Payload,
}

/// A frame body waiting for the writer. Chat events stay typed until they
/// are serialized into the frame, so a delta is not first copied into a
/// `serde_json::Value` tree.
enum Payload {
    Value(Value),
    Event(Box<EventPayload>),
}

/// Registry entry of one generation on a connection.
///
/// `Running` holds the cancel signal. `Settling` marks an entry whose terminal
/// event has already been chosen, so a cancel request arriving from then on
/// reports `false` instead of racing with the outcome. Only the generation
/// task removes its entry, and only after its terminal event has been queued,
/// so a request id stays in use until its previous request is done.
enum Generation {
    Running(watch::Sender<bool>),
    Settling,
}

/// Cancel handles of the generations running on one connection.
type InFlight = Arc<Mutex<HashMap<u64, Generation>>>;

/// Accepts connections until `shutdown` completes, then returns. Accept
/// errors never end the server. Each connection must first send an `auth`
/// request carrying `token`.
///
/// Each connection runs in its own task, so one client closing or sending a
/// broken frame does not affect the others.
pub async fn run<F>(
    listener: TcpListener,
    core: Arc<Core>,
    token: String,
    shutdown: F,
) -> std::io::Result<()>
where
    F: Future<Output = ()> + Send,
{
    let token: Arc<str> = Arc::from(token);
    accept(listener, shutdown, move |stream| {
        let (read_half, write_half) = stream.into_split();
        serve_connection(
            Source::Tcp(FrameReader::new(read_half)),
            Sink::Tcp(FrameWriter::new(write_half)),
            Arc::clone(&core),
            Admission::Token(Arc::clone(&token)),
        )
    })
    .await
}

/// Accepts remote WebSocket connections until `shutdown` completes, then
/// returns (docs/evidence/remote-access.md). Each text message carries one
/// frame of the TCP protocol as `{"stream_id": n, "message": {...}}`, and
/// the connection is served like a TCP one. With `password_hash`, the first
/// request must be `auth` with the password; without it every connection
/// is served. An upgrade request with an `Origin` header, which browsers
/// send, is refused with 403.
pub async fn run_remote<F>(
    listener: TcpListener,
    core: Arc<Core>,
    password_hash: Option<String>,
    shutdown: F,
) -> std::io::Result<()>
where
    F: Future<Output = ()> + Send,
{
    let admission = match password_hash {
        Some(hash) => Admission::Password(Arc::from(hash)),
        None => Admission::Open,
    };
    accept(listener, shutdown, move |stream| {
        serve_remote(stream, Arc::clone(&core), admission.clone())
    })
    .await
}

async fn accept<F, S, C>(listener: TcpListener, shutdown: F, serve: S) -> std::io::Result<()>
where
    F: Future<Output = ()> + Send,
    S: Fn(TcpStream) -> C,
    C: Future<Output = ()> + Send + 'static,
{
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let stream = match accepted {
                    Ok((stream, _)) => stream,
                    Err(error) if is_connection_error(&error) => continue,
                    Err(_) => {
                        // Resource exhaustion such as EMFILE clears as other
                        // connections close; pausing keeps the loop from
                        // spinning on the same error (axum `serve`).
                        tokio::select! {
                            () = tokio::time::sleep(ACCEPT_ERROR_PAUSE) => continue,
                            () = &mut shutdown => return Ok(()),
                        }
                    }
                };
                // A socket option failure affects only this connection: drop
                // it and keep serving the others instead of ending the server.
                if stream.set_nodelay(true).is_err() {
                    continue;
                }
                tokio::spawn(serve(stream));
            }
            () = &mut shutdown => return Ok(()),
        }
    }
}

/// Errors that concern only the connection being accepted, not the listener.
fn is_connection_error(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::ConnectionRefused
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::ConnectionReset
    )
}

/// The room a WebSocket message has besides the frame it carries:
/// `{"stream_id":18446744073709551615,"message":}` and some white space.
const ENVELOPE_ALLOWANCE: usize = 256;

async fn serve_remote(stream: TcpStream, core: Arc<Core>, admission: Admission) {
    let limit = DEFAULT_MAX_PAYLOAD_LEN + ENVELOPE_ALLOWANCE;
    let config = WebSocketConfig::default()
        .max_message_size(Some(limit))
        .max_frame_size(Some(limit));
    let handshake =
        tokio_tungstenite::accept_hdr_async_with_config(stream, refuse_origin, Some(config));
    let Ok(Ok(socket)) = tokio::time::timeout(AUTH_WAIT, handshake).await else {
        return;
    };
    let (sink, source) = socket.split();
    serve_connection(
        Source::WebSocket(source),
        Sink::WebSocket(sink),
        core,
        admission,
    )
    .await;
}

/// Browsers send `Origin` on every WebSocket upgrade and native clients
/// need not, so refusing it keeps a web page in the person's browser from
/// driving the agent through the listener, which also answers on loopback.
// The signature is tungstenite's handshake callback.
#[allow(clippy::result_large_err)]
fn refuse_origin(
    request: &HandshakeRequest,
    response: HandshakeResponse,
) -> Result<HandshakeResponse, ErrorResponse> {
    if request.headers().contains_key(ORIGIN) {
        let mut refused = ErrorResponse::new(Some(
            "requests with an Origin header are refused".to_string(),
        ));
        *refused.status_mut() = StatusCode::FORBIDDEN;
        return Err(refused);
    }
    Ok(response)
}

/// What the first request of a connection must prove.
#[derive(Clone)]
enum Admission {
    /// The loopback listener: an `auth` request with the token of this start.
    Token(Arc<str>),
    /// Remote access with a password: an `auth` request with the password
    /// whose hash this is.
    Password(Arc<str>),
    /// Remote access without a password: every connection is served.
    Open,
}

/// Where a connection's requests are read from.
enum Source {
    Tcp(FrameReader<OwnedReadHalf>),
    WebSocket(SplitStream<WebSocketStream<TcpStream>>),
}

/// Where a connection's frames are written to.
enum Sink {
    Tcp(FrameWriter<OwnedWriteHalf>),
    WebSocket(SplitSink<WebSocketStream<TcpStream>, Message>),
}

/// One thing read from a connection.
enum Received {
    Request {
        stream_id: u64,
        request: Value,
    },
    /// Not a request: `error` is answered on `stream_id` and the connection
    /// goes on.
    Refused {
        stream_id: u64,
        error: Value,
    },
    /// A frame over the size limit, which the connection cannot read past:
    /// `error` is answered and the connection ends.
    TooLarge(Value),
    /// The client closed the connection, or sent what can never be read.
    End,
}

fn too_large_to_read(payload_len: usize, max: usize) -> Received {
    Received::TooLarge(error_value(
        "frame_too_large",
        &format!(
            "a frame of {payload_len} bytes is over the limit of {max} bytes; \
             the connection is closed"
        ),
    ))
}

fn only_json(stream_id: u64) -> Received {
    Received::Refused {
        stream_id,
        error: error_value("invalid_request", "Only JSON request frames are accepted"),
    }
}

impl Source {
    async fn next(&mut self) -> Received {
        match self {
            Source::Tcp(reader) => {
                let header = match reader.read_header().await {
                    Ok(Some(header)) => header,
                    Err(ProtocolError::PayloadTooLarge { payload_len, max }) => {
                        return too_large_to_read(payload_len, max)
                    }
                    // Clean close, or a frame that can never complete.
                    Ok(None) | Err(_) => return Received::End,
                };
                if header.frame_type != FrameType::Json {
                    let mut discard = tokio::io::sink();
                    if reader.read_payload_to(&header, &mut discard).await.is_err() {
                        return Received::End;
                    }
                    return only_json(header.stream_id);
                }
                let Ok(payload) = reader.read_payload_to_vec(&header).await else {
                    return Received::End;
                };
                match serde_json::from_slice(&payload) {
                    Ok(request) => Received::Request {
                        stream_id: header.stream_id,
                        request,
                    },
                    Err(_) => Received::Refused {
                        stream_id: header.stream_id,
                        error: error_value("invalid_json", "Invalid JSON"),
                    },
                }
            }
            Source::WebSocket(stream) => loop {
                match stream.next().await {
                    Some(Ok(Message::Text(text))) => return unwrap_envelope(text.as_str()),
                    Some(Ok(Message::Binary(_))) => return only_json(0),
                    // Pings are answered by the WebSocket layer.
                    Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {}
                    Some(Err(WsError::Capacity(CapacityError::MessageTooLong {
                        size,
                        max_size,
                    }))) => return too_large_to_read(size, max_size),
                    Some(Ok(Message::Close(_)) | Err(_)) | None => return Received::End,
                }
            },
        }
    }
}

/// Takes the frame out of a WebSocket message
/// `{"stream_id": n, "message": {...}}`. A message without that shape is
/// answered on stream 0, as its stream is not known.
fn unwrap_envelope(text: &str) -> Received {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return Received::Refused {
            stream_id: 0,
            error: error_value("invalid_json", "Invalid JSON"),
        };
    };
    if let Value::Object(mut envelope) = value {
        let stream_id = envelope.get("stream_id").and_then(Value::as_u64);
        let request = envelope.remove("message");
        if let (Some(stream_id), Some(request), 1) = (stream_id, request, envelope.len()) {
            return Received::Request { stream_id, request };
        }
    }
    Received::Refused {
        stream_id: 0,
        error: error_value(
            "invalid_request",
            "a WebSocket message is {\"stream_id\": n, \"message\": {...}}",
        ),
    }
}

impl Sink {
    /// Writes one frame. An error ends the connection's writer.
    async fn write(&mut self, message: &Outgoing) -> Result<(), ()> {
        match self {
            Sink::Tcp(writer) => {
                let written = match &message.payload {
                    Payload::Value(value) => writer.write_json(message.stream_id, value).await,
                    Payload::Event(event) => writer.write_json(message.stream_id, &**event).await,
                };
                match written {
                    Ok(()) => Ok(()),
                    // Nothing of the frame was written, so the connection
                    // stays usable: the client is told what it did not get.
                    Err(ProtocolError::PayloadTooLarge { payload_len, max }) => {
                        let replaced = too_large(&message.payload, payload_len, max);
                        writer
                            .write_json(message.stream_id, &replaced)
                            .await
                            .map_err(|_| ())
                    }
                    Err(_) => Err(()),
                }
            }
            Sink::WebSocket(sink) => {
                let body = match &message.payload {
                    Payload::Value(value) => serde_json::to_string(value),
                    Payload::Event(event) => serde_json::to_string(&**event),
                }
                .map_err(|_| ())?;
                // The same limit as a TCP frame's payload.
                let body = if body.len() > DEFAULT_MAX_PAYLOAD_LEN {
                    too_large(&message.payload, body.len(), DEFAULT_MAX_PAYLOAD_LEN).to_string()
                } else {
                    body
                };
                let text = format!("{{\"stream_id\":{},\"message\":{body}}}", message.stream_id);
                sink.send(Message::text(text)).await.map_err(|_| ())
            }
        }
    }

    /// Ends the connection after its last frame.
    async fn close(self) {
        if let Sink::WebSocket(mut sink) = self {
            let _ = sink.close().await;
        }
    }
}

async fn serve_connection(
    mut source: Source,
    mut sink: Sink,
    core: Arc<Core>,
    admission: Admission,
) {
    let (queue, mut queued) = mpsc::channel::<Outgoing>(SEND_QUEUE_CAPACITY);
    let writer_task = tokio::spawn(async move {
        while let Some(message) = queued.recv().await {
            if sink.write(&message).await.is_err() {
                return;
            }
        }
        sink.close().await;
    });

    let in_flight: InFlight = Arc::new(Mutex::new(HashMap::new()));
    let mut flush = authenticate(&mut source, &queue, &admission).await;
    if !flush {
        loop {
            let (stream_id, request) = match source.next().await {
                Received::Request { stream_id, request } => (stream_id, request),
                Received::Refused { stream_id, error } => {
                    if !send(&queue, stream_id, error).await {
                        break;
                    }
                    continue;
                }
                Received::TooLarge(error) => {
                    // The rest of the frame is not read, so the connection
                    // cannot go on; the client is told why it ends.
                    flush = send(&queue, 0, error).await;
                    break;
                }
                Received::End => break,
            };
            if !dispatch(request, stream_id, &core, &queue, &in_flight).await {
                break;
            }
        }
    }

    // The read side is over: signal every generation of this connection and
    // stop the writer at its next await point. Frames still queued are dropped
    // with the connection — the client closed it or sent an unreadable frame —
    // and awaiting a writer stalled on a dead socket would pin this task. An
    // error the connection ends on gets a short time to be written.
    in_flight.lock().await.clear();
    drop(queue);
    if flush {
        let abort = writer_task.abort_handle();
        if tokio::time::timeout(CLOSE_FLUSH, writer_task)
            .await
            .is_err()
        {
            abort.abort();
        }
    } else {
        writer_task.abort();
    }
}

/// Reads the first request of a connection, which must be an `auth`
/// request with what `admission` asks for, and answers it. Returns `true`
/// when the connection is refused: its answer is queued and nothing more
/// is read. A connection `admission` lets in without asking is not read.
async fn authenticate(
    source: &mut Source,
    queue: &mpsc::Sender<Outgoing>,
    admission: &Admission,
) -> bool {
    let (field, refusal) = match admission {
        Admission::Open => return false,
        Admission::Token(_) => (
            "token",
            "the first request must be auth with the token of this server's start",
        ),
        Admission::Password(_) => (
            "password",
            "the first request must be auth with the password of remote access",
        ),
    };
    let (stream_id, request) = match tokio::time::timeout(AUTH_WAIT, source.next()).await {
        Ok(Received::Request { stream_id, request }) => (stream_id, request),
        Ok(Received::Refused { stream_id, .. }) => (stream_id, Value::Null),
        Ok(Received::TooLarge(_) | Received::End) | Err(_) => return true,
    };
    let id = request.get("id").and_then(Value::as_u64);
    let given = match request.get("method").and_then(Value::as_str) {
        Some("auth") => request
            .get("params")
            .and_then(|params| params.get(field))
            .and_then(Value::as_str),
        _ => None,
    };
    let accepted = match (given, admission) {
        (None, _) | (_, Admission::Open) => false,
        (Some(given), Admission::Token(token)) => same_secret(given.as_bytes(), token.as_bytes()),
        (Some(given), Admission::Password(hash)) => {
            // Slow on purpose: off the runtime's threads.
            let (hash, given) = (Arc::clone(hash), given.to_string());
            tokio::task::spawn_blocking(move || {
                inocode_core::remote_password::verify(&hash, &given)
            })
            .await
            .unwrap_or(false)
        }
    };
    let body = if accepted {
        ResponseBody::Result {
            result: json!({ "authenticated": true }),
        }
    } else {
        ResponseBody::Error {
            error: ErrorInfo::new("unauthorized", refusal),
        }
    };
    let answer = serde_json::to_value(Response { id, body }).unwrap_or(Value::Null);
    let queued = send(queue, stream_id, answer).await;
    !(accepted && queued)
}

/// Compares two secrets in a time that does not depend on where they
/// first differ.
fn same_secret(given: &[u8], expected: &[u8]) -> bool {
    given.len() == expected.len()
        && given
            .iter()
            .zip(expected)
            .fold(0u8, |difference, (a, b)| difference | (a ^ b))
            == 0
}

/// What is sent in place of a frame over the size limit: an error answer
/// to the request, or for an event, a `dropped` event saying so.
fn too_large(payload: &Payload, payload_len: usize, max: usize) -> Value {
    let error = ErrorInfo::new(
        "frame_too_large",
        format!("the answer of {payload_len} bytes is over the frame limit of {max} bytes"),
    );
    match payload {
        Payload::Value(value) => json!({ "id": value.get("id"), "error": error }),
        Payload::Event(event) => json!({ "id": event.id, "event": "dropped", "error": error }),
    }
}

/// Returns `false` when the connection should stop being read.
async fn dispatch(
    value: Value,
    stream_id: u64,
    core: &Arc<Core>,
    queue: &mpsc::Sender<Outgoing>,
    in_flight: &InFlight,
) -> bool {
    let request: Request = match serde_json::from_value(value) {
        Ok(request) => request,
        Err(_) => {
            return send(
                queue,
                stream_id,
                error_value("invalid_request", "Invalid request"),
            )
            .await
        }
    };
    match request.method.as_str() {
        "chat" => start_chat(request, stream_id, core, queue, in_flight).await,
        "turn/start" => start_turn(request, stream_id, core, queue, in_flight).await,
        "turn/answer" => {
            let body = match (request.thread(), request.elicitation_answer()) {
                (Ok(thread), Ok((elicitation, answer))) => ResponseBody::Result {
                    result: json!({ "accepted": core.answer(thread, elicitation, answer) }),
                },
                (Err(error), _) | (_, Err(error)) => ResponseBody::Error { error },
            };
            let response = Response {
                id: Some(request.id),
                body,
            };
            send(
                queue,
                stream_id,
                serde_json::to_value(response).unwrap_or(Value::Null),
            )
            .await
        }
        "turn/interrupt" => {
            let body = match request.thread() {
                Ok(thread) => ResponseBody::Result {
                    result: json!({ "interrupted": core.interrupt(thread) }),
                },
                Err(error) => ResponseBody::Error { error },
            };
            let response = Response {
                id: Some(request.id),
                body,
            };
            send(
                queue,
                stream_id,
                serde_json::to_value(response).unwrap_or(Value::Null),
            )
            .await
        }
        "turn/steer" => {
            let body = match (request.thread(), request.prompt()) {
                (Ok(thread), Ok(prompt)) => ResponseBody::Result {
                    result: json!({ "accepted": core.steer(thread, prompt.to_string()) }),
                },
                (Err(error), _) | (_, Err(error)) => ResponseBody::Error { error },
            };
            let response = Response {
                id: Some(request.id),
                body,
            };
            send(
                queue,
                stream_id,
                serde_json::to_value(response).unwrap_or(Value::Null),
            )
            .await
        }
        "thread/start"
        | "thread/read"
        | "thread/fork"
        | "thread/rewind"
        | "thread/rename"
        | "thread/delete"
        | "thread/archive"
        | "thread/unarchive"
        | "thread/export"
        | "thread/search"
        | "thread/diff"
        | "goal/set"
        | "goal/read"
        | "goal/stop"
        | "thread/queue/add"
        | "thread/queue/list"
        | "thread/queue/update"
        | "thread/queue/delete"
        | "thread/queue/reorder"
        | "thread/queue/start"
        | "thread/jobs/list"
        | "thread/jobs/read"
        | "thread/jobs/write"
        | "thread/jobs/stop"
        | "plugins/list"
        | "plugins/search"
        | "plugins/install"
        | "plugins/remove"
        | "plugins/enable"
        | "plugins/disable"
        | "fs/readFile"
        | "fs/writeFile"
        | "fs/createDirectory"
        | "fs/getMetadata"
        | "fs/readDirectory"
        | "fs/remove"
        | "fs/copy"
        | "files/search" => {
            // Store work runs off the read loop, like discovery.
            tokio::spawn(thread_request(
                request,
                stream_id,
                Arc::clone(core),
                queue.clone(),
            ));
            true
        }
        "providers/list"
        | "providers/add"
        | "providers/update"
        | "providers/remove"
        | "models/list"
        | "models/visibility"
        | "thread/settings"
        | "settings/get"
        | "settings/set"
        | "accounts/list"
        | "accounts/login/start"
        | "accounts/login/code"
        | "accounts/login/cancel"
        | "accounts/login/status"
        | "accounts/rename"
        | "accounts/pause"
        | "accounts/remove"
        | "accounts/use"
        | "accounts/priority"
        | "accounts/clear_cooldown"
        | "accounts/add_key"
        | "pool/get"
        | "pool/set"
        | "pool/reset_affinity"
        | "pool/status"
        | "members/list"
        | "members/add"
        | "members/update"
        | "members/remove"
        | "allow/get"
        | "allow/set"
        | "allow/approve_repository" => {
            // Settings and credential files, and for models/list an HTTP
            // round trip: off the read loop like the store requests.
            tokio::spawn(settings_request(
                request,
                stream_id,
                Arc::clone(core),
                queue.clone(),
            ));
            true
        }
        "cancel" => cancel(request, stream_id, queue, in_flight).await,
        "command/run" => {
            // Commands read files and the store, and some start a turn.
            tokio::spawn(run_command(
                request,
                stream_id,
                Arc::clone(core),
                queue.clone(),
                Arc::clone(in_flight),
            ));
            true
        }
        "commands/list" | "thread/list" => {
            // Store and plugin files: off the read loop like the store requests.
            let (id, core, queue) = (request.id, Arc::clone(core), queue.clone());
            tokio::spawn(async move {
                let params = request.params.clone().unwrap_or_else(|| json!({}));
                let result = if request.method == "commands/list" {
                    match params.get("thread_id") {
                        None | Some(Value::Null) => core.commands(None).await,
                        Some(_) => match request.thread() {
                            Ok(thread) => core.commands(Some(thread)).await,
                            Err(error) => Err(error),
                        },
                    }
                } else {
                    list_threads(&params, &core).await
                };
                let payload = match result {
                    Ok(result) => json!({ "id": id, "result": result }),
                    Err(error) => json!({ "id": id, "error": error }),
                };
                let _ = send(&queue, stream_id, payload).await;
            });
            true
        }
        "mcp/resources/list"
        | "mcp/resources/templates/list"
        | "mcp/resources/read"
        | "mcp/complete"
        | "mcp/login/start"
        | "mcp/login/status"
        | "mcp/login/cancel"
        | "mcp/logout" => {
            // MCP servers may be started and asked over the network, and
            // sign-ins find their authorization servers: off the read loop,
            // like discovery.
            let (id, core, queue) = (request.id, Arc::clone(core), queue.clone());
            let method = request.method.clone();
            let params = request.params.clone().unwrap_or_else(|| json!({}));
            tokio::spawn(async move {
                let payload = match core.mcp_request(&method, &params).await {
                    Ok(result) => json!({ "id": id, "result": result }),
                    Err(error) => json!({ "id": id, "error": error }),
                };
                let _ = send(&queue, stream_id, payload).await;
            });
            true
        }
        "mcp/resources/subscribe" => {
            watch_resource(request, stream_id, core, queue, in_flight).await
        }
        "thread/titles/subscribe" => watch_titles(request, stream_id, core, queue, in_flight).await,
        "thread/subscribe" | "thread/status/subscribe" => {
            watch_threads(request, stream_id, core, queue, in_flight).await
        }
        "thread/jobs/watch" => watch_job(request, stream_id, core, queue, in_flight).await,
        "thread/btw" => ask_aside(request, stream_id, core, queue, in_flight).await,
        "models/catalog" => {
            // Fetches prices and scores: off the read loop, like discovery.
            let (id, core, queue) = (request.id, Arc::clone(core), queue.clone());
            tokio::spawn(async move {
                let params = request.params.clone().unwrap_or_else(|| json!({}));
                let provider = params["provider"].as_str();
                let include_hidden = params["include_hidden"].as_bool().unwrap_or(false);
                let payload = match core.model_catalog(provider, include_hidden).await {
                    Ok(catalog) => json!({ "id": id, "result": catalog }),
                    Err(error) => json!({ "id": id, "error": error }),
                };
                let _ = send(&queue, stream_id, payload).await;
            });
            true
        }
        "settings/translation" => {
            // A file write, off the read loop like the store requests.
            let (id, core, queue) = (request.id, Arc::clone(core), queue.clone());
            let patch = request.params.clone().unwrap_or_else(|| json!({}));
            tokio::spawn(async move {
                let payload = match core.set_translation(&patch).await {
                    Ok(settings) => json!({ "id": id, "result": settings }),
                    Err(error) => json!({ "id": id, "error": error }),
                };
                let _ = send(&queue, stream_id, payload).await;
            });
            true
        }
        "translation/models" | "translation/models/remove" | "translation/languages" => {
            // File and HTTP work, off the read loop like the store requests.
            let (id, core, queue) = (request.id, Arc::clone(core), queue.clone());
            let method = request.method.clone();
            let params = request.params.clone().unwrap_or_else(|| json!({}));
            tokio::spawn(async move {
                let result = match method.as_str() {
                    "translation/models" => core.translation_models().await,
                    "translation/models/remove" => core.remove_translation_model(&params).await,
                    _ => core.translation_languages(&params).await,
                };
                let payload = match result {
                    Ok(result) => json!({ "id": id, "result": result }),
                    Err(error) => json!({ "id": id, "error": error }),
                };
                let _ = send(&queue, stream_id, payload).await;
            });
            true
        }
        "stats/models" => {
            // A store read, off the read loop like the thread requests.
            tokio::spawn(model_stats(
                request,
                stream_id,
                Arc::clone(core),
                queue.clone(),
            ));
            true
        }
        "estimate" => {
            // Store reads, off the read loop like the thread requests.
            tokio::spawn(estimate(
                request,
                stream_id,
                Arc::clone(core),
                queue.clone(),
            ));
            true
        }
        "usage" => {
            // An HTTP round trip like discovery, so it runs beside the read loop.
            tokio::spawn(read_usage(
                request.id,
                stream_id,
                Arc::clone(core),
                queue.clone(),
            ));
            true
        }
        "models" => {
            // Discovery is an HTTP round trip: it runs beside the read loop,
            // like `chat`, so the connection keeps reading `cancel` and other
            // requests while the provider answers.
            let thread = match request
                .params
                .as_ref()
                .and_then(|params| params.get("thread_id"))
            {
                None | Some(Value::Null) => Ok(None),
                Some(_) => request.thread().map(Some),
            };
            let include_hidden = include_hidden(&request);
            let (id, core, queue) = (request.id, Arc::clone(core), queue.clone());
            tokio::spawn(async move {
                let models = match thread {
                    Ok(thread) => core.models(None, thread, include_hidden).await,
                    Err(error) => Err(error),
                };
                let _ = send(&queue, stream_id, models_payload(id, models)).await;
            });
            true
        }
        _ => {
            let response = core.handle(request);
            let payload = serde_json::to_value(response).unwrap_or(Value::Null);
            send(queue, stream_id, payload).await
        }
    }
}

/// Whether a model list includes the models the settings hide
/// (`include_hidden`, `false` when absent).
fn include_hidden(request: &Request) -> bool {
    request
        .params
        .as_ref()
        .and_then(|params| params.get("include_hidden"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// The answer to `models` and `models/list`.
fn models_payload(id: u64, models: Result<Vec<inocode_core::ModelInfo>, ErrorInfo>) -> Value {
    match models {
        Ok(models) => {
            let listed: Vec<Value> = models
                .iter()
                .map(|model| {
                    // A model without a published list may be asked for
                    // any effort; the provider reports what it refuses.
                    let levels = model.reasoning_levels.clone().unwrap_or_else(|| {
                        inocode_core::reasoning::ALL_EFFORTS
                            .iter()
                            .map(|effort| effort.as_str().to_string())
                            .collect()
                    });
                    json!({
                        "id": model.id,
                        "display_name": model.display_name,
                        "owned_by": model.owned_by,
                        "reasoning_levels": levels,
                        "default_reasoning": model.default_reasoning,
                    })
                })
                .collect();
            json!({ "id": id, "result": { "models": listed } })
        }
        Err(error) => {
            json!({ "id": id, "error": error })
        }
    }
}

async fn settings_request(
    request: Request,
    stream_id: u64,
    core: Arc<Core>,
    queue: mpsc::Sender<Outgoing>,
) {
    let params = request.params.clone().unwrap_or_else(|| json!({}));
    let payload = if request.method == "models/list" {
        let models = match params.get("provider") {
            Some(Value::String(provider)) => {
                core.models(Some(provider), None, include_hidden(&request))
                    .await
            }
            _ => Err(ErrorInfo::new(
                "invalid_params",
                "requires a \"provider\" string",
            )),
        };
        models_payload(request.id, models)
    } else {
        let result = match request.method.as_str() {
            "providers/list" => core.providers().await,
            "providers/add" => core.add_provider(&params).await,
            "providers/update" => core.update_provider(&params).await,
            "providers/remove" => core.remove_provider(&params).await,
            "settings/get" => core.get_settings().await,
            "settings/set" => core.set_settings(&params).await,
            "models/visibility" => core.set_model_visibility(&params).await,
            "accounts/list" => core.accounts_list(&params).await,
            "accounts/login/start" => core.login_start(&params).await,
            "accounts/login/code" => core.login_code(&params).await,
            "accounts/login/cancel" => core.login_cancel(&params).await,
            "accounts/login/status" => core.login_status(&params).await,
            "accounts/rename" => core.rename_account(&params).await,
            "accounts/pause" => core.pause_account(&params).await,
            "accounts/remove" => core.remove_account(&params).await,
            "accounts/use" => core.use_account(&params).await,
            "accounts/priority" => core.set_account_priority(&params).await,
            "accounts/clear_cooldown" => core.clear_account_cooldown(&params).await,
            "accounts/add_key" => core.add_key(&params).await,
            "pool/get" => core.pool_get(&params).await,
            "pool/set" => core.pool_set(&params).await,
            "pool/reset_affinity" => core.pool_reset_affinity(&params).await,
            "pool/status" => core.pool_status(&params).await,
            "members/list" => core.members().await,
            "members/add" => core.add_member(&params).await,
            "members/update" => core.update_member(&params).await,
            "members/remove" => core.remove_member(&params).await,
            "allow/get" => core.allow(&params).await,
            "allow/set" => core.set_allow(&params).await,
            "allow/approve_repository" => core.approve_repository(&params).await,
            // `thread/settings` with nothing but the thread reads; any
            // other field is a change.
            _ => match request.thread() {
                Ok(thread)
                    if params
                        .as_object()
                        .is_some_and(|fields| fields.keys().any(|name| name != "thread_id")) =>
                {
                    core.set_thread_settings(thread, &params).await
                }
                Ok(thread) => core.thread_settings(thread).await,
                Err(error) => Err(error),
            },
        };
        match result {
            Ok(result) => json!({ "id": request.id, "result": result }),
            Err(error) => json!({ "id": request.id, "error": error }),
        }
    };
    // A closed connection has nobody to answer.
    let _ = send(&queue, stream_id, payload).await;
}

async fn thread_request(
    request: Request,
    stream_id: u64,
    core: Arc<Core>,
    queue: mpsc::Sender<Outgoing>,
) {
    let result = match request.method.as_str() {
        "thread/start" => start_thread(&request, &core).await,
        "thread/fork" => fork_thread(&request, &core).await,
        "thread/rewind" => rewind_thread(&request, &core).await,
        "thread/rename" | "thread/delete" | "thread/archive" | "thread/unarchive"
        | "thread/export" | "thread/search" | "thread/diff" => {
            thread_operation(&request, &core).await
        }
        "goal/set" => set_goal(&request, &core).await,
        "files/search" => search_files(&request, &core).await,
        method if method.starts_with("thread/queue/") => queue_request(&request, &core).await,
        method if method.starts_with("thread/jobs/") => job_request(&request, &core).await,
        method if method.starts_with("plugins/") => plugin_request(&request, &core).await,
        method if method.starts_with("fs/") => {
            let request = request.clone();
            tokio::task::spawn_blocking(move || file_request(&request))
                .await
                .unwrap_or_else(|error| Err(ErrorInfo::new("internal", error.to_string())))
        }
        "goal/read" => match request.thread() {
            Ok(thread) => core.goal(thread).await.map(|goal| json!({ "goal": goal })),
            Err(error) => Err(error),
        },
        "goal/stop" => match request.thread() {
            Ok(thread) => core
                .stop_goal(thread)
                .await
                .map(|goal| json!({ "goal": goal })),
            Err(error) => Err(error),
        },
        _ => read_thread(&request, &core).await,
    };
    let payload = match result {
        Ok(result) => json!({ "id": request.id, "result": result }),
        Err(error) => json!({ "id": request.id, "error": error }),
    };
    // A closed connection has nobody to answer.
    let _ = send(&queue, stream_id, payload).await;
}

/// `thread/queue/*`: the messages lined up to be sent as the next turns.
/// `fs/*`: files by absolute path (docs/evidence/file-requests.md).
fn file_request(request: &Request) -> Result<Value, ErrorInfo> {
    use inocode_core::files;
    let field = |name: &str| {
        request
            .params
            .as_ref()
            .and_then(|params| params.get(name))
            .filter(|value| !value.is_null())
    };
    let path = |name: &str| {
        field(name)
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ErrorInfo::new("invalid_params", format!("requires a \"{name}\" string"))
            })
            .and_then(files::absolute)
    };
    let flag = |name: &str, default: bool| match field(name) {
        None => Ok(default),
        Some(value) => value.as_bool().ok_or_else(|| {
            ErrorInfo::new(
                "invalid_params",
                format!("\"{name}\" must be true or false"),
            )
        }),
    };
    let number = |name: &str| match field(name) {
        None => Ok(None),
        Some(value) => value.as_u64().map(Some).ok_or_else(|| {
            ErrorInfo::new("invalid_params", format!("\"{name}\" must be a byte count"))
        }),
    };
    match request.method.as_str() {
        "fs/readFile" => {
            let base64 = match field("encoding").map(|value| value.as_str()) {
                None | Some(Some("utf8")) => false,
                Some(Some("base64")) => true,
                Some(_) => {
                    return Err(ErrorInfo::new(
                        "invalid_params",
                        "\"encoding\" must be \"utf8\" or \"base64\"",
                    ))
                }
            };
            files::read_file(
                &path("path")?,
                number("offset")?.unwrap_or(0),
                number("limit")?,
                base64,
            )
        }
        "fs/writeFile" => {
            let text = |name: &str| field(name).and_then(Value::as_str).map(str::to_string);
            let contents = match (text("text"), text("data")) {
                (Some(text), None) => files::Contents::Text(text),
                (None, Some(data)) => files::Contents::Base64(data),
                _ => {
                    return Err(ErrorInfo::new(
                        "invalid_params",
                        "requires a \"text\" string or a base64 \"data\" string, not both",
                    ))
                }
            };
            files::write_file(&path("path")?, contents, flag("create_parents", false)?)
        }
        "fs/createDirectory" => files::create_directory(&path("path")?, flag("recursive", true)?),
        "fs/getMetadata" => files::metadata(&path("path")?).map(|meta| json!(meta)),
        "fs/readDirectory" => files::read_directory(&path("path")?),
        "fs/remove" => files::remove(&path("path")?, flag("recursive", false)?),
        "fs/copy" => files::copy(
            &path("from")?,
            &path("to")?,
            flag("recursive", false)?,
            flag("overwrite", false)?,
        ),
        _ => Err(ErrorInfo::new("method_not_found", "unknown fs method")),
    }
}

/// `plugins/*` (docs/evidence/plugin-management.md).
async fn plugin_request(request: &Request, core: &Arc<Core>) -> Result<Value, ErrorInfo> {
    let text = |name: &str| {
        request
            .params
            .as_ref()
            .and_then(|params| params.get(name))
            .and_then(Value::as_str)
            .filter(|text| !text.trim().is_empty())
            .ok_or_else(|| {
                ErrorInfo::new("invalid_params", format!("requires a \"{name}\" string"))
            })
    };
    match request.method.as_str() {
        "plugins/list" => core.plugins().await,
        "plugins/search" => core.search_plugins(text("query")?).await,
        "plugins/install" => core.install_plugin(text("plugin")?).await,
        "plugins/remove" => core.remove_plugin(text("plugin")?).await,
        "plugins/enable" => core.enable_plugin(text("plugin")?, true).await,
        "plugins/disable" => core.enable_plugin(text("plugin")?, false).await,
        _ => Err(ErrorInfo::new("method_not_found", "unknown plugins method")),
    }
}

/// The bytes of a job's log one `thread/jobs/read` gives at most.
const JOB_READ_DEFAULT: u64 = 64 * 1024;
const JOB_READ_MAX: u64 = 1024 * 1024;

fn job_id(request: &Request) -> Result<u64, ErrorInfo> {
    request
        .params
        .as_ref()
        .and_then(|params| params.get("job_id"))
        .and_then(Value::as_u64)
        .ok_or_else(|| ErrorInfo::new("invalid_params", "requires a \"job_id\" number"))
}

/// `thread/jobs/*`: the commands a thread's turns started
/// (docs/evidence/background-jobs.md).
async fn job_request(request: &Request, core: &Arc<Core>) -> Result<Value, ErrorInfo> {
    let thread = request.thread()?;
    let field = |name: &str| {
        request
            .params
            .as_ref()
            .and_then(|params| params.get(name))
            .filter(|value| !value.is_null())
    };
    match request.method.as_str() {
        "thread/jobs/list" => Ok(json!({ "jobs": core.jobs(thread).await? })),
        "thread/jobs/read" => {
            let offset = match field("offset") {
                None => 0,
                Some(value) => value.as_u64().ok_or_else(|| {
                    ErrorInfo::new("invalid_params", "\"offset\" must be a byte position")
                })?,
            };
            let limit = match field("limit") {
                None => JOB_READ_DEFAULT,
                Some(value) => value
                    .as_u64()
                    .filter(|limit| (1..=JOB_READ_MAX).contains(limit))
                    .ok_or_else(|| {
                        ErrorInfo::new(
                            "invalid_params",
                            format!("\"limit\" must be 1 to {JOB_READ_MAX}"),
                        )
                    })?,
            };
            let log = core
                .job_log(thread, job_id(request)?, offset, limit)
                .await?;
            Ok(json!(log))
        }
        "thread/jobs/write" => {
            let input = match field("input") {
                None => None,
                Some(value) => Some(value.as_str().ok_or_else(|| {
                    ErrorInfo::new("invalid_params", "\"input\" must be a string")
                })?),
            };
            let keys = match field("keys") {
                None => Vec::new(),
                Some(value) => value
                    .as_array()
                    .and_then(|keys| {
                        keys.iter()
                            .map(|key| key.as_str().map(str::to_string))
                            .collect::<Option<Vec<_>>>()
                    })
                    .ok_or_else(|| {
                        ErrorInfo::new("invalid_params", "\"keys\" must be a list of key names")
                    })?,
            };
            if input.is_none_or(str::is_empty) && keys.is_empty() {
                return Err(ErrorInfo::new(
                    "invalid_params",
                    "requires an \"input\" string or \"keys\"",
                ));
            }
            let written = core
                .write_job(thread, job_id(request)?, input, &keys)
                .await?;
            Ok(json!({ "written": written }))
        }
        "thread/jobs/stop" => {
            let stopped = core.stop_job(thread, job_id(request)?).await?;
            Ok(json!({ "stopped": stopped }))
        }
        _ => Err(ErrorInfo::new("method_not_found", "unknown jobs method")),
    }
}

/// `thread/jobs/watch {thread_id, job_id}`: a job's screen, then its
/// output as it comes (`output`, terminal text with its control
/// sequences), then `job_ended` and `completed` when it ends. A watcher
/// that falls behind by more than the job keeps gets `lagged` and misses
/// that output; the log keeps it all.
async fn watch_job(
    request: Request,
    stream_id: u64,
    core: &Arc<Core>,
    queue: &mpsc::Sender<Outgoing>,
    in_flight: &InFlight,
) -> bool {
    let id = request.id;
    let watched = match request.thread() {
        Ok(thread) => match job_id(&request) {
            Ok(job) => core.watch_job(thread, job).await,
            Err(error) => Err(error),
        },
        Err(error) => Err(error),
    };
    let watched = match watched {
        Ok(watched) => watched,
        Err(error) => {
            return send_event(queue, stream_id, EventPayload::failed(id, error))
                .await
                .is_ok()
        }
    };
    let mut pending = in_flight.lock().await;
    if pending.contains_key(&id) {
        drop(pending);
        let error = ErrorInfo::new("duplicate_id", "request id is already in flight");
        let event = EventPayload::failed(id, error);
        return send_event(queue, stream_id, event).await.is_ok();
    }
    let (cancel_tx, cancel_rx) = watch::channel(false);
    pending.insert(id, Generation::Running(cancel_tx));
    drop(pending);
    tokio::spawn(relay_job(
        id,
        watched,
        stream_id,
        queue.clone(),
        Arc::clone(in_flight),
        cancel_rx,
    ));
    true
}

/// `thread/btw {thread_id, question}`: a side question answered from the
/// thread's conversation without tools, while a turn may run. The answer
/// streams as `delta` events and ends with `completed`, whose `result`
/// holds its whole text; nothing is kept in the thread
/// (docs/evidence/side-question.md).
async fn ask_aside(
    request: Request,
    stream_id: u64,
    core: &Arc<Core>,
    queue: &mpsc::Sender<Outgoing>,
    in_flight: &InFlight,
) -> bool {
    let id = request.id;
    let parsed = request.thread().and_then(|thread| {
        request
            .params
            .as_ref()
            .and_then(|params| params.get("question"))
            .and_then(Value::as_str)
            .filter(|question| !question.trim().is_empty())
            .map(|question| (thread, question.to_string()))
            .ok_or_else(|| {
                ErrorInfo::new("invalid_params", "requires a non-empty \"question\" string")
            })
    });
    let (thread, question) = match parsed {
        Ok(parsed) => parsed,
        Err(error) => {
            return send_event(queue, stream_id, EventPayload::failed(id, error))
                .await
                .is_ok()
        }
    };
    let mut pending = in_flight.lock().await;
    if pending.contains_key(&id) {
        drop(pending);
        let error = ErrorInfo::new("duplicate_id", "request id is already in flight");
        let event = EventPayload::failed(id, error);
        return send_event(queue, stream_id, event).await.is_ok();
    }
    let (cancel_tx, cancel_rx) = watch::channel(false);
    pending.insert(id, Generation::Running(cancel_tx));
    drop(pending);
    let (core, queue, in_flight) = (Arc::clone(core), queue.clone(), Arc::clone(in_flight));
    tokio::spawn(async move {
        let mut cancelled = cancel_rx;
        let (deltas, mut texts) = mpsc::channel(64);
        let answer = core.aside(thread, &question, deltas, cancelled.clone());
        tokio::pin!(answer);
        let mut gone = false;
        let mut stopped = false;
        let started = deliver(&queue, stream_id, EventPayload::started(id), &mut cancelled).await;
        match started {
            Delivery::Sent => {}
            Delivery::ConnectionGone => gone = true,
            Delivery::Cancelled => stopped = true,
        }
        let answered = loop {
            tokio::select! {
                answered = &mut answer => break answered,
                Some(text) = texts.recv() => {
                    if gone || stopped {
                        continue;
                    }
                    match deliver(&queue, stream_id, EventPayload::delta(id, text), &mut cancelled).await {
                        Delivery::Sent => {}
                        Delivery::ConnectionGone => gone = true,
                        Delivery::Cancelled => stopped = true,
                    }
                }
            }
        };
        if gone {
            abandon(&in_flight, id).await;
            return;
        }
        while let Ok(text) = texts.try_recv() {
            if stopped {
                break;
            }
            if let Delivery::ConnectionGone = deliver(
                &queue,
                stream_id,
                EventPayload::delta(id, text),
                &mut cancelled,
            )
            .await
            {
                abandon(&in_flight, id).await;
                return;
            }
        }
        let terminal = match answered {
            _ if stopped => Terminal::Cancelled,
            Ok(text) => {
                let mut ended = EventPayload::completed(id);
                ended.result = Some(json!({ "text": text }));
                Terminal::Ended(Box::new(ended))
            }
            Err(error) if error.code == "cancelled" => Terminal::Cancelled,
            Err(error) => Terminal::Failed(error),
        };
        settle(id, stream_id, &queue, &in_flight, cancelled, terminal).await;
    });
    true
}

fn job_text(id: u64, kind: &'static str, text: String) -> EventPayload {
    let mut event = EventPayload::named(id, kind);
    event.text = Some(text);
    event
}

async fn relay_job(
    id: u64,
    watched: inocode_core::JobWatch,
    stream_id: u64,
    queue: mpsc::Sender<Outgoing>,
    in_flight: InFlight,
    mut cancelled: watch::Receiver<bool>,
) {
    use tokio::sync::broadcast::error::RecvError;
    let inocode_core::JobWatch {
        job,
        screen,
        mut output,
        ended,
    } = watched;
    let mut ending = ended.clone();
    let mut opening = EventPayload::started(id);
    opening.result = Some(json!({ "job": job }));
    let mut first = vec![opening, job_text(id, "screen", screen)];
    // Bytes of a character the last chunk cut in two.
    let mut carry: Vec<u8> = Vec::new();
    let terminal = 'relay: {
        for event in first.drain(..) {
            match deliver(&queue, stream_id, event, &mut cancelled).await {
                Delivery::Sent => {}
                Delivery::ConnectionGone => break 'relay None,
                Delivery::Cancelled => break 'relay Some(Terminal::Cancelled),
            }
        }
        loop {
            let event = tokio::select! {
                biased;
                chunk = output.recv() => match chunk {
                    Ok(chunk) => {
                        carry.extend(chunk);
                        let whole = inocode_core::complete_utf8(&carry);
                        let rest = carry.split_off(whole);
                        let text = String::from_utf8_lossy(&carry).into_owned();
                        carry = rest;
                        if text.is_empty() {
                            continue;
                        }
                        job_text(id, "output", text)
                    }
                    Err(RecvError::Lagged(_)) => EventPayload::named(id, "lagged"),
                    Err(RecvError::Closed) => break 'relay Some(Terminal::Cancelled),
                },
                // All of a job's output is sent before it is marked ended.
                () = async { drop(ending.wait_for(Option::is_some).await) } => {
                    let mut lagged = false;
                    loop {
                        match output.try_recv() {
                            Ok(chunk) => carry.extend(chunk),
                            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {
                                lagged = true;
                            }
                            Err(_) => break,
                        }
                    }
                    let end = *ended.borrow();
                    let mut last = Vec::new();
                    if lagged {
                        last.push(EventPayload::named(id, "lagged"));
                    }
                    if !carry.is_empty() {
                        let text = String::from_utf8_lossy(&carry).into_owned();
                        last.push(job_text(id, "output", text));
                    }
                    let mut event = EventPayload::named(id, "job_ended");
                    event.result = Some(json!({
                        "exit_code": end.map(|(code, _)| code),
                        "duration_ms": end.map(|(_, took)| took.as_millis() as u64),
                    }));
                    last.push(event);
                    for event in last {
                        match deliver(&queue, stream_id, event, &mut cancelled).await {
                            Delivery::Sent => {}
                            Delivery::ConnectionGone => break 'relay None,
                            Delivery::Cancelled => break 'relay Some(Terminal::Cancelled),
                        }
                    }
                    break 'relay Some(Terminal::Completed);
                }
                _ = cancelled.changed() => break 'relay Some(Terminal::Cancelled),
            };
            match deliver(&queue, stream_id, event, &mut cancelled).await {
                Delivery::Sent => {}
                Delivery::ConnectionGone => break 'relay None,
                Delivery::Cancelled => break 'relay Some(Terminal::Cancelled),
            }
        }
    };
    match terminal {
        Some(terminal) => settle(id, stream_id, &queue, &in_flight, cancelled, terminal).await,
        None => abandon(&in_flight, id).await,
    }
}

async fn queue_request(request: &Request, core: &Arc<Core>) -> Result<Value, ErrorInfo> {
    let thread = request.thread()?;
    let params = request.params.as_ref();
    let queue_id = || {
        params
            .and_then(|params| params.get("queue_id"))
            .and_then(Value::as_i64)
            .ok_or_else(|| ErrorInfo::new("invalid_params", "requires a \"queue_id\" number"))
    };
    let queue = match request.method.as_str() {
        "thread/queue/add" => {
            let (queue_id, queue) = core
                .add_to_queue(thread, request.prompt()?.to_string())
                .await?;
            return Ok(json!({ "queue_id": queue_id, "queue": queue }));
        }
        "thread/queue/start" => {
            return Ok(json!({ "started": core.start_queue(thread).await? }));
        }
        "thread/queue/update" => {
            let id = queue_id()?;
            let prompt = request.prompt()?.to_string();
            core.update_queued(thread, id, prompt).await?
        }
        "thread/queue/delete" => core.delete_queued(thread, queue_id()?).await?,
        "thread/queue/reorder" => {
            let ids = params
                .and_then(|params| params.get("queue_ids"))
                .and_then(Value::as_array)
                .and_then(|ids| ids.iter().map(Value::as_i64).collect::<Option<Vec<_>>>())
                .ok_or_else(|| {
                    ErrorInfo::new(
                        "invalid_params",
                        "requires a \"queue_ids\" array of numbers",
                    )
                })?;
            core.reorder_queue(thread, ids).await?
        }
        _ => core.queue(thread).await?,
    };
    Ok(json!({ "queue": queue }))
}

async fn start_thread(request: &Request, core: &Core) -> Result<Value, ErrorInfo> {
    let project = match request
        .params
        .as_ref()
        .and_then(|params| params.get("project"))
    {
        None | Some(Value::Null) => None,
        Some(Value::String(project)) if !project.is_empty() => Some(project.as_str()),
        Some(_) => {
            return Err(ErrorInfo::new(
                "invalid_params",
                "\"project\" must be a non-empty string",
            ))
        }
    };
    let thread = core.start_thread(project).await?;
    Ok(json!({ "thread_id": thread.0 }))
}

/// `thread/fork {thread_id, before?, through?}`: a copy of the thread, of
/// all its messages, of those before the person's message numbered
/// `before`, or of those through the person's message numbered `through`
/// and what followed it until the person's next message.
async fn fork_thread(request: &Request, core: &Core) -> Result<Value, ErrorInfo> {
    use inocode_core::ForkCut;
    let thread = request.thread()?;
    let field = |name: &str| match request.params.as_ref().and_then(|params| params.get(name)) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value.as_i64().map(Some).ok_or_else(|| {
            ErrorInfo::new(
                "invalid_params",
                format!("\"{name}\" must be a message number"),
            )
        }),
    };
    let cut = match (field("before")?, field("through")?) {
        (None, None) => ForkCut::Whole,
        (Some(seq), None) => ForkCut::Before(seq),
        (None, Some(seq)) => ForkCut::Through(seq),
        (Some(_), Some(_)) => {
            return Err(ErrorInfo::new(
                "invalid_params",
                "give \"before\" or \"through\", not both",
            ))
        }
    };
    let fork = core.fork_thread(thread, cut).await?;
    Ok(json!({ "thread_id": fork.0, "from": thread.0 }))
}

/// `thread/rename {thread_id, title}`, `thread/delete {thread_id}`,
/// `thread/archive {thread_id}`, `thread/unarchive {thread_id}`,
/// `thread/export {thread_id, format?}` (`markdown`, the default, or
/// `json`), `thread/search {query, limit?}` and
/// `thread/diff {thread_id, seq?}` (docs/evidence/thread-operations.md,
/// docs/evidence/turn-diff.md).
async fn thread_operation(request: &Request, core: &Core) -> Result<Value, ErrorInfo> {
    let field = |name: &str| {
        request
            .params
            .as_ref()
            .and_then(|params| params.get(name))
            .filter(|value| !value.is_null())
    };
    if request.method == "thread/search" {
        let query = field("query")
            .and_then(Value::as_str)
            .filter(|query| !query.trim().is_empty())
            .ok_or_else(|| ErrorInfo::new("invalid_params", "requires a \"query\" string"))?;
        let limit = match field("limit") {
            None => SEARCH_DEFAULT_LIMIT,
            Some(value) => value
                .as_u64()
                .filter(|limit| (1..=SEARCH_MAX_LIMIT).contains(limit))
                .ok_or_else(|| {
                    ErrorInfo::new(
                        "invalid_params",
                        format!("\"limit\" must be 1 to {SEARCH_MAX_LIMIT}"),
                    )
                })?,
        };
        return core.search_threads(query, limit as usize).await;
    }
    let thread = request.thread()?;
    match request.method.as_str() {
        "thread/rename" => {
            let title = field("title")
                .and_then(Value::as_str)
                .filter(|title| !title.trim().is_empty())
                .ok_or_else(|| {
                    ErrorInfo::new("invalid_params", "requires a non-empty \"title\" string")
                })?;
            core.rename_thread(thread, title).await
        }
        "thread/delete" => core.delete_thread(thread).await,
        "thread/archive" => core.set_thread_archived(thread, true).await,
        "thread/unarchive" => core.set_thread_archived(thread, false).await,
        "thread/export" => {
            let format = match field("format") {
                None => "markdown",
                Some(Value::String(format)) if format == "markdown" || format == "json" => {
                    format.as_str()
                }
                Some(_) => {
                    return Err(ErrorInfo::new(
                        "invalid_params",
                        "\"format\" must be \"markdown\" or \"json\"",
                    ))
                }
            };
            core.export_thread(thread, format).await
        }
        _ => {
            let seq = match field("seq") {
                None => None,
                Some(value) => Some(value.as_i64().ok_or_else(|| {
                    ErrorInfo::new("invalid_params", "\"seq\" must be a message number")
                })?),
            };
            core.thread_diff(thread, seq).await
        }
    }
}

/// `thread/rewind {thread_id, seq, restore?}`: `restore` is
/// `conversation` (the default), `files` or `both`.
async fn rewind_thread(request: &Request, core: &Core) -> Result<Value, ErrorInfo> {
    use inocode_core::Restore;
    let thread = request.thread()?;
    let params = request.params.as_ref();
    let seq = params
        .and_then(|params| params["seq"].as_i64())
        .ok_or_else(|| {
            ErrorInfo::new(
                "invalid_params",
                "requires \"seq\", the number of a message of the person",
            )
        })?;
    let restore = match params.map(|params| &params["restore"]) {
        None | Some(Value::Null) => Restore::Conversation,
        Some(Value::String(restore)) if restore == "conversation" => Restore::Conversation,
        Some(Value::String(restore)) if restore == "files" => Restore::Files,
        Some(Value::String(restore)) if restore == "both" => Restore::Both,
        Some(_) => {
            return Err(ErrorInfo::new(
                "invalid_params",
                "\"restore\" is \"conversation\", \"files\" or \"both\"",
            ))
        }
    };
    core.rewind_thread(thread, seq, restore).await
}

/// `goal/set`: `objective`, and optionally the `check` command, the
/// `rounds` and the `tokens` the goal may use.
async fn set_goal(request: &Request, core: &Core) -> Result<Value, ErrorInfo> {
    let thread = request.thread()?;
    let field = |name: &str| request.params.as_ref().and_then(|params| params.get(name));
    let objective = field("objective")
        .and_then(Value::as_str)
        .ok_or_else(|| ErrorInfo::new("invalid_params", "requires an \"objective\" string"))?;
    let check = match field("check") {
        None | Some(Value::Null) => None,
        Some(Value::String(check)) => Some(check.as_str()),
        Some(_) => {
            return Err(ErrorInfo::new(
                "invalid_params",
                "\"check\" must be a command string",
            ))
        }
    };
    let number = |name: &str| match field(name) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value.as_u64().map(Some).ok_or_else(|| {
            ErrorInfo::new("invalid_params", format!("\"{name}\" must be a number"))
        }),
    };
    let rounds = number("rounds")?.map(|rounds| u32::try_from(rounds).unwrap_or(u32::MAX));
    let tokens = number("tokens")?;
    let goal = core
        .set_goal(thread, objective, check, rounds, tokens)
        .await?;
    Ok(json!({ "goal": goal }))
}

/// `files/search {thread_id, query, limit?}`: files of the thread's project
/// whose paths match `query` fuzzily, for completing a mention; `limit` is
/// 20 when absent, at most 100.
async fn search_files(request: &Request, core: &Core) -> Result<Value, ErrorInfo> {
    let thread = request.thread()?;
    let field = |name: &str| request.params.as_ref().and_then(|params| params.get(name));
    let query = field("query")
        .and_then(Value::as_str)
        .ok_or_else(|| ErrorInfo::new("invalid_params", "requires a \"query\" string"))?;
    let limit = match field("limit") {
        None | Some(Value::Null) => SEARCH_DEFAULT_LIMIT,
        Some(value) => value
            .as_u64()
            .filter(|limit| (1..=SEARCH_MAX_LIMIT).contains(limit))
            .ok_or_else(|| {
                ErrorInfo::new(
                    "invalid_params",
                    format!("\"limit\" must be 1 to {SEARCH_MAX_LIMIT}"),
                )
            })?,
    };
    core.search_files(thread, query.to_string(), limit as usize)
        .await
}

async fn read_thread(request: &Request, core: &Core) -> Result<Value, ErrorInfo> {
    let thread = request.thread()?;
    let field = |name: &str| request.params.as_ref().and_then(|params| params.get(name));
    let before = match field("before") {
        None | Some(Value::Null) => None,
        Some(value) => Some(value.as_i64().ok_or_else(|| {
            ErrorInfo::new("invalid_params", "\"before\" must be a message position")
        })?),
    };
    let limit = match field("limit") {
        None | Some(Value::Null) => READ_DEFAULT_LIMIT,
        Some(value) => value
            .as_u64()
            .filter(|limit| (1..=READ_MAX_LIMIT).contains(limit))
            .ok_or_else(|| {
                ErrorInfo::new(
                    "invalid_params",
                    format!("\"limit\" must be 1 to {READ_MAX_LIMIT}"),
                )
            })?,
    };
    let messages: Vec<Value> = core
        .read_thread(thread, before, limit as usize)
        .await?
        .into_iter()
        .map(|stored| {
            json!({ "seq": stored.seq, "created": stored.created, "message": stored.message })
        })
        .collect();
    Ok(json!({ "messages": messages }))
}

/// `thread/list`: `project`, a project's key, `archived` (`false` when
/// absent: the archived threads are left out; `true` lists only them), and
/// `limit` (100 when absent, at most 1,000).
async fn list_threads(params: &Value, core: &Core) -> Result<Value, ErrorInfo> {
    let project = match params.get("project") {
        None | Some(Value::Null) => None,
        Some(Value::String(project)) if !project.is_empty() => Some(project.as_str()),
        Some(_) => {
            return Err(ErrorInfo::new(
                "invalid_params",
                "\"project\" must be a non-empty string",
            ))
        }
    };
    let limit = match params.get("limit") {
        None | Some(Value::Null) => READ_DEFAULT_LIMIT,
        Some(value) => value
            .as_u64()
            .filter(|limit| (1..=READ_MAX_LIMIT).contains(limit))
            .ok_or_else(|| {
                ErrorInfo::new(
                    "invalid_params",
                    format!("\"limit\" must be 1 to {READ_MAX_LIMIT}"),
                )
            })?,
    };
    let archived = match params.get("archived") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(archived)) => *archived,
        Some(_) => {
            return Err(ErrorInfo::new(
                "invalid_params",
                "\"archived\" must be true or false",
            ))
        }
    };
    core.threads(project, archived, limit as usize).await
}

/// `command/run`: `thread_id`, and the command as typed in `text`
/// (`"/goal fix the tests"`) or as `name` and `args`. A command that gives
/// a result is answered with it; one that starts a turn sends a `command`
/// event with what it did, then the turn's events as `turn/start` does.
async fn run_command(
    request: Request,
    stream_id: u64,
    core: Arc<Core>,
    queue: mpsc::Sender<Outgoing>,
    in_flight: InFlight,
) {
    let id = request.id;
    let failed = |error: inocode_core::commands::CommandError| {
        let mut error_value = json!(error.error);
        if let Some(when) = error.when {
            error_value["when"] = json!(when);
        }
        json!({ "id": id, "error": error_value })
    };
    let params = request.params.clone().unwrap_or_else(|| json!({}));
    let parsed = request.thread().and_then(|thread| {
        let (name, args) = match (&params["text"], &params["name"]) {
            (Value::String(text), _) => inocode_core::commands::split(text)?,
            (_, Value::String(name)) => {
                let args = match &params["args"] {
                    Value::Null => String::new(),
                    Value::String(args) => args.clone(),
                    _ => {
                        return Err(ErrorInfo::new(
                            "invalid_params",
                            "\"args\" must be a string",
                        ))
                    }
                };
                (name.clone(), args)
            }
            _ => {
                return Err(ErrorInfo::new(
                    "invalid_params",
                    "requires a \"text\" string, or a \"name\" string with \"args\"",
                ))
            }
        };
        Ok((thread, name, args))
    });
    let (thread, name, args) = match parsed {
        Ok(parsed) => parsed,
        Err(error) => {
            let _ = send(&queue, stream_id, failed(error.into())).await;
            return;
        }
    };
    let start = match core.run_command(thread, &name, &args).await {
        Ok(inocode_core::commands::Ran::Result(result)) => {
            let _ = send(&queue, stream_id, json!({ "id": id, "result": result })).await;
            return;
        }
        Ok(inocode_core::commands::Ran::Turn(start)) => start,
        Err(error) => {
            let _ = send(&queue, stream_id, failed(error)).await;
            return;
        }
    };
    let mut pending = in_flight.lock().await;
    if pending.contains_key(&id) {
        drop(pending);
        let error = ErrorInfo::new("duplicate_id", "request id is already in flight");
        let _ = send_event(&queue, stream_id, EventPayload::failed(id, error)).await;
        return;
    }
    let (cancel_tx, cancel_rx) = watch::channel(false);
    pending.insert(id, Generation::Running(cancel_tx));
    drop(pending);
    let _ = send_event(&queue, stream_id, EventPayload::command(id, start.result)).await;
    run_turn(
        id,
        thread,
        (
            (start.prompt, start.reply_to, Default::default(), None),
            (Default::default(), false, None, core.choices_seen(thread)),
        ),
        stream_id,
        core,
        queue,
        in_flight,
        cancel_rx,
    )
    .await;
}

async fn model_stats(
    request: Request,
    stream_id: u64,
    core: Arc<Core>,
    queue: mpsc::Sender<Outgoing>,
) {
    let id = request.id;
    let payload = match request.stats_window() {
        Ok((days, offset)) => match core.model_stats(days, offset).await {
            Ok(models) => json!({ "id": id, "result": { "models": models } }),
            Err(error) => json!({ "id": id, "error": error }),
        },
        Err(error) => json!({ "id": id, "error": error }),
    };
    let _ = send(&queue, stream_id, payload).await;
}

/// `estimate {thread_id?, text}`: the time and tokens the work `text`
/// describes may take (docs/evidence/effort-estimates.md).
async fn estimate(
    request: Request,
    stream_id: u64,
    core: Arc<Core>,
    queue: mpsc::Sender<Outgoing>,
) {
    let id = request.id;
    let params = request.params.clone().unwrap_or_else(|| json!({}));
    let thread = match params.get("thread_id") {
        None | Some(Value::Null) => Ok(None),
        Some(_) => request.thread().map(Some),
    };
    let result = match (thread, params["text"].as_str()) {
        (Ok(thread), Some(text)) => core.estimate(thread, text).await,
        (Err(error), _) => Err(error),
        (_, None) => Err(ErrorInfo::new(
            "invalid_params",
            "\"text\" must be a string",
        )),
    };
    let payload = match result {
        Ok(result) => json!({ "id": id, "result": result }),
        Err(error) => json!({ "id": id, "error": error }),
    };
    let _ = send(&queue, stream_id, payload).await;
}

async fn read_usage(id: u64, stream_id: u64, core: Arc<Core>, queue: mpsc::Sender<Outgoing>) {
    let payload = match core.usage().await {
        Ok(usage) => json!({ "id": id, "result": usage }),
        Err(error) => json!({ "id": id, "error": error }),
    };
    // A closed connection has nobody to answer.
    let _ = send(&queue, stream_id, payload).await;
}

async fn start_chat(
    request: Request,
    stream_id: u64,
    core: &Arc<Core>,
    queue: &mpsc::Sender<Outgoing>,
    in_flight: &InFlight,
) -> bool {
    let prompt = match request.prompt() {
        Ok(prompt) => prompt.to_string(),
        Err(error) => {
            let event = EventPayload::failed(request.id, error);
            return send_event(queue, stream_id, event).await.is_ok();
        }
    };
    let id = request.id;
    let mut pending = in_flight.lock().await;
    if pending.contains_key(&id) {
        drop(pending);
        let error = ErrorInfo::new("duplicate_id", "request id is already in flight");
        let event = EventPayload::failed(id, error);
        return send_event(queue, stream_id, event).await.is_ok();
    }
    let (cancel_tx, cancel_rx) = watch::channel(false);
    pending.insert(id, Generation::Running(cancel_tx));
    drop(pending);

    tokio::spawn(generate(
        id,
        prompt,
        stream_id,
        Arc::clone(core),
        queue.clone(),
        Arc::clone(in_flight),
        cancel_rx,
    ));
    true
}

/// `mcp/resources/subscribe {server, uri}`: a stream of
/// `mcp_resource_updated` events (`mcp: {server, uri}`) after `started`,
/// until `cancel` names it or the connection closes; `failed` when the
/// subscription cannot be made or the server stops.
async fn watch_resource(
    request: Request,
    stream_id: u64,
    core: &Arc<Core>,
    queue: &mpsc::Sender<Outgoing>,
    in_flight: &InFlight,
) -> bool {
    let id = request.id;
    let mut pending = in_flight.lock().await;
    if pending.contains_key(&id) {
        drop(pending);
        let error = ErrorInfo::new("duplicate_id", "request id is already in flight");
        let event = EventPayload::failed(id, error);
        return send_event(queue, stream_id, event).await.is_ok();
    }
    let (cancel_tx, cancel_rx) = watch::channel(false);
    pending.insert(id, Generation::Running(cancel_tx));
    drop(pending);
    let params = request.params.clone().unwrap_or_else(|| json!({}));
    tokio::spawn(relay_updates(
        id,
        params,
        stream_id,
        Arc::clone(core),
        queue.clone(),
        Arc::clone(in_flight),
        cancel_rx,
    ));
    true
}

async fn relay_updates(
    id: u64,
    params: Value,
    stream_id: u64,
    core: Arc<Core>,
    queue: mpsc::Sender<Outgoing>,
    in_flight: InFlight,
    mut cancelled: watch::Receiver<bool>,
) {
    let mut updates = tokio::select! {
        subscribed = core.mcp_subscribe(&params) => match subscribed {
            Ok(updates) => updates,
            Err(error) => {
                settle(id, stream_id, &queue, &in_flight, cancelled, Terminal::Failed(error)).await;
                return;
            }
        },
        _ = cancelled.changed() => {
            settle(id, stream_id, &queue, &in_flight, cancelled, Terminal::Cancelled).await;
            return;
        }
    };
    let terminal = match deliver(&queue, stream_id, EventPayload::started(id), &mut cancelled).await
    {
        Delivery::Sent => loop {
            tokio::select! {
                update = updates.next() => {
                    let Some(update) = update else {
                        let error = ErrorInfo::new("mcp_error", "the MCP server stopped");
                        break Some(Terminal::Failed(error));
                    };
                    let event = EventPayload::mcp(id, "mcp_resource_updated", update);
                    match deliver(&queue, stream_id, event, &mut cancelled).await {
                        Delivery::Sent => {}
                        Delivery::ConnectionGone => break None,
                        Delivery::Cancelled => break Some(Terminal::Cancelled),
                    }
                }
                _ = cancelled.changed() => break Some(Terminal::Cancelled),
            }
        },
        Delivery::ConnectionGone => None,
        Delivery::Cancelled => Some(Terminal::Cancelled),
    };
    drop(updates);
    match terminal {
        Some(terminal) => settle(id, stream_id, &queue, &in_flight, cancelled, terminal).await,
        None => abandon(&in_flight, id).await,
    }
}

/// `thread/titles/subscribe`: a stream of `title_updated` events
/// (`thread_id`, `title`) for every title a thread is given after `started`,
/// whether or not a turn runs, until `cancel` names it or the connection
/// closes.
async fn watch_titles(
    request: Request,
    stream_id: u64,
    core: &Arc<Core>,
    queue: &mpsc::Sender<Outgoing>,
    in_flight: &InFlight,
) -> bool {
    let id = request.id;
    let changes = match core.title_changes() {
        Ok(changes) => changes,
        Err(error) => {
            return send_event(queue, stream_id, EventPayload::failed(id, error))
                .await
                .is_ok()
        }
    };
    let mut pending = in_flight.lock().await;
    if pending.contains_key(&id) {
        drop(pending);
        let error = ErrorInfo::new("duplicate_id", "request id is already in flight");
        let event = EventPayload::failed(id, error);
        return send_event(queue, stream_id, event).await.is_ok();
    }
    let (cancel_tx, cancel_rx) = watch::channel(false);
    pending.insert(id, Generation::Running(cancel_tx));
    drop(pending);
    tokio::spawn(relay_titles(
        id,
        changes,
        stream_id,
        queue.clone(),
        Arc::clone(in_flight),
        cancel_rx,
    ));
    true
}

async fn relay_titles(
    id: u64,
    mut changes: tokio::sync::broadcast::Receiver<(inocode_core::store::SessionId, String)>,
    stream_id: u64,
    queue: mpsc::Sender<Outgoing>,
    in_flight: InFlight,
    mut cancelled: watch::Receiver<bool>,
) {
    use tokio::sync::broadcast::error::RecvError;
    let terminal = match deliver(&queue, stream_id, EventPayload::started(id), &mut cancelled).await
    {
        Delivery::Sent => loop {
            tokio::select! {
                change = changes.recv() => {
                    let (thread, title) = match change {
                        Ok(change) => change,
                        Err(RecvError::Lagged(_)) => continue,
                        Err(RecvError::Closed) => break Some(Terminal::Cancelled),
                    };
                    let event = EventPayload::thread_title(id, thread, title);
                    match deliver(&queue, stream_id, event, &mut cancelled).await {
                        Delivery::Sent => {}
                        Delivery::ConnectionGone => break None,
                        Delivery::Cancelled => break Some(Terminal::Cancelled),
                    }
                }
                _ = cancelled.changed() => break Some(Terminal::Cancelled),
            }
        },
        Delivery::ConnectionGone => None,
        Delivery::Cancelled => Some(Terminal::Cancelled),
    };
    match terminal {
        Some(terminal) => settle(id, stream_id, &queue, &in_flight, cancelled, terminal).await,
        None => abandon(&in_flight, id).await,
    }
}

/// `thread/subscribe {thread_id}`: what happens in one thread from now on,
/// whichever connection started its turns. After `started`, a `status`
/// event says where the thread stands, and an `elicitation` event comes
/// for each question its running turn waits on; then each turn comes as
/// `turn_started`, the events its own connection receives (with
/// `answered` when a question is settled), and `turn_ended`, with a
/// `status` event whenever the status changes.
/// `lagged` says events were missed, so `thread/read` should be asked
/// again. `thread_renamed`, `thread_archived` and `thread_unarchived` say
/// what happened to the thread; `thread_deleted` ends the stream.
/// `thread/status/subscribe`: the `status` events of every thread, and the
/// changes to the list of threads (`thread_created`, with `from` for a
/// fork, `thread_deleted`, `thread_renamed`, `thread_archived`,
/// `thread_unarchived`), with `thread_id`. Both last until `cancel` names
/// them or the connection closes.
async fn watch_threads(
    request: Request,
    stream_id: u64,
    core: &Arc<Core>,
    queue: &mpsc::Sender<Outgoing>,
    in_flight: &InFlight,
) -> bool {
    let id = request.id;
    let thread = if request.method == "thread/subscribe" {
        match request.thread() {
            Ok(thread) => Some(thread),
            Err(error) => {
                return send_event(queue, stream_id, EventPayload::failed(id, error))
                    .await
                    .is_ok()
            }
        }
    } else {
        None
    };
    let mut pending = in_flight.lock().await;
    if pending.contains_key(&id) {
        drop(pending);
        let error = ErrorInfo::new("duplicate_id", "request id is already in flight");
        let event = EventPayload::failed(id, error);
        return send_event(queue, stream_id, event).await.is_ok();
    }
    let (cancel_tx, cancel_rx) = watch::channel(false);
    pending.insert(id, Generation::Running(cancel_tx));
    drop(pending);
    // Subscribed before the status is read, so no change after it is missed.
    let activity = core.activity();
    tokio::spawn(relay_threads(
        id,
        thread,
        activity,
        stream_id,
        Arc::clone(core),
        queue.clone(),
        Arc::clone(in_flight),
        cancel_rx,
    ));
    true
}

#[allow(clippy::too_many_arguments)]
async fn relay_threads(
    id: u64,
    thread: Option<inocode_core::store::SessionId>,
    mut activity: tokio::sync::broadcast::Receiver<(
        inocode_core::store::SessionId,
        inocode_core::Activity,
    )>,
    stream_id: u64,
    core: Arc<Core>,
    queue: mpsc::Sender<Outgoing>,
    in_flight: InFlight,
    mut cancelled: watch::Receiver<bool>,
) {
    use inocode_core::{Activity, ThreadChange};
    use tokio::sync::broadcast::error::RecvError;
    let first = match thread {
        Some(thread) => match core.thread_status(thread).await {
            Ok(status) => Ok(EventPayload::thread_status(id, None, status)),
            Err(error) => Err(error),
        },
        None => Ok(EventPayload::started(id)),
    };
    let first = match first {
        Ok(first) => first,
        Err(error) => {
            settle(
                id,
                stream_id,
                &queue,
                &in_flight,
                cancelled,
                Terminal::Failed(error),
            )
            .await;
            return;
        }
    };
    let mut opening = vec![first];
    // The questions already open, which the activity may still bring
    // again.
    let mut shown = std::collections::HashSet::new();
    if let Some(thread) = thread {
        opening.insert(0, EventPayload::started(id));
        match core.queue(thread).await {
            Ok(queue) if !queue.is_empty() => opening.push(EventPayload::queue(id, &queue)),
            Ok(_) => {}
            Err(error) => {
                settle(
                    id,
                    stream_id,
                    &queue,
                    &in_flight,
                    cancelled,
                    Terminal::Failed(error),
                )
                .await;
                return;
            }
        }
        if let Ok(jobs) = core.jobs(thread).await {
            if !jobs.is_empty() {
                opening.push(EventPayload::jobs(id, &jobs));
            }
        }
        for question in core.elicitations(thread) {
            if let Some(asked) = question["id"].as_u64() {
                shown.insert(asked);
            }
            opening.push(EventPayload::elicitation(id, question));
        }
    }
    let mut terminal = None;
    for event in opening {
        match deliver(&queue, stream_id, event, &mut cancelled).await {
            Delivery::Sent => {}
            Delivery::ConnectionGone => {
                abandon(&in_flight, id).await;
                return;
            }
            Delivery::Cancelled => {
                terminal = Some(Terminal::Cancelled);
                break;
            }
        }
    }
    let terminal = match terminal {
        Some(terminal) => Some(terminal),
        None => loop {
            tokio::select! {
                item = activity.recv() => {
                    let event = match item {
                        Ok((of, item)) => match (thread, item) {
                            (None, Activity::Status(status)) => {
                                EventPayload::thread_status(id, Some(of), status)
                            }
                            (None, Activity::Changed(change)) => {
                                EventPayload::thread_change(id, Some(of), &change)
                            }
                            (None, _) => continue,
                            (Some(watched), _) if watched != of => continue,
                            (Some(_), Activity::Changed(ThreadChange::Deleted)) => {
                                let deleted = EventPayload::thread_change(
                                    id,
                                    None,
                                    &ThreadChange::Deleted,
                                );
                                break Some(Terminal::Ended(Box::new(deleted)));
                            }
                            (Some(_), Activity::Changed(ThreadChange::Created { .. })) => continue,
                            (Some(_), Activity::Changed(change)) => {
                                EventPayload::thread_change(id, None, &change)
                            }
                            (Some(_), Activity::Status(status)) => {
                                EventPayload::thread_status(id, None, status)
                            }
                            (Some(_), Activity::TurnStarted) => {
                                EventPayload::named(id, "turn_started")
                            }
                            (Some(_), Activity::Event(TurnEvent::Elicitation(question)))
                                if question["id"]
                                    .as_u64()
                                    .is_some_and(|asked| shown.remove(&asked)) =>
                            {
                                continue
                            }
                            (Some(_), Activity::Event(event)) => turn_event(id, event),
                            (Some(_), Activity::TurnEnded(end)) => EventPayload::turn_ended(id, end),
                            (Some(_), Activity::Rewound(from)) => EventPayload::rewound(id, from),
                            (Some(_), Activity::Queue(queue)) => EventPayload::queue(id, &queue),
                            (Some(_), Activity::Jobs(jobs)) => EventPayload::jobs(id, &jobs),
                        },
                        Err(RecvError::Lagged(_)) => EventPayload::named(id, "lagged"),
                        Err(RecvError::Closed) => break Some(Terminal::Cancelled),
                    };
                    match deliver(&queue, stream_id, event, &mut cancelled).await {
                        Delivery::Sent => {}
                        Delivery::ConnectionGone => break None,
                        Delivery::Cancelled => break Some(Terminal::Cancelled),
                    }
                }
                _ = cancelled.changed() => break Some(Terminal::Cancelled),
            }
        },
    };
    match terminal {
        Some(terminal) => settle(id, stream_id, &queue, &in_flight, cancelled, terminal).await,
        None => abandon(&in_flight, id).await,
    }
}

/// A turn's event as the connection that started the turn receives it.
fn turn_event(id: u64, event: TurnEvent) -> EventPayload {
    match event {
        TurnEvent::Delta(text) => EventPayload::delta(id, text),
        TurnEvent::Thinking(text) => EventPayload::thinking(id, text),
        TurnEvent::ThinkingTranslation(piece) => EventPayload::thinking_translation(id, piece),
        TurnEvent::Message { seq, message } => EventPayload::message(id, seq, &message),
        TurnEvent::Elicitation(elicitation) => EventPayload::elicitation(id, elicitation),
        TurnEvent::Answered(elicitation) => EventPayload::answered(id, elicitation),
        TurnEvent::McpProgress(progress) => EventPayload::mcp(id, "mcp_progress", progress),
        TurnEvent::McpLog(log) => EventPayload::mcp(id, "mcp_log", log),
        TurnEvent::Estimate(estimate) => EventPayload::estimate(id, &estimate),
        TurnEvent::Usage(report) => EventPayload::usage(id, &report),
        TurnEvent::ShellStarted(job) => EventPayload::shell_started(id, job),
        TurnEvent::Helper(step) => EventPayload::helper(id, &step),
        TurnEvent::CompactionStarted => EventPayload::compaction(id, None),
        TurnEvent::CompactionEnded { seq } => EventPayload::compaction(id, Some(seq)),
        TurnEvent::Delegated {
            call_id,
            thread_id,
            member,
        } => EventPayload::delegated(id, &call_id, thread_id, &member),
        TurnEvent::Title(title) => EventPayload::title(id, "title_updated", title),
        TurnEvent::TitleProposed(title) => EventPayload::title(id, "title_proposed", title),
        TurnEvent::Diff(diff) => EventPayload::diff(id, diff),
        TurnEvent::PlanMode(on) => EventPayload::plan_mode(id, on),
    }
}

async fn start_turn(
    request: Request,
    stream_id: u64,
    core: &Arc<Core>,
    queue: &mpsc::Sender<Outgoing>,
    in_flight: &InFlight,
) -> bool {
    let parsed = request.thread().and_then(|thread| {
        let prompt = match person_shell(&request)? {
            Some(command) => (command.clone(), None, Default::default(), Some(command)),
            None => (
                request.prompt()?.to_string(),
                request.reply_to()?,
                request.attached()?,
                None,
            ),
        };
        Ok((
            thread,
            prompt,
            (request.approvals()?, request.names_mode(), request.plan()?),
        ))
    });
    let (thread, prompt, allowed) = match parsed {
        Ok(parsed) => parsed,
        Err(error) => {
            let event = EventPayload::failed(request.id, error);
            return send_event(queue, stream_id, event).await.is_ok();
        }
    };
    let id = request.id;
    let mut pending = in_flight.lock().await;
    if pending.contains_key(&id) {
        drop(pending);
        let error = ErrorInfo::new("duplicate_id", "request id is already in flight");
        let event = EventPayload::failed(id, error);
        return send_event(queue, stream_id, event).await.is_ok();
    }
    let (cancel_tx, cancel_rx) = watch::channel(false);
    pending.insert(id, Generation::Running(cancel_tx));
    drop(pending);

    // Counted when the turn is asked for, so a choice written from now on
    // reaches it even if it is written before the turn is registered.
    let (approvals, names_mode, plan) = allowed;
    let asked = core.choices_seen(thread);
    tokio::spawn(run_turn(
        id,
        thread,
        (prompt, (approvals, names_mode, plan, asked)),
        stream_id,
        Arc::clone(core),
        queue.clone(),
        Arc::clone(in_flight),
        cancel_rx,
    ));
    true
}

/// The command of a `turn/start` that runs the person's command instead of
/// sending a prompt; it takes no prompt, reply or attachments.
fn person_shell(request: &Request) -> Result<Option<String>, ErrorInfo> {
    let params = request.params.as_ref();
    let Some(shell) = params
        .and_then(|params| params.get("shell"))
        .filter(|shell| !shell.is_null())
    else {
        return Ok(None);
    };
    let command = shell
        .as_str()
        .filter(|command| !command.trim().is_empty())
        .ok_or_else(|| ErrorInfo::new("invalid_params", "\"shell\" must be a non-empty string"))?;
    let given = |name: &str| {
        params
            .and_then(|params| params.get(name))
            .is_some_and(|value| !value.is_null())
    };
    if ["prompt", "reply_to", "attachments", "mentions"]
        .iter()
        .any(|name| given(name))
    {
        return Err(ErrorInfo::new(
            "invalid_params",
            "\"shell\" takes no prompt, reply_to, attachments or mentions",
        ));
    }
    Ok(Some(command.to_string()))
}

/// What a `turn/start` chose for its turn: the approvals, whether it named
/// its mode, plan mode when it sets it, and how many times the thread's
/// choices had been written when it was asked for.
type TurnChoices = (inocode_core::turn::Approvals, bool, Option<bool>, u64);

/// Runs one turn and relays its events. The turn goes on when the
/// connection closes: what it makes is stored and `thread/read` returns it.
#[allow(clippy::too_many_arguments)]
async fn run_turn(
    id: u64,
    thread: inocode_core::store::SessionId,
    ((prompt, reply_to, attached, shell), (mut approvals, names_mode, plan, asked)): (
        (
            String,
            Option<i64>,
            inocode_core::attach::Requested,
            Option<String>,
        ),
        TurnChoices,
    ),
    stream_id: u64,
    core: Arc<Core>,
    queue: mpsc::Sender<Outgoing>,
    in_flight: InFlight,
    cancelled: watch::Receiver<bool>,
) {
    let prompt = match core.turn_prompt(thread, prompt, reply_to, attached).await {
        Ok(prompt) => inocode_core::turn::Prompt { shell, ..prompt },
        Err(error) => {
            settle(
                id,
                stream_id,
                &queue,
                &in_flight,
                cancelled,
                Terminal::Failed(error),
            )
            .await;
            return;
        }
    };
    let claim = match core.begin_turn_asked(thread, asked).await {
        Ok(claim) => claim,
        Err(error) => {
            settle(
                id,
                stream_id,
                &queue,
                &in_flight,
                cancelled,
                Terminal::Failed(error),
            )
            .await;
            return;
        }
    };
    // Read once the turn is claimed: a mode chosen after this reaches the
    // running turn, so none chosen while the turn starts is lost.
    if !names_mode {
        match core.thread_mode(thread).await {
            Ok(Some(mode)) => approvals.mode = mode,
            Ok(None) => {}
            Err(error) => {
                core.end_turn(thread);
                settle(
                    id,
                    stream_id,
                    &queue,
                    &in_flight,
                    cancelled,
                    Terminal::Failed(error),
                )
                .await;
                return;
            }
        }
    }
    // Set once the turn is claimed, so a refused turn leaves plan mode as
    // it was.
    if let Some(plan) = plan {
        if let Err(error) = core
            .set_thread_settings(thread, &json!({ "plan": plan }))
            .await
        {
            core.end_turn(thread);
            settle(
                id,
                stream_id,
                &queue,
                &in_flight,
                cancelled,
                Terminal::Failed(error),
            )
            .await;
            return;
        }
    }
    let _ = send_event(&queue, stream_id, EventPayload::started(id)).await;
    let (events, mut received) = mpsc::channel(SEND_QUEUE_CAPACITY);
    let relay = |event: TurnEvent| turn_event(id, event);
    let outcome = {
        let run = core.run_turn(thread, prompt, approvals, claim, cancelled.clone(), events);
        tokio::pin!(run);
        loop {
            tokio::select! {
                biased;
                Some(event) = received.recv() => {
                    let _ = send_event(&queue, stream_id, relay(event)).await;
                }
                outcome = &mut run => break outcome,
            }
        }
    };
    while let Ok(event) = received.try_recv() {
        let _ = send_event(&queue, stream_id, relay(event)).await;
    }
    // Released before the terminal event, so a client may start the next
    // turn as soon as it reads it.
    core.end_turn(thread);
    core.after_turn(thread, &outcome.ending);
    let terminal = match &outcome.ending {
        Ending::Interrupted => Terminal::Cancelled,
        Ending::Failed(error) => Terminal::Failed(error.clone()),
        ending => {
            let reason = ending.as_str();
            Terminal::Ended(Box::new(EventPayload::turn_completed(id, reason, outcome)))
        }
    };
    settle(id, stream_id, &queue, &in_flight, cancelled, terminal).await;
}

async fn generate(
    id: u64,
    prompt: String,
    stream_id: u64,
    core: Arc<Core>,
    queue: mpsc::Sender<Outgoing>,
    in_flight: InFlight,
    mut cancelled: watch::Receiver<bool>,
) {
    // Starting the provider request is cancellable at every await: dropping
    // the future or the stream below closes the HTTP connection.
    let mut stream = tokio::select! {
        started = core.chat(&prompt) => match started {
            Ok(stream) => stream,
            Err(error) => {
                settle(id, stream_id, &queue, &in_flight, cancelled, Terminal::Failed(error)).await;
                return;
            }
        },
        _ = cancelled.changed() => {
            settle(id, stream_id, &queue, &in_flight, cancelled, Terminal::Cancelled).await;
            return;
        }
    };

    let terminal = relay_chat(id, stream_id, &mut stream, &queue, &mut cancelled).await;
    // Logged like a turn's requests, so the statistics count it.
    core.log_chat(&stream).await;
    match terminal {
        Some(terminal) => settle(id, stream_id, &queue, &in_flight, cancelled, terminal).await,
        None => abandon(&in_flight, id).await,
    }
}

/// Relays a started `chat` stream; the terminal event, or `None` when the
/// connection is gone and none can be delivered.
async fn relay_chat(
    id: u64,
    stream_id: u64,
    stream: &mut inocode_core::ChatStream,
    queue: &mpsc::Sender<Outgoing>,
    cancelled: &mut watch::Receiver<bool>,
) -> Option<Terminal> {
    match deliver(queue, stream_id, EventPayload::started(id), cancelled).await {
        Delivery::Sent => {}
        // A cancel raced with the start, or the connection is gone: the
        // generation is dead either way and the registry entry must go.
        Delivery::ConnectionGone | Delivery::Cancelled => return Some(Terminal::Cancelled),
    }
    loop {
        tokio::select! {
            next = stream.next() => match next {
                Ok(Some(text)) => {
                    match deliver(queue, stream_id, EventPayload::delta(id, text), cancelled).await {
                        Delivery::Sent => {}
                        Delivery::ConnectionGone => return None,
                        Delivery::Cancelled => return Some(Terminal::Cancelled),
                    }
                }
                Ok(None) => return Some(Terminal::Completed),
                Err(error) => return Some(Terminal::Failed(error)),
            },
            _ = cancelled.changed() => return Some(Terminal::Cancelled),
        }
    }
}

/// The terminal event a generation settles with.
enum Terminal {
    Cancelled,
    Completed,
    Failed(ErrorInfo),
    /// A turn's own completion event.
    Ended(Box<EventPayload>),
}

/// Delivers one event of a running generation, staying responsive to cancel
/// requests while the bounded queue is full.
async fn deliver(
    queue: &mpsc::Sender<Outgoing>,
    stream_id: u64,
    event: EventPayload,
    cancelled: &mut watch::Receiver<bool>,
) -> Delivery {
    tokio::select! {
        sent = send_event(queue, stream_id, event) => {
            if sent.is_ok() {
                Delivery::Sent
            } else {
                // The writer ended: the connection can receive nothing more.
                Delivery::ConnectionGone
            }
        }
        _ = cancelled.changed() => Delivery::Cancelled,
    }
}

enum Delivery {
    Sent,
    ConnectionGone,
    Cancelled,
}

/// Chooses and queues the single terminal event of one generation, then frees
/// its id.
///
/// The registry entry is switched to [`Generation::Settling`] before the flag
/// is read: no further cancel request can be reported `true`, so the flag is
/// final and `cancelled: true` from a cancel request always means the terminal
/// event below is `cancelled`.
async fn settle(
    id: u64,
    stream_id: u64,
    queue: &mpsc::Sender<Outgoing>,
    in_flight: &InFlight,
    cancelled: watch::Receiver<bool>,
    terminal: Terminal,
) {
    let marked = mark_settling(in_flight, id).await;
    let was_cancelled = matches!(terminal, Terminal::Cancelled) || *cancelled.borrow();
    drop(cancelled);
    if !marked {
        // Teardown removed the registry entry: the connection is gone and no
        // event can be delivered.
        return;
    }
    let event = if was_cancelled {
        EventPayload::cancelled(id)
    } else {
        match terminal {
            Terminal::Cancelled => EventPayload::cancelled(id),
            Terminal::Completed => EventPayload::completed(id),
            Terminal::Failed(error) => EventPayload::failed(id, error),
            Terminal::Ended(event) => *event,
        }
    };
    // The queue may be full behind a slow reader; waiting here is bounded by
    // the queue draining or the connection being torn down.
    let _ = send_event(queue, stream_id, event).await;
    // The terminal is queued, so the id may be reused. Only this task removes
    // the entry and the key belonged to it since it started, so this cannot
    // drop another request's slot.
    in_flight.lock().await.remove(&id);
}

/// Removes the registry entry when the connection is gone and no terminal
/// event can be delivered.
async fn abandon(in_flight: &InFlight, id: u64) {
    in_flight.lock().await.remove(&id);
}

/// Switches the entry of `id` to [`Generation::Settling`].
///
/// Returns `false` when the entry is already gone, which only teardown does.
async fn mark_settling(in_flight: &InFlight, id: u64) -> bool {
    let mut pending = in_flight.lock().await;
    match pending.get_mut(&id) {
        Some(slot @ Generation::Running(_)) => {
            *slot = Generation::Settling;
            true
        }
        Some(Generation::Settling) | None => false,
    }
}

async fn cancel(
    request: Request,
    stream_id: u64,
    queue: &mpsc::Sender<Outgoing>,
    in_flight: &InFlight,
) -> bool {
    let response = match request.cancel_target() {
        Ok(target) => {
            // The entry stays in the registry either way: freeing the id is the
            // generation task's job, once its terminal event has been queued.
            let cancelled = match in_flight.lock().await.get(&target) {
                Some(Generation::Running(cancel_tx)) => cancel_tx.send(true).is_ok(),
                Some(Generation::Settling) | None => false,
            };
            Response {
                id: Some(request.id),
                body: ResponseBody::Result {
                    result: json!({ "cancelled": cancelled }),
                },
            }
        }
        Err(error) => Response {
            id: Some(request.id),
            body: ResponseBody::Error { error },
        },
    };
    let payload = serde_json::to_value(response).unwrap_or(Value::Null);
    send(queue, stream_id, payload).await
}

async fn send(queue: &mpsc::Sender<Outgoing>, stream_id: u64, payload: Value) -> bool {
    queue
        .send(Outgoing {
            stream_id,
            payload: Payload::Value(payload),
        })
        .await
        .is_ok()
}

async fn send_event(
    queue: &mpsc::Sender<Outgoing>,
    stream_id: u64,
    event: EventPayload,
) -> Result<(), ()> {
    queue
        .send(Outgoing {
            stream_id,
            payload: Payload::Event(Box::new(event)),
        })
        .await
        .map_err(|_| ())
}

fn error_value(code: &'static str, message: &str) -> Value {
    let response = Response {
        id: None,
        body: ResponseBody::Error {
            error: ErrorInfo::new(code, message),
        },
    };
    serde_json::to_value(response).unwrap_or(Value::Null)
}
