//! Streaming frame transport over any `AsyncRead` / `AsyncWrite` pair.
//!
//! A TCP connection is a byte stream: one read may return part of a header,
//! several frames, or one byte of a payload. [`FrameReader`] therefore reads
//! exact byte ranges per frame and streams payloads in fixed-size pieces, and
//! [`FrameWriter`] writes a header followed by its payload for one frame at a
//! time.

use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::ProtocolError;
use crate::frame::{FrameHeader, FrameType, CHUNK_SIZE, DEFAULT_MAX_PAYLOAD_LEN, HEADER_LEN};

fn map_eof(error: std::io::Error) -> ProtocolError {
    if error.kind() == std::io::ErrorKind::UnexpectedEof {
        ProtocolError::UnexpectedEof
    } else {
        ProtocolError::Io(error)
    }
}

/// Serializes into `inner` while refusing to buffer more than a fixed number
/// of payload bytes.
struct LimitedWriter<'a> {
    inner: &'a mut Vec<u8>,
    remaining: usize,
}

impl std::io::Write for LimitedWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if buf.len() > self.remaining {
            return Err(std::io::Error::from(std::io::ErrorKind::WriteZero));
        }
        self.inner.extend_from_slice(buf);
        self.remaining -= buf.len();
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

async fn read_exact_checked<R>(reader: &mut R, buf: &mut [u8]) -> Result<(), ProtocolError>
where
    R: AsyncRead + Unpin,
{
    reader.read_exact(buf).await.map_err(map_eof)?;
    Ok(())
}

/// Writes a header and its payload in one vectored write.
///
/// Keeping a frame in a single write keeps the small header from being held
/// back by Nagle while a large payload is still pending.
async fn write_parts<W>(
    writer: &mut W,
    header: &[u8; HEADER_LEN],
    payload: &[u8],
) -> Result<(), ProtocolError>
where
    W: AsyncWrite + Unpin,
{
    let mut header_done = 0usize;
    let mut payload_done = 0usize;
    while header_done < header.len() || payload_done < payload.len() {
        let buffers = [
            std::io::IoSlice::new(&header[header_done..]),
            std::io::IoSlice::new(&payload[payload_done..]),
        ];
        let written = writer.write_vectored(&buffers).await?;
        if written == 0 {
            return Err(ProtocolError::Io(std::io::Error::from(
                std::io::ErrorKind::WriteZero,
            )));
        }
        let header_remaining = header.len() - header_done;
        if written < header_remaining {
            header_done += written;
        } else {
            payload_done += written - header_remaining;
            header_done = header.len();
        }
    }
    Ok(())
}

/// Reads frames from a byte stream.
///
/// Payloads are streamed through a fixed-size buffer, so memory use does not
/// grow with the size of a transfer. `max_payload_len` bounds the memory a
/// frame from the peer can ask for.
pub struct FrameReader<R> {
    inner: R,
    max_payload_len: usize,
    buf: Vec<u8>,
}

