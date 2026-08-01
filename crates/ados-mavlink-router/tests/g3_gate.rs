//! G3 — Betaflight agnosticism gate.
//!
//! The same NORMALIZED attitude/body-rate command that flies ArduPilot must
//! also fly a real Betaflight flight controller. G3 is the reason the lane
//! exists; until it passes this work has only added a second way to fly
//! ArduPilot.
//!
//! This gate is written FAILING-FIRST and deliberately left `#[ignore]`d: a
//! real Betaflight FC on a rig is required, and hardware proof is UNAVAILABLE
//! this session — recorded as **unproven**, never marked done. It is NOT a
//! substitute for the SITL gate, and it is not run by `cargo test` by default.
//!
//! ## What counts as an acknowledgement
//!
//! The acknowledgement is a matching body-rate echo: an `ATTITUDE_TARGET`
//! (id 83) frame whose `body_*_rate` fields carry back the rates that were
//! commanded. It is deliberately NOT "any inbound byte". An FC that streams
//! telemetry continuously — which every FC does — delivers inbound bytes
//! whether or not it accepted the attitude command, so a byte-presence test is
//! a guaranteed pass on a safety gate and proves nothing at all.
//!
//! ## Honesty caveat (verbatim from `ados-swarm-control::scenarios`)
//!
//! Gate scenario tests falsify a control-law bug; they say nothing about the
//! autopilot's attitude loop, its EKF or its failsafes, and they are not a
//! substitute for SITL before flight.

use std::collections::BTreeSet;

use ados_protocol::mavlink::{parse_any, serialize_v2, AttitudeSetpoint, MavHeader, MavMessage};
use ados_rate_control::rate_command;

/// How long to wait, after sending the normalized command, for the FC to
/// acknowledge it in its telemetry (the commanded body rate read back).
const G3_ACK_WINDOW: std::time::Duration = std::time::Duration::from_secs(2);

/// How close the echoed body rates must be to the commanded ones. Wide enough
/// for the f32 round trip through the wire, tight enough that a different
/// command or a stale frame does not read as a match.
const RATE_EPSILON: f32 = 1e-2;

/// The commanded body rates, in rad/s. The echo must carry these back.
const COMMANDED_ROLL_RATE: f32 = 0.5;
const COMMANDED_PITCH_RATE: f32 = -0.2;
const COMMANDED_YAW_RATE: f32 = 0.1;

/// The serial device of the real Betaflight FC under test, e.g.
/// `ADOS_G3_FC_PORT=/dev/ttyACM0`. Absent (the normal case without a rig) the
/// gate fails as unproven rather than silently passing.
fn fc_port() -> Result<String, String> {
    std::env::var("ADOS_G3_FC_PORT")
        .map_err(|_| "ADOS_G3_FC_PORT unset: no real Betaflight FC attached (unproven)".to_string())
}

fn normalized_command() -> AttitudeSetpoint {
    // The same NORMALIZED body-rate + thrust command the attitude rung sends to
    // any attitude-capable FC: rates in rad/s, thrust 0..1, quaternion ignored.
    let cmd = rate_command(
        COMMANDED_ROLL_RATE,
        COMMANDED_PITCH_RATE,
        COMMANDED_YAW_RATE,
        0.6,
    );
    AttitudeSetpoint {
        type_mask: 128, // ATTITUDE_TARGET_TYPEMASK_ATTITUDE_IGNORE
        q: [1.0, 0.0, 0.0, 0.0],
        body_roll_rate: cmd.body_roll_rate,
        body_pitch_rate: cmd.body_pitch_rate,
        body_yaw_rate: cmd.body_yaw_rate,
        thrust: cmd.thrust,
    }
}

/// A MAVLink frame located in a rolling byte buffer: where it starts, how long
/// it is on the wire, and its message id read straight from the header bytes.
///
/// The length is computed rather than inferred from the parser because
/// `parse_any` reports only the decoded message, not how many bytes it
/// consumed — and a rolling buffer has to advance by exactly one frame or it
/// resyncs onto its own tail.
struct FrameSpan {
    start: usize,
    len: usize,
    msg_id: u32,
}

