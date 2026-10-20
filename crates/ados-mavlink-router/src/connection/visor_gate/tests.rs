use super::*;
use std::pin::Pin;
use std::task::{Context, Poll};

use ados_protocol::mavlink::{MavMessage, Message};
use tokio::io::AsyncWrite;

use crate::config::MavlinkConfig;
use crate::connection::ClientOrigin;
use crate::param_cache::ParamCache;
use crate::state::VehicleState;

const HEARTBEAT: u32 = 0;
const PARAM_REQUEST_READ: u32 = 20;
const PARAM_REQUEST_LIST: u32 = 21;
const PARAM_SET: u32 = 23;
const MISSION_REQUEST_LIST: u32 = 43;
const REQUEST_DATA_STREAM: u32 = 66;
const TIMESYNC: u32 = 111;
const LOG_REQUEST_LIST: u32 = 117;
const SETUP_SIGNING: u32 = 256;
const RC_CHANNELS_OVERRIDE: u32 = 70;

const CMD_NAV_TAKEOFF: u16 = 22;
const CMD_DO_SET_HOME: u16 = 179;
const CMD_DO_REPOSITION: u16 = 192;
const CMD_PREFLIGHT_CALIBRATION: u16 = 241;
const CMD_SET_MESSAGE_INTERVAL: u16 = 511;
const CMD_REQUEST_MESSAGE: u16 = 512;

/// One X.25 checksum step, as MAVLink folds each byte.
fn x25(byte: u8, crc: u16) -> u16 {
    let mut tmp = byte ^ (crc & 0xFF) as u8;
    tmp ^= tmp << 4;
    let tmp = u16::from(tmp);
    (crc >> 8) ^ (tmp << 8) ^ (tmp << 3) ^ (tmp >> 4)
}

/// Append the checksum over `frame[1..]` folded with `msg_id`'s CRC_EXTRA.
fn push_crc(frame: &mut Vec<u8>, msg_id: u32) {
    let mut crc = frame[1..].iter().fold(0xFFFF, |c, &b| x25(b, c));
    crc = x25(<MavMessage as Message>::extra_crc(msg_id), crc);
    frame.extend_from_slice(&crc.to_le_bytes());
}

/// A v2 frame for `msg_id` over `payload` with `incompat` flags; a signed
/// frame carries a 13-byte signature after the checksum.
fn v2(msg_id: u32, payload: &[u8], incompat: u8) -> Vec<u8> {
    let id = msg_id.to_le_bytes();
    let mut frame = vec![
        STX_V2,
        payload.len() as u8,
        incompat,
        0,
        0,
        255,
        190,
        id[0],
        id[1],
        id[2],
    ];
    frame.extend_from_slice(payload);
    push_crc(&mut frame, msg_id);
    if incompat & IFLAG_SIGNED != 0 {
        frame.extend_from_slice(&[0xA5; 13]);
    }
    frame
}

/// A v1 frame for `msg_id` over `payload`.
fn v1(msg_id: u32, payload: &[u8]) -> Vec<u8> {
    let mut frame = vec![STX_V1, payload.len() as u8, 0, 255, 190, msg_id as u8];
    frame.extend_from_slice(payload);
    push_crc(&mut frame, msg_id);
    frame
}

/// A full-length `COMMAND_LONG` / `COMMAND_INT` payload carrying `command`
/// (both put the command at byte 28), addressed to 1/1.
fn command_payload(command: u16) -> Vec<u8> {
    let mut p = vec![0u8; COMMAND_OFFSET];
    p[0..4].copy_from_slice(&1.0f32.to_le_bytes());
    p.extend_from_slice(&command.to_le_bytes());
    p.extend_from_slice(&[1, 1, 0, 0, 0]);
    p
}

/// The same payload with MAVLink 2 trailing-zero truncation applied.
fn truncated(mut payload: Vec<u8>) -> Vec<u8> {
    while payload.len() > 1 && payload.last() == Some(&0) {
        payload.pop();
    }
    payload
}

