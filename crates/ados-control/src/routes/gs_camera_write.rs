//! Ground-station camera-source switch write route.
//!
//! Lets a paired multi-camera drone toggle between its onboard camera sources
//! from a connected GCS. The route builds one `SET_CAMERA_SOURCE` (command 534)
//! COMMAND_LONG frame and writes it to the local MAVLink IPC socket, which the
//! router forwards to the FC over the radio link (fire-and-forget; the FC's
//! COMMAND_ACK lands on the MAVLink WS bridge, not here).
//!
//! - **`POST /api/v1/ground-station/camera/switch`** — `{camera_id}`.
//!   Returns `{camera_id, sent, accepted: null, reason}` on a multi-camera drone
//!   (the ACK is not observed here, so acceptance is unknown); a
//!   single-camera drone, or one whose camera count this station cannot
//!   establish, is a `501` naming which of those it is; a malformed/out-of-range
//!   id is a `400`; an unreachable MAVLink IPC bus is a `503`.
//!
//! ## Where the camera count comes from
//!
//! The radio carries no IP, so a ground station cannot ask the drone it relays
//! anything. The drone therefore PUSHES its video-leg count up the auxiliary
//! lane on its status frame, and the groundlink daemon lands it in the
//! relayed-status sidecar; this route reads it from there.
//!
//! It used to be a hardcoded `1`, which made the route answer `501` on every
//! request ever made to it — including from a drone flying an optical pod with
//! two concurrent legs, which was told its second camera did not exist. The
//! three outcomes are now distinguished, because "this drone has one camera"
//! and "this station does not know how many cameras that drone has" call for
//! different things from the operator:
//!
//! * a relayed drone reporting more than one leg → the switch is dispatched;
//! * a relayed drone reporting exactly one leg → `501`, single-camera;
//! * nothing relayed, no count reported, or more than one drone relayed (the
//!   request names none, so there is no way to tell which one it means) →
//!   `501` saying the count is unknown and why.
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
//! ## Error shape
//!
//! The `501` carries a bare-string `detail` ("drone does not advertise
//! multi-camera support"), the `400`/`503` carry the error-OBJECT detail
//! (`{"detail":{"error":{"code","message"}}}`), and the profile-mismatch `404`
//! carries the `E_PROFILE_MISMATCH` object.

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

/// How stale the relayed-status sidecar may be before its contents stop counting
/// as current.
///
/// The groundlink daemon rewrites it every [`ados_groundlink::aux_peers::PERSIST_CADENCE`]
/// (2 s). A file that has stopped being rewritten means that daemon is gone, and
/// its last contents describe a fleet that may no longer be there — so past this
/// window the count reads as unknown rather than as its last value.
const RELAYED_STATUS_STALE_S: f64 = 20.0;

/// What this ground station can say about the camera count of the drone it
/// relays.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CameraCount {
    /// One relayed drone reported how many video legs it serves.
    Known(u32),
    /// The count could not be established, for the stated reason. Carried as
    /// text because the operator's next move differs per reason and a bare
    /// "unsupported" hides which one it was.
    Unknown(&'static str),
}