/// Locate the first complete MAVLink frame at or after `from`.
///
/// Returns `None` when the buffer holds no start-of-frame magic yet, or holds a
/// truncated frame whose remaining bytes have not arrived. `Some(span)` with a
/// `len` past the buffer end never happens — the caller can slice it directly.
fn next_frame(buf: &[u8], from: usize) -> Option<FrameSpan> {
    let mut i = from;
    while i < buf.len() {
        match buf[i] {
            // MAVLink v2: STX, len, incompat, compat, seq, sysid, compid,
            // msgid[3], payload, checksum[2], and a 13-byte signature when the
            // incompat flag's low bit is set.
            0xFD => {
                if i + 10 > buf.len() {
                    return None;
                }
                let payload_len = buf[i + 1] as usize;
                let signed = buf[i + 2] & 0x01 != 0;
                let len = 12 + payload_len + if signed { 13 } else { 0 };
                if i + len > buf.len() {
                    return None;
                }
                let msg_id = u32::from(buf[i + 7])
                    | (u32::from(buf[i + 8]) << 8)
                    | (u32::from(buf[i + 9]) << 16);
                return Some(FrameSpan {
                    start: i,
                    len,
                    msg_id,
                });
            }
            // MAVLink v1: STX, len, seq, sysid, compid, msgid, payload, crc[2].
            0xFE => {
                if i + 6 > buf.len() {
                    return None;
                }
                let payload_len = buf[i + 1] as usize;
                let len = 8 + payload_len;
                if i + len > buf.len() {
                    return None;
                }
                return Some(FrameSpan {
                    start: i,
                    len,
                    msg_id: u32::from(buf[i + 5]),
                });
            }
            _ => i += 1,
        }
    }
    None
}

/// True when this message is the commanded body-rate echoed back.
fn is_matching_echo(msg: &MavMessage) -> bool {
    let MavMessage::ATTITUDE_TARGET(d) = msg else {
        return false;
    };
    (d.body_roll_rate - COMMANDED_ROLL_RATE).abs() <= RATE_EPSILON
        && (d.body_pitch_rate - COMMANDED_PITCH_RATE).abs() <= RATE_EPSILON
        && (d.body_yaw_rate - COMMANDED_YAW_RATE).abs() <= RATE_EPSILON
}

/// Put the FC tty into raw binary mode with a bounded read.
///
/// A CDC-ACM port comes up in canonical mode: `read()` blocks until a newline,
/// and `ICRNL`/`ISTRIP` rewrite bytes inside a binary MAVLink frame. Both would
/// make the echo assertion measure corrupted data, or hang past the deadline on
/// a silent FC — which is precisely the case the failure message must be able
/// to report. `VMIN = 0` / `VTIME = 1` gives a 100 ms bounded read that returns
/// zero bytes when the FC is quiet.
#[cfg(target_os = "linux")]
fn set_raw_binary_mode(port: &std::fs::File) -> Result<(), String> {
    use nix::sys::termios::{cfmakeraw, tcgetattr, tcsetattr, SetArg, SpecialCharacterIndices};
    use std::os::fd::AsFd;

    let mut t = tcgetattr(port.as_fd()).map_err(|e| format!("tcgetattr on the FC port: {e}"))?;
    cfmakeraw(&mut t);
    t.control_chars[SpecialCharacterIndices::VMIN as usize] = 0;
    t.control_chars[SpecialCharacterIndices::VTIME as usize] = 1; // 100 ms
    tcsetattr(port.as_fd(), SetArg::TCSANOW, &t)
        .map_err(|e| format!("tcsetattr on the FC port: {e}"))
}

#[cfg(not(target_os = "linux"))]
fn set_raw_binary_mode(_port: &std::fs::File) -> Result<(), String> {
    Err("the G3 gate needs a Linux FC tty; this host is not Linux".to_string())
}

