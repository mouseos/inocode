//! The fixed 16-byte frame header.
//!
//! ```text
//! 0       4   5     6        8                        16
//! +-------+---+-----+--------+-------------------------+
//! | len   | T | F   | rsvd   | stream id               |
//! +-------+---+-----+--------+-------------------------+
//! ```
//!
//! `len` (u32 big endian) counts the 12 header bytes after it plus the payload,
//! so the whole frame on the wire is `4 + len` bytes.

use crate::error::ProtocolError;

/// Size of the fixed frame header in bytes.
pub const HEADER_LEN: usize = 16;

/// Largest payload a reader or writer accepts unless configured otherwise.
///
/// The bound keeps a single frame from requesting an unbounded allocation; large
/// transfers are split into many frames instead of raising it.
pub const DEFAULT_MAX_PAYLOAD_LEN: usize = 16 * 1024 * 1024;

/// Buffer size used when a payload is streamed through a connection.
pub(crate) const CHUNK_SIZE: usize = 64 * 1024;

const LENGTH_FIELD_LEN: usize = 4;
const HEADER_AFTER_LENGTH: u32 = (HEADER_LEN - LENGTH_FIELD_LEN) as u32;

/// What a frame carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameType {
    /// UTF-8 JSON control message.
    Json,
    /// Raw binary payload, not interpreted as JSON.
    Binary,
    /// End of a binary transfer on its stream.
    End,
}

impl FrameType {
    /// The type value on the wire.
    pub fn code(self) -> u8 {
        match self {
            Self::Json => 1,
            Self::Binary => 2,
            Self::End => 3,
        }
    }

    fn from_code(code: u8) -> Result<Self, ProtocolError> {
        match code {
            1 => Ok(Self::Json),
            2 => Ok(Self::Binary),
            3 => Ok(Self::End),
            other => Err(ProtocolError::UnknownFrameType(other)),
        }
    }
}

/// A validated frame header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub frame_type: FrameType,
    pub stream_id: u64,
    pub payload_len: usize,
}

impl FrameHeader {
    /// Builds a header whose payload length fits the 32-bit length field.
    pub fn new(
        frame_type: FrameType,
        stream_id: u64,
        payload_len: usize,
    ) -> Result<Self, ProtocolError> {
        let header = Self {
            frame_type,
            stream_id,
            payload_len,
        };
        header.frame_length()?;
        Ok(header)
    }

    fn frame_length(&self) -> Result<u32, ProtocolError> {
        self.payload_len
            .checked_add(HEADER_AFTER_LENGTH as usize)
            .filter(|total| *total <= u32::MAX as usize)
            .map(|total| total as u32)
            .ok_or(ProtocolError::PayloadTooLarge {
                payload_len: self.payload_len,
                max: u32::MAX as usize - HEADER_AFTER_LENGTH as usize,
            })
    }

    /// Encodes the header with flags and reserved bytes set to zero.
    pub fn encode(&self) -> Result<[u8; HEADER_LEN], ProtocolError> {
        let mut raw = [0u8; HEADER_LEN];
        raw[..LENGTH_FIELD_LEN].copy_from_slice(&self.frame_length()?.to_be_bytes());
        raw[4] = self.frame_type.code();
        raw[8..].copy_from_slice(&self.stream_id.to_be_bytes());
        Ok(raw)
    }

