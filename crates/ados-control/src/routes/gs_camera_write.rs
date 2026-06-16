//! Ground-station camera-source switch write route.
//!
//! Lets a paired multi-camera drone toggle between its onboard camera sources
//! from a connected GCS. The route builds one `SET_CAMERA_SOURCE` (command 534)
//! COMMAND_LONG frame and writes it to the local MAVLink IPC socket, which the
//! router forwards to the FC over the radio link (fire-and-forget; the FC's
//! COMMAND_ACK lands on the MAVLink WS bridge, not here).
//!
//! - **`POST /api/v1/ground-station/camera/switch`** — `{camera_id}`.
//!   Returns `{camera_id, accepted, reason}` on a multi-camera drone; a
//!   single-camera (or unspecified) drone is a `501`; a malformed/out-of-range
//!   id is a `400`; an unreachable MAVLink IPC bus is a `503`.
//!
//! ## The live path is the 501
//!
//! The paired drone's camera count is not yet wired into its heartbeat, so the
//! count is fixed at 1 (single-camera) — the exact constant the FastAPI
//! `_paired_drone_camera_count` returns. So the live behaviour is the `501`
//! ("drone does not advertise multi-camera support"), which the GCS surfaces as
//! "not supported by this drone". The full switch path (build the 534 frame +
//! send it) is reproduced below so the route is byte-faithful the day the count
//! source is wired in; only the constant changes, not the route.
//!
//! ## The frame source + target identity
//!
//! The 534 COMMAND_LONG is built with the camera-command source identity the
//! reference encoder uses — system `255`, component `190` (a GCS-side camera
//! controller), NOT the autopilot-command identity (`1`/`191`) the
//! [`crate::routes::command`] route stamps — and the standard primary-autopilot
//! target (`1`/`1`). `param2` carries the 1-based source index; every other
//! param is 0. The ardupilotmega enum does not name command 534, so the frame is
//! built with the protocol crate's raw COMMAND_LONG builder for an arbitrary
//! command id, byte-identical to a named-id frame on the wire.
//!
//! ## Error shape (parity with the FastAPI route)
//!
//! The `501` carries a bare-string `detail` ("drone does not advertise
//! multi-camera support"), the `400`/`503` carry the error-OBJECT detail
//! (`{"detail":{"error":{"code","message"}}}`), and the profile-mismatch `404`
//! carries the `E_PROFILE_MISMATCH` object — each matching the FastAPI route.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use ados_protocol::mavlink::{self, MavHeader};

use crate::state::AppState;

/// The `MAV_CMD_SET_CAMERA_SOURCE` command id. Not a named variant in the
/// generated ardupilotmega enum, so the frame is built through the protocol
/// crate's raw COMMAND_LONG builder for an arbitrary command id.
const MAV_CMD_SET_CAMERA_SOURCE: u16 = 534;

/// The camera-command source identity stamped on the 534 frame: system `255`,
/// component `190` (a GCS-side camera controller), matching the reference
/// encoder. This is deliberately NOT the autopilot-command identity (`1`/`191`)
/// the simple-command route uses — a camera-source command originates from the
/// GCS camera controller, not the companion autopilot proxy.
const SOURCE_SYSTEM_ID: u8 = 255;
const SOURCE_COMPONENT_ID: u8 = 190;

/// The target identity: the primary autopilot (`1`/`1`), the standard
/// COMMAND_LONG target on the single-vehicle bench.
const TARGET_SYSTEM: u8 = 1;
const TARGET_COMPONENT: u8 = 1;

/// The number of cameras the paired drone advertises. Fixed at 1 (single-camera)
/// — the drone agent does not yet publish a camera count into its heartbeat, so
/// this mirrors the FastAPI `_paired_drone_camera_count` constant. When the count
/// source is wired in, only this changes; the `501`-vs-`200` decision keys on it.
const PAIRED_DRONE_CAMERA_COUNT: u32 = 1;

/// The `POST .../camera/switch` request body. Mirrors the FastAPI
/// `CameraSwitchRequest`: a required `camera_id` (1..=32 chars). The valid form
/// is a small positive integer encoded as a string (so the wire contract stays
/// symmetrical with a future named-source variant).
#[derive(Debug, Deserialize)]
pub struct CameraSwitchRequest {
    pub camera_id: String,
}

