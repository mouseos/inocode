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
use tokio::sync::{mpsc, oneshot, Mutex};

/// Frames buffered per connection before a slow client blocks its senders.
const SEND_QUEUE_CAPACITY: usize = 64;

struct Outgoing {
    stream_id: u64,
    payload: Value,
}

/// Cancel handles of the generations running on one connection.
type InFlight = Arc<Mutex<HashMap<u64, oneshot::Sender<()>>>>;

/// Accepts connections until `shutdown` completes, then returns.
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
                let (stream, _) = accepted?;
                stream.set_nodelay(true)?;
                tokio::spawn(serve_connection(stream, Arc::clone(&core)));
            }
            () = &mut shutdown => return Ok(()),
        }
    }
}

async fn serve_connection(stream: TcpStream, core: Arc<Core>) {
    let (read_half, write_half) = stream.into_split();
    let mut reader = FrameReader::new(read_half);
    let mut writer = FrameWriter::new(write_half);
    let (queue, mut queued) = mpsc::channel::<Outgoing>(SEND_QUEUE_CAPACITY);
    let writer_task = tokio::spawn(async move {
        while let Some(message) = queued.recv().await {
            if writer
                .write_json(message.stream_id, &message.payload)
                .await
                .is_err()
            {
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
            let payload = error_value("invalid_request", "Only JSON request frames are accepted");
            let message = Outgoing {
                stream_id: header.stream_id,
                payload,
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

    // Dropping the last queue sender ends the writer; dropping the cancel
    // handles stops the generations this connection still owns.
    in_flight.lock().await.clear();
    drop(queue);
    let _ = writer_task.await;
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
        _ => {
            let response = core.handle(request);
            let payload = serde_json::to_value(response).unwrap_or(Value::Null);
            send(queue, stream_id, payload).await
        }
    }
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
        return send_event(queue, stream_id, EventPayload::failed(id, error))
            .await
            .is_ok();
    }
    let (cancel_tx, cancel_rx) = oneshot::channel();
    pending.insert(id, cancel_tx);
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
    mut cancel: oneshot::Receiver<()>,
) {
    let _ = stream_chat(id, &prompt, stream_id, &core, &queue, &mut cancel).await;
    in_flight.lock().await.remove(&id);
}

async fn stream_chat(
    id: u64,
    prompt: &str,
    stream_id: u64,
    core: &Core,
    queue: &mpsc::Sender<Outgoing>,
    cancel: &mut oneshot::Receiver<()>,
) -> Result<(), ()> {
    let mut stream = match core.chat(prompt).await {
        Ok(stream) => stream,
        Err(error) => {
            send_event(queue, stream_id, EventPayload::failed(id, error)).await?;
            return Ok(());
        }
    };
    send_event(queue, stream_id, EventPayload::started(id)).await?;
    loop {
        tokio::select! {
            next = stream.next() => match next {
                Ok(Some(text)) => {
                    send_event(queue, stream_id, EventPayload::delta(id, text)).await?;
                }
                Ok(None) => {
                    send_event(queue, stream_id, EventPayload::completed(id)).await?;
                    return Ok(());
                }
                Err(error) => {
                    send_event(queue, stream_id, EventPayload::failed(id, error)).await?;
                    return Ok(());
                }
            },
            _ = &mut *cancel => {
                send_event(queue, stream_id, EventPayload::cancelled(id)).await?;
                return Ok(());
            }
        }
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
            let cancelled = match in_flight.lock().await.remove(&target) {
                Some(cancel) => cancel.send(()).is_ok(),
                None => false,
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
    queue.send(Outgoing { stream_id, payload }).await.is_ok()
}

async fn send_event(
    queue: &mpsc::Sender<Outgoing>,
    stream_id: u64,
    event: EventPayload,
) -> Result<(), ()> {
    let payload = serde_json::to_value(event).map_err(|_| ())?;
    queue
        .send(Outgoing { stream_id, payload })
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
