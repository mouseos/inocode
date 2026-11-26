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
//! Each connection has one writer task fed by a bounded queue, so frames from
//! concurrent requests never interleave and a client that stops reading cannot
//! grow the queue without limit.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use inocode::{FrameReader, FrameType, FrameWriter, ProtocolError};
use inocode_core::turn::{Ending, TurnEvent};
use inocode_core::{Core, ErrorInfo, EventPayload, Request, Response, ResponseBody};
use serde_json::{json, Value};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch, Mutex};

/// Frames buffered per connection before a slow client blocks its senders.
const SEND_QUEUE_CAPACITY: usize = 64;

/// Messages `thread/read` returns when the request names no `limit`, and the
/// most it accepts.
const READ_DEFAULT_LIMIT: u64 = 100;
const READ_MAX_LIMIT: u64 = 1000;

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
                tokio::spawn(serve_connection(stream, Arc::clone(&core), Arc::clone(&token)));
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

async fn serve_connection(stream: TcpStream, core: Arc<Core>, token: Arc<str>) {
    let (read_half, write_half) = stream.into_split();
    let mut reader = FrameReader::new(read_half);
    let mut writer = FrameWriter::new(write_half);
    let (queue, mut queued) = mpsc::channel::<Outgoing>(SEND_QUEUE_CAPACITY);
    let writer_task = tokio::spawn(async move {
        while let Some(message) = queued.recv().await {
            let written = match &message.payload {
                Payload::Value(value) => writer.write_json(message.stream_id, value).await,
                Payload::Event(event) => writer.write_json(message.stream_id, &**event).await,
            };
            match written {
                Ok(()) => {}
                // Nothing of the frame was written, so the connection stays
                // usable: the client is told what it did not get.
                Err(ProtocolError::PayloadTooLarge { payload_len, max }) => {
                    let replaced = too_large(&message.payload, payload_len, max);
                    if writer
                        .write_json(message.stream_id, &replaced)
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let in_flight: InFlight = Arc::new(Mutex::new(HashMap::new()));
    let mut flush = authenticate(&mut reader, &queue, &token).await;
    if !flush {
        loop {
            let header = match reader.read_header().await {
                Ok(Some(header)) => header,
                Err(ProtocolError::PayloadTooLarge { payload_len, max }) => {
                    // The rest of the frame is not read, so the connection
                    // cannot go on; the client is told why it ends.
                    let message = Outgoing {
                        stream_id: 0,
                        payload: Payload::Value(error_value(
                            "frame_too_large",
                            &format!(
                                "a frame of {payload_len} bytes is over the limit of {max} \
                                 bytes; the connection is closed"
                            ),
                        )),
                    };
                    flush = queue.send(message).await.is_ok();
                    break;
                }
                // Clean close, or a frame that can never complete.
                Ok(None) | Err(_) => break,
            };
            if header.frame_type != FrameType::Json {
                let mut discard = tokio::io::sink();
                if reader.read_payload_to(&header, &mut discard).await.is_err() {
                    break;
                }
                let message = Outgoing {
                    stream_id: header.stream_id,
                    payload: Payload::Value(error_value(
                        "invalid_request",
                        "Only JSON request frames are accepted",
                    )),
                };
                if queue.send(message).await.is_err() {
                    break;
                }
                continue;
            }
            let payload = match reader.read_payload_to_vec(&header).await {
                Ok(payload) => payload,
                Err(_) => break,
            };
            if !dispatch(&payload, header.stream_id, &core, &queue, &in_flight).await {
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

/// Reads the first frame of a connection, which must be an `auth` request
/// with the server's token, and answers it. Returns `true` when the
/// connection is refused: its answer is queued and nothing more is read.
async fn authenticate(
    reader: &mut FrameReader<tokio::net::tcp::OwnedReadHalf>,
    queue: &mpsc::Sender<Outgoing>,
    token: &str,
) -> bool {
    let first = tokio::time::timeout(AUTH_WAIT, async {
        let header = reader.read_header().await.ok()??;
        if header.frame_type != FrameType::Json {
            return Some((header.stream_id, Value::Null));
        }
        let payload = reader.read_payload_to_vec(&header).await.ok()?;
        Some((
            header.stream_id,
            serde_json::from_slice(&payload).unwrap_or(Value::Null),
        ))
    })
    .await;
    let Ok(Some((stream_id, request))) = first else {
        return true;
    };
    let id = request.get("id").and_then(Value::as_u64);
    let given = match request.get("method").and_then(Value::as_str) {
        Some("auth") => request
            .get("params")
            .and_then(|params| params.get("token"))
            .and_then(Value::as_str),
        _ => None,
    };
    let accepted = given.is_some_and(|given| same_secret(given.as_bytes(), token.as_bytes()));
    let body = if accepted {
        ResponseBody::Result {
            result: json!({ "authenticated": true }),
        }
    } else {
        ResponseBody::Error {
            error: ErrorInfo::new(
                "unauthorized",
                "the first request must be auth with the token of this server's start",
            ),
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
    payload: &[u8],
    stream_id: u64,
    core: &Arc<Core>,
    queue: &mpsc::Sender<Outgoing>,
    in_flight: &InFlight,
) -> bool {
    let value: Value = match serde_json::from_slice(payload) {
        Ok(value) => value,
        Err(_) => {
            return send(
                queue,
                stream_id,
                error_value("invalid_json", "Invalid JSON"),
            )
            .await
        }
    };
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
        "thread/start" | "thread/read" | "goal/set" | "goal/read" | "goal/stop" => {
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
        "goal/set" => set_goal(&request, &core).await,
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

/// `thread/list`: `project`, a project's key, and `limit` (100 when absent,
/// at most 1,000).
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
    core.threads(project, limit as usize).await
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
        ((start.prompt, start.reply_to), (start.approvals, false)),
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

async fn start_turn(
    request: Request,
    stream_id: u64,
    core: &Arc<Core>,
    queue: &mpsc::Sender<Outgoing>,
    in_flight: &InFlight,
) -> bool {
    let parsed = request.thread().and_then(|thread| {
        let prompt = (request.prompt()?.to_string(), request.reply_to()?);
        Ok((thread, prompt, (request.approvals()?, request.names_mode())))
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

    tokio::spawn(run_turn(
        id,
        thread,
        (prompt, allowed),
        stream_id,
        Arc::clone(core),
        queue.clone(),
        Arc::clone(in_flight),
        cancel_rx,
    ));
    true
}

/// Runs one turn and relays its events. The turn goes on when the
/// connection closes: what it makes is stored and `thread/read` returns it.
#[allow(clippy::too_many_arguments)]
async fn run_turn(
    id: u64,
    thread: inocode_core::store::SessionId,
    ((prompt, reply_to), (mut approvals, names_mode)): (
        (String, Option<i64>),
        (inocode_core::turn::Approvals, bool),
    ),
    stream_id: u64,
    core: Arc<Core>,
    queue: mpsc::Sender<Outgoing>,
    in_flight: InFlight,
    cancelled: watch::Receiver<bool>,
) {
    if !names_mode {
        match core.thread_mode(thread).await {
            Ok(Some(mode)) => approvals.mode = mode,
            Ok(None) => {}
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
    }
    let claim = match core.begin_turn(thread).await {
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
    let _ = send_event(&queue, stream_id, EventPayload::started(id)).await;
    let (events, mut received) = mpsc::channel(SEND_QUEUE_CAPACITY);
    let relay = |event: TurnEvent| match event {
        TurnEvent::Delta(text) => EventPayload::delta(id, text),
        TurnEvent::Thinking(text) => EventPayload::thinking(id, text),
        TurnEvent::ThinkingTranslation(piece) => EventPayload::thinking_translation(id, piece),
        TurnEvent::Message { seq, message } => EventPayload::message(id, seq, &message),
        TurnEvent::Elicitation(elicitation) => EventPayload::elicitation(id, elicitation),
        TurnEvent::McpProgress(progress) => EventPayload::mcp(id, "mcp_progress", progress),
        TurnEvent::McpLog(log) => EventPayload::mcp(id, "mcp_log", log),
        TurnEvent::Estimate(estimate) => EventPayload::estimate(id, &estimate),
        TurnEvent::Title(title) => EventPayload::title(id, "title_updated", title),
        TurnEvent::TitleProposed(title) => EventPayload::title(id, "title_proposed", title),
    };
    let outcome = {
        let run = core.run_turn(
            thread,
            prompt,
            reply_to,
            approvals,
            claim,
            cancelled.clone(),
            events,
        );
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