fn command_long(command: u16) -> Vec<u8> {
    v2(MSG_COMMAND_LONG, &truncated(command_payload(command)), 0)
}

#[test]
fn the_frame_builders_produce_valid_checksums() {
    for frame in [
        command_long(CMD_COMPONENT_ARM_DISARM),
        v1(MSG_COMMAND_LONG, &command_payload(CMD_NAV_TAKEOFF)),
        v2(HEARTBEAT, &[0; 9], IFLAG_SIGNED),
    ] {
        assert!(ados_protocol::mavlink::frame_checksum_ok(&frame));
    }
    let reference = ados_protocol::mavlink::build_command_long_v2(
        ados_protocol::mavlink::MavHeader {
            system_id: 255,
            component_id: 190,
            sequence: 0,
        },
        CMD_COMPONENT_ARM_DISARM,
        1,
        1,
        [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
    );
    assert_eq!(command_long(CMD_COMPONENT_ARM_DISARM), reference);
}

#[test]
fn flight_messages_are_flight_actions_whatever_their_payload() {
    for id in [
        11, 38, 39, 41, 44, 45, 69, 70, 73, 82, 84, 86, 139, 386, 387,
    ] {
        assert!(is_flight_action_frame(id, &[0; 20]), "message {id}");
    }
}

#[test]
fn can_forwarding_and_actuator_commands_are_flight_actions() {
    // The router's twin of the control plane's CAN passthrough refusal, plus
    // the PX4 actuator commands.
    for cmd in [187u16, 310, 32000] {
        assert!(FLIGHT_COMMANDS.contains(&cmd), "{cmd}");
        for id in [MSG_COMMAND_LONG, MSG_COMMAND_INT] {
            assert!(
                is_flight_action_frame(id, &command_payload(cmd)),
                "{id} {cmd}"
            );
        }
    }
}

#[test]
fn parameter_calibration_info_sync_signing_and_logs_are_not_flight_actions() {
    for id in [
        HEARTBEAT,
        PARAM_REQUEST_READ,
        PARAM_REQUEST_LIST,
        PARAM_SET,
        MISSION_REQUEST_LIST,
        REQUEST_DATA_STREAM,
        TIMESYNC,
        LOG_REQUEST_LIST,
        SETUP_SIGNING,
    ] {
        assert!(!is_flight_action_frame(id, &[0; 20]), "message {id}");
    }
    for cmd in [
        CMD_PREFLIGHT_CALIBRATION,
        CMD_REQUEST_MESSAGE,
        CMD_SET_MESSAGE_INTERVAL,
        CMD_DO_SET_HOME,
    ] {
        for id in [MSG_COMMAND_LONG, MSG_COMMAND_INT] {
            assert!(
                !is_flight_action_frame(id, &command_payload(cmd)),
                "message {id} command {cmd}"
            );
        }
    }
}

#[test]
fn flight_commands_are_flight_actions_in_both_command_messages() {
    for &cmd in FLIGHT_COMMANDS {
        for id in [MSG_COMMAND_LONG, MSG_COMMAND_INT] {
            assert!(
                is_flight_action_frame(id, &command_payload(cmd)),
                "message {id} command {cmd}"
            );
        }
    }
}

#[test]
fn a_zero_truncated_command_field_reads_its_missing_bytes_as_zero() {
    // NAV_TAKEOFF to no target: the high command byte and everything after it
    // are zero, so a v2 sender ends the payload on the low command byte.
    let mut p = vec![0u8; COMMAND_OFFSET];
    p.extend_from_slice(&CMD_NAV_TAKEOFF.to_le_bytes());
    p.extend_from_slice(&[0, 0, 0]);
    let p = truncated(p);
    assert_eq!(p.len(), COMMAND_OFFSET + 1);
    assert!(is_flight_action_frame(MSG_COMMAND_LONG, &p));
    assert!(is_flight_action_frame(MSG_COMMAND_INT, &p));
    // Truncated before the command: command 0, not a flight action.
    assert!(!is_flight_action_frame(
        MSG_COMMAND_LONG,
        &p[..COMMAND_OFFSET]
    ));
    // And a truncated frame is filtered by the same reading.
    let frame = v2(MSG_COMMAND_LONG, &p, 0);
    assert_eq!(strip_flight_actions(&frame).1, 1);
}

#[test]
fn v2_flight_frames_are_removed_and_the_rest_forwarded_verbatim() {
    let heartbeat = v2(HEARTBEAT, &[0; 9], 0);
    let param_set = v2(PARAM_SET, &[7; 23], 0);
    let calibrate = command_long(CMD_PREFLIGHT_CALIBRATION);
    let mut data = heartbeat.clone();
    data.extend_from_slice(&command_long(CMD_COMPONENT_ARM_DISARM));
    data.extend_from_slice(&param_set);
    data.extend_from_slice(&v2(RC_CHANNELS_OVERRIDE, &[1; 18], 0));
    data.extend_from_slice(&calibrate);
    data.extend_from_slice(&v2(
        MSG_COMMAND_INT,
        &truncated(command_payload(CMD_DO_REPOSITION)),
        0,
    ));
    let (kept, refused) = strip_flight_actions(&data);
    assert_eq!(refused, 3);
    assert_eq!(kept.as_ref(), [heartbeat, param_set, calibrate].concat());
}

#[test]
fn v1_flight_frames_are_removed() {
    let param_set = v1(PARAM_SET, &[7; 23]);
    let mut data = v1(MSG_COMMAND_LONG, &command_payload(CMD_COMPONENT_ARM_DISARM));
    data.extend_from_slice(&param_set);
    data.extend_from_slice(&v1(11, &[0; 6]));
    let (kept, refused) = strip_flight_actions(&data);
    assert_eq!(refused, 2);
    assert_eq!(kept.as_ref(), param_set.as_slice());
}

#[test]
fn signed_frames_are_judged_by_content() {
    let signed_param = v2(PARAM_SET, &[7; 23], IFLAG_SIGNED);
    let mut data = v2(
        MSG_COMMAND_LONG,
        &truncated(command_payload(CMD_COMPONENT_ARM_DISARM)),
        IFLAG_SIGNED,
    );
    data.extend_from_slice(&signed_param);
    let (kept, refused) = strip_flight_actions(&data);
    assert_eq!(refused, 1);
    assert_eq!(kept.as_ref(), signed_param.as_slice());
}

#[test]
fn a_frame_with_an_unknown_incompat_flag_is_refused() {
    // The flight controller abandons such a header and parses its payload as
    // fresh frames, so a flight frame could hide inside it.
    let inner = command_long(CMD_COMPONENT_ARM_DISARM);
    let outer = v2(HEARTBEAT, &inner, 0x02);
    let (kept, refused) = strip_flight_actions(&outer);
    assert_eq!(refused, 1);
    assert!(kept.is_empty());
}

#[test]
fn clean_input_is_borrowed_unchanged() {
    let mut data = v2(HEARTBEAT, &[0; 9], 0);
    data.extend_from_slice(&v1(PARAM_REQUEST_LIST, &[1, 1]));
    let (kept, refused) = strip_flight_actions(&data);
    assert_eq!(refused, 0);
    assert!(matches!(kept, Cow::Borrowed(_)));
    assert_eq!(kept.as_ref(), data.as_slice());
}

#[test]
fn junk_partial_tails_and_bad_checksums_are_never_forwarded() {
    let heartbeat = v2(HEARTBEAT, &[0; 9], 0);
    let arm = command_long(CMD_COMPONENT_ARM_DISARM);
    let mut corrupt = arm.clone();
    let last = corrupt.len() - 1;
    corrupt[last] ^= 0xFF;
    let mut data = vec![0x00, 0x42];
    data.extend_from_slice(&heartbeat);
    data.extend_from_slice(&corrupt);
    data.extend_from_slice(&arm[..arm.len() - 4]);
    let (kept, refused) = strip_flight_actions(&data);
    assert_eq!(refused, 0, "nothing whole and valid was a flight action");
    assert_eq!(kept.as_ref(), heartbeat.as_slice());
}

/// A write half that sends everything written to it down a channel.
struct CapturingWriter(std::sync::mpsc::Sender<Vec<u8>>);

impl AsyncWrite for CapturingWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let _ = self.0.send(data.to_vec());
        Poll::Ready(Ok(data.len()))
    }
    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

