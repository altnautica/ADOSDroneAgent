//! MAVLink v2 codec for the ardupilotmega dialect.
//!
//! Behind the `mavlink` feature because the dialect is a large, slow-to-compile
//! generated enum and most consumers do not need it. This wraps `rust-mavlink`
//! with parse/serialize helpers over byte slices, which is what flows on the
//! MAVLink socket. The router (which owns the serial link and fans frames out
//! to the socket) builds on this; per the migration plan the decode is
//! validated against the ground station's decoder before cutover, since
//! `rust-mavlink` is not assumed to be bit-identical to the audited decoder.

use std::io::Cursor;

use thiserror::Error;

pub use rust_mavlink::dialects::ardupilotmega::MavMessage;
pub use rust_mavlink::{MavHeader, MavlinkVersion, Message};

// Re-export the dialect module so services built on this crate (the router)
// can construct and match the concrete message payloads and enums without
// declaring their own copy of the dialect dependency.
pub use rust_mavlink::dialects::ardupilotmega;

mod attitude;
mod command_long;
mod frame_enum;
mod setpoint;
mod tunnel;
pub use attitude::*;
pub use command_long::*;
use frame_enum::{clamp_to_i32, mav_frame_from_u8};
pub use setpoint::*;
pub use tunnel::*;

#[derive(Debug, Error)]
pub enum MavlinkError {
    #[error("failed to read MAVLink v2 frame: {0}")]
    Read(String),
    #[error("failed to write MAVLink v2 frame: {0}")]
    Write(String),
}

/// Parse one MAVLink v2 frame from a byte slice into its header and message.
pub fn parse_v2(bytes: &[u8]) -> Result<(MavHeader, MavMessage), MavlinkError> {
    let mut reader = rust_mavlink::peek_reader::PeekReader::new(Cursor::new(bytes));
    rust_mavlink::read_v2_msg::<MavMessage, _>(&mut reader)
        .map_err(|e| MavlinkError::Read(e.to_string()))
}

/// Parse one MAVLink v1 frame (STX `0xFE`) from a byte slice into its header and
/// message. A v1 frame has a 6-byte header (STX, payload length, sequence,
/// system id, component id, message id), the payload, and a 2-byte checksum.
/// There are no incompat/compat flags and no signature block.
pub fn parse_v1(bytes: &[u8]) -> Result<(MavHeader, MavMessage), MavlinkError> {
    let mut reader = rust_mavlink::peek_reader::PeekReader::new(Cursor::new(bytes));
    rust_mavlink::read_v1_msg::<MavMessage, _>(&mut reader)
        .map_err(|e| MavlinkError::Read(e.to_string()))
}

/// Parse one MAVLink frame of either protocol version, selected by the leading
/// start-of-frame magic byte (`0xFD` for v2, `0xFE` for v1). The original frame
/// bytes are unchanged and are not re-encoded.
pub fn parse_any(bytes: &[u8]) -> Result<(MavHeader, MavMessage), MavlinkError> {
    match bytes.first() {
        Some(&0xFD) => parse_v2(bytes),
        Some(&0xFE) => parse_v1(bytes),
        Some(other) => Err(MavlinkError::Read(format!(
            "unknown MAVLink start-of-frame byte 0x{other:02X}"
        ))),
        None => Err(MavlinkError::Read("empty MAVLink frame".to_string())),
    }
}

/// The message id the ardupilotmega dialect assigns to `name` (e.g.
/// `"GLOBAL_POSITION_INT"` → 33), or `None` for a name the dialect does not
/// define.
#[must_use]
pub fn message_id_from_name(name: &str) -> Option<u32> {
    <MavMessage as rust_mavlink::Message>::message_id_from_name(name)
}

/// The one incompat flag MAVLink 2 defines (`MAVLINK_IFLAG_SIGNED`).
const IFLAG_SIGNED: u8 = 0x01;