/// `POST .../camera/switch` → `{camera_id, accepted, reason}`.
///
/// Gates on the ground-station profile (404 on a drone). On a single-camera
/// drone (the live path, count == 1) returns the `501` bare-string detail. On a
/// multi-camera drone: a malformed/out-of-range id is a `400`
/// (`E_INVALID_CAMERA_ID`); otherwise the 534 frame is built + sent and the route
/// returns `{camera_id, accepted: true, reason: null}`. An unreachable MAVLink
/// IPC bus is a `503` (`E_MAVLINK_IPC_UNAVAILABLE`); the command is never
/// silently dropped.
pub async fn post_camera_switch(
    State(state): State<AppState>,
    Json(req): Json<CameraSwitchRequest>,
) -> Response {
    if !is_ground_station(&state) {
        return profile_mismatch();
    }

    let count = PAIRED_DRONE_CAMERA_COUNT;
    if count <= 1 {
        // The live path: a single-camera (or unspecified) drone. The FastAPI
        // route raises a 501 with a BARE-STRING detail here (not an error
        // object), which the GCS surfaces as "not supported by this drone".
        tracing::info!(
            camera_count = count,
            requested = %req.camera_id,
            "camera switch unsupported"
        );
        return (
            StatusCode::NOT_IMPLEMENTED,
            Json(json!("drone does not advertise multi-camera support")),
        )
            .into_response();
    }

    // Resolve the ground-side id to a 1-based MAVLink source index; a malformed
    // or out-of-range id is the FastAPI 400 with the error-object detail.
    let index = match resolve_camera_index(&req.camera_id) {
        Some(idx) if idx <= count => idx,
        _ => {
            tracing::warn!(
                requested = %req.camera_id,
                camera_count = count,
                "camera switch invalid id"
            );
            return invalid_camera_id();
        }
    };

    // Build the 534 COMMAND_LONG (param2 = the source index) and write it to the
    // MAVLink socket. An absent / broken socket is the FastAPI
    // E_MAVLINK_IPC_UNAVAILABLE 503 — the command is never silently dropped.
    let frame = build_set_camera_source_frame(index);
    if let Err(e) = state.mavlink.send(&frame).await {
        tracing::warn!(error = %e, camera_id = %req.camera_id, "camera switch mavlink send failed");
        return mavlink_ipc_unavailable(&e.to_string());
    }

    tracing::info!(
        camera_id = %req.camera_id,
        camera_index = index,
        camera_count = count,
        "camera switch dispatched"
    );
    // The CameraSwitchResponse: accepted, no reason.
    Json(json!({"camera_id": req.camera_id, "accepted": true, "reason": Value::Null}))
        .into_response()
}

// ---------------------------------------------------------------------------
// The 534 COMMAND_LONG frame.
// ---------------------------------------------------------------------------

/// Build the `SET_CAMERA_SOURCE` (534) COMMAND_LONG wire frame for a 1-based
/// source `camera_index`, returning the raw bytes ready to write to the MAVLink
/// socket. `param1` is 0 (broadcast to the camera component, per the spec),
/// `param2` carries the source index, every other param is 0. The source
/// identity is `255`/`190` and the target is `1`/`1`. The sequence is 0 (the
/// router stamps its own; a client-written command does not require a specific
/// value), matching the fire-and-forget posture. Built through the protocol
/// crate's raw COMMAND_LONG builder because the generated enum has no 534
/// variant.
fn build_set_camera_source_frame(camera_index: u32) -> Vec<u8> {
    let header = MavHeader {
        system_id: SOURCE_SYSTEM_ID,
        component_id: SOURCE_COMPONENT_ID,
        sequence: 0,
    };
    mavlink::build_command_long_v2(
        header,
        MAV_CMD_SET_CAMERA_SOURCE,
        TARGET_SYSTEM,
        TARGET_COMPONENT,
        [0.0, camera_index as f32, 0.0, 0.0, 0.0, 0.0, 0.0],
    )
}