/// A link of `profile` with a capturing writer installed.
async fn link(
    profile: &str,
) -> (
    std::sync::Arc<FcConnection>,
    std::sync::mpsc::Receiver<Vec<u8>>,
) {
    let state = std::sync::Arc::new(tokio::sync::Mutex::new(VehicleState::default()));
    let params = std::sync::Arc::new(tokio::sync::Mutex::new(ParamCache::new(
        "/tmp/ados-visor-gate-params.json",
    )));
    let cfg = MavlinkConfig {
        profile: profile.into(),
        ..MavlinkConfig::default()
    };
    let conn = FcConnection::new(cfg, state, params);
    let (tx, rx) = std::sync::mpsc::channel();
    conn.connected.store(true, Ordering::Relaxed);
    *conn.writer.lock().await = Some(Box::pin(CapturingWriter(tx)));
    (conn, rx)
}

fn written(rx: &std::sync::mpsc::Receiver<Vec<u8>>) -> Vec<u8> {
    rx.try_iter().flatten().collect()
}

#[tokio::test]
async fn a_visor_never_writes_a_client_flight_action_to_the_fc() {
    let (conn, rx) = link("visor").await;
    let arm = command_long(CMD_COMPONENT_ARM_DISARM);
    for origin in [
        ClientOrigin::Trusted,
        ClientOrigin::Relayed,
        ClientOrigin::Unauthenticated,
    ] {
        assert!(!conn.send_client_bytes(&arm, origin, None).await);
    }
    conn.send_client_raw(&arm, ClientOrigin::Trusted, None)
        .await;
    assert!(written(&rx).is_empty());
    assert_eq!(conn.visor_flight_frames_dropped(), 4);

    let param_set = v2(PARAM_SET, &[7; 23], 0);
    let mut mixed = arm.clone();
    mixed.extend_from_slice(&param_set);
    assert!(
        conn.send_client_bytes(&mixed, ClientOrigin::Trusted, None)
            .await
    );
    assert_eq!(written(&rx), param_set);
    assert_eq!(conn.visor_flight_frames_dropped(), 5);
}

