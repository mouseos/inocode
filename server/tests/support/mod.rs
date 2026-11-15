// Several test binaries include this module, and each uses a different part
// of it, so unused-in-this-binary items are expected here.
#![allow(dead_code)]

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use inocode::{FrameReader, FrameWriter};
use inocode_core::Core;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

pub type Client = (FrameReader<OwnedReadHalf>, FrameWriter<OwnedWriteHalf>);

pub const TIMEOUT: Duration = Duration::from_secs(60);
pub const LIVE_TIMEOUT: Duration = Duration::from_secs(180);

pub async fn with_timeout<T>(future: impl Future<Output = T>) -> T {
    with_timeout_of(TIMEOUT, future).await
}

pub async fn with_timeout_of<T>(timeout: Duration, future: impl Future<Output = T>) -> T {
    tokio::time::timeout(timeout, future)
        .await
        .expect("test timed out")
}

/// Starts the server on an ephemeral loopback port. The sender in the returned
/// tuple triggers a clean shutdown when used or dropped.
pub async fn start_server(
    core: Arc<Core>,
) -> (SocketAddr, oneshot::Sender<()>, JoinHandle<io::Result<()>>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback listener");
    let addr = listener.local_addr().expect("listener address");
    let (stop, stopped) = oneshot::channel();
    let handle = tokio::spawn(async move {
        inocode_server::run(listener, core, async {
            let _ = stopped.await;
        })
        .await
    });
    (addr, stop, handle)
}

pub async fn connect(addr: SocketAddr) -> Client {
    let stream = TcpStream::connect(addr).await.expect("connect to server");
    stream.set_nodelay(true).expect("set nodelay");
    let (read_half, write_half) = stream.into_split();
    (FrameReader::new(read_half), FrameWriter::new(write_half))
}

/// Reads one frame without framing the write side, for tests that must send
/// hand-crafted bytes.
pub async fn connect_raw(addr: SocketAddr) -> (FrameReader<OwnedReadHalf>, OwnedWriteHalf) {
    let stream = TcpStream::connect(addr).await.expect("connect to server");
    stream.set_nodelay(true).expect("set nodelay");
    let (read_half, write_half) = stream.into_split();
    (FrameReader::new(read_half), write_half)
}

pub async fn call(client: &mut Client, stream_id: u64, request: Value) -> Value {
    client
        .1
        .write_json(stream_id, &request)
        .await
        .expect("send request");
    next_message(client, stream_id).await
}

pub async fn next_message(client: &mut Client, stream_id: u64) -> Value {
    let (response_stream, response): (u64, Value) =
        client.0.read_json().await.expect("read response");
    assert_eq!(
        response_stream, stream_id,
        "response must arrive on the request stream"
    );
    response
}

/// One chunk of a mock HTTP response body.
#[derive(Debug, Clone)]
pub struct MockReply {
    pub status: u16,
    pub content_type: &'static str,
    pub chunks: Vec<Vec<u8>>,
    pub head_delay: Duration,
    pub chunk_delay: Duration,
}

impl MockReply {
    /// A `200 text/event-stream` reply that writes `chunks` one by one.
    pub fn stream(chunks: Vec<Vec<u8>>) -> Self {
        Self {
            status: 200,
            content_type: "text/event-stream",
            chunks,
            head_delay: Duration::ZERO,
            chunk_delay: Duration::ZERO,
        }
    }

    pub fn status(status: u16, body: &str) -> Self {
        Self {
            status,
            content_type: "application/json",
            chunks: vec![body.as_bytes().to_vec()],
            head_delay: Duration::ZERO,
            chunk_delay: Duration::ZERO,
        }
    }

    /// Delays the response headers, so the request is answered late.
    pub fn with_head_delay(mut self, delay: Duration) -> Self {
        self.head_delay = delay;
        self
    }

    pub fn with_delay(mut self, delay: Duration) -> Self {
        self.chunk_delay = delay;
        self
    }
}

