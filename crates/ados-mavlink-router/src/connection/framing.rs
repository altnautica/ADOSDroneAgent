//! MAVLink byte-stream framing.
//!
//! Splits the inbound serial/network byte stream into complete MAVLink frames
//! (v1 `0xFE` and v2 `0xFD`), tolerating junk before the next start-of-frame
//! magic and leaving any partial trailing frame buffered for the next read.

/// Both MAVLink start-of-frame magic bytes: `0xFD` (v2) and `0xFE` (v1).
pub(crate) const STX_V2: u8 = 0xFD;
pub(crate) const STX_V1: u8 = 0xFE;

/// Count MSP frame-start sequences in a raw byte slice: MSPv1 `$M<` / `$M>`
/// (`24 4D 3C/3E`) and MSPv2 `$X<` / `$X>` (`24 58 3C/3E`).
///
/// Used to detect a flight controller that is emitting MSP instead of MAVLink on
/// its serial port (e.g. a board whose USB port is left on MSP rather than
/// MAVLink), so the agent can tell the operator the link speaks the wrong
/// protocol rather than silently reporting no flight controller. This is
/// observe-only: MSP frames are never decoded, acted on, or forwarded to the
/// flight controller — only the three-byte frame-start signatures are counted.
pub(crate) fn count_msp_frame_starts(bytes: &[u8]) -> usize {
    bytes
        .windows(3)
        .filter(|w| {
            w[0] == b'$' && (w[1] == b'M' || w[1] == b'X') && (w[2] == b'<' || w[2] == b'>')
        })
        .count()
}

/// Total byte length of a complete frame whose head is at `buf[0]`, or `None`
/// when more bytes are needed (or the head is not a recognised magic byte).
///
/// A v2 frame is `0xFD`, a 1-byte payload length `L`, the rest of the 10-byte
/// header, `L` payload bytes, a 2-byte checksum, and (when the incompat-flags
/// signed bit is set) a 13-byte signature. A v1 frame is `0xFE`, a 1-byte
/// payload length `L`, a 6-byte header total, `L` payload bytes, and a 2-byte
/// checksum (no incompat/compat flags, no signature).
pub(crate) fn frame_total_len(buf: &[u8]) -> Option<usize> {
    match buf.first().copied() {
        Some(STX_V2) => {
            // Need the length and incompat-flags bytes to size a v2 frame.
            if buf.len() < 3 {
                return None;
            }
            let payload_len = buf[1] as usize;
            let signed = (buf[2] & 0x01) != 0;
            Some(10 + payload_len + 2 + if signed { 13 } else { 0 })
        }
        Some(STX_V1) => {
            // Need the length byte to size a v1 frame.
            if buf.len() < 2 {
                return None;
            }
            let payload_len = buf[1] as usize;
            Some(6 + payload_len + 2)
        }
        _ => None,
    }
}

