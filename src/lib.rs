//! TCP framing protocol for InoCode.
//!
//! One TCP connection carries both JSON control frames and raw binary frames.
//! Every frame starts with a 16-byte big-endian header that names its type,
//! stream, and payload length, so the two payload kinds never need separate
//! connections and binary data is never encoded as text.
//!
//! * [`FrameHeader`] encodes and decodes the header and is the format authority.
//! * [`FrameReader`] and [`FrameWriter`] move frames over any `AsyncRead` /
//!   `AsyncWrite` pair, streaming binary payloads instead of buffering them whole.

mod error;
mod frame;
mod stream;

pub use error::ProtocolError;
pub use frame::{FrameHeader, FrameType, DEFAULT_MAX_PAYLOAD_LEN, HEADER_LEN};
pub use stream::{FrameReader, FrameWriter};