#[tokio::test]
async fn vehicle_profiles_forward_flight_actions_unchanged() {
    for profile in ["drone", "ground-station"] {
        let (conn, rx) = link(profile).await;
        let arm = command_long(CMD_COMPONENT_ARM_DISARM);
        assert!(
            conn.send_client_bytes(&arm, ClientOrigin::Trusted, None)
                .await
        );
        assert_eq!(written(&rx), arm, "{profile}");
        assert_eq!(conn.visor_flight_frames_dropped(), 0);
    }
}

/// A `COMMAND_LONG` / `COMMAND_INT` payload for `command` with `param1` and
/// `param2`, truncated as a v2 sender would.
fn command_with(command: u16, param1: f32, param2: f32) -> Vec<u8> {
    let mut p = command_payload(command);
    p[0..4].copy_from_slice(&param1.to_le_bytes());
    p[4..8].copy_from_slice(&param2.to_le_bytes());
    truncated(p)
}

#[test]
fn disarm_and_flight_termination_are_not_flight_actions() {
    const CMD_DO_FLIGHTTERMINATION: u16 = 185;
    for msg in [MSG_COMMAND_LONG, MSG_COMMAND_INT] {
        // Plain and forced (21196) disarm.
        for force in [0.0, 21196.0] {
            let disarm = command_with(CMD_COMPONENT_ARM_DISARM, 0.0, force);
            assert!(
                !is_flight_action_frame(msg, &disarm),
                "msg {msg} force {force}"
            );
        }
        let terminate = command_with(CMD_DO_FLIGHTTERMINATION, 1.0, 0.0);
        assert!(!is_flight_action_frame(msg, &terminate), "msg {msg}");
        // Arming, forced or not, and a malformed param1 stay refused.
        for param1 in [1.0, 2.0, f32::NAN] {
            let arm = command_with(CMD_COMPONENT_ARM_DISARM, param1, 21196.0);
            assert!(
                is_flight_action_frame(msg, &arm),
                "msg {msg} param1 {param1}"
            );
        }
    }
}