/// Drain every complete MAVLink frame (v1 `0xFE` and v2 `0xFD`) from the head of
/// `buf`, returning the raw frame byte vectors and leaving any partial trailing
/// frame in `buf`. Junk before the next magic byte is dropped. Returns when the
/// buffer holds only a partial frame.
pub(crate) fn extract_frames(buf: &mut Vec<u8>) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        // Drop bytes before the next start-of-frame magic (either version).
        match buf.iter().position(|&b| b == STX_V2 || b == STX_V1) {
            Some(0) => {}
            Some(n) => {
                buf.drain(..n);
            }
            None => {
                buf.clear();
                break;
            }
        }
        let Some(total) = frame_total_len(buf) else {
            // Either too few bytes to size the frame yet, or the head byte is
            // not a magic byte (cannot happen after the search above). Wait for
            // more bytes.
            break;
        };
        if buf.len() < total {
            break;
        }
        out.push(buf[..total].to_vec());
        buf.drain(..total);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_protocol::mavlink::ardupilotmega::{
        MavAutopilot, MavMessage, MavModeFlag, MavState, MavType, HEARTBEAT_DATA,
    };
    use ados_protocol::mavlink::{self, MavHeader};

    fn heartbeat_frame() -> Vec<u8> {
        let msg = MavMessage::HEARTBEAT(HEARTBEAT_DATA {
            custom_mode: 0,
            mavtype: MavType::MAV_TYPE_QUADROTOR,
            autopilot: MavAutopilot::MAV_AUTOPILOT_ARDUPILOTMEGA,
            base_mode: MavModeFlag::empty(),
            system_status: MavState::MAV_STATE_STANDBY,
            mavlink_version: 3,
        });
        mavlink::serialize_v2(
            MavHeader {
                system_id: 1,
                component_id: 1,
                sequence: 0,
            },
            &msg,
        )
        .unwrap()
    }

    #[test]
    fn extract_one_complete_frame() {
        let frame = heartbeat_frame();
        let mut buf = frame.clone();
        let frames = extract_frames(&mut buf);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0], frame);
        assert!(buf.is_empty());
    }

    #[test]
    fn partial_frame_is_retained() {
        let frame = heartbeat_frame();
        let split = frame.len() - 2;
        let mut buf = frame[..split].to_vec();
        let frames = extract_frames(&mut buf);
        assert!(frames.is_empty());
        assert_eq!(buf.len(), split); // kept for the next read
                                      // Deliver the rest; now it parses.
        buf.extend_from_slice(&frame[split..]);
        let frames = extract_frames(&mut buf);
        assert_eq!(frames.len(), 1);
        assert!(buf.is_empty());
    }

    #[test]
    fn junk_before_magic_is_dropped_and_two_frames_extracted() {
        let frame = heartbeat_frame();
        let mut buf = vec![0x11, 0x22, 0x33];
        buf.extend_from_slice(&frame);
        buf.extend_from_slice(&frame);
        let frames = extract_frames(&mut buf);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0], frame);
        assert_eq!(frames[1], frame);
    }

    #[test]
    fn parsed_extracted_frame_is_a_heartbeat() {
        let frame = heartbeat_frame();
        let (_h, msg) = mavlink::parse_v2(&frame).unwrap();
        assert!(matches!(msg, MavMessage::HEARTBEAT(_)));
    }

    fn heartbeat_frame_v1() -> Vec<u8> {
        let msg = MavMessage::HEARTBEAT(HEARTBEAT_DATA {
            custom_mode: 0,
            mavtype: MavType::MAV_TYPE_QUADROTOR,
            autopilot: MavAutopilot::MAV_AUTOPILOT_ARDUPILOTMEGA,
            base_mode: MavModeFlag::empty(),
            system_status: MavState::MAV_STATE_STANDBY,
            mavlink_version: 3,
        });
        mavlink::serialize_v1(
            MavHeader {
                system_id: 1,
                component_id: 1,
                sequence: 0,
            },
            &msg,
        )
        .unwrap()
    }

    #[test]
    fn extract_one_v1_frame() {
        let frame = heartbeat_frame_v1();
        assert_eq!(frame[0], 0xFE);
        let mut buf = frame.clone();
        let frames = extract_frames(&mut buf);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0], frame);
        assert!(buf.is_empty());
    }

    #[test]
    fn extracted_v1_frame_decodes_and_round_trips_bytes() {
        let frame = heartbeat_frame_v1();
        // The framer returns the exact bytes (re-broadcast verbatim, no re-encode).
        let mut buf = frame.clone();
        let frames = extract_frames(&mut buf);
        assert_eq!(frames[0], frame);
        // The decode path recognises it as a v1 heartbeat.
        let (_h, msg) = mavlink::parse_any(&frames[0]).unwrap();
        assert!(matches!(msg, MavMessage::HEARTBEAT(_)));
    }

    #[test]
    fn extract_mixed_v1_and_v2_frames_in_one_buffer() {
        let v2 = heartbeat_frame();
        let v1 = heartbeat_frame_v1();
        let mut buf = Vec::new();
        buf.extend_from_slice(&v2);
        buf.extend_from_slice(&v1);
        buf.extend_from_slice(&v2);
        let frames = extract_frames(&mut buf);
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0], v2);
        assert_eq!(frames[1], v1);
        assert_eq!(frames[2], v2);
        assert!(buf.is_empty());
    }

    #[test]
    fn partial_v1_frame_is_retained() {
        let frame = heartbeat_frame_v1();
        let split = frame.len() - 2;
        let mut buf = frame[..split].to_vec();
        assert!(extract_frames(&mut buf).is_empty());
        buf.extend_from_slice(&frame[split..]);
        let frames = extract_frames(&mut buf);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0], frame);
        assert!(buf.is_empty());
    }

    #[test]
    fn counts_mspv1_and_mspv2_frame_starts() {
        // Two MSPv1 starts (`$M<` request, `$M>` response) plus one MSPv2 (`$X<`),
        // each with arbitrary trailing payload bytes between them.
        let buf = b"\x24\x4D\x3C\x01\x02\x24\x4D\x3E\x03\x24\x58\x3C\x04";
        assert_eq!(count_msp_frame_starts(buf), 3);
    }

    #[test]
    fn ignores_lone_dollar_or_wrong_third_byte() {
        // `$M` not followed by `<`/`>` is not a frame start.
        assert_eq!(count_msp_frame_starts(b"\x24\x4D\x41"), 0);
        // A bare `$` and ordinary text carry no frame starts.
        assert_eq!(count_msp_frame_starts(b"\x24"), 0);
        assert_eq!(count_msp_frame_starts(b"hello world"), 0);
        // The second byte must be `M` or `X`; `$A<` is not MSP.
        assert_eq!(count_msp_frame_starts(b"\x24\x41\x3C"), 0);
        // A slice shorter than one signature is zero, never a panic.
        assert_eq!(count_msp_frame_starts(b""), 0);
        assert_eq!(count_msp_frame_starts(b"\x24\x4D"), 0);
    }
}
