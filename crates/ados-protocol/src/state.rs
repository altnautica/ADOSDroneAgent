//! Vehicle-state codec for the state socket (Contract B).
//!
//! The state is a JSON telemetry snapshot (attitude, position, GPS, battery,
//! mode, armed, link stats). The producer broadcasts it as length-prefixed
//! msgpack (v2): a 4-byte big-endian length followed by a versioned map body
//! `{"v": <version>, "s": <state>}`. The `v` integer lets a reader reject a
//! frame it does not understand rather than mis-decode it.
//!
//! This module also keeps the legacy v1 reader (newline-terminated JSON) so a
//! stray v1 frame still decodes; the auto-detect reader picks the format per
//! frame off the leading byte. The state is kept as an open
//! [`serde_json::Value`] map rather than a fixed struct so new telemetry fields
//! round-trip without a schema change.

use std::io;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::frame;

/// Maximum v2 state frame payload. State snapshots are small (a few KiB); the
/// cap is generous headroom and guards against a runaway producer.
pub const STATE_V2_MAX_FRAME: usize = 1024 * 1024;

/// The on-wire version integer carried in the v2 frame body's `v` field. Kept in
/// lock-step with the contract registry (`state.v2`) — a test asserts they are
/// equal so the wire and the registry can never silently diverge. A reader
/// rejects any frame whose `v` does not match this.
pub const STATE_WIRE_VERSION: u16 = 2;

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
    #[error("state wire version mismatch: got {got}, expected {ours}")]
    Version { got: u16, ours: u16 },
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

/// The v2 frame body: a versioned msgpack map `{"v": <version>, "s": <state>}`.
/// `to_vec_named` emits string map keys, byte-for-byte matching the Python
/// producer's `msgpack.packb({"v": version, "s": state}, use_bin_type=True)`.
/// This owned form is the decode target; the borrowed [`StateFrameV2Ref`] is the
/// encode form, which avoids cloning the (potentially large — e.g. the full
/// param blob) snapshot on the 10 Hz publish path.
#[derive(Debug, Serialize, Deserialize)]
struct StateFrameV2 {
    #[serde(rename = "v")]
    v: u16,
    #[serde(rename = "s")]
    s: Value,
}

/// Borrowed encode-side sibling of [`StateFrameV2`]. Serializes byte-identical
/// output (`&Value` serializes the same as `Value`) without taking ownership.
#[derive(Serialize)]
struct StateFrameV2Ref<'a> {
    #[serde(rename = "v")]
    v: u16,
    #[serde(rename = "s")]
    s: &'a Value,
}

/// Encode a state snapshot as a complete v2 frame: a 4-byte big-endian length
/// followed by the msgpack body `{"v": STATE_WIRE_VERSION, "s": state}`.
pub fn encode_v2(state: &Value) -> Result<Vec<u8>, StateError> {
    let body = rmp_serde::to_vec_named(&StateFrameV2Ref {
        v: STATE_WIRE_VERSION,
        s: state,
    })?;
    Ok(frame::encode_frame(&body, STATE_V2_MAX_FRAME)?)
}

/// Decode a v2 msgpack body (the frame payload, without the length prefix) into
/// its inner state snapshot. Reads the version integer first and returns
/// [`StateError::Version`] if it is not [`STATE_WIRE_VERSION`], so a newer /
/// unknown frame is rejected rather than mis-read as the current shape.
pub fn decode_v2(body: &[u8]) -> Result<Value, StateError> {
    let frame: StateFrameV2 = rmp_serde::from_slice(body)?;
    if frame.v != STATE_WIRE_VERSION {
        return Err(StateError::Version {
            got: frame.v,
            ours: STATE_WIRE_VERSION,
        });
    }
    Ok(frame.s)
}

/// One frame off the wire: a decoded snapshot, or a single malformed-but-frame-
/// aligned frame to skip without ending the connection.
enum StateFrame {
    Value(Value),
    Skip,
}

/// Validate a v2 frame's 4-byte big-endian length header and return the body
/// length. A zero or over-cap length means the stream is unframable from here on
/// (an unrecoverable error), not a skippable body.
fn v2_body_len(header: [u8; frame::HEADER_SIZE]) -> io::Result<usize> {
    let len = u32::from_be_bytes(header) as usize;
    if len == 0 || len > STATE_V2_MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "state v2 frame length out of range",
        ));
    }
    Ok(len)
}

/// Decode a complete frame body of the given wire kind, or `None` on a malformed
/// body (which the reader skips rather than treating as fatal). Sharing this with
/// [`v2_body_len`] keeps the framing + decode logic identical across the async
/// and blocking readers; only the byte pump differs between them.
fn decode_state_body(is_v2: bool, body: &[u8]) -> Option<Value> {
    if is_v2 {
        decode_v2(body).ok()
    } else {
        decode_v1_line(body).ok()
    }
}