/// Decode `frame` as exactly one whole MAVLink frame, with no resynchronisation.
///
/// [`parse_any`] is a stream reader: on a bad checksum it steps one byte and
/// scans on for the next start byte, so it can return a message that begins
/// INSIDE the bytes it was handed. That is the wrong question for a gate
/// deciding what a buffer carries. This answers "is this buffer one frame a
/// receiver consumes whole", and refuses everything a receiving parser would
/// drop partway and rescan from:
///
/// * a v2 frame carrying an incompat flag other than the signing bit (the
///   reference parser drops it at byte 2 and rescans the rest);
/// * a declared length that does not match the buffer;
/// * a message id the dialect does not define;
/// * a payload longer than the message's full encoded length (a parser that
///   checks message length drops it at the header and rescans);
/// * a checksum that does not match with the message's CRC_EXTRA;
/// * a payload that does not parse as the message.
pub fn decode_exact_frame(frame: &[u8]) -> Result<MavMessage, MavlinkError> {
    let bad = |why: &str| MavlinkError::Read(why.to_string());
    let (version, header_len, msg_id, signature_len) = match frame.first() {
        Some(&0xFD) => {
            if frame.len() < 10 {
                return Err(bad("truncated MAVLink v2 header"));
            }
            let incompat = frame[2];
            if incompat & !IFLAG_SIGNED != 0 {
                return Err(bad("unsupported MAVLink v2 incompat flag"));
            }
            let signature = if incompat & IFLAG_SIGNED != 0 { 13 } else { 0 };
            let id = u32::from_le_bytes([frame[7], frame[8], frame[9], 0]);
            (MavlinkVersion::V2, 10usize, id, signature)
        }
        Some(&0xFE) => {
            if frame.len() < 6 {
                return Err(bad("truncated MAVLink v1 header"));
            }
            (MavlinkVersion::V1, 6usize, u32::from(frame[5]), 0usize)
        }
        Some(_) => return Err(bad("unknown MAVLink start-of-frame byte")),
        None => return Err(bad("empty MAVLink frame")),
    };
    let payload_len = frame[1] as usize;
    let crc_at = header_len + payload_len;
    if frame.len() != crc_at + 2 + signature_len {
        return Err(bad("declared MAVLink length does not match the frame"));
    }
    let Some(template) = <MavMessage as rust_mavlink::Message>::default_message_from_id(msg_id)
    else {
        return Err(bad("message id is not in the dialect"));
    };
    // A v1 serialize never truncates, so it reports the full encoded length.
    // The dialect is generated without extension fields, so a MAVLink 2
    // payload may legitimately run past that base length up to the
    // message's extended length; parse reads the base fields and ignores the
    // rest, and CRC_EXTRA covers base fields only.
    let mut scratch = [0u8; 255];
    let base_len = rust_mavlink::Message::ser(&template, MavlinkVersion::V1, &mut scratch);
    let max_len = match version {
        MavlinkVersion::V2 => v2_extended_len(msg_id).map_or(base_len, |ext| ext.max(base_len)),
        MavlinkVersion::V1 => base_len,
    };
    if payload_len > max_len {
        return Err(bad("payload is longer than the message"));
    }
    let mut crc = X25_INIT;
    for &b in &frame[1..crc_at] {
        crc = x25_accumulate(b, crc);
    }
    crc = x25_accumulate(
        <MavMessage as rust_mavlink::Message>::extra_crc(msg_id),
        crc,
    );
    if frame[crc_at..crc_at + 2] != crc.to_le_bytes() {
        return Err(bad("MAVLink checksum mismatch"));
    }
    <MavMessage as rust_mavlink::Message>::parse(version, msg_id, &frame[header_len..crc_at])
        .map_err(|e| MavlinkError::Read(e.to_string()))
}

