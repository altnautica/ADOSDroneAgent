//! 4-byte big-endian length-prefixed framing.
//!
//! A frame is a 4-byte big-endian unsigned length followed by exactly that
//! many payload bytes. This is the framing used by three contracts:
//!
//! - MAVLink socket: payload is one raw MAVLink frame, max 65536, zero-length
//!   permitted (the Python server only checks the upper bound).
//! - plugin RPC socket: payload is a msgpack envelope, max 4 MiB, zero-length
//!   rejected (the Python reader treats `length == 0` as a protocol error).
//! - state socket v2: payload is a msgpack state snapshot.
//!
//! The reject-zero behaviour differs per contract, so [`decode_len`] takes it
//! as an argument rather than baking in one policy.

use thiserror::Error;

/// Size of the length prefix in bytes.
pub const HEADER_SIZE: usize = 4;

/// Maximum MAVLink frame payload (Contract A: `MAX_FRAME_SIZE`).
pub const MAVLINK_MAX_FRAME: usize = 65536;

/// Maximum plugin RPC envelope payload (Contract C: `MAX_FRAME_BYTES`).
pub const PLUGIN_MAX_FRAME: usize = 4 * 1024 * 1024;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum FrameError {
    #[error("frame length {len} exceeds max {max}")]
    TooLarge { len: usize, max: usize },
    #[error("frame length is zero")]
    ZeroLength,
    #[error("payload of {0} bytes does not fit in a u32 length prefix")]
    PayloadTooBig(usize),
}

/// Encode a payload as a length-prefixed frame: 4-byte big-endian length
/// followed by the payload. Returns an error if the payload exceeds `max`
/// or cannot be represented in a u32 length.
pub fn encode_frame(payload: &[u8], max: usize) -> Result<Vec<u8>, FrameError> {
    let n = payload.len();
    if n > max {
        return Err(FrameError::TooLarge { len: n, max });
    }
    let len = u32::try_from(n).map_err(|_| FrameError::PayloadTooBig(n))?;
    let mut out = Vec::with_capacity(HEADER_SIZE + n);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}

/// Decode the payload length from a 4-byte big-endian header.
///
/// `reject_zero` selects the per-contract policy: the plugin contract rejects
/// zero-length frames, the MAVLink contract does not.
pub fn decode_len(
    header: [u8; HEADER_SIZE],
    max: usize,
    reject_zero: bool,
) -> Result<usize, FrameError> {
    let len = u32::from_be_bytes(header) as usize;
    if reject_zero && len == 0 {
        return Err(FrameError::ZeroLength);
    }
    if len > max {
        return Err(FrameError::TooLarge { len, max });
    }
    Ok(len)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_header_matches_python_struct_pack() {
        // Python: struct.pack("!I", len(data)) is big-endian unsigned 32-bit.
        let payload = b"hello world";
        let frame = encode_frame(payload, MAVLINK_MAX_FRAME).unwrap();
        assert_eq!(&frame[..HEADER_SIZE], &[0, 0, 0, 11]);
        assert_eq!(&frame[HEADER_SIZE..], payload);

        let header: [u8; HEADER_SIZE] = frame[..HEADER_SIZE].try_into().unwrap();
        let len = decode_len(header, MAVLINK_MAX_FRAME, false).unwrap();
        assert_eq!(len, payload.len());
    }

    #[test]
    fn empty_payload_encodes_zero_length() {
        let frame = encode_frame(b"", MAVLINK_MAX_FRAME).unwrap();
        assert_eq!(frame, vec![0, 0, 0, 0]);
    }

    #[test]
    fn mavlink_allows_zero_length_plugin_rejects_it() {
        let header = [0u8, 0, 0, 0];
        // MAVLink contract: zero length is fine.
        assert_eq!(decode_len(header, MAVLINK_MAX_FRAME, false).unwrap(), 0);
        // Plugin contract: zero length is a protocol error.
        assert_eq!(
            decode_len(header, PLUGIN_MAX_FRAME, true),
            Err(FrameError::ZeroLength)
        );
    }

    #[test]
    fn oversized_payload_is_rejected_on_encode() {
        let payload = vec![0u8; MAVLINK_MAX_FRAME + 1];
        assert_eq!(
            encode_frame(&payload, MAVLINK_MAX_FRAME),
            Err(FrameError::TooLarge {
                len: MAVLINK_MAX_FRAME + 1,
                max: MAVLINK_MAX_FRAME
            })
        );
    }

    #[test]
    fn oversized_length_is_rejected_on_decode() {
        let header = (PLUGIN_MAX_FRAME as u32 + 1).to_be_bytes();
        assert!(matches!(
            decode_len(header, PLUGIN_MAX_FRAME, true),
            Err(FrameError::TooLarge { .. })
        ));
    }
}