impl<R> FrameReader<R>
where
    R: AsyncRead + Unpin,
{
    /// Reads frames with [`DEFAULT_MAX_PAYLOAD_LEN`] as the payload bound.
    pub fn new(inner: R) -> Self {
        Self::with_max_payload_len(inner, DEFAULT_MAX_PAYLOAD_LEN)
    }

    /// Reads frames with an explicit payload bound in bytes.
    pub fn with_max_payload_len(inner: R, max_payload_len: usize) -> Self {
        Self {
            inner,
            max_payload_len,
            buf: vec![0; CHUNK_SIZE],
        }
    }

    /// Reads the next frame header.
    ///
    /// `Ok(None)` means the peer closed the connection between frames. A close
    /// inside a header or payload is [`ProtocolError::UnexpectedEof`], because
    /// the frame that was announced will never arrive complete.
    pub async fn read_header(&mut self) -> Result<Option<FrameHeader>, ProtocolError> {
        let mut raw = [0u8; HEADER_LEN];
        let read = self.inner.read(&mut raw).await?;
        if read == 0 {
            return Ok(None);
        }
        read_exact_checked(&mut self.inner, &mut raw[read..]).await?;
        let header = FrameHeader::decode(&raw)?;
        if header.payload_len > self.max_payload_len {
            return Err(ProtocolError::PayloadTooLarge {
                payload_len: header.payload_len,
                max: self.max_payload_len,
            });
        }
        Ok(Some(header))
    }

    /// Streams the payload of `header` into `out`.
    pub async fn read_payload_to<W>(
        &mut self,
        header: &FrameHeader,
        out: &mut W,
    ) -> Result<u64, ProtocolError>
    where
        W: AsyncWrite + Unpin,
    {
        let mut remaining = header.payload_len;
        while remaining > 0 {
            let take = remaining.min(self.buf.len());
            read_exact_checked(&mut self.inner, &mut self.buf[..take]).await?;
            out.write_all(&self.buf[..take]).await?;
            remaining -= take;
        }
        Ok(header.payload_len as u64)
    }

    /// Buffers the payload of `header` in memory.
    ///
    /// Intended for control messages and other payloads known to be small; use
    /// [`Self::read_payload_to`] for binary data of unknown size.
    pub async fn read_payload_to_vec(
        &mut self,
        header: &FrameHeader,
    ) -> Result<Vec<u8>, ProtocolError> {
        let mut out = Vec::with_capacity(header.payload_len);
        let mut remaining = header.payload_len;
        while remaining > 0 {
            let take = remaining.min(self.buf.len());
            read_exact_checked(&mut self.inner, &mut self.buf[..take]).await?;
            out.extend_from_slice(&self.buf[..take]);
            remaining -= take;
        }
        Ok(out)
    }

    /// Reads one JSON frame and returns its stream id with the parsed value.
    pub async fn read_json<T>(&mut self) -> Result<(u64, T), ProtocolError>
    where
        T: DeserializeOwned,
    {
        let header = self
            .read_header()
            .await?
            .ok_or(ProtocolError::UnexpectedEof)?;
        if header.frame_type != FrameType::Json {
            return Err(ProtocolError::UnexpectedFrameType {
                expected: FrameType::Json,
                actual: header.frame_type,
            });
        }
        let payload = self.read_payload_to_vec(&header).await?;
        let value = serde_json::from_slice(&payload)?;
        Ok((header.stream_id, value))
    }

    /// Reads the binary transfer on `stream_id` until its End frame, streaming
    /// the payload bytes into `out` and returning the byte count.
    pub async fn read_binary_to<W>(
        &mut self,
        stream_id: u64,
        out: &mut W,
    ) -> Result<u64, ProtocolError>
    where
        W: AsyncWrite + Unpin,
    {
        let mut total = 0u64;
        loop {
            let header = self
                .read_header()
                .await?
                .ok_or(ProtocolError::UnexpectedEof)?;
            if header.stream_id != stream_id {
                return Err(ProtocolError::UnexpectedStreamId {
                    expected: stream_id,
                    actual: header.stream_id,
                });
            }
            match header.frame_type {
                FrameType::Binary => total += self.read_payload_to(&header, out).await?,
                FrameType::End => {
                    let mut discard = tokio::io::sink();
                    self.read_payload_to(&header, &mut discard).await?;
                    return Ok(total);
                }
                FrameType::Json => {
                    return Err(ProtocolError::UnexpectedFrameType {
                        expected: FrameType::Binary,
                        actual: FrameType::Json,
                    })
                }
            }
        }
    }
}

/// Writes frames to a byte stream.
///
/// JSON messages are serialized into a reusable buffer and written as a single
/// header-plus-payload write. Binary chunks are written as the header followed
/// by the caller's slice, so large data is never copied into a frame buffer.
pub struct FrameWriter<W> {
    inner: W,
    max_payload_len: usize,
    buf: Vec<u8>,
}

