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

/// Serialize a message into a complete MAVLink v2 frame.
pub fn serialize_v2(header: MavHeader, msg: &MavMessage) -> Result<Vec<u8>, MavlinkError> {
    let mut buf = Vec::new();
    rust_mavlink::write_v2_msg(&mut buf, header, msg)
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
}