/// Full MAVLink 2 payload length (base plus extension fields) for messages the
/// agent receives with extensions. The generated dialect omits extension
/// fields, so their length is not derivable from it.
fn v2_extended_len(msg_id: u32) -> Option<usize> {
    match msg_id {
        // OPTICAL_FLOW: 26 base bytes + flow_rate_x/flow_rate_y (2 x f32).
        100 => Some(34),
        _ => None,
    }
}

/// Whether a complete MAVLink v1/v2 frame carries a valid checksum: the X.25
/// over every byte after the start-of-frame through the payload, folded with
/// the message's CRC_EXTRA. `false` for a truncated frame and for a message id
/// outside the dialect, whose CRC_EXTRA is unknown: its checksum cannot be
/// checked, and a byte-stream framer resyncing after line noise must not take
/// an unverifiable candidate as a frame.
pub fn frame_checksum_ok(frame: &[u8]) -> bool {
    let (header_len, msg_id) = match frame.first() {
        Some(&0xFD) if frame.len() >= 10 => (
            10usize,
            u32::from_le_bytes([frame[7], frame[8], frame[9], 0]),
        ),
        Some(&0xFE) if frame.len() >= 6 => (6usize, u32::from(frame[5])),
        _ => return false,
    };
    let crc_at = header_len + frame[1] as usize;
    if frame.len() < crc_at + 2 {
        return false;
    }
    if <MavMessage as rust_mavlink::Message>::default_message_from_id(msg_id).is_none() {
        return false;
    }
    let mut crc = X25_INIT;
    for &b in &frame[1..crc_at] {
        crc = x25_accumulate(b, crc);
    }
    crc = x25_accumulate(
        <MavMessage as rust_mavlink::Message>::extra_crc(msg_id),
        crc,
    );
    frame[crc_at..crc_at + 2] == crc.to_le_bytes()
}

/// Serialize a message into a complete MAVLink v2 frame.
pub fn serialize_v2(header: MavHeader, msg: &MavMessage) -> Result<Vec<u8>, MavlinkError> {
    let mut buf = Vec::new();
    rust_mavlink::write_v2_msg(&mut buf, header, msg)
        .map_err(|e| MavlinkError::Write(e.to_string()))?;
    Ok(buf)
}

/// Serialize a message into a complete MAVLink v1 frame (STX `0xFE`).
pub fn serialize_v1(header: MavHeader, msg: &MavMessage) -> Result<Vec<u8>, MavlinkError> {
    let mut buf = Vec::new();
    rust_mavlink::write_v1_msg(&mut buf, header, msg)
        .map_err(|e| MavlinkError::Write(e.to_string()))?;
    Ok(buf)
}

// ---------------------------------------------------------------------------
// Raw COMMAND_LONG builder for an arbitrary command id.
// ---------------------------------------------------------------------------

/// Drop trailing zero bytes off a MAVLink2 payload, keeping at least one byte
/// (an all-zero payload truncates to a single zero byte, never empty).
fn truncate_trailing_zeros(payload: &mut Vec<u8>) {
    while payload.len() > 1 && *payload.last().unwrap() == 0 {
        payload.pop();
    }
}

/// X.25 / CRC-16-MCRF4XX initial value (the MAVLink checksum seed).
const X25_INIT: u16 = 0xFFFF;

/// Accumulate one byte into the running X.25 checksum, the same per-byte fold the
/// MAVLink checksum uses (and rust-mavlink's own CRC), so a frame this builder
/// emits is byte-identical to one the typed serializer would for a named id.
fn x25_accumulate(byte: u8, crc: u16) -> u16 {
    let mut tmp = byte ^ (crc & 0xFF) as u8;
    tmp ^= tmp << 4;
    let tmp = tmp as u16;
    (crc >> 8) ^ (tmp << 8) ^ (tmp << 3) ^ (tmp >> 4)
}

// ---------------------------------------------------------------------------
// Guided position/velocity setpoint builder (SET_POSITION_TARGET 84 / 86).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