/// G3: the same normalized command flies a real Betaflight FC.
///
/// FAILING-FIRST and `#[ignore]`d — do not remove the ignore. Hardware proof is
/// unproven this session; until G3 passes, this lane is a second way to fly
/// ArduPilot, never a live attitude command to an airframe. The gate is honest
/// about the control-law-only scope it proves (see the module caveat).
#[test]
#[ignore = "G3 hardware gate: requires a real Betaflight FC on a rig"]
fn same_normalized_command_flies_a_real_betaflight_fc() {
    let port_path = fc_port().expect(
        "no real Betaflight FC is attached this session; hardware proof unproven, \
         gate not passed (fail-closed, never a silent pass)",
    );
    // Serialize the normalized command to a real MAVLink v2 SET_ATTITUDE_TARGET.
    let sp = normalized_command();
    sp.validate().expect("the normalized command must validate");
    let msg = sp.build_message(1, 1).expect("builds");
    let bytes = serialize_v2(
        MavHeader {
            system_id: 1,
            component_id: 191,
            sequence: 0,
        },
        &msg,
    )
    .expect("serializes");

    // Open the FC port (a Linux character device; real hardware only — no
    // mock, no fallback) and write the frame.
    let mut port = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&port_path)
        .unwrap_or_else(|e| panic!("cannot open Betaflight FC on {port_path}: {e}"));
    set_raw_binary_mode(&port)
        .unwrap_or_else(|e| panic!("cannot put {port_path} into raw binary mode: {e}"));

    // Drive the Betaflight FC into its equivalent of the input, then command.
    // A real Betaflight board does not speak the guided/offboard handshake this
    // side assumes; G3 exists precisely to prove the normalized command reaches
    // it regardless. Send the command and require a matching body-rate echo.
    use std::io::{Read, Write};
    port.write_all(&bytes)
        .expect("writes the SET_ATTITUDE_TARGET frame");

    let mut chunk = [0u8; 512];
    let mut pending: Vec<u8> = Vec::with_capacity(4096);
    let mut seen_ids: BTreeSet<u32> = BTreeSet::new();
    let mut framed = 0usize;
    let mut parsed = 0usize;
    let mut inbound_bytes = 0usize;
    let mut ack = false;
    let deadline = std::time::Instant::now() + G3_ACK_WINDOW;
    'read: while std::time::Instant::now() < deadline {
        match port.read(&mut chunk) {
            Ok(0) => continue,
            Ok(n) => {
                inbound_bytes += n;
                pending.extend_from_slice(&chunk[..n]);
                // Drain every complete frame the buffer now holds.
                while let Some(span) = next_frame(&pending, 0) {
                    let frame = &pending[span.start..span.start + span.len];
                    // Record the id from the HEADER BYTES, before and regardless
                    // of the decode. A message id outside the ardupilotmega
                    // dialect, or a CRC the parser rejects, still proves the port
                    // is speaking MAVLink — and hiding it here would make a
                    // chattering FC read as "not MAVLink at all", which is the
                    // exact misdiagnosis the failure message exists to prevent.
                    framed += 1;
                    seen_ids.insert(span.msg_id);
                    if let Ok((_, msg)) = parse_any(frame) {
                        parsed += 1;
                        if is_matching_echo(&msg) {
                            ack = true;
                            break 'read;
                        }
                    }
                    pending.drain(..span.start + span.len);
                }
                // A buffer that only ever grows means the stream is not MAVLink
                // at all; cap it so a chatty non-MAVLink port cannot balloon.
                if pending.len() > 8192 {
                    pending.clear();
                }
            }
            Err(e) => panic!("reading the Betaflight FC on {port_path}: {e}"),
        }
    }

    let ids: Vec<String> = seen_ids.iter().map(|id| id.to_string()).collect();
    assert!(
        ack,
        "the real Betaflight FC did not echo the commanded body rates \
         ({COMMANDED_ROLL_RATE}, {COMMANDED_PITCH_RATE}, {COMMANDED_YAW_RATE} rad/s) in an \
         ATTITUDE_TARGET (id 83) within {G3_ACK_WINDOW:?} — G3 not passed (hardware unproven).\n\
         Observed on {port_path}: {inbound_bytes} inbound bytes, {framed} MAVLink frames \
         framed, {parsed} of them decoded, message ids [{}].\n\
         Reading the cases apart: 0 bytes = the FC is silent on this port (wrong port, no \
         MAVLink UART configured, or the board is not powered); bytes but 0 framed = the port \
         carries something that is not MAVLink (Betaflight's USB VCP speaks MSP/CLI, not \
         MAVLink — a MAVLink UART must be configured and wired); framed but 0 decoded = \
         MAVLink in a dialect this build does not carry; framed with ids but no id 83 = the FC \
         speaks MAVLink but never echoes the attitude target. Stock Betaflight MAVLink \
         telemetry emits HEARTBEAT/SYS_STATUS/ATTITUDE/RC_CHANNELS/GPS/VFR_HUD/BATTERY_STATUS \
         and does NOT emit ATTITUDE_TARGET, so on that firmware this gate cannot be satisfied \
         on this port and the honest close is UNPROVEN, not a wiring fault.",
        ids.join(", ")
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The framing walker must find a v2 frame, report its real wire length and
    /// its message id, so the rolling buffer advances by exactly one frame.
    #[test]
    fn next_frame_locates_a_v2_frame_and_its_length() {
        let sp = normalized_command();
        let msg = sp.build_message(1, 1).expect("builds");
        let bytes = serialize_v2(
            MavHeader {
                system_id: 1,
                component_id: 191,
                sequence: 0,
            },
            &msg,
        )
        .expect("serializes");

        // Leading garbage the walker must skip, then the frame, then a tail.
        let mut buf = vec![0x00, 0x11, 0x22];
        buf.extend_from_slice(&bytes);
        buf.extend_from_slice(&[0x77, 0x88]);

        let span = next_frame(&buf, 0).expect("finds the frame");
        assert_eq!(span.start, 3, "skips the leading garbage");
        assert_eq!(span.len, bytes.len(), "reports the real wire length");
        assert_eq!(span.msg_id, 82, "SET_ATTITUDE_TARGET is id 82");
        assert!(
            parse_any(&buf[span.start..span.start + span.len]).is_ok(),
            "the located span parses as a MAVLink frame"
        );
    }

    /// A frame whose bytes have not all arrived yet is not a frame. Reporting it
    /// as one would make the buffer advance past data it never saw.
    #[test]
    fn next_frame_waits_for_a_truncated_frame() {
        let sp = normalized_command();
        let msg = sp.build_message(1, 1).expect("builds");
        let bytes = serialize_v2(
            MavHeader {
                system_id: 1,
                component_id: 191,
                sequence: 0,
            },
            &msg,
        )
        .expect("serializes");
        let truncated = &bytes[..bytes.len() - 3];
        assert!(next_frame(truncated, 0).is_none());
    }

    /// A buffer with no start-of-frame magic yields nothing rather than
    /// misreading a payload byte as a header.
    #[test]
    fn next_frame_ignores_a_stream_with_no_magic() {
        assert!(next_frame(&[0x01, 0x02, 0x03, 0x04], 0).is_none());
        assert!(next_frame(&[], 0).is_none());
    }

    /// The acknowledgement is the commanded rates echoed in an ATTITUDE_TARGET.
    /// Any other message, and an ATTITUDE_TARGET carrying different rates, are
    /// both non-acknowledgements — that is the whole point of the gate.
    #[test]
    fn only_a_matching_attitude_target_echo_is_an_acknowledgement() {
        use ados_protocol::mavlink::ardupilotmega::{
            AttitudeTargetTypemask, ATTITUDE_TARGET_DATA, HEARTBEAT_DATA,
        };

        let echo = |roll: f32, pitch: f32, yaw: f32| {
            MavMessage::ATTITUDE_TARGET(ATTITUDE_TARGET_DATA {
                time_boot_ms: 0,
                q: [1.0, 0.0, 0.0, 0.0],
                body_roll_rate: roll,
                body_pitch_rate: pitch,
                body_yaw_rate: yaw,
                thrust: 0.6,
                type_mask: AttitudeTargetTypemask::empty(),
            })
        };

        assert!(is_matching_echo(&echo(
            COMMANDED_ROLL_RATE,
            COMMANDED_PITCH_RATE,
            COMMANDED_YAW_RATE
        )));
        // Within the f32 round-trip tolerance.
        assert!(is_matching_echo(&echo(0.5001, -0.1999, 0.1001)));
        // A different command is not this command's acknowledgement.
        assert!(!is_matching_echo(&echo(0.9, -0.2, 0.1)));
        assert!(!is_matching_echo(&echo(0.5, 0.2, 0.1)));
        // Plain telemetry is not an acknowledgement — the false pass this gate
        // used to have was accepting exactly this.
        assert!(!is_matching_echo(&MavMessage::HEARTBEAT(
            HEARTBEAT_DATA::default()
        )));
    }
}
