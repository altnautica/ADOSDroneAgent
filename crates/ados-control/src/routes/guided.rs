//! Guided position/velocity setpoint sender.
//!
//! A single-shot sender for the two `SET_POSITION_TARGET` messages
//! (`SET_POSITION_TARGET_LOCAL_NED`, id 84, and `SET_POSITION_TARGET_GLOBAL_INT`,
//! id 86): it takes a validated setpoint, builds the typed MAVLink message, frames
//! it as a v2 frame, and writes it to `/run/ados/mavlink.sock` for the router to
//! forward to the FC. It is the same build-then-write seam the text-command path
//! (`command.rs`) uses for `COMMAND_LONG`; the only difference is the message it
//! builds.
//!
//! ## This is a primitive, not a controller
//!
//! Sending one setpoint is one fire-and-forget MAVLink frame. This sender owns no
//! flight state, mode, or schedule. The caller is responsible for the policy:
//!
//! * The autopilot accepts a setpoint only while it is in its guided/offboard
//!   mode; the caller must put the vehicle there and confirm it. Sending a
//!   setpoint does not change the mode and does not check it.
//! * A guided setpoint stream is a heartbeat. Most autopilots brake (then
//!   fail-safe, e.g. RTL) roughly three seconds after the last setpoint, so to
//!   hold a commanded velocity the caller must re-send well above that rate
//!   (about 3 Hz or faster). One frame on its own decays.
//!
//! The validation (finite numbers on every active axis, a sane `type_mask`, a
//! coordinate frame valid for the message kind) lives in
//! [`ados_protocol::mavlink::GuidedSetpoint`], so the policy is in one place and
//! the sender just builds + writes.

use ados_protocol::mavlink::{self, GuidedSetpoint, MavHeader};

use crate::ipc::mavlink_client::SendError;
use crate::state::AppState;

/// The source identity stamped on the setpoint frame: the agent/companion
/// identity the router uses on its own FC send path (defaults 1/191), so a
/// setpoint from this surface is wire-identical to one the router sent. Matches
/// the text-command surface's source identity.
const SOURCE_SYSTEM_ID: u8 = 1;
const SOURCE_COMPONENT_ID: u8 = 191;

/// The target identity: the single-vehicle ArduPilot defaults (1/1), the same as
/// the text-command surface. A caller addressing a non-default sysid passes it
/// through `target_system` / `target_component`.
pub const DEFAULT_TARGET_SYSTEM: u8 = 1;
pub const DEFAULT_TARGET_COMPONENT: u8 = 1;