#[derive(Debug, Clone)]
pub struct MockRequest {
    pub method: String,
    pub path: String,
    pub authorization: Option<String>,
    /// All request headers with lowercased names (duplicates keep the last).
    pub headers: std::collections::HashMap<String, String>,
    pub body: Value,
}

/// Minimal HTTP/1.1 server used as a local stand-in for an OpenAI-compatible
/// provider. It writes the response body in caller-controlled chunks, which is
/// how SSE delivery is split and truncated in tests.
pub struct MockApi {
    pub base_url: String,
    requests: Arc<Mutex<Vec<MockRequest>>>,
    write_errors: Arc<AtomicUsize>,
    closed: Arc<AtomicUsize>,
}

impl MockApi {
    pub fn requests(&self) -> Vec<MockRequest> {
        self.requests.lock().expect("mock request log").clone()
    }

    /// Number of response writes the mock failed, which shows that the client
    /// closed its connection while the mock was writing.
    pub fn write_errors(&self) -> usize {
        self.write_errors.load(Ordering::Relaxed)
    }

    /// Number of connections on which the mock observed the client's close.
    ///
    /// Writing after a peer's FIN can still succeed on some operating
    /// systems, so a close is detected by reading until the connection ends,
    /// not by the failure of a single write.
    pub fn closed_count(&self) -> usize {
        self.closed.load(Ordering::Relaxed)
    }
}

pub async fn start_mock<F>(handler: F) -> MockApi
where
    F: Fn(&MockRequest) -> MockReply + Send + Sync + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock listener");
    let addr = listener.local_addr().expect("mock address");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let write_errors = Arc::new(AtomicUsize::new(0));
    let closed = Arc::new(AtomicUsize::new(0));
    let handler = Arc::new(handler);
    let recorded = Arc::clone(&requests);
    let failures = Arc::clone(&write_errors);
    let peers_closed = Arc::clone(&closed);
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let recorded = Arc::clone(&recorded);
            let handler = Arc::clone(&handler);
            let failures = Arc::clone(&failures);
            let closed = Arc::clone(&peers_closed);
            tokio::spawn(async move {
                let _ = serve_mock(stream, recorded, handler, failures, closed).await;
            });
        }
    });
    MockApi {
        base_url: format!("http://{addr}/v1"),
        requests,
        write_errors,
        closed,
    }
}

async fn serve_mock<F>(
    mut stream: TcpStream,
    recorded: Arc<Mutex<Vec<MockRequest>>>,
    handler: Arc<F>,
    write_errors: Arc<AtomicUsize>,
    closed: Arc<AtomicUsize>,
) -> io::Result<()>
where
    F: Fn(&MockRequest) -> MockReply + Send + Sync,
{
    loop {
        let request = match read_request(&mut stream).await {
            Ok(request) => request,
            // The client closed the connection, before a request or while the
            // next one was expected.
            Err(_) => {
                closed.fetch_add(1, Ordering::Relaxed);
                return Ok(());
            }
        };
        recorded
            .lock()
            .expect("mock request log")
            .push(request.clone());
        let reply = handler(&request);
        if write_reply(&mut stream, &reply).await.is_err() {
            write_errors.fetch_add(1, Ordering::Relaxed);
            closed.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }
        // The response declares `connection: close`: end the body so the
        // client sees it complete, then observe the client's own close by
        // reading until the connection ends.
        stream.shutdown().await?;
    }
}

async fn write_reply(stream: &mut TcpStream, reply: &MockReply) -> io::Result<()> {
    if !reply.head_delay.is_zero() {
        tokio::time::sleep(reply.head_delay).await;
    }
    let head = format!(
        "HTTP/1.1 {} {}\r\ncontent-type: {}\r\nconnection: close\r\n\r\n",
        reply.status,
        reason(reply.status),
        reply.content_type
    );
    stream.write_all(head.as_bytes()).await?;
    for chunk in &reply.chunks {
        if !reply.chunk_delay.is_zero() {
            tokio::time::sleep(reply.chunk_delay).await;
        }
        stream.write_all(chunk).await?;
        stream.flush().await?;
    }
    Ok(())
}