/// The `POST .../camera/switch` request body. A required `camera_id` (1..=32 chars).
/// The valid form is a small positive integer encoded as a string (so the wire contract
/// stays symmetrical with a future named-source variant).
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

    let count = match resolve_camera_count() {
        CameraCount::Known(n) if n > 1 => n,
        CameraCount::Known(n) => {
            // A drone that told us it serves one leg. This is a real answer, not
            // a default, so it keeps the historical wording.
            tracing::info!(
                camera_count = n,
                requested = %req.camera_id,
                "camera switch unsupported"
            );
            return unsupported("drone does not advertise multi-camera support");
        }
        CameraCount::Unknown(why) => {
            // Not the same thing as a single-camera drone: nobody said. Say so,
            // rather than reporting the drone as having one camera.
            tracing::info!(
                reason = why,
                requested = %req.camera_id,
                "camera switch camera count unknown"
            );
            return unsupported(why);
        }
    };

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
    let frame = build_set_camera_source_frame(index, crate::fc_target::fc_target(&state));
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
    // The frame reached the MAVLink socket; the autopilot's COMMAND_ACK is not
    // observed on this path, so acceptance is unknown rather than claimed.
    Json(json!({
        "camera_id": req.camera_id,
        "sent": true,
        "accepted": Value::Null,
        "reason": "sent to the flight controller; its acknowledgement is not observed",
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// The relayed drone's camera count.
// ---------------------------------------------------------------------------

/// The relayed-status sidecar the groundlink daemon writes
/// (`/run/ados/relayed-status.json`), honouring `ADOS_RUN_DIR`.
fn relayed_status_path() -> std::path::PathBuf {
    std::path::PathBuf::from(
        std::env::var("ADOS_RUN_DIR").unwrap_or_else(|_| "/run/ados".to_string()),
    )
    .join(ados_groundlink::aux_peers::AUX_PEERS_SIDECAR)
}

/// Resolve the camera count of the drone this station relays.
fn resolve_camera_count() -> CameraCount {
    let Ok(text) = std::fs::read_to_string(relayed_status_path()) else {
        return CameraCount::Unknown(
            "no drone is relayed over the radio link, so its camera count is unknown",
        );
    };
    camera_count_from_sidecar(&text, now_unix())
}

/// The pure core: resolve the count from a relayed-status document at a given
/// wall clock. Split out so every branch is testable without a live daemon.
fn camera_count_from_sidecar(text: &str, now: f64) -> CameraCount {
    let Ok(doc) = serde_json::from_str::<Value>(text) else {
        return CameraCount::Unknown(
            "the relayed-drone status file could not be read, so the camera count is unknown",
        );
    };
    // A file nobody is rewriting describes a fleet that may no longer be there.
    let written = doc
        .get("wall_time_unix")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    if written <= 0.0 || now - written > RELAYED_STATUS_STALE_S {
        return CameraCount::Unknown(
            "the relayed-drone status is stale, so the camera count is unknown",
        );
    }
    let peers: &[Value] = doc
        .get("peers")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    // The guard counts drones, not reports: `peers_payload` keeps a peer whose
    // status went stale but drops its status block, so counting reports alone
    // would read two relayed drones as one whenever one of them is quiet.
    if peers.len() > 1 {
        return CameraCount::Unknown(
            "more than one drone is relayed and this request names none, so the camera count is unknown",
        );
    }
    let counts: Vec<u32> = peers
        .iter()
        .filter_map(|p| {
            // `peers_payload` omits the status block entirely once it
            // goes stale, so a count found here is a current one.
            p.get("status")?
                .get("video_stream_count")?
                .as_u64()
                .and_then(|n| u32::try_from(n).ok())
        })
        .collect();

    match counts.len() {
        0 => CameraCount::Unknown(
            "the relayed drone has not reported how many cameras it serves, so the camera count is unknown",
        ),
        // At most one peer is listed, so there is at most one count.
        _ => CameraCount::Known(counts[0]),
    }
}

fn now_unix() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// The `501`, carrying a bare-string detail (not an error object) — the shape
/// the GCS renders verbatim as the reason the switch is unavailable.
fn unsupported(reason: &str) -> Response {
    (StatusCode::NOT_IMPLEMENTED, Json(json!(reason))).into_response()
}

// ---------------------------------------------------------------------------
// The 534 COMMAND_LONG frame.
// ---------------------------------------------------------------------------

/// Build the `SET_CAMERA_SOURCE` (534) COMMAND_LONG wire frame for a 1-based
/// source `camera_index`, returning the raw bytes ready to write to the MAVLink
/// socket. `param1` is 0 (broadcast to the camera component, per the spec),
/// `param2` carries the source index, every other param is 0. The source
/// identity is `255`/`190` and the target is the FC `(system, component)` the
/// request resolved. The sequence is 0 (the router stamps its own; a
/// client-written command does not require a specific value), matching the
/// fire-and-forget posture. Built through the protocol crate's raw COMMAND_LONG
/// builder because the generated enum has no 534 variant.
fn build_set_camera_source_frame(
    camera_index: u32,
    (target_system, target_component): (u8, u8),
) -> Vec<u8> {
    let header = MavHeader {
        system_id: SOURCE_SYSTEM_ID,
        component_id: SOURCE_COMPONENT_ID,
        sequence: 0,
    };
    mavlink::build_command_long_v2(
        header,
        MAV_CMD_SET_CAMERA_SOURCE,
        target_system,
        target_component,
        [0.0, camera_index as f32, 0.0, 0.0, 0.0, 0.0, 0.0],
    )
}

/// Resolve a ground-side `camera_id` string to a 1-based MAVLink source index. Returns
/// `None` when the id is malformed or not a positive integer.
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
/// underlying connection error.
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
mod tests;