impl<W> FrameWriter<W>
where
    W: AsyncWrite + Unpin,
{
    /// Writes frames with [`DEFAULT_MAX_PAYLOAD_LEN`] as the payload bound.
    pub fn new(inner: W) -> Self {
        Self::with_max_payload_len(inner, DEFAULT_MAX_PAYLOAD_LEN)
    }

    /// Writes frames with an explicit payload bound in bytes.
    pub fn with_max_payload_len(inner: W, max_payload_len: usize) -> Self {
        Self {
            inner,
            max_payload_len,
            buf: Vec::new(),
        }
    }

    /// The writer the frames go to.
    pub fn into_inner(self) -> W {
        self.inner
    }

    fn prepare(
        &self,
        frame_type: FrameType,
        stream_id: u64,
        payload_len: usize,
    ) -> Result<[u8; HEADER_LEN], ProtocolError> {
        if payload_len > self.max_payload_len {
            return Err(ProtocolError::PayloadTooLarge {
                payload_len,
                max: self.max_payload_len,
            });
        }
        FrameHeader::new(frame_type, stream_id, payload_len)?.encode()
    }

    /// Sends one JSON control frame.
    ///
    /// Serialization is capped at the payload limit through a counting writer,
    /// so an oversized value is rejected without ever buffering more than the
    /// configured maximum.
    pub async fn write_json<T>(&mut self, stream_id: u64, value: &T) -> Result<(), ProtocolError>
    where
        T: Serialize,
    {
        self.buf.clear();
        self.buf.resize(HEADER_LEN, 0);
        let mut limited = LimitedWriter {
            inner: &mut self.buf,
            remaining: self.max_payload_len,
        };
        if serde_json::to_writer(&mut limited, value).is_err() {
            return Err(ProtocolError::PayloadTooLarge {
                // Serialization stopped at the limit, so only a lower bound is
                // known.
                payload_len: self.max_payload_len + 1,
                max: self.max_payload_len,
            });
        }
        let payload_len = self.buf.len() - HEADER_LEN;
        let header = self.prepare(FrameType::Json, stream_id, payload_len)?;
        self.buf[..HEADER_LEN].copy_from_slice(&header);
        self.inner.write_all(&self.buf).await?;
        Ok(())
    }

    /// Sends `chunk` as one Binary frame without copying it.
    pub async fn write_binary_chunk(
        &mut self,
        stream_id: u64,
        chunk: &[u8],
    ) -> Result<(), ProtocolError> {
        let header = self.prepare(FrameType::Binary, stream_id, chunk.len())?;
        write_parts(&mut self.inner, &header, chunk).await
    }

    /// Sends the End frame that terminates the binary transfer on `stream_id`.
    pub async fn write_binary_end(&mut self, stream_id: u64) -> Result<(), ProtocolError> {
        let header = self.prepare(FrameType::End, stream_id, 0)?;
        self.inner.write_all(&header).await?;
        Ok(())
    }

    /// Sends everything `src` yields as Binary frames of at most 64 KiB, then
    /// an End frame, and returns the byte count sent.
    pub async fn write_binary_stream<R>(
        &mut self,
        stream_id: u64,
        src: &mut R,
    ) -> Result<u64, ProtocolError>
    where
        R: AsyncRead + Unpin,
    {
        if self.buf.len() < CHUNK_SIZE {
            self.buf.resize(CHUNK_SIZE, 0);
        }
        let mut total = 0u64;
        loop {
            let read = src.read(&mut self.buf[..CHUNK_SIZE]).await?;
            if read == 0 {
                break;
            }
            let header = self.prepare(FrameType::Binary, stream_id, read)?;
            write_parts(&mut self.inner, &header, &self.buf[..read]).await?;
            total += read as u64;
        }
        self.write_binary_end(stream_id).await?;
        Ok(total)
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    use tokio::io::duplex;

    use super::*;

    #[derive(Default)]
    struct VecWriter(Vec<u8>);

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

    fn header_of(frame_type: FrameType, stream_id: u64, payload_len: usize) -> [u8; HEADER_LEN] {
        FrameHeader::new(frame_type, stream_id, payload_len)
            .unwrap()
            .encode()
            .unwrap()
    }

    #[tokio::test]
    async fn clean_close_between_frames_is_not_an_error() {
        let (client, server) = duplex(64);
        drop(client);
        let mut reader = FrameReader::new(server);
        assert!(reader.read_header().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn close_inside_a_header_is_an_error() {
        let (mut client, server) = duplex(64);
        client
            .write_all(&header_of(FrameType::Json, 1, 4)[..10])
            .await
            .unwrap();
        drop(client);
        let mut reader = FrameReader::new(server);
        assert!(matches!(
            reader.read_header().await,
            Err(ProtocolError::UnexpectedEof)
        ));
    }

    #[tokio::test]
    async fn close_inside_a_payload_is_an_error() {
        let (mut client, server) = duplex(64);
        client
            .write_all(&header_of(FrameType::Binary, 1, 8))
            .await
            .unwrap();
        client.write_all(b"1234").await.unwrap();
        drop(client);
        let mut reader = FrameReader::new(server);
        let header = reader.read_header().await.unwrap().unwrap();
        assert!(matches!(
            reader.read_payload_to_vec(&header).await,
            Err(ProtocolError::UnexpectedEof)
        ));
    }

    #[tokio::test]
    async fn rejects_payload_above_the_configured_bound() {
        let (mut client, server) = duplex(64);
        client
            .write_all(&header_of(FrameType::Binary, 1, 9))
            .await
            .unwrap();
        let mut reader = FrameReader::with_max_payload_len(server, 8);
        assert!(matches!(
            reader.read_header().await,
            Err(ProtocolError::PayloadTooLarge {
                payload_len: 9,
                max: 8
            })
        ));
    }

    #[tokio::test]
    async fn rejects_json_payloads_above_the_writer_bound() {
        let (client, mut server) = duplex(64);
        let mut writer = FrameWriter::with_max_payload_len(client, 8);

        // A JSON string whose quoted form is exactly 8 payload bytes.
        writer.write_json(1, &"123456").await.unwrap();
        let (stream_id, value): (u64, String) =
            FrameReader::new(&mut server).read_json().await.unwrap();
        assert_eq!((stream_id, value.as_str()), (1, "123456"));

        // One character more must be rejected without buffering it.
        assert!(matches!(
            writer.write_json(2, &"1234567").await,
            Err(ProtocolError::PayloadTooLarge { max: 8, .. })
        ));
        // After the rejection the writer is still usable for small values.
        writer.write_json(3, &"ok").await.unwrap();
    }

    #[tokio::test]
    async fn read_json_rejects_frames_of_another_type() {
        let (client, server) = duplex(64);
        let mut writer = FrameWriter::new(client);
        writer.write_binary_chunk(1, b"bytes").await.unwrap();
        drop(writer);
        let mut reader = FrameReader::new(server);
        assert!(matches!(
            reader.read_json::<serde_json::Value>().await,
            Err(ProtocolError::UnexpectedFrameType {
                expected: FrameType::Json,
                actual: FrameType::Binary,
            })
        ));
    }

    #[tokio::test]
    async fn read_binary_rejects_json_frames() {
        let (client, server) = duplex(64);
        let mut writer = FrameWriter::new(client);
        writer.write_json(1, &"control").await.unwrap();
        drop(writer);
        let mut reader = FrameReader::new(server);
        let mut sink = VecWriter::default();
        assert!(matches!(
            reader.read_binary_to(1, &mut sink).await,
            Err(ProtocolError::UnexpectedFrameType {
                expected: FrameType::Binary,
                actual: FrameType::Json,
            })
        ));
    }

    #[tokio::test]
    async fn streams_a_binary_transfer_until_its_end_frame() {
        let (client, mut server) = duplex(1024);
        let writer = tokio::spawn(async move {
            let mut writer = FrameWriter::new(client);
            writer.write_binary_chunk(5, b"abc").await.unwrap();
            writer.write_binary_chunk(5, b"defg").await.unwrap();
            writer.write_binary_end(5).await.unwrap();
            writer.write_json(6, &"next").await.unwrap();
        });

        let mut reader = FrameReader::new(&mut server);
        let mut received = VecWriter::default();
        let bytes = reader.read_binary_to(5, &mut received).await.unwrap();
        assert_eq!(bytes, 7);
        assert_eq!(received.0, b"abcdefg");
        assert_eq!(
            reader.read_json::<String>().await.unwrap(),
            (6, "next".to_string())
        );
        writer.await.unwrap();
    }

    #[tokio::test]
    async fn rejects_frames_of_another_stream_mid_transfer() {
        let (client, mut server) = duplex(1024);
        let writer = tokio::spawn(async move {
            let mut writer = FrameWriter::new(client);
            writer.write_binary_chunk(1, b"a").await.unwrap();
            writer.write_binary_chunk(2, b"b").await.unwrap();
        });

        let mut reader = FrameReader::new(&mut server);
        let mut received = VecWriter::default();
        assert!(matches!(
            reader.read_binary_to(1, &mut received).await,
            Err(ProtocolError::UnexpectedStreamId {
                expected: 1,
                actual: 2
            })
        ));
        writer.await.unwrap();
    }

    #[tokio::test]
    async fn splits_streamed_data_into_chunk_frames() {
        let data = vec![7u8; CHUNK_SIZE + 10];
        let expected = data.clone();
        let (client, mut server) = duplex(CHUNK_SIZE);
        let writer = tokio::spawn(async move {
            let mut src = data.as_slice();
            let mut writer = FrameWriter::new(client);
            writer.write_binary_stream(3, &mut src).await.unwrap()
        });

        let mut reader = FrameReader::new(&mut server);
        let mut received = VecWriter::default();
        let bytes = reader.read_binary_to(3, &mut received).await.unwrap();
        assert_eq!(reader.read_header().await.unwrap(), None);
        assert_eq!(received.0, expected);
        assert_eq!(bytes as usize, expected.len());
        assert_eq!(writer.await.unwrap() as usize, expected.len());
    }
}