async fn read_request(stream: &mut TcpStream) -> io::Result<MockRequest> {
    let mut buffer = Vec::new();
    let head_end = loop {
        let mut chunk = [0u8; 1024];
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "request ended before its headers",
            ));
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(position) = find(&buffer, b"\r\n\r\n") {
            break position + 4;
        }
    };

    let head = String::from_utf8_lossy(&buffer[..head_end]).to_string();
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next().unwrap_or_default().split_whitespace();
    let method = request_line.next().unwrap_or_default().to_string();
    let path = request_line.next().unwrap_or_default().to_string();

    let mut authorization = None;
    let mut content_length = 0usize;
    let mut headers = std::collections::HashMap::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            let (name, value) = (name.trim().to_ascii_lowercase(), value.trim().to_string());
            match name.as_str() {
                "authorization" => authorization = Some(value.clone()),
                "content-length" => content_length = value.parse().unwrap_or(0),
                _ => {}
            }
            headers.insert(name, value);
        }
    }

    let mut body = buffer[head_end..].to_vec();
    while body.len() < content_length {
        let mut chunk = [0u8; 1024];
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(content_length);

    Ok(MockRequest {
        method,
        path,
        authorization,
        headers,
        body: serde_json::from_slice(&body).unwrap_or(Value::Null),
    })
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        _ => "Status",
    }
}

/// One SSE event that carries a text delta.
pub fn sse_delta(text: &str) -> Vec<u8> {
    let payload = json!({ "choices": [{ "delta": { "content": text } }] });
    format!("data: {payload}\n\n").into_bytes()
}

/// The terminal SSE event of a chat completion.
pub fn sse_done() -> Vec<u8> {
    b"data: [DONE]\n\n".to_vec()
}

/// One Anthropic SSE event written in the documented `event:`/`data:` form.
pub fn anthropic_event(event: &str, payload: &Value) -> Vec<u8> {
    format!("event: {event}\ndata: {payload}\n\n").into_bytes()
}

/// A `content_block_delta` event that carries answer text.
pub fn anthropic_text_delta(text: &str) -> Vec<u8> {
    anthropic_event(
        "content_block_delta",
        &json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": { "type": "text_delta", "text": text },
        }),
    )
}

/// A `content_block_delta` event whose delta is not answer text, such as the
/// input of a tool call.
pub fn anthropic_other_delta() -> Vec<u8> {
    anthropic_event(
        "content_block_delta",
        &json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": { "type": "input_json_delta", "partial_json": "{\"tool\":1}" },
        }),
    )
}

/// The `message_stop` event that ends a Messages response.
pub fn anthropic_message_stop() -> Vec<u8> {
    anthropic_event("message_stop", &json!({ "type": "message_stop" }))
}

/// An `error` event as the Messages API sends it inside a stream.
pub fn anthropic_error(message: &str) -> Vec<u8> {
    anthropic_event(
        "error",
        &json!({ "type": "error", "error": { "type": "overloaded_error", "message": message } }),
    )
}

/// The events of a Messages response, in the order the documented stream shows
/// them.
pub fn anthropic_stream(text: &str) -> Vec<Vec<u8>> {
    vec![
        anthropic_event(
            "message_start",
            &json!({ "type": "message_start", "message": { "role": "assistant", "content": [] } }),
        ),
        anthropic_event(
            "content_block_start",
            &json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } }),
        ),
        anthropic_event("ping", &json!({ "type": "ping" })),
        anthropic_text_delta(text),
        anthropic_event(
            "content_block_stop",
            &json!({ "type": "content_block_stop", "index": 0 }),
        ),
        anthropic_event(
            "message_delta",
            &json!({ "type": "message_delta", "delta": { "stop_reason": "end_turn", "stop_sequence": null }, "usage": { "output_tokens": 15 } }),
        ),
        anthropic_message_stop(),
    ]
}

/// Text that the mock handler returns for a given prompt, so tests can check
/// that responses are not mixed between clients.
pub fn reply_for(request: &MockRequest) -> String {
    let prompt = request.body["messages"][0]["content"]
        .as_str()
        .unwrap_or("unknown");
    format!("reply[{prompt}]")
}