#[tokio::test]
async fn a_visor_writes_a_client_disarm_to_the_fc() {
    let (conn, rx) = link("visor").await;
    let disarm = v2(
        MSG_COMMAND_LONG,
        &command_with(CMD_COMPONENT_ARM_DISARM, 0.0, 21196.0),
        0,
    );
    assert!(
        conn.send_client_bytes(&disarm, ClientOrigin::Trusted, None)
            .await
    );
    assert_eq!(written(&rx), disarm);
    assert_eq!(conn.visor_flight_frames_dropped(), 0);
}

#[tokio::test]
async fn an_msp_board_on_a_visor_gets_client_bytes_untouched() {
    let (conn, rx) = link("visor").await;
    *conn.fc_variant.lock().await = Some("betaflight".into());
    // MSP framing is not MAVLink: the filter must not strip it, even when the
    // bytes happen to look like a MAVLink arm frame.
    let msp = b"$M<\x00\x6c\x6c".to_vec();
    let arm = command_long(CMD_COMPONENT_ARM_DISARM);
    for bytes in [msp, arm] {
        conn.send_client_raw(&bytes, ClientOrigin::Trusted, None)
            .await;
        assert_eq!(written(&rx), bytes);
    }
    assert_eq!(conn.visor_flight_frames_dropped(), 0);
}

#[tokio::test]
async fn a_stream_frame_split_across_writes_reaches_a_visor_fc_whole() {
    let (conn, rx) = link("visor").await;
    let param_set = v2(PARAM_SET, &[7; 23], 0);
    let arm = command_long(CMD_COMPONENT_ARM_DISARM);
    let mut stream = param_set.clone();
    stream.extend_from_slice(&arm);
    stream.extend_from_slice(&param_set);
    // One IPC connection writing the stream in odd-sized pieces, interleaved
    // with a second connection's split frame.
    let mut framers = crate::connection::ConnFramers::default();
    let mut batches = Vec::new();
    for (i, piece) in stream.chunks(11).enumerate() {
        batches.extend(framers.push(1, piece));
        if i == 1 {
            batches.extend(framers.push(2, &param_set[..9]));
        }
    }
    batches.extend(framers.push(2, &param_set[9..]));
    for batch in &batches {
        conn.send_client_bytes(batch, ClientOrigin::Trusted, None)
            .await;
    }
    assert_eq!(
        written(&rx),
        [param_set.clone(), param_set.clone(), param_set].concat()
    );
    assert_eq!(conn.visor_flight_frames_dropped(), 1);
}

#[tokio::test]
async fn a_udp_datagram_of_whole_frames_passes_and_a_cut_one_loses_only_its_tail() {
    let (conn, rx) = link("visor").await;
    let param_set = v2(PARAM_SET, &[7; 23], 0);
    let mut datagram = param_set.clone();
    datagram.extend_from_slice(&param_set);
    assert!(
        conn.send_client_bytes(&datagram, ClientOrigin::Trusted, None)
            .await
    );
    assert_eq!(written(&rx), datagram);
    // A datagram cut mid-frame is malformed: its whole frame passes, the
    // fragment does not.
    let cut = &datagram[..param_set.len() + 7];
    conn.send_client_bytes(cut, ClientOrigin::Trusted, None)
        .await;
    assert_eq!(written(&rx), param_set);
}