/// Read exactly one state snapshot from `reader`, auto-detecting the wire format
/// from the leading byte:
///
/// - `0x00` ⇒ **v2** length-prefixed msgpack. The 4-byte big-endian length's
///   most-significant byte is always `0x00` for a snapshot far smaller than
///   16 MiB, and `0x00` never begins valid JSON, so the two formats are mutually
///   exclusive on the first byte and `0x00` is the positive signal for v2.
/// - anything else ⇒ **v1** newline-terminated JSON.
///
/// Returns `Ok(Some(value))` on a decoded snapshot, `Ok(None)` on a clean EOF at
/// a frame boundary (the caller reconnects), and `Err(e)` on an unrecoverable
/// framing or IO error. A single malformed-but-frame-aligned frame (bad msgpack
/// body or bad JSON line) is skipped internally and the next frame is read, so
/// one bad snapshot never ends a hot connection.
///
/// This is the single reader every `state.sock` consumer uses. Keeping the wire
/// detection in one place is what stops a producer and a consumer in the same
/// build from silently disagreeing on the format (the failure that stalled the
/// world-model capture: a v2 producer read v1-only, yielding no data and no
/// error).
pub async fn read_state_value<R>(reader: &mut R) -> io::Result<Option<Value>>
where
    R: AsyncRead + Unpin,
{
    loop {
        match read_state_frame(reader).await? {
            Some(StateFrame::Value(v)) => return Ok(Some(v)),
            Some(StateFrame::Skip) => continue,
            None => return Ok(None),
        }
    }
}

/// Read one frame asynchronously. `Ok(None)` = a clean EOF at a frame boundary.
async fn read_state_frame<R>(reader: &mut R) -> io::Result<Option<StateFrame>>
where
    R: AsyncRead + Unpin,
{
    let mut first = [0u8; 1];
    match reader.read_exact(&mut first).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }

    if first[0] == 0x00 {
        let mut rest = [0u8; frame::HEADER_SIZE - 1];
        reader.read_exact(&mut rest).await?;
        let len = v2_body_len([first[0], rest[0], rest[1], rest[2]])?;
        let mut body = vec![0u8; len];
        reader.read_exact(&mut body).await?;
        Ok(Some(match decode_state_body(true, &body) {
            Some(v) => StateFrame::Value(v),
            None => StateFrame::Skip,
        }))
    } else {
        let mut line = vec![first[0]];
        let mut byte = [0u8; 1];
        loop {
            match reader.read_exact(&mut byte).await {
                Ok(_) => {}
                // EOF mid-line: the connection ended without terminating the
                // frame; treat it as a clean boundary EOF and reconnect.
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
                Err(e) => return Err(e),
            }
            if byte[0] == b'\n' {
                break;
            }
            if line.len() >= STATE_V2_MAX_FRAME {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "state v1 line exceeded the frame cap without a newline",
                ));
            }
            line.push(byte[0]);
        }
        Ok(Some(match decode_state_body(false, &line) {
            Some(v) => StateFrame::Value(v),
            None => StateFrame::Skip,
        }))
    }
}

/// Blocking sibling of [`read_state_value`] for the sync heartbeat-enrichment
/// path, which reads one snapshot under a socket read timeout rather than on a
/// tokio task. Shares the exact framing + decode core ([`v2_body_len`] +
/// [`decode_state_body`]); only the byte pump differs.
pub fn read_state_value_blocking<R>(reader: &mut R) -> io::Result<Option<Value>>
where
    R: io::Read,
{
    loop {
        match read_state_frame_blocking(reader)? {
            Some(StateFrame::Value(v)) => return Ok(Some(v)),
            Some(StateFrame::Skip) => continue,
            None => return Ok(None),
        }
    }
}