    /// Decodes and validates a header received from the wire.
    pub fn decode(raw: &[u8; HEADER_LEN]) -> Result<Self, ProtocolError> {
        let frame_length = u32::from_be_bytes(
            raw[..LENGTH_FIELD_LEN]
                .try_into()
                .expect("4-byte length field"),
        );
        if frame_length < HEADER_AFTER_LENGTH {
            return Err(ProtocolError::InvalidFrameLength(frame_length));
        }
        let flags = raw[5];
        if flags != 0 {
            return Err(ProtocolError::NonZeroFlags(flags));
        }
        let reserved = u16::from_be_bytes([raw[6], raw[7]]);
        if reserved != 0 {
            return Err(ProtocolError::NonZeroReserved(reserved));
        }
        Ok(Self {
            frame_type: FrameType::from_code(raw[4])?,
            stream_id: u64::from_be_bytes(raw[8..].try_into().expect("8-byte stream id")),
            payload_len: (frame_length - HEADER_AFTER_LENGTH) as usize,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(raw: [u8; HEADER_LEN]) -> Result<FrameHeader, ProtocolError> {
        FrameHeader::decode(&raw)
    }

    fn header_bytes(
        frame_length: u32,
        type_code: u8,
        flags: u8,
        reserved: u16,
    ) -> [u8; HEADER_LEN] {
        let mut raw = [0u8; HEADER_LEN];
        raw[..4].copy_from_slice(&frame_length.to_be_bytes());
        raw[4] = type_code;
        raw[5] = flags;
        raw[6..8].copy_from_slice(&reserved.to_be_bytes());
        raw[8..].copy_from_slice(&0x0102_0304_0506_0708u64.to_be_bytes());
        raw
    }

    #[test]
    fn round_trips_every_frame_type() {
        for frame_type in [FrameType::Json, FrameType::Binary, FrameType::End] {
            let header = FrameHeader::new(frame_type, 42, 1000).unwrap();
            assert_eq!(
                FrameHeader::decode(&header.encode().unwrap()).unwrap(),
                header
            );
        }
    }

    #[test]
    fn length_field_covers_remaining_header_and_payload() {
        let header = FrameHeader::new(FrameType::Json, 7, 5).unwrap();
        let raw = header.encode().unwrap();
        assert_eq!(u32::from_be_bytes(raw[..4].try_into().unwrap()), 17);
        assert_eq!(raw[4], 1);
        assert_eq!(raw[5], 0);
        assert_eq!(raw[6], 0);
        assert_eq!(raw[7], 0);
        assert_eq!(u64::from_be_bytes(raw[8..].try_into().unwrap()), 7);
    }

    #[test]
    fn accepts_largest_payload_the_length_field_can_describe() {
        let payload_len = u32::MAX as usize - HEADER_AFTER_LENGTH as usize;
        let header = FrameHeader::new(FrameType::Binary, 1, payload_len).unwrap();
        let raw = header.encode().unwrap();
        assert_eq!(u32::from_be_bytes(raw[..4].try_into().unwrap()), u32::MAX);
        assert_eq!(FrameHeader::decode(&raw).unwrap().payload_len, payload_len);
    }

    #[test]
    fn rejects_payload_that_does_not_fit_the_length_field() {
        let error = FrameHeader::new(FrameType::Binary, 1, u32::MAX as usize).unwrap_err();
        assert!(matches!(error, ProtocolError::PayloadTooLarge { .. }));
    }

    #[test]
    fn rejects_frame_length_below_the_remaining_header() {
        assert!(matches!(
            decode(header_bytes(11, 1, 0, 0)),
            Err(ProtocolError::InvalidFrameLength(11))
        ));
        assert!(matches!(
            decode(header_bytes(0, 1, 0, 0)),
            Err(ProtocolError::InvalidFrameLength(0))
        ));
    }

    #[test]
    fn accepts_empty_payload() {
        assert_eq!(
            decode(header_bytes(12, 3, 0, 0)).unwrap(),
            FrameHeader {
                frame_type: FrameType::End,
                stream_id: 0x0102_0304_0506_0708,
                payload_len: 0,
            }
        );
    }

    #[test]
    fn rejects_unknown_types_and_unset_reserved_fields() {
        assert!(matches!(
            decode(header_bytes(12, 0, 0, 0)),
            Err(ProtocolError::UnknownFrameType(0))
        ));
        assert!(matches!(
            decode(header_bytes(12, 4, 0, 0)),
            Err(ProtocolError::UnknownFrameType(4))
        ));
        assert!(matches!(
            decode(header_bytes(12, 1, 1, 0)),
            Err(ProtocolError::NonZeroFlags(1))
        ));
        assert!(matches!(
            decode(header_bytes(12, 1, 0, 1)),
            Err(ProtocolError::NonZeroReserved(1))
        ));
    }
}
