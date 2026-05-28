//! Vehicle-state codec for the state socket (Contract B).
//!
//! The state is a JSON telemetry snapshot (attitude, position, GPS, battery,
//! mode, armed, link stats). The agent currently broadcasts it as
//! newline-terminated JSON (v1). The hybrid migration upgrades the socket to
//! length-prefixed msgpack (v2) for lower serialization overhead on Pi-class
//! hardware.
//!
//! This module carries both: the v1 reader for the migration window and the v2
//! codec for the Rust state hub. The state is kept as an open
//! [`serde_json::Value`] map rather than a fixed struct so new telemetry fields
//! round-trip without a schema change.

use serde_json::Value;
use thiserror::Error;

use crate::frame;

/// Maximum v2 state frame payload. State snapshots are small (a few KiB); the
/// cap is generous headroom and guards against a runaway producer.
pub const STATE_V2_MAX_FRAME: usize = 1024 * 1024;

#[derive(Debug, Error)]
pub enum StateError {
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("msgpack encode error: {0}")]
    Encode(#[from] rmp_serde::encode::Error),
    #[error("msgpack decode error: {0}")]
    Decode(#[from] rmp_serde::decode::Error),
    #[error("framing error: {0}")]
    Frame(#[from] frame::FrameError),
}

/// Encode a state snapshot in the v1 wire format: compact JSON terminated by a
/// newline. Equivalent to Python `json.dumps(state).encode() + b"\n"`.
pub fn encode_v1(state: &Value) -> Result<Vec<u8>, StateError> {
    let mut buf = serde_json::to_vec(state)?;
    buf.push(b'\n');
    Ok(buf)
}

/// Decode one v1 line (with or without the trailing newline) into a state
/// snapshot.
pub fn decode_v1_line(line: &[u8]) -> Result<Value, StateError> {
    let trimmed = line.strip_suffix(b"\n").unwrap_or(line);
    Ok(serde_json::from_slice(trimmed)?)
}

/// Encode a state snapshot as a complete v2 frame: 4-byte big-endian length +
/// msgpack body.
pub fn encode_v2(state: &Value) -> Result<Vec<u8>, StateError> {
    let body = rmp_serde::to_vec(state)?;
    Ok(frame::encode_frame(&body, STATE_V2_MAX_FRAME)?)
}

/// Decode a v2 msgpack body (the frame payload, without the length prefix).
pub fn decode_v2(body: &[u8]) -> Result<Value, StateError> {
    Ok(rmp_serde::from_slice(body)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample() -> Value {
        json!({
            "armed": false,
            "mode": "STABILIZE",
            "battery": {"voltage": 16.4, "remaining": 87},
            "gps": {"fix": 3, "sats": 14, "lat": 12.9716, "lon": 77.5946},
            "attitude": {"roll": 0.01, "pitch": -0.02, "yaw": 1.57},
            "link": {"rssi_dbm": -48, "valid_rx_packets_per_s": 630}
        })
    }

    #[test]
    fn v1_round_trip_with_newline() {
        let state = sample();
        let wire = encode_v1(&state).unwrap();
        assert_eq!(*wire.last().unwrap(), b'\n');
        let back = decode_v1_line(&wire).unwrap();
        assert_eq!(state, back);
    }

    #[test]
    fn v1_decode_tolerates_missing_newline() {
        let state = sample();
        let json = serde_json::to_vec(&state).unwrap();
        assert_eq!(decode_v1_line(&json).unwrap(), state);
    }

    #[test]
    fn v1_decode_rejects_malformed_json() {
        assert!(decode_v1_line(b"{not json}\n").is_err());
    }

    #[test]
    fn v2_round_trip() {
        let state = sample();
        let frame_bytes = encode_v2(&state).unwrap();
        // Strip the 4-byte length prefix before decoding the body.
        let body = &frame_bytes[frame::HEADER_SIZE..];
        let back = decode_v2(body).unwrap();
        assert_eq!(state, back);
    }

    #[test]
    fn v1_and_v2_preserve_the_same_fields() {
        let state = sample();
        let via_v1 = decode_v1_line(&encode_v1(&state).unwrap()).unwrap();
        let frame_bytes = encode_v2(&state).unwrap();
        let via_v2 = decode_v2(&frame_bytes[frame::HEADER_SIZE..]).unwrap();
        assert_eq!(via_v1, via_v2);
    }
}