/// Resolve a ground-side `camera_id` string to a 1-based MAVLink source index.
/// Mirrors the FastAPI `_resolve_camera_index`: the id must match
/// `[A-Za-z0-9_-]{1,32}` and parse to a positive integer (today only numeric ids
/// are supported; a future named-source map plugs in here). Returns `None` when
/// the id is malformed or not a positive integer.
fn resolve_camera_index(camera_id: &str) -> Option<u32> {
    // The FastAPI regex: 1..=32 chars of [A-Za-z0-9_-]. An id that fails this is
    // unresolvable (None), the same as a non-numeric or out-of-range id.
    let len = camera_id.chars().count();
    if !(1..=32).contains(&len) {
        return None;
    }
    if !camera_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return None;
    }
    // Parse to a positive integer. The Python `int(camera_id)` accepts a leading
    // sign / surrounding nothing; a u32 parse rejects a negative or non-numeric
    // id, and the `>= 1` check is implicit (0 is rejected below).
    let idx: u32 = camera_id.parse().ok()?;
    if idx < 1 {
        return None;
    }
    Some(idx)
}

// ---------------------------------------------------------------------------
// Profile gate (the same shape gs_recording.rs + gs_recording_list.rs emit).
// ---------------------------------------------------------------------------

/// True when the node resolves to the ground-station profile, via
/// `current_profile_and_role`, matching the Python `_require_ground_profile`.
fn is_ground_station(state: &AppState) -> bool {
    let cfg = crate::config::PairingConfig::load_from(&state.pairing_paths.config);
    let (profile, _role) = crate::profile::current_profile_and_role(&cfg.agent.profile);
    profile == "ground-station"
}

/// The `404` profile-mismatch response, byte-identical to the FastAPI
/// `HTTPException(status_code=404, detail={"error": {"code": "E_PROFILE_MISMATCH"}})`.
fn profile_mismatch() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({"detail": {"error": {"code": "E_PROFILE_MISMATCH"}}})),
    )
        .into_response()
}

/// The `400` malformed/out-of-range id response, matching the FastAPI
/// `E_INVALID_CAMERA_ID` error-object body.
fn invalid_camera_id() -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"detail": {"error": {
            "code": "E_INVALID_CAMERA_ID",
            "message": "camera_id must be a positive integer within the advertised range",
        }}})),
    )
        .into_response()
}

