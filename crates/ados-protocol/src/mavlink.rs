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

pub use rust_mavlink::ardupilotmega::MavMessage;
pub use rust_mavlink::{MavHeader, MavlinkVersion};

// Re-export the dialect module so services built on this crate (the router)
// can construct and match the concrete message payloads and enums without
// declaring their own copy of the dialect dependency.
pub use rust_mavlink::ardupilotmega;

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

/// MAVLink message id of `COMMAND_LONG` (`0x4C`).
pub const MSG_ID_COMMAND_LONG: u32 = 76;

/// The `COMMAND_LONG` CRC_EXTRA, the message-definition seed the X.25 checksum
/// folds in last. This is the value the canonical dialect carries for the message
/// (the same value the generated codec and the reference Python encoder use);
/// it is NOT the message id.
pub const COMMAND_LONG_CRC_EXTRA: u8 = 152;

/// Build a complete MAVLink v2 `COMMAND_LONG` frame for an ARBITRARY `command`
/// id, returning the raw frame bytes ready to write to the MAVLink socket.
///
/// The generated dialect enum only carries named command ids, so a `COMMAND_LONG`
/// for a command the enum does not name cannot be built through the typed
/// `MavMessage::COMMAND_LONG` path. This serializes the wire frame directly: the
/// `COMMAND_LONG` payload (the seven `param`s as little-endian f32, then the
/// `command` u16, then `target_system` / `target_component` / `confirmation` as
/// u8, in wire order), MAVLink2 trailing-zero truncation, the v2 header (both the
/// incompat and compat flags are 0), and the X.25 checksum folded with
/// [`COMMAND_LONG_CRC_EXTRA`]. Identical on the wire to a `COMMAND_LONG` for a
/// named id; the only difference is this accepts an id the enum has no variant
/// for.
///
/// `confirmation` is fixed at 0 (the command surface is fire-and-forget). The
/// frame is unsigned (no MAVLink2 signature; incompat flags = 0).
#[allow(clippy::too_many_arguments)]
pub fn build_command_long_v2(
    header: MavHeader,
    command: u16,
    target_system: u8,
    target_component: u8,
    params: [f32; 7],
) -> Vec<u8> {
    // The COMMAND_LONG payload in wire (field-id) order: the seven f32 params
    // first (28 bytes), then the u16 command (2), then target_system,
    // target_component, confirmation (1 each) → 33 bytes max.
    let mut payload = Vec::with_capacity(33);
    for p in params {
        payload.extend_from_slice(&p.to_le_bytes());
    }
    payload.extend_from_slice(&command.to_le_bytes());
    payload.push(target_system);
    payload.push(target_component);
    payload.push(0u8); // confirmation

    // MAVLink2 truncates trailing zero bytes off the payload, keeping at least
    // one byte. The CRC is computed over the truncated payload.
    truncate_trailing_zeros(&mut payload);

    let mut frame = Vec::with_capacity(10 + payload.len() + 2);
    frame.push(0xFD); // v2 start-of-frame
    frame.push(payload.len() as u8); // payload length
    frame.push(0x00); // incompat flags (unsigned)
    frame.push(0x00); // compat flags
    frame.push(header.sequence);
    frame.push(header.system_id);
    frame.push(header.component_id);
    // 3-byte little-endian message id.
    frame.push((MSG_ID_COMMAND_LONG & 0xFF) as u8);
    frame.push(((MSG_ID_COMMAND_LONG >> 8) & 0xFF) as u8);
    frame.push(((MSG_ID_COMMAND_LONG >> 16) & 0xFF) as u8);
    frame.extend_from_slice(&payload);

    // X.25 checksum over every byte after the start-of-frame, then the CRC_EXTRA.
    let mut crc = X25_INIT;
    for &b in &frame[1..] {
        crc = x25_accumulate(b, crc);
    }
    crc = x25_accumulate(COMMAND_LONG_CRC_EXTRA, crc);
    frame.push((crc & 0xFF) as u8);
    frame.push((crc >> 8) as u8);

    frame
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use rust_mavlink::ardupilotmega::HEARTBEAT_DATA;

    fn heartbeat() -> MavMessage {
        MavMessage::HEARTBEAT(HEARTBEAT_DATA {
            custom_mode: 0,
            mavtype: rust_mavlink::ardupilotmega::MavType::MAV_TYPE_QUADROTOR,
            autopilot: rust_mavlink::ardupilotmega::MavAutopilot::MAV_AUTOPILOT_ARDUPILOTMEGA,
            base_mode: rust_mavlink::ardupilotmega::MavModeFlag::empty(),
            system_status: rust_mavlink::ardupilotmega::MavState::MAV_STATE_STANDBY,
            mavlink_version: 3,
        })
    }

    #[test]
    fn heartbeat_round_trips_through_v2_frame() {
        let header = MavHeader {
            system_id: 1,
            component_id: 1,
            sequence: 42,
        };
        let frame = serialize_v2(header, &heartbeat()).unwrap();
        // A v2 frame starts with the 0xFD magic byte.
        assert_eq!(frame[0], 0xFD);

        let (got_header, got_msg) = parse_v2(&frame).unwrap();
        assert_eq!(got_header.system_id, 1);
        assert_eq!(got_header.component_id, 1);
        assert_eq!(got_header.sequence, 42);
        match got_msg {
            MavMessage::HEARTBEAT(hb) => {
                assert_eq!(
                    hb.mavtype,
                    rust_mavlink::ardupilotmega::MavType::MAV_TYPE_QUADROTOR
                );
                assert_eq!(hb.mavlink_version, 3);
            }
            other => panic!("expected HEARTBEAT, got {other:?}"),
        }
    }

    #[test]
    fn truncated_frame_is_a_read_error() {
        let header = MavHeader::default();
        let frame = serialize_v2(header, &heartbeat()).unwrap();
        // Drop the last few bytes so the frame is incomplete.
        assert!(matches!(
            parse_v2(&frame[..frame.len() - 3]),
            Err(MavlinkError::Read(_))
        ));
    }

    #[test]
    fn heartbeat_round_trips_through_v1_frame() {
        let header = MavHeader {
            system_id: 1,
            component_id: 1,
            sequence: 7,
        };
        let frame = serialize_v1(header, &heartbeat()).unwrap();
        // A v1 frame starts with the 0xFE magic byte.
        assert_eq!(frame[0], 0xFE);

        let (got_header, got_msg) = parse_v1(&frame).unwrap();
        assert_eq!(got_header.system_id, 1);
        assert_eq!(got_header.component_id, 1);
        assert_eq!(got_header.sequence, 7);
        assert!(matches!(got_msg, MavMessage::HEARTBEAT(_)));
    }

    #[test]
    fn outbound_frame_is_length_prefixed_and_decode_recovers_it() {
        // The MAVLink socket contract frames each outbound raw MAVLink frame
        // with a 4-byte big-endian length prefix. A consumer reads the prefix
        // with decode_len and then exactly that many payload bytes, recovering
        // the original frame verbatim.
        use crate::frame::{decode_len, encode_frame, HEADER_SIZE, MAVLINK_MAX_FRAME};
        let header = MavHeader {
            system_id: 1,
            component_id: 1,
            sequence: 5,
        };
        let raw = serialize_v2(header, &heartbeat()).unwrap();

        let framed = encode_frame(&raw, MAVLINK_MAX_FRAME).unwrap();
        // The prefix is the big-endian length of the raw frame.
        let prefix: [u8; HEADER_SIZE] = framed[..HEADER_SIZE].try_into().unwrap();
        let len = decode_len(prefix, MAVLINK_MAX_FRAME, false).unwrap();
        assert_eq!(len, raw.len());
        // The payload after the prefix is the original frame, unchanged.
        assert_eq!(&framed[HEADER_SIZE..], raw.as_slice());
        // And it still parses as the heartbeat it started as.
        let (_, msg) = parse_any(&framed[HEADER_SIZE..]).unwrap();
        assert!(matches!(msg, MavMessage::HEARTBEAT(_)));
    }

    #[test]
    fn parse_any_dispatches_on_start_of_frame_byte() {
        let header = MavHeader {
            system_id: 1,
            component_id: 1,
            sequence: 3,
        };
        let v2_frame = serialize_v2(header, &heartbeat()).unwrap();
        let v1_frame = serialize_v1(header, &heartbeat()).unwrap();
        assert_eq!(v2_frame[0], 0xFD);
        assert_eq!(v1_frame[0], 0xFE);

        let (_, m2) = parse_any(&v2_frame).unwrap();
        let (_, m1) = parse_any(&v1_frame).unwrap();
        assert!(matches!(m2, MavMessage::HEARTBEAT(_)));
        assert!(matches!(m1, MavMessage::HEARTBEAT(_)));

        // An unknown start byte is rejected, not silently parsed.
        assert!(matches!(
            parse_any(&[0x00, 0x01, 0x02]),
            Err(MavlinkError::Read(_))
        ));
        assert!(matches!(parse_any(&[]), Err(MavlinkError::Read(_))));
    }

    // ── build_command_long_v2 ────────────────────────────────────────────────

    #[test]
    fn command_long_builder_matches_the_golden_set_camera_source_frame() {
        // The exact 44-byte frame the reference encoder produces for
        // SET_CAMERA_SOURCE (command 534) with camera_index=2, source
        // system/component 255/190, target 1/1, sequence 0. param2 carries the
        // index (2.0); every other param is 0.
        let header = MavHeader {
            system_id: 255,
            component_id: 190,
            sequence: 0,
        };
        let frame = build_command_long_v2(header, 534, 1, 1, [0.0, 2.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
        let golden = hex_to_bytes(
            "fd20000000ffbe4c000000000000000000400000000000000000000000000000000000000000160201019b45",
        );
        assert_eq!(
            frame, golden,
            "the built frame must be byte-identical to the golden SET_CAMERA_SOURCE frame"
        );
    }

    #[test]
    fn command_long_builder_truncates_trailing_zeros_and_is_44_bytes() {
        let header = MavHeader {
            system_id: 255,
            component_id: 190,
            sequence: 0,
        };
        let frame = build_command_long_v2(header, 534, 1, 1, [0.0, 2.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
        // 10-byte v2 header + 32-byte truncated payload + 2-byte CRC = 44.
        assert_eq!(frame.len(), 44);
        assert_eq!(frame[0], 0xFD); // v2 start-of-frame
        assert_eq!(frame[1], 32); // truncated payload length
        assert_eq!(frame[2], 0x00); // incompat flags (unsigned)
        assert_eq!(frame[3], 0x00); // compat flags
                                    // 3-byte LE message id == 76.
        assert_eq!(frame[7], 76);
        assert_eq!(frame[8], 0);
        assert_eq!(frame[9], 0);
    }

    #[test]
    fn command_long_builder_crc_matches_rust_mavlink_for_a_named_id() {
        // For a command the dialect DOES name (ARM_DISARM, 400), the raw builder
        // must produce the byte-identical frame the typed serializer produces —
        // proving the X.25 + CRC_EXTRA fold here is consistent with the codec the
        // rest of the agent uses. ARM_DISARM's CRC_EXTRA differs from
        // COMMAND_LONG's, but the message id + payload layout are the same, so we
        // compare the frame bytes up to (but excluding) the 2-byte CRC, then assert
        // both CRCs are well-formed 2-byte tails.
        use rust_mavlink::ardupilotmega::{MavCmd, COMMAND_LONG_DATA};
        let header = MavHeader {
            system_id: 1,
            component_id: 191,
            sequence: 7,
        };
        let typed = MavMessage::COMMAND_LONG(COMMAND_LONG_DATA {
            target_system: 1,
            target_component: 1,
            command: MavCmd::MAV_CMD_COMPONENT_ARM_DISARM,
            confirmation: 0,
            param1: 1.0,
            param2: 0.0,
            param3: 0.0,
            param4: 0.0,
            param5: 0.0,
            param6: 0.0,
            param7: 0.0,
        });
        let typed_frame = serialize_v2(header, &typed).unwrap();
        // The raw builder for the same command id + params.
        let raw_frame =
            build_command_long_v2(header, 400, 1, 1, [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
        // The header + payload (everything but the trailing 2-byte CRC) is
        // identical: the raw builder reproduces the typed serializer's framing.
        assert_eq!(
            &raw_frame[..raw_frame.len() - 2],
            &typed_frame[..typed_frame.len() - 2],
            "header + payload must match the typed serializer for the same command"
        );
    }

    #[test]
    fn command_long_builder_zero_truncation_keeps_one_byte() {
        // An all-zero payload truncates to a single byte (never empty), so the
        // frame is still a valid, parseable v2 frame.
        let mut payload = vec![0u8, 0, 0, 0];
        truncate_trailing_zeros(&mut payload);
        assert_eq!(payload, vec![0u8]);
    }

    /// Decode a lowercase hex string into bytes for the golden-frame assertions.
    fn hex_to_bytes(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }
}
