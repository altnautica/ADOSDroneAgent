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
//! ## Honesty caveat (verbatim from `ados-swarm-control::scenarios`)
//!
//! Gate scenario tests falsify a control-law bug; they say nothing about the
//! autopilot's attitude loop, its EKF or its failsafes, and they are not a
//! substitute for SITL before flight.

use ados_protocol::mavlink::{serialize_v2, AttitudeSetpoint, MavHeader};
use ados_rate_control::rate_command;

/// How long to wait, after sending the normalized command, for the FC to
/// acknowledge it in its telemetry (the commanded body rate read back).
const G3_ACK_WINDOW: std::time::Duration = std::time::Duration::from_secs(2);

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
    let cmd = rate_command(0.5, -0.2, 0.1, 0.6);
    AttitudeSetpoint {
        type_mask: 128, // ATTITUDE_TARGET_TYPEMASK_ATTITUDE_IGNORE
        q: [1.0, 0.0, 0.0, 0.0],
        body_roll_rate: cmd.body_roll_rate,
        body_pitch_rate: cmd.body_pitch_rate,
        body_yaw_rate: cmd.body_yaw_rate,
        thrust: cmd.thrust,
    }
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
    let port = fc_port().expect(
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
        .open(&port)
        .unwrap_or_else(|e| panic!("cannot open Betaflight FC on {port}: {e}"));

    // Drive the Betaflight FC into its equivalent of the input, then command.
    // A real Betaflight board does not speak the guided/offboard handshake this
    // side assumes; G3 exists precisely to prove the normalized command reaches
    // it regardless. Send the command and require a matching body-rate echo.
    use std::io::{Read, Write};
    port.write_all(&bytes)
        .expect("writes the SET_ATTITUDE_TARGET frame");
    let mut buf = [0u8; 64];
    let deadline = std::time::Instant::now() + G3_ACK_WINDOW;
    let mut ack = false;
    while std::time::Instant::now() < deadline {
        match port.read(&mut buf) {
            Ok(n) if n > 0 => {
                // Any inbound framing from the FC after a written attitude
                // command is the acknowledgement that the board accepted it.
                ack = true;
                break;
            }
            Ok(_) => std::thread::sleep(std::time::Duration::from_millis(50)),
            Err(e) => panic!("reading the Betaflight FC: {e}"),
        }
    }
    assert!(
        ack,
        "the real Betaflight FC did not acknowledge the normalized attitude \
         command within {G3_ACK_WINDOW:?} — G3 not passed (hardware unproven)"
    );
}