/// A guided-setpoint send failure: the setpoint failed validation, or the frame
/// could not be written to the MAVLink socket (no FC link).
#[derive(Debug, thiserror::Error)]
pub enum GuidedSendError {
    /// The setpoint failed validation (a non-finite active axis, a malformed
    /// type mask, or a coordinate frame wrong for the message kind). The string
    /// is the validator's stable message.
    #[error("invalid setpoint: {0}")]
    Invalid(String),
    /// The MAVLink socket could not be written (the router/FC link is down). The
    /// caller maps this to the same no-FC-link failure the command path returns.
    #[error("setpoint send failed: {0}")]
    Send(#[from] SendError),
}

/// Build one `SET_POSITION_TARGET` frame for `setpoint` to the default target and
/// write it to the MAVLink socket. See [`send_setpoint_to`] for the target-aware
/// form.
pub async fn send_setpoint(
    state: &AppState,
    setpoint: &GuidedSetpoint,
) -> Result<usize, GuidedSendError> {
    send_setpoint_to(
        state,
        setpoint,
        DEFAULT_TARGET_SYSTEM,
        DEFAULT_TARGET_COMPONENT,
    )
    .await
}

/// Build one `SET_POSITION_TARGET` frame for `setpoint` addressed to
/// `target_system` / `target_component` and write it to the MAVLink socket.
///
/// Returns the number of bytes written (the frame length) on success. The
/// setpoint is validated by [`GuidedSetpoint::build_message`] before any send, so
/// an invalid setpoint is an [`GuidedSendError::Invalid`] with no socket touched;
/// a write failure (no router/FC link) is an [`GuidedSendError::Send`]. This is a
/// single-shot send — see the module docs for the re-send + guided-mode contract.
pub async fn send_setpoint_to(
    state: &AppState,
    setpoint: &GuidedSetpoint,
    target_system: u8,
    target_component: u8,
) -> Result<usize, GuidedSendError> {
    // Build the typed message (validates inside): a non-finite active axis, a
    // malformed mask, or a frame wrong for the kind fails here before any send.
    let msg = setpoint
        .build_message(target_system, target_component)
        .map_err(|e| GuidedSendError::Invalid(e.to_string()))?;

    // Serialize the v2 frame with the companion source identity. The sequence is
    // not load-bearing on a setpoint (the router stamps its own on its own send
    // path), so 0 is used, matching the fire-and-forget command path.
    let header = MavHeader {
        system_id: SOURCE_SYSTEM_ID,
        component_id: SOURCE_COMPONENT_ID,
        sequence: 0,
    };
    let frame = mavlink::serialize_v2(header, &msg).map_err(|e| {
        // A serialize failure on a fixed-shape setpoint is a programmer error,
        // surfaced as an Invalid rather than a panic.
        GuidedSendError::Invalid(format!("setpoint frame encode failed: {e}"))
    })?;

    state.mavlink.send(&frame).await?;
    Ok(frame.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_protocol::mavlink::{ardupilotmega, parse_v2, MavMessage, SetpointKind};

    /// A pure-velocity local-NED setpoint in the body frame.
    fn velocity_setpoint(kind: SetpointKind, frame: u8) -> GuidedSetpoint {
        // Ignore position / accel / yaw; command vx/vy/vz.
        let mask = 1 + 2 + 4 + 64 + 128 + 256 + 1024 + 2048;
        GuidedSetpoint {
            kind,
            coordinate_frame: frame,
            type_mask: mask,
            x: 0.0,
            y: 0.0,
            z: 0.0,
            vx: 2.5,
            vy: -1.0,
            vz: 0.5,
            afx: 0.0,
            afy: 0.0,
            afz: 0.0,
            yaw: 0.0,
            yaw_rate: 0.0,
        }
    }

    /// The sender builds the exact frame the wire expects: round-trip the bytes
    /// the sender would write (built via the same builder + serializer) and assert
    /// the message id + fields. This proves the build path independent of a live
    /// socket; the socket write itself is the MavlinkIpcClient's own tested seam.
    fn frame_for(setpoint: &GuidedSetpoint, ts: u8, tc: u8) -> Vec<u8> {
        let msg = setpoint
            .build_message(ts, tc)
            .expect("valid setpoint builds");
        let header = MavHeader {
            system_id: SOURCE_SYSTEM_ID,
            component_id: SOURCE_COMPONENT_ID,
            sequence: 0,
        };
        mavlink::serialize_v2(header, &msg).expect("serialize succeeds")
    }

    #[test]
    fn local_ned_frame_builds_and_round_trips() {
        let sp = velocity_setpoint(SetpointKind::LocalNed, 8); // MAV_FRAME_BODY_NED
        let frame = frame_for(&sp, DEFAULT_TARGET_SYSTEM, DEFAULT_TARGET_COMPONENT);
        // Message id 84 in the 3-byte v2 id field.
        assert_eq!(frame[7], 84);
        assert_eq!((frame[8], frame[9]), (0, 0));
        match parse_v2(&frame).expect("decode").1 {
            MavMessage::SET_POSITION_TARGET_LOCAL_NED(d) => {
                assert_eq!(d.vx, 2.5);
                assert_eq!(d.vy, -1.0);
                assert_eq!(d.vz, 0.5);
                assert_eq!(d.target_system, 1);
                assert_eq!(d.target_component, 1);
                assert_eq!(
                    d.coordinate_frame,
                    ardupilotmega::MavFrame::MAV_FRAME_BODY_NED
                );
            }
            other => panic!("expected LOCAL_NED, got {other:?}"),
        }
    }

    #[test]
    fn global_int_frame_builds_and_round_trips() {
        let sp = velocity_setpoint(SetpointKind::GlobalInt, 6); // GLOBAL_RELATIVE_ALT_INT
        let frame = frame_for(&sp, 2, 1);
        assert_eq!(frame[7], 86);
        assert_eq!((frame[8], frame[9]), (0, 0));
        match parse_v2(&frame).expect("decode").1 {
            MavMessage::SET_POSITION_TARGET_GLOBAL_INT(d) => {
                assert_eq!(d.vx, 2.5);
                assert_eq!(d.target_system, 2, "target override is honoured");
            }
            other => panic!("expected GLOBAL_INT, got {other:?}"),
        }
    }

    /// Build an `AppState` for a sender test: a disconnected state client, a
    /// MAVLink client pointed at `mavlink_sock`, and inert paths for the rest.
    /// Mirrors the harness the param-write route tests use.
    fn test_state(dir: &std::path::Path, mavlink_sock: std::path::PathBuf) -> AppState {
        use crate::auth::PairingState;
        use crate::ipc::{LogdQueryClient, MavlinkIpcClient, StateIpcClient};
        use crate::state::PairingPaths;
        use std::sync::Arc;
        AppState::new(
            Arc::new(PairingState::with_path(dir.join("pairing.json"))),
            StateIpcClient::disconnected(),
            MavlinkIpcClient::new(mavlink_sock),
            LogdQueryClient::new(dir.join("absent-logd.sock")),
            dir.join("board.json"),
            PairingPaths {
                config: dir.join("config.yaml"),
                pairing_json: dir.join("pairing.json"),
                wfb_key_dir: dir.join("wfb"),
                bind_state: dir.join("bind-state.json"),
            },
        )
    }

    #[tokio::test]
    async fn send_setpoint_writes_a_decodable_frame_to_the_socket() {
        use ados_protocol::frame::{decode_len, HEADER_SIZE, MAVLINK_MAX_FRAME};
        use tokio::io::AsyncReadExt;
        use tokio::net::UnixListener;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mavlink.sock");
        let listener = UnixListener::bind(&path).unwrap();

        // The router side: accept, read the one length-prefixed frame.
        let server = tokio::spawn(async move {
            let (mut conn, _addr) = listener.accept().await.unwrap();
            let mut header = [0u8; HEADER_SIZE];
            conn.read_exact(&mut header).await.unwrap();
            let len = decode_len(header, MAVLINK_MAX_FRAME, false).unwrap();
            let mut body = vec![0u8; len];
            conn.read_exact(&mut body).await.unwrap();
            body
        });

        let state = test_state(dir.path(), path.clone());

        let sp = velocity_setpoint(SetpointKind::LocalNed, 1); // MAV_FRAME_LOCAL_NED
        let written = send_setpoint(&state, &sp).await.expect("send succeeds");

        let got = server.await.unwrap();
        assert_eq!(got.len(), written, "the byte count is the frame length");
        match parse_v2(&got).expect("decode").1 {
            MavMessage::SET_POSITION_TARGET_LOCAL_NED(d) => {
                assert_eq!(d.vx, 2.5);
                assert_eq!(d.vz, 0.5);
            }
            other => panic!("expected LOCAL_NED, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_setpoint_rejects_invalid_without_touching_the_socket() {
        let dir = tempfile::tempdir().unwrap();
        // The MAVLink socket is absent: if the sender tried to write it would fail
        // with a Send error, but the invalid setpoint fails validation first.
        let state = test_state(dir.path(), dir.path().join("absent.sock"));
        let mut sp = velocity_setpoint(SetpointKind::LocalNed, 1);
        sp.vx = f32::NAN; // active axis non-finite
        let err = send_setpoint(&state, &sp).await.unwrap_err();
        assert!(
            matches!(err, GuidedSendError::Invalid(ref m) if m.contains("vx must be a finite number")),
            "got: {err:?}"
        );
    }

    #[tokio::test]
    async fn send_setpoint_maps_an_absent_socket_to_a_send_error() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path(), dir.path().join("absent.sock"));
        // A valid setpoint but no router socket: the write fails → Send error,
        // which a caller maps to the same no-FC-link failure the command path uses.
        let sp = velocity_setpoint(SetpointKind::LocalNed, 1);
        let err = send_setpoint(&state, &sp).await.unwrap_err();
        assert!(matches!(err, GuidedSendError::Send(_)), "got: {err:?}");
    }
}
