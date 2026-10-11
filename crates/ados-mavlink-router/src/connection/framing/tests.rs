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
fn a_dropped_byte_costs_only_the_damaged_frame() {
    // A UART overrun drops one payload byte. The damaged frame then reaches
    // into the next one; its checksum fails, and every real frame after it
    // must still come out.
    let frame = heartbeat_frame();
    let mut damaged = frame.clone();
    damaged.remove(12);
    let mut buf = damaged;
    for _ in 0..3 {
        buf.extend_from_slice(&frame);
    }
    let frames = extract_frames(&mut buf);
    assert_eq!(frames.len(), 3, "the three intact frames survive");
    assert!(frames.iter().all(|f| f == &frame));
}

#[test]
fn a_stx_inside_noise_is_not_taken_as_a_frame() {
    // Line noise that happens to contain a start byte and a plausible
    // length: without a checksum it would be cut and forwarded.
    let frame = heartbeat_frame();
    let mut buf = vec![0xFD, 0x05, 0x00, 0x00, 0x11, 0x01, 0x01, 0x21, 0x00, 0x00];
    buf.extend_from_slice(&[0xAA; 7]);
    buf.extend_from_slice(&frame);
    let frames = extract_frames(&mut buf);
    assert_eq!(frames, vec![Bytes::from(frame)]);
}

#[test]
fn hundred_frame_chunk_matches_frame_by_frame() {
    let frame = heartbeat_frame();
    let mut chunk = Vec::new();
    for i in 0..100u8 {
        // Junk between some frames exercises the skip path too.
        if i % 7 == 0 {
            chunk.extend_from_slice(&[0x11, 0x22]);
        }
        chunk.extend_from_slice(&frame);
    }
    // A partial trailing frame stays buffered.
    chunk.extend_from_slice(&frame[..5]);
    let mut buf = chunk.clone();
    let frames = extract_frames(&mut buf);
    assert_eq!(frames.len(), 100);
    assert!(frames.iter().all(|f| f.as_ref() == frame.as_slice()));
    assert_eq!(buf, frame[..5].to_vec());
    // Feeding the same bytes one at a time yields the same frames.
    let mut incremental = Vec::new();
    let mut stepwise = Vec::new();
    for b in chunk {
        incremental.push(b);
        stepwise.extend(extract_frames(&mut incremental));
    }
    assert_eq!(stepwise, frames);
    assert_eq!(incremental, frame[..5].to_vec());
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

/// A v2 frame with an arbitrary id and payload. The checksum bytes are
/// synthetic: an id outside the dialect has no CRC_EXTRA to verify against.
fn v2_unknown_frame(msg_id: u32, payload: &[u8], signed: bool) -> Vec<u8> {
    assert!(!msg_id_known(msg_id));
    let id = msg_id.to_le_bytes();
    let mut f = vec![
        0xFD,
        payload.len() as u8,
        u8::from(signed),
        0,
        7,
        42,
        200,
        id[0],
        id[1],
        id[2],
    ];
    f.extend_from_slice(payload);
    f.extend_from_slice(&[0x5A, 0xA5]);
    if signed {
        f.extend_from_slice(&[0x33; 13]);
    }
    f
}

fn x25(bytes: &[u8], extra: u8) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &b in bytes.iter().chain(std::iter::once(&extra)) {
        let mut tmp = b ^ (crc & 0xFF) as u8;
        tmp ^= tmp << 4;
        let t = u16::from(tmp);
        crc = (crc >> 8) ^ (t << 8) ^ (t << 3) ^ (t >> 4);
    }
    crc
}

#[test]
fn v2_unknown_msg_id_is_forwarded_intact() {
    let unknown = v2_unknown_frame(0xFF_FFF0, &[1, 2, 3, 4, 5], false);
    let hb = heartbeat_frame();
    let mut buf = unknown.clone();
    buf.extend_from_slice(&hb);
    let frames = extract_frames(&mut buf);
    assert_eq!(frames, vec![Bytes::from(unknown), Bytes::from(hb)]);
    assert!(buf.is_empty());
}

#[test]
fn v1_unknown_msg_id_is_forwarded_intact() {
    let id = (0u32..=255)
        .find(|&id| !msg_id_known(id))
        .expect("the dialect leaves some v1 id unassigned");
    let mut unknown = vec![0xFE, 3, 9, 1, 1, id as u8, 0xAA, 0xBB, 0xCC];
    unknown.extend_from_slice(&[0x12, 0x34]);
    let hb = heartbeat_frame_v1();
    let mut buf = unknown.clone();
    buf.extend_from_slice(&hb);
    let frames = extract_frames(&mut buf);
    assert_eq!(frames, vec![Bytes::from(unknown), Bytes::from(hb)]);
    assert!(buf.is_empty());
}

#[test]
fn signed_v2_frames_include_the_13_byte_signature() {
    let unknown = v2_unknown_frame(0xFF_FFF0, &[9; 4], true);
    assert_eq!(unknown.len(), 10 + 4 + 2 + 13);
    // A signed known-id heartbeat: the incompat flag is covered by the CRC.
    let mut signed_hb = heartbeat_frame();
    signed_hb[2] = 0x01;
    let crc_at = signed_hb.len() - 2;
    let crc = x25(&signed_hb[1..crc_at], <MavMessage as Message>::extra_crc(0));
    signed_hb[crc_at..].copy_from_slice(&crc.to_le_bytes());
    signed_hb.extend_from_slice(&[0x44; 13]);
    let hb = heartbeat_frame();
    let mut buf = unknown.clone();
    buf.extend_from_slice(&signed_hb);
    buf.extend_from_slice(&hb);
    let frames = extract_frames(&mut buf);
    assert_eq!(
        frames,
        vec![
            Bytes::from(unknown),
            Bytes::from(signed_hb),
            Bytes::from(hb)
        ]
    );
    assert!(buf.is_empty());
}

#[test]
fn known_id_with_bad_crc_is_dropped_and_next_frame_parses() {
    let hb = heartbeat_frame();
    let mut bad = hb.clone();
    let last = bad.len() - 1;
    bad[last] ^= 0xFF;
    let mut buf = bad;
    buf.extend_from_slice(&hb);
    let frames = extract_frames(&mut buf);
    assert_eq!(frames, vec![Bytes::from(hb)]);
    assert!(buf.is_empty());
}

#[test]
fn unknown_id_frame_split_across_reads_is_reassembled() {
    let unknown = v2_unknown_frame(0xFF_FFF0, &[7; 20], true);
    for split in [1, 2, 9, 12, unknown.len() - 1] {
        let mut buf = unknown[..split].to_vec();
        assert!(extract_frames(&mut buf).is_empty());
        assert_eq!(buf.len(), split);
        buf.extend_from_slice(&unknown[split..]);
        assert_eq!(extract_frames(&mut buf), vec![Bytes::from(unknown.clone())]);
        assert!(buf.is_empty());
    }
}

#[test]
fn garbage_prefix_before_unknown_id_frame_is_skipped() {
    let unknown = v2_unknown_frame(0xFF_FFF0, &[1, 2], false);
    let mut buf = vec![0x00, 0x11, 0x22, 0x7F];
    buf.extend_from_slice(&unknown);
    assert_eq!(extract_frames(&mut buf), vec![Bytes::from(unknown)]);
    assert!(buf.is_empty());
}
