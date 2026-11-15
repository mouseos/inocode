//! Standalone TCP server for InoCode.
//!
//! The server owns the transport: it accepts connections, decodes JSON request
//! frames, asks [`inocode_core::Core`] to handle each decoded request, and
//! writes the response back on the stream it arrived on. Binary and end frames
//! belong to the transport protocol, not to the request protocol, so they are
//! answered with an error instead of being handled.

use std::future::Future;
use std::sync::Arc;

use inocode::{FrameReader, FrameType, FrameWriter};
use inocode_core::{Core, ErrorInfo, Request, Response, ResponseBody};
use serde_json::Value;
use tokio::net::{TcpListener, TcpStream};

/// Accepts connections until `shutdown` completes, then returns.
///
/// Each connection runs in its own task, so one client closing or sending a
/// broken frame does not affect the others.
pub async fn run<F>(listener: TcpListener, shutdown: F) -> std::io::Result<()>
where
    F: Future<Output = ()> + Send,
{
    let core = Arc::new(Core);
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
    loop {
        let header = match reader.read_header().await {
            Ok(Some(header)) => header,
            // Clean close, or a frame that can never complete: end this
            // connection and keep accepting others.
            Ok(None) | Err(_) => return,
        };
        let response = match header.frame_type {
            FrameType::Json => match reader.read_payload_to_vec(&header).await {
                Ok(payload) => handle_payload(&payload, &core),
                Err(_) => return,
            },
            FrameType::Binary | FrameType::End => {
                let mut discard = tokio::io::sink();
                if reader.read_payload_to(&header, &mut discard).await.is_err() {
                    return;
                }
                error_response("invalid_request", "Only JSON request frames are accepted")
            }
        };
        if writer
            .write_json(header.stream_id, &response)
            .await
            .is_err()
        {
            return;
        }
    }
}

fn handle_payload(payload: &[u8], core: &Core) -> Response {
    let value: Value = match serde_json::from_slice(payload) {
        Ok(value) => value,
        Err(_) => return error_response("invalid_json", "Invalid JSON"),
    };
    match serde_json::from_value::<Request>(value) {
        Ok(request) => core.handle(request),
        Err(_) => error_response("invalid_request", "Invalid request"),
    }
}

fn error_response(code: &'static str, message: &str) -> Response {
    Response {
        id: None,
        body: ResponseBody::Error {
            error: ErrorInfo::new(code, message),
        },
    }
}