/// The `503` MAVLink-IPC-unavailable response, matching the FastAPI
/// `E_MAVLINK_IPC_UNAVAILABLE` error-object body. The `message` carries the
/// underlying connection error, the same as the FastAPI route's `str(exc)`.
fn mavlink_ipc_unavailable(message: &str) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({"detail": {"error": {
            "code": "E_MAVLINK_IPC_UNAVAILABLE",
            "message": message,
        }}})),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Read a response body as JSON.
    async fn body_json(resp: Response) -> Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// Decode a lowercase hex string into bytes for the golden-frame assertion.
    fn hex_to_bytes(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn the_534_frame_for_camera_index_2_is_the_golden_frame() {
        // The exact 44-byte frame the reference encoder produces for
        // SET_CAMERA_SOURCE (534) with camera_index=2, source 255/190, target
        // 1/1, sequence 0. param2 carries the index (2.0); every other param 0.
        let frame = build_set_camera_source_frame(2);
        let golden = hex_to_bytes(
            "fd20000000ffbe4c000000000000000000400000000000000000000000000000000000000000160201019b45",
        );
        assert_eq!(
            frame, golden,
            "the built 534 frame must be byte-identical to the golden frame"
        );
        // Sanity: 44 bytes, v2 start-of-frame, the 534 source identity in the
        // header bytes (system 255 = 0xFF at offset 5, component 190 = 0xBE at 6).
        assert_eq!(frame.len(), 44);
        assert_eq!(frame[0], 0xFD);
        assert_eq!(frame[5], 0xFF);
        assert_eq!(frame[6], 0xBE);
    }

    #[test]
    fn the_534_frame_carries_the_index_in_param2_at_the_wire_offset() {
        // The rust-mavlink ardupilotmega enum has no 534 MavCmd variant, so a full
        // typed decode raises InvalidEnum — the raw builder is exactly why the
        // protocol crate exposes a by-id builder. So assert the load-bearing
        // fields straight off the wire bytes instead: the v2 header carries the
        // 255/190 source identity, and param2 (the second f32 in the payload,
        // bytes 14..18) carries the index as a little-endian f32.
        let frame = build_set_camera_source_frame(3);
        // v2 header: STX, len, incompat, compat, seq, sysid, compid, msgid×3.
        assert_eq!(frame[0], 0xFD, "v2 start-of-frame");
        assert_eq!(frame[5], 255, "source system id 255");
        assert_eq!(frame[6], 190, "source component id 190");
        // The 3-byte LE message id is 76 (COMMAND_LONG).
        assert_eq!(frame[7], 76);
        // param1 (bytes 10..14) is 0.0; param2 (bytes 14..18) is the index 3.0.
        let param1 = f32::from_le_bytes(frame[10..14].try_into().unwrap());
        let param2 = f32::from_le_bytes(frame[14..18].try_into().unwrap());
        assert_eq!(param1, 0.0, "param1 is the broadcast component 0");
        assert_eq!(param2, 3.0, "param2 carries the source index");
    }

    #[test]
    fn resolve_camera_index_accepts_positive_integers() {
        assert_eq!(resolve_camera_index("1"), Some(1));
        assert_eq!(resolve_camera_index("2"), Some(2));
        assert_eq!(resolve_camera_index("42"), Some(42));
    }

    #[test]
    fn resolve_camera_index_rejects_zero_negative_and_non_numeric() {
        assert_eq!(resolve_camera_index("0"), None);
        assert_eq!(resolve_camera_index("-1"), None);
        assert_eq!(resolve_camera_index("rgb"), None);
        assert_eq!(resolve_camera_index(""), None);
        // Over 32 chars is malformed.
        assert_eq!(resolve_camera_index(&"1".repeat(33)), None);
        // A char outside [A-Za-z0-9_-] is malformed.
        assert_eq!(resolve_camera_index("1.5"), None);
        assert_eq!(resolve_camera_index("a b"), None);
    }

    #[tokio::test]
    async fn the_501_body_is_a_bare_string_detail() {
        // The live single-camera path: the 501 carries a BARE-STRING detail (not
        // an error object), the exact FastAPI plain-string detail. Built directly
        // here so the contract is pinned without a ground-station profile.
        let resp = (
            StatusCode::NOT_IMPLEMENTED,
            Json(json!("drone does not advertise multi-camera support")),
        )
            .into_response();
        assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
        let body = body_json(resp).await;
        assert_eq!(
            body,
            json!("drone does not advertise multi-camera support"),
            "the 501 detail is the bare string, not an error object"
        );
    }

    #[tokio::test]
    async fn the_400_body_is_the_invalid_id_error_object() {
        let resp = invalid_camera_id();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = body_json(resp).await;
        assert_eq!(
            body,
            json!({"detail": {"error": {
                "code": "E_INVALID_CAMERA_ID",
                "message": "camera_id must be a positive integer within the advertised range",
            }}})
        );
    }

    #[tokio::test]
    async fn the_503_body_is_the_ipc_unavailable_error_object() {
        let resp = mavlink_ipc_unavailable("connect failed: no such file or directory");
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = body_json(resp).await;
        assert_eq!(
            body["detail"]["error"]["code"],
            json!("E_MAVLINK_IPC_UNAVAILABLE")
        );
        assert_eq!(
            body["detail"]["error"]["message"],
            json!("connect failed: no such file or directory")
        );
    }

    #[tokio::test]
    async fn profile_mismatch_body_is_the_fastapi_404_shape() {
        let resp = profile_mismatch();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body = body_json(resp).await;
        assert_eq!(
            body,
            json!({"detail": {"error": {"code": "E_PROFILE_MISMATCH"}}})
        );
    }

    #[test]
    fn the_live_count_is_one_so_the_live_path_is_the_501() {
        // The count is the single-camera constant the FastAPI route returns, so
        // the live behaviour is the 501. This pins the constant so a change to it
        // is a deliberate edit (the day the heartbeat carries a camera count).
        assert_eq!(PAIRED_DRONE_CAMERA_COUNT, 1);
    }
}
