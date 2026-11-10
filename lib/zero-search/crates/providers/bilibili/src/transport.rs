//! Minimal gRPC client: hand-rolled HTTP/2 framing + HPACK (`hpack`
//! crate) on top of a rustls stream.
//!
//! Why hand-rolled: bilibili's gRPC metadata (`x-bili-*-bin`) carries
//! raw protobuf bytes — including control bytes in length prefixes —
//! which the `http` crate's `HeaderValue` refuses to hold (it models
//! printable header text). On the wire, HTTP/2 header values are opaque
//! octet strings, so the framing layer here writes them verbatim.
//!
//! Scope: one unary call per connection, generous flow-control windows,
//! no stream multiplexing. That is all this provider needs.
//!
//! Egress choice is a deployment concern of the caller, never a
//! hardcoded behaviour: the default is a direct connection.

use anyhow::{anyhow, Context, Result};
use bytes::{BufMut, BytesMut};
use prost::Message;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

/// How the connection to the gRPC host is established.
#[derive(Clone, Debug)]
pub enum Egress {
    /// Direct TCP to the host.
    Direct,
    /// TCP via a SOCKS5 proxy (`host:port`, e.g. `127.0.0.1:1080`).
    Socks5(String),
}

fn tls_config() -> Arc<rustls::ClientConfig> {
    // The dependency tree can enable more than one rustls crypto backend;
    // pin the ring provider explicitly so no auto-detection is needed.
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let mut config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"h2".to_vec()];
    Arc::new(config)
}

// HTTP/2 frame types.
const FRAME_DATA: u8 = 0x0;
const FRAME_HEADERS: u8 = 0x1;
const FRAME_RST_STREAM: u8 = 0x3;
const FRAME_SETTINGS: u8 = 0x4;
const FRAME_PING: u8 = 0x6;
const FRAME_GOAWAY: u8 = 0x7;
const FRAME_WINDOW_UPDATE: u8 = 0x8;
const FRAME_CONTINUATION: u8 = 0x9;

const FLAG_END_STREAM: u8 = 0x1;
const FLAG_ACK: u8 = 0x1;
const FLAG_END_HEADERS: u8 = 0x4;

const MAX_FRAME: usize = 16_384;
const INITIAL_WINDOW: u32 = 16_777_215; // 2^24 - 1

