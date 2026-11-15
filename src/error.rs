use std::error::Error;
use std::fmt;

use crate::frame::FrameType;

/// Failure while encoding, decoding, or transferring a frame.
///
/// A protocol error other than a clean end of stream leaves the connection in
/// an unknown state: the offending frame is only partly consumed, so callers
/// must close the connection instead of reading further.
#[derive(Debug)]
pub enum ProtocolError {
    /// Socket I/O failed.
    Io(std::io::Error),
    /// The peer closed the connection inside a frame rather than between frames.
    UnexpectedEof,
    /// `frame_length` was smaller than the 12 header bytes it covers.
    InvalidFrameLength(u32),
    /// The payload length exceeds the limit the reader or writer was configured with.
    PayloadTooLarge { payload_len: usize, max: usize },
    /// The type byte was not 1 (JSON), 2 (Binary), or 3 (End).
    UnknownFrameType(u8),
    /// The flags byte must be zero.
    NonZeroFlags(u8),
    /// The reserved field must be zero.
    NonZeroReserved(u16),
    /// A frame of another type arrived where the caller needed this one.
    UnexpectedFrameType {
        expected: FrameType,
        actual: FrameType,
    },
    /// A frame of another stream arrived where the caller needed this one.
    UnexpectedStreamId { expected: u64, actual: u64 },
    /// The JSON payload could not be serialized or parsed.
    Json(serde_json::Error),
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "socket error: {error}"),
            Self::UnexpectedEof => write!(f, "connection closed in the middle of a frame"),
            Self::InvalidFrameLength(length) => {
                write!(
                    f,
                    "frame length {length} is shorter than the 12-byte header"
                )
            }
            Self::PayloadTooLarge { payload_len, max } => {
                write!(
                    f,
                    "payload of {payload_len} bytes exceeds the limit of {max} bytes"
                )
            }
            Self::UnknownFrameType(code) => write!(f, "unknown frame type {code}"),
            Self::NonZeroFlags(flags) => write!(f, "flags must be zero, got {flags}"),
            Self::NonZeroReserved(reserved) => write!(f, "reserved must be zero, got {reserved}"),
            Self::UnexpectedFrameType { expected, actual } => {
                write!(f, "expected a {expected:?} frame, got {actual:?}")
            }
            Self::UnexpectedStreamId { expected, actual } => {
                write!(f, "expected stream {expected}, got stream {actual}")
            }
            Self::Json(error) => write!(f, "invalid JSON payload: {error}"),
        }
    }
}

impl Error for ProtocolError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Json(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for ProtocolError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for ProtocolError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}