fn read_state_frame_blocking<R>(reader: &mut R) -> io::Result<Option<StateFrame>>
where
    R: io::Read,
{
    let mut first = [0u8; 1];
    match reader.read_exact(&mut first) {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }

    if first[0] == 0x00 {
        let mut rest = [0u8; frame::HEADER_SIZE - 1];
        reader.read_exact(&mut rest)?;
        let len = v2_body_len([first[0], rest[0], rest[1], rest[2]])?;
        let mut body = vec![0u8; len];
        reader.read_exact(&mut body)?;
        Ok(Some(match decode_state_body(true, &body) {
            Some(v) => StateFrame::Value(v),
            None => StateFrame::Skip,
        }))
    } else {
        let mut line = vec![first[0]];
        let mut byte = [0u8; 1];
        loop {
            match reader.read_exact(&mut byte) {
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
                Err(e) => return Err(e),
            }
            if byte[0] == b'\n' {
                break;
            }
            if line.len() >= STATE_V2_MAX_FRAME {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "state v1 line exceeded the frame cap without a newline",
                ));
            }
            line.push(byte[0]);
        }
        Ok(Some(match decode_state_body(false, &line) {
            Some(v) => StateFrame::Value(v),
            None => StateFrame::Skip,
        }))
    }
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
        // The body is the versioned wrapper map {"v": 2, "s": <state>}; the raw
        // deserialize proves the shape, and decode_v2 returns the inner state.
        let raw: StateFrameV2 = rmp_serde::from_slice(body).unwrap();
        assert_eq!(raw.v, STATE_WIRE_VERSION);
        assert_eq!(raw.s, state);
        let back = decode_v2(body).unwrap();
        assert_eq!(state, back);
    }

    #[test]
    fn version_matches_registry() {
        // The wire version and the contract registry must never silently
        // diverge; if the registry bumps state.v2, this const must move with it.
        assert_eq!(
            STATE_WIRE_VERSION,
            crate::contracts::contract_version("state.v2").unwrap()
        );
    }

    #[test]
    fn decode_v2_rejects_a_future_version() {
        // A frame tagged with a version this build does not speak is rejected,
        // not mis-decoded as the current shape.
        let body = rmp_serde::to_vec_named(&StateFrameV2Ref {
            v: STATE_WIRE_VERSION + 1,
            s: &sample(),
        })
        .unwrap();
        match decode_v2(&body) {
            Err(StateError::Version { got, ours }) => {
                assert_eq!(got, STATE_WIRE_VERSION + 1);
                assert_eq!(ours, STATE_WIRE_VERSION);
            }
            other => panic!("expected a Version error, got {other:?}"),
        }
    }

    #[test]
    fn v1_and_v2_preserve_the_same_fields() {
        let state = sample();
        let via_v1 = decode_v1_line(&encode_v1(&state).unwrap()).unwrap();
        let frame_bytes = encode_v2(&state).unwrap();
        let via_v2 = decode_v2(&frame_bytes[frame::HEADER_SIZE..]).unwrap();
        assert_eq!(via_v1, via_v2);
    }

    #[test]
    fn leading_byte_discriminates_the_two_wire_forms() {
        // The auto-detect reader relies on these invariants: a small v2 frame
        // always leads with 0x00 (its length's high byte), a v1 frame always
        // leads with `{` (an object). 0x00 never begins valid JSON.
        assert_eq!(encode_v2(&sample()).unwrap()[0], 0x00);
        assert_eq!(encode_v1(&sample()).unwrap()[0], b'{');
    }

    #[tokio::test]
    async fn read_state_value_decodes_v1_and_v2_identically() {
        let state = sample();
        let mut v1 = std::io::Cursor::new(encode_v1(&state).unwrap());
        let mut v2 = std::io::Cursor::new(encode_v2(&state).unwrap());
        let from_v1 = read_state_value(&mut v1).await.unwrap().unwrap();
        let from_v2 = read_state_value(&mut v2).await.unwrap().unwrap();
        assert_eq!(from_v1, state);
        assert_eq!(from_v2, state);
    }

    #[tokio::test]
    async fn read_state_value_reads_a_mixed_v1_v2_stream_in_order() {
        // Auto-detect is per-frame, so a producer that switches encoding
        // mid-stream is still read correctly.
        let a = json!({"mode": "STABILIZE"});
        let b = json!({"mode": "GUIDED"});
        let mut wire = encode_v1(&a).unwrap();
        wire.extend(encode_v2(&b).unwrap());
        let mut r = std::io::Cursor::new(wire);
        assert_eq!(read_state_value(&mut r).await.unwrap().unwrap(), a);
        assert_eq!(read_state_value(&mut r).await.unwrap().unwrap(), b);
        assert!(read_state_value(&mut r).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn read_state_value_skips_a_malformed_frame_then_reads_the_next() {
        let good = sample();
        let mut wire = b"{ not json }\n".to_vec();
        wire.extend(encode_v2(&good).unwrap());
        let mut r = std::io::Cursor::new(wire);
        // The one call skips the bad v1 line internally and returns the good v2.
        assert_eq!(read_state_value(&mut r).await.unwrap().unwrap(), good);
    }

    #[tokio::test]
    async fn read_state_value_returns_none_on_clean_eof() {
        let mut r = std::io::Cursor::new(Vec::new());
        assert!(read_state_value(&mut r).await.unwrap().is_none());
    }

    #[test]
    fn read_state_value_blocking_decodes_both_wire_forms() {
        let state = sample();
        let mut v1 = std::io::Cursor::new(encode_v1(&state).unwrap());
        let mut v2 = std::io::Cursor::new(encode_v2(&state).unwrap());
        assert_eq!(read_state_value_blocking(&mut v1).unwrap().unwrap(), state);
        assert_eq!(read_state_value_blocking(&mut v2).unwrap().unwrap(), state);
    }
}
