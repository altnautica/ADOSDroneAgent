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
        assert!(matches!(parse_any(&[0x00, 0x01, 0x02]), Err(MavlinkError::Read(_))));
        assert!(matches!(parse_any(&[]), Err(MavlinkError::Read(_))));
    }
}
