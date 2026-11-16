//! Standalone TCP server for InoCode.
//!
//! The server owns the transport: it accepts connections, decodes JSON request
//! frames, and dispatches them to [`inocode_core::Core`]. Immediate methods
//! (`ping`, `echo`, `cancel`) are answered with a response frame; `chat` is run
//! in its own task and reports progress as `started`, `delta`, `completed`,
//! `failed`, and `cancelled` events.
//!
//! Each connection has one writer task fed by a bounded queue, so frames from
//! concurrent requests never interleave and a client that stops reading cannot
//! grow the queue without limit.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use inocode::{FrameReader, FrameType, FrameWriter};
use inocode_core::{Core, ErrorInfo, EventPayload, Request, Response, ResponseBody};
use serde_json::{json, Value};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch, Mutex};

/// Frames buffered per connection before a slow client blocks its senders.
const SEND_QUEUE_CAPACITY: usize = 64;

/// Pause after an accept error that is not about a single connection.
const ACCEPT_ERROR_PAUSE: std::time::Duration = std::time::Duration::from_secs(1);

struct Outgoing {
    stream_id: u64,
    payload: Payload,
}

/// A frame body waiting for the writer. Chat events stay typed until they
/// are serialized into the frame, so a delta is not first copied into a
/// `serde_json::Value` tree.
enum Payload {
    Value(Value),
    Event(EventPayload),
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
/// errors never end the server.
///
/// Each connection runs in its own task, so one client closing or sending a
/// broken frame does not affect the others.
pub async fn run<F>(listener: TcpListener, core: Arc<Core>, shutdown: F) -> std::io::Result<()>
where
    F: Future<Output = ()> + Send,
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
                tokio::spawn(serve_connection(stream, Arc::clone(&core)));
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

async fn serve_connection(stream: TcpStream, core: Arc<Core>) {
    let (read_half, write_half) = stream.into_split();
    let mut reader = FrameReader::new(read_half);
    let mut writer = FrameWriter::new(write_half);
    let (queue, mut queued) = mpsc::channel::<Outgoing>(SEND_QUEUE_CAPACITY);
    let writer_task = tokio::spawn(async move {
        while let Some(message) = queued.recv().await {
            let written = match &message.payload {
                Payload::Value(value) => writer.write_json(message.stream_id, value).await,
                Payload::Event(event) => writer.write_json(message.stream_id, event).await,
            };
            if written.is_err() {
                break;
            }
        }
    });

    let in_flight: InFlight = Arc::new(Mutex::new(HashMap::new()));
    loop {
        let header = match reader.read_header().await {
            Ok(Some(header)) => header,
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

    // The read side is over: signal every generation of this connection and
    // stop the writer at its next await point. Frames still queued are dropped
    // with the connection — the client closed it or sent an unreadable frame —
    // and awaiting a writer stalled on a dead socket would pin this task.
    in_flight.lock().await.clear();
    drop(queue);
    writer_task.abort();
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
        "cancel" => cancel(request, stream_id, queue, in_flight).await,
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
            tokio::spawn(list_models(
                request.id,
                stream_id,
                Arc::clone(core),
                queue.clone(),
            ));
            true
        }
        _ => {
            let response = core.handle(request);
            let payload = serde_json::to_value(response).unwrap_or(Value::Null);
            send(queue, stream_id, payload).await
        }
    }
}

async fn list_models(id: u64, stream_id: u64, core: Arc<Core>, queue: mpsc::Sender<Outgoing>) {
    let payload = match core.models().await {
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
    };
    // A closed connection has nobody to answer.
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

    match deliver(&queue, stream_id, EventPayload::started(id), &mut cancelled).await {
        Delivery::Sent => {}
        Delivery::ConnectionGone | Delivery::Cancelled => {
            // A cancel raced with the start, or the connection is gone: the
            // generation is dead either way and the registry entry must go.
            settle(
                id,
                stream_id,
                &queue,
                &in_flight,
                cancelled,
                Terminal::Cancelled,
            )
            .await;
            return;
        }
    }

    loop {
        tokio::select! {
            next = stream.next() => match next {
                Ok(Some(text)) => {
                    match deliver(&queue, stream_id, EventPayload::delta(id, text), &mut cancelled).await {
                        Delivery::Sent => {}
                        Delivery::ConnectionGone => {
                            abandon(&in_flight, id).await;
                            return;
                        }
                        Delivery::Cancelled => {
                            settle(id, stream_id, &queue, &in_flight, cancelled, Terminal::Cancelled).await;
                            return;
                        }
                    }
                }
                Ok(None) => {
                    settle(id, stream_id, &queue, &in_flight, cancelled, Terminal::Completed).await;
                    return;
                }
                Err(error) => {
                    settle(id, stream_id, &queue, &in_flight, cancelled, Terminal::Failed(error)).await;
                    return;
                }
            },
            _ = cancelled.changed() => {
                settle(id, stream_id, &queue, &in_flight, cancelled, Terminal::Cancelled).await;
                return;
            }
        }
    }
}

/// The terminal event a generation settles with.
enum Terminal {
    Cancelled,
    Completed,
    Failed(ErrorInfo),
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
            payload: Payload::Event(event),
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
