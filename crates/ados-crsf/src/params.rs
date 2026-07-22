//! Parameter (configuration) frame codecs: settings entry (0x2D), read
//! request (0x2E), and write command (0x2F).
//!
//! These are extended frames: the payload leads with a destination and an
//! origin device address, then the type-specific fields. The RC module's
//! parameter system (packet rate, TX power, telemetry ratio, binding phrase)
//! is driven through these three types. The lane is a transparent carrier —
//! it frames and unframes the parameter data without interpreting field
//! values; the consumer (a configuration UI) owns the semantics.
//!
//! Settings entries larger than one frame arrive chunked: `chunks_remaining`
//! counts the chunks still to come after the current one, and the reader
//! re-requests with an incrementing `chunk_index` until it reaches zero.

use crate::frame::{
    build_frame, FrameError, TYPE_PARAMETER_READ, TYPE_PARAMETER_SETTINGS_ENTRY,
    TYPE_PARAMETER_WRITE,
};

/// A parameter read request (type 0x2E): ask `dest` for chunk `chunk_index`
/// of parameter `field_index`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParameterRead {
    pub dest: u8,
    pub origin: u8,
    pub field_index: u8,
    pub chunk_index: u8,
}

impl ParameterRead {
    pub const PAYLOAD_SIZE: usize = 4;

    pub fn decode(payload: &[u8]) -> Result<Self, FrameError> {
        if payload.len() != Self::PAYLOAD_SIZE {
            return Err(FrameError::PayloadSizeMismatch {
                frame_type: TYPE_PARAMETER_READ,
                expected: Self::PAYLOAD_SIZE,
                got: payload.len(),
            });
        }
        Ok(Self {
            dest: payload[0],
            origin: payload[1],
            field_index: payload[2],
            chunk_index: payload[3],
        })
    }

    pub fn encode_payload(&self) -> [u8; Self::PAYLOAD_SIZE] {
        [self.dest, self.origin, self.field_index, self.chunk_index]
    }

    pub fn to_frame(&self, sync: u8) -> Result<Vec<u8>, FrameError> {
        build_frame(sync, TYPE_PARAMETER_READ, &self.encode_payload())
    }
}

/// A parameter write command (type 0x2F): set parameter `field_index` on
/// `dest` to the raw `data` bytes (the value encoding is parameter-specific).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParameterWrite {
    pub dest: u8,
    pub origin: u8,
    pub field_index: u8,
    pub data: Vec<u8>,
}

impl ParameterWrite {
    /// Header bytes before the value data: dest + origin + field index.
    pub const HEADER_SIZE: usize = 3;

    pub fn decode(payload: &[u8]) -> Result<Self, FrameError> {
        if payload.len() < Self::HEADER_SIZE {
            return Err(FrameError::PayloadSizeMismatch {
                frame_type: TYPE_PARAMETER_WRITE,
                expected: Self::HEADER_SIZE,
                got: payload.len(),
            });
        }
        Ok(Self {
            dest: payload[0],
            origin: payload[1],
            field_index: payload[2],
            data: payload[Self::HEADER_SIZE..].to_vec(),
        })
    }

    pub fn encode_payload(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::HEADER_SIZE + self.data.len());
        out.push(self.dest);
        out.push(self.origin);
        out.push(self.field_index);
        out.extend_from_slice(&self.data);
        out
    }

    pub fn to_frame(&self, sync: u8) -> Result<Vec<u8>, FrameError> {
        build_frame(sync, TYPE_PARAMETER_WRITE, &self.encode_payload())
    }
}

/// A parameter settings entry (type 0x2D): one chunk of parameter
/// `field_index`'s definition/value data, with `chunks_remaining` still to
/// come after it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParameterSettingsEntry {
    pub dest: u8,
    pub origin: u8,
    pub field_index: u8,
    pub chunks_remaining: u8,
    pub chunk: Vec<u8>,
}

impl ParameterSettingsEntry {
    /// Header bytes before the chunk data: dest + origin + field index +
    /// chunks remaining.
    pub const HEADER_SIZE: usize = 4;

    pub fn decode(payload: &[u8]) -> Result<Self, FrameError> {
        if payload.len() < Self::HEADER_SIZE {
            return Err(FrameError::PayloadSizeMismatch {
                frame_type: TYPE_PARAMETER_SETTINGS_ENTRY,
                expected: Self::HEADER_SIZE,
                got: payload.len(),
            });
        }
        Ok(Self {
            dest: payload[0],
            origin: payload[1],
            field_index: payload[2],
            chunks_remaining: payload[3],
            chunk: payload[Self::HEADER_SIZE..].to_vec(),
        })
    }

    pub fn encode_payload(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::HEADER_SIZE + self.chunk.len());
        out.push(self.dest);
        out.push(self.origin);
        out.push(self.field_index);
        out.push(self.chunks_remaining);
        out.extend_from_slice(&self.chunk);
        out
    }

    pub fn to_frame(&self, sync: u8) -> Result<Vec<u8>, FrameError> {
        build_frame(sync, TYPE_PARAMETER_SETTINGS_ENTRY, &self.encode_payload())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{parse_frame, ADDR_HANDSET, ADDR_TRANSMITTER_MODULE};

    #[test]
    fn parameter_read_roundtrip() {
        let read = ParameterRead {
            dest: ADDR_TRANSMITTER_MODULE,
            origin: ADDR_HANDSET,
            field_index: 3,
            chunk_index: 0,
        };
        let wire = read.to_frame(ADDR_TRANSMITTER_MODULE).unwrap();
        let (frame, _) = parse_frame(&wire).unwrap();
        assert_eq!(frame.frame_type, TYPE_PARAMETER_READ);
        assert_eq!(ParameterRead::decode(&frame.payload).unwrap(), read);
    }

    #[test]
    fn parameter_write_roundtrip_with_and_without_data() {
        for data in [vec![], vec![2], vec![0xDE, 0xAD, 0xBE, 0xEF]] {
            let write = ParameterWrite {
                dest: ADDR_TRANSMITTER_MODULE,
                origin: ADDR_HANDSET,
                field_index: 7,
                data,
            };
            let wire = write.to_frame(ADDR_TRANSMITTER_MODULE).unwrap();
            let (frame, _) = parse_frame(&wire).unwrap();
            assert_eq!(frame.frame_type, TYPE_PARAMETER_WRITE);
            assert_eq!(ParameterWrite::decode(&frame.payload).unwrap(), write);
        }
    }

    #[test]
    fn parameter_settings_entry_roundtrip() {
        let entry = ParameterSettingsEntry {
            dest: ADDR_HANDSET,
            origin: ADDR_TRANSMITTER_MODULE,
            field_index: 5,
            chunks_remaining: 2,
            chunk: b"Pkt. Rate".to_vec(),
        };
        let wire = entry.to_frame(ADDR_HANDSET).unwrap();
        let (frame, _) = parse_frame(&wire).unwrap();
        assert_eq!(frame.frame_type, TYPE_PARAMETER_SETTINGS_ENTRY);
        assert_eq!(
            ParameterSettingsEntry::decode(&frame.payload).unwrap(),
            entry
        );
    }

    #[test]
    fn truncated_parameter_payloads_are_rejected() {
        assert!(ParameterRead::decode(&[1, 2, 3]).is_err());
        assert!(ParameterWrite::decode(&[1, 2]).is_err());
        assert!(ParameterSettingsEntry::decode(&[1, 2, 3]).is_err());
    }
}
