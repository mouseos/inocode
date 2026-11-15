#![allow(dead_code)]

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use inocode::{FrameHeader, FrameReader, FrameType, FrameWriter, HEADER_LEN};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

pub const TIMEOUT: Duration = Duration::from_secs(120);

pub type Socket = (FrameReader<OwnedReadHalf>, FrameWriter<OwnedWriteHalf>);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestMessage {
    pub id: u64,
    #[serde(rename = "type")]
    pub kind: String,
    pub message: String,
}

impl TestMessage {
    pub fn new(id: u64) -> Self {
        Self {
            id,
            kind: "test".to_string(),
            message: "日本語の双方向通信テスト".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ack {
    pub ok: bool,
    pub stream_id: u64,
    pub bytes: u64,
}

pub fn start_message(stream_id: u64, bytes: u64) -> TestMessage {
    TestMessage {
        id: stream_id,
        kind: "start".to_string(),
        message: bytes.to_string(),
    }
}

pub fn split(stream: TcpStream) -> Socket {
    let (read_half, write_half) = stream.into_split();
    (FrameReader::new(read_half), FrameWriter::new(write_half))
}

/// Serves one connection from `handler` and returns the connected raw client.
pub async fn connect_stream<R, F, Fut>(handler: F) -> (TcpStream, JoinHandle<R>)
where
    F: FnOnce(TcpStream) -> Fut + Send + 'static,
    Fut: Future<Output = R> + Send,
    R: Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback listener");
    let address = listener.local_addr().expect("listener address");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept connection");
        handler(stream).await
    });
    let client = TcpStream::connect(address).await.expect("connect loopback");
    (client, server)
}

/// Serves one connection from `handler` and returns the connected raw client.
pub async fn connect_raw<R, F, Fut>(handler: F) -> (TcpStream, JoinHandle<R>)
where
    F: FnOnce(Socket) -> Fut + Send + 'static,
    Fut: Future<Output = R> + Send,
    R: Send + 'static,
{
    connect_stream(move |stream| async move { handler(split(stream)).await }).await
}

/// Serves one connection from `handler` and returns the connected framed client.
pub async fn connect_pair<R, F, Fut>(handler: F) -> (Socket, JoinHandle<R>)
where
    F: FnOnce(Socket) -> Fut + Send + 'static,
    Fut: Future<Output = R> + Send,
    R: Send + 'static,
{
    let (stream, server) = connect_raw(handler).await;
    (split(stream), server)
}

pub async fn with_timeout<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(TIMEOUT, future)
        .await
        .expect("test operation timed out")
}

/// Reads one frame, buffering its payload, or `None` on a clean close.
pub async fn read_frame_from<R>(reader: &mut FrameReader<R>) -> Option<(FrameHeader, Vec<u8>)>
where
    R: AsyncRead + Unpin,
{
    let header = reader.read_header().await.expect("read frame header")?;
    let payload = reader
        .read_payload_to_vec(&header)
        .await
        .expect("read frame payload");
    Some((header, payload))
}

pub fn header_bytes(
    frame_length: u32,
    type_code: u8,
    flags: u8,
    reserved: u16,
    stream_id: u64,
) -> [u8; HEADER_LEN] {
    let mut raw = [0u8; HEADER_LEN];
    raw[..4].copy_from_slice(&frame_length.to_be_bytes());
    raw[4] = type_code;
    raw[5] = flags;
    raw[6..8].copy_from_slice(&reserved.to_be_bytes());
    raw[8..].copy_from_slice(&stream_id.to_be_bytes());
    raw
}

pub fn encode_frame(frame_type: FrameType, stream_id: u64, payload: &[u8]) -> Vec<u8> {
    let header = FrameHeader::new(frame_type, stream_id, payload.len())
        .expect("frame header")
        .encode()
        .expect("encoded header");
    let mut frame = Vec::with_capacity(HEADER_LEN + payload.len());
    frame.extend_from_slice(&header);
    frame.extend_from_slice(payload);
    frame
}

pub fn encode_json_frame(stream_id: u64, value: &TestMessage) -> Vec<u8> {
    let payload = serde_json::to_vec(value).expect("serialized test message");
    encode_frame(FrameType::Json, stream_id, &payload)
}

/// Accepts only one byte per write, to exercise frame reassembly from a
/// byte-wise split stream.
pub struct OneByteAtATime<W> {
    inner: W,
}

impl<W> OneByteAtATime<W> {
    pub fn new(inner: W) -> Self {
        Self { inner }
    }
}

impl<W: AsyncWrite + Unpin> AsyncWrite for OneByteAtATime<W> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let len = buf.len().min(1);
        Pin::new(&mut self.inner).poll_write(cx, &buf[..len])
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// Writes bytes to memory, for tests that need the reconstructed payload.
#[derive(Default)]
pub struct VecWriter(pub Vec<u8>);

impl AsyncWrite for VecWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.0.extend_from_slice(buf);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

/// Produces `len` deterministic bytes without allocating, so a large transfer
/// can be checked against a receiver that only hashes.
pub struct PatternReader {
    remaining: u64,
    offset: u8,
}

impl PatternReader {
    pub fn new(len: u64) -> Self {
        Self {
            remaining: len,
            offset: 0,
        }
    }
}

impl AsyncRead for PatternReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let len = (buf.remaining() as u64).min(self.remaining) as usize;
        if len == 0 {
            return Poll::Ready(Ok(()));
        }
        {
            let unfilled = buf.initialize_unfilled();
            for (index, slot) in unfilled[..len].iter_mut().enumerate() {
                *slot = self.offset.wrapping_add(index as u8);
            }
        }
        self.offset = self.offset.wrapping_add(len as u8);
        self.remaining -= len as u64;
        buf.advance(len);
        Poll::Ready(Ok(()))
    }
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0100_0000_01b3;

/// Receives a payload while hashing it, so the whole transfer is never held in
/// memory.
pub struct HashSink {
    hash: u64,
    bytes: u64,
}

impl HashSink {
    pub fn new() -> Self {
        Self {
            hash: FNV_OFFSET,
            bytes: 0,
        }
    }

    pub fn hash(&self) -> u64 {
        self.hash
    }

    pub fn bytes(&self) -> u64 {
        self.bytes
    }
}

impl AsyncWrite for HashSink {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let mut hash = self.hash;
        for &byte in buf {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(FNV_PRIME);
        }
        self.hash = hash;
        self.bytes += buf.len() as u64;
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

/// Hash of what [`PatternReader`] writes for `len` bytes.
pub fn pattern_hash(len: u64) -> u64 {
    let mut hash = FNV_OFFSET;
    for index in 0..len {
        hash ^= index % 256;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}