fn frame(stream_id: u32, ty: u8, flags: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(9 + payload.len());
    let len = payload.len() as u32;
    out.push((len >> 16) as u8);
    out.push((len >> 8) as u8);
    out.push(len as u8);
    out.push(ty);
    out.push(flags);
    out.extend_from_slice(&(stream_id & 0x7fff_ffff).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// One unary gRPC call. `headers` are raw (name, value-bytes) pairs —
/// binary metadata (`*-bin`) values are arbitrary protobuf bytes.
pub async fn grpc_unary<Req, Resp>(
    egress: &Egress,
    host: &'static str,
    path: &'static str,
    headers: &[(String, Vec<u8>)],
    msg: &Req,
) -> Result<Resp>
where
    Req: Message,
    Resp: Message + Default,
{
    tokio::time::timeout(
        Duration::from_secs(40),
        grpc_unary_inner(egress, host, path, headers, msg),
    )
    .await
    .context("grpc call timed out")?
}

async fn grpc_unary_inner<Req, Resp>(
    egress: &Egress,
    host: &'static str,
    path: &'static str,
    headers: &[(String, Vec<u8>)],
    msg: &Req,
) -> Result<Resp>
where
    Req: Message,
    Resp: Message + Default,
{
    // --- connect (TCP, optionally via SOCKS5) + TLS ---
    let tcp = match egress {
        Egress::Direct => TcpStream::connect((host, 443))
            .await
            .context("tcp connect")?,
        Egress::Socks5(proxy) => {
            tokio_socks::tcp::Socks5Stream::connect(proxy.as_str(), (host.to_string(), 443u16))
                .await
                .map_err(|e| anyhow!("socks5 connect via {proxy}: {e}"))?
                .into_inner()
        }
    };
    let connector = TlsConnector::from(tls_config());
    let server_name = rustls::pki_types::ServerName::try_from(host)
        .map_err(|_| anyhow!("invalid server name"))?
        .to_owned();
    let mut stream = connector
        .connect(server_name, tcp)
        .await
        .context("tls handshake")?;

    // --- HTTP/2 client preface + SETTINGS ---
    let mut out = Vec::new();
    out.extend_from_slice(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n");
    let mut settings = Vec::new();
    settings.extend_from_slice(&0x3u16.to_be_bytes()); // INITIAL_WINDOW_SIZE
    settings.extend_from_slice(&INITIAL_WINDOW.to_be_bytes());
    out.extend_from_slice(&frame(0, FRAME_SETTINGS, 0, &settings));
    // Grow the connection-level window too.
    out.extend_from_slice(&frame(
        0,
        FRAME_WINDOW_UPDATE,
        0,
        &INITIAL_WINDOW.to_be_bytes(),
    ));

    // --- request HEADERS (HPACK) ---
    let mut all_headers: Vec<(&[u8], &[u8])> = vec![
        (b":method", b"POST"),
        (b":scheme", b"https"),
        (b":path", path.as_bytes()),
        (b":authority", host.as_bytes()),
        (b"content-type", b"application/grpc"),
        (b"te", b"trailers"),
    ];
    for (name, value) in headers {
        all_headers.push((name.as_bytes(), value.as_slice()));
    }
    let mut encoder = hpack::Encoder::new();
    let block = encoder.encode(all_headers);
    // HEADERS + CONTINUATION split at the frame size limit.
    let mut chunks = block.chunks(MAX_FRAME).peekable();
    let first = chunks.next().unwrap_or(&[]);
    let first_flags = if chunks.peek().is_none() {
        FLAG_END_HEADERS
    } else {
        0
    };
    out.extend_from_slice(&frame(1, FRAME_HEADERS, first_flags, first));
    for chunk in chunks {
        let is_last = chunk.len() < MAX_FRAME;
        let flags = if is_last { FLAG_END_HEADERS } else { 0 };
        out.extend_from_slice(&frame(1, FRAME_CONTINUATION, flags, chunk));
    }

    // --- request DATA (one gRPC frame), END_STREAM ---
    let body_bytes = msg.encode_to_vec();
    let mut grpc_frame = BytesMut::with_capacity(body_bytes.len() + 5);
    grpc_frame.put_u8(0);
    grpc_frame.put_u32(body_bytes.len() as u32);
    grpc_frame.extend_from_slice(&body_bytes);
    out.extend_from_slice(&frame(1, FRAME_DATA, FLAG_END_STREAM, &grpc_frame));

    stream.write_all(&out).await.context("write request")?;
    stream.flush().await.ok();

    // --- read loop ---
    let mut decoder = hpack::Decoder::new();
    let mut data = BytesMut::new();
    let mut header_block = BytesMut::new();
    let mut http_status: Option<String> = None;
    let mut debug_headers: Vec<String> = Vec::new();
    let mut grpc_status: Option<String> = None;
    let mut grpc_message: Option<String> = None;
    let mut pending_end_stream = false;
    let mut stream_ended = false;

    while !stream_ended {
        let mut hdr = [0u8; 9];
        stream
            .read_exact(&mut hdr)
            .await
            .context("read frame header")?;
        let len = ((hdr[0] as usize) << 16) | ((hdr[1] as usize) << 8) | hdr[2] as usize;
        if len > 64 * 1024 * 1024 {
            return Err(anyhow!("frame too large: {len}"));
        }
        let ty = hdr[3];
        let flags = hdr[4];
        let sid = u32::from_be_bytes([hdr[5] & 0x7f, hdr[6], hdr[7], hdr[8]]);
        let mut payload = vec![0u8; len];
        if len > 0 {
            stream
                .read_exact(&mut payload)
                .await
                .context("read frame payload")?;
        }

        match ty {
            FRAME_SETTINGS => {
                if flags & FLAG_ACK == 0 {
                    stream
                        .write_all(&frame(0, FRAME_SETTINGS, FLAG_ACK, &[]))
                        .await
                        .context("settings ack")?;
                }
            }
            FRAME_PING => {
                if flags & FLAG_ACK == 0 && payload.len() == 8 {
                    stream
                        .write_all(&frame(0, FRAME_PING, FLAG_ACK, &payload))
                        .await
                        .context("ping ack")?;
                }
            }
            FRAME_GOAWAY => {
                // Server is closing; keep reading what it already sent
                // until the stream ends or the connection drops.
            }
            FRAME_WINDOW_UPDATE => {}
            FRAME_RST_STREAM if sid == 1 => {
                let code = if payload.len() >= 4 {
                    u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]])
                } else {
                    0
                };
                return Err(anyhow!("stream reset by server (error code {code})"));
            }
            FRAME_DATA if sid == 1 => {
                // Strip padding if PADDED.
                let mut body: &[u8] = &payload;
                if flags & 0x8 != 0 {
                    if body.is_empty() {
                        return Err(anyhow!("malformed padded data frame"));
                    }
                    let pad = body[0] as usize;
                    if body.len() < 1 + pad {
                        return Err(anyhow!("malformed padded data frame"));
                    }
                    body = &body[1..body.len() - pad];
                }
                data.extend_from_slice(body);
                if flags & FLAG_END_STREAM != 0 {
                    stream_ended = true;
                }
                // Return flow-control credit immediately.
                if len > 0 {
                    let inc = (len as u32).to_be_bytes();
                    let mut w = frame(0, FRAME_WINDOW_UPDATE, 0, &inc);
                    w.extend_from_slice(&frame(1, FRAME_WINDOW_UPDATE, 0, &inc));
                    stream.write_all(&w).await.context("window update")?;
                }
            }
            FRAME_HEADERS | FRAME_CONTINUATION if sid == 1 => {
                if ty == FRAME_HEADERS && flags & FLAG_END_STREAM != 0 {
                    pending_end_stream = true;
                }
                header_block.extend_from_slice(&payload);
                if flags & FLAG_END_HEADERS != 0 {
                    let decoded = decoder
                        .decode(&header_block)
                        .map_err(|e| anyhow!("hpack decode: {e:?}"))?;
                    header_block.clear();
                    let end_stream = pending_end_stream;
                    pending_end_stream = false;
                    for (name, value) in &decoded {
                        debug_headers.push(format!(
                            "{}: {}",
                            String::from_utf8_lossy(name),
                            String::from_utf8_lossy(value)
                        ));
                        match name.as_slice() {
                            b":status" => {
                                http_status = Some(String::from_utf8_lossy(value).into_owned())
                            }
                            b"grpc-status" => {
                                grpc_status = Some(String::from_utf8_lossy(value).into_owned())
                            }
                            b"grpc-message" => {
                                grpc_message = Some(String::from_utf8_lossy(value).into_owned())
                            }
                            _ => {}
                        }
                    }
                    if end_stream {
                        stream_ended = true;
                    }
                }
            }
            _ => {
                // Unknown/other-stream frames: payload already skipped.
            }
        }
    }

    if let Some(status) = &http_status {
        if status != "200" {
            let preview: String = data
                .iter()
                .take(700)
                .map(|b| {
                    if b.is_ascii_graphic() || *b == b' ' {
                        *b as char
                    } else {
                        '.'
                    }
                })
                .collect();
            return Err(anyhow!(
                "http status {status}, grpc-status={grpc_status:?} grpc-message={grpc_message:?} headers={debug_headers:?} body={preview:?}"
            ));
        }
    }
    if let Some(code) = &grpc_status {
        if code != "0" {
            return Err(anyhow!(
                "grpc status {code}: {}",
                grpc_message.as_deref().unwrap_or("")
            ));
        }
    }

    // --- decode the first gRPC message frame ---
    let buf: &[u8] = &data;
    if buf.len() < 5 {
        return Err(anyhow!(
            "grpc response too short ({} bytes), grpc-status={grpc_status:?} grpc-message={grpc_message:?}",
            buf.len()
        ));
    }
    let compressed = buf[0];
    if compressed != 0 {
        return Err(anyhow!("grpc response compressed (unsupported)"));
    }
    let mlen = u32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]) as usize;
    if buf.len() < 5 + mlen {
        return Err(anyhow!("grpc frame truncated"));
    }
    let resp = Resp::decode(&buf[5..5 + mlen]).context("decode grpc response")?;
    Ok(resp)
}
