//! Ground-station video recording write routes (start / stop).
//!
//! Two operator on-demand writes that drive the server-side recorder on the
//! ground node: it taps the local mediamtx RTSP source and remuxes the live
//! drone video stream to MP4 on disk. The listing read lives in
//! [`crate::routes::gs_recording_list`]; this module owns the lifecycle writes.
//!
//! - **`POST /api/v1/ground-station/recording/start`** — `{filename_hint?}`.
//!   Spawns the recorder ffmpeg and returns `{filename, started_at, path}`. A
//!   capture already in flight is a `409`; a missing ffmpeg / unwritable
//!   recordings dir / spawn failure is a `503`; a full volume is a `507`.
//! - **`POST /api/v1/ground-station/recording/stop`** — stops the in-flight
//!   capture (`SIGTERM` → wait → `SIGKILL`) and returns
//!   `{filename, stopped_at, duration_seconds, size_bytes, segments,
//!   interrupted_count}`. A capture whose ffmpeg exited while wanted (an RTSP
//!   EOF on a degrading link) continued in a new segment file; `segments`
//!   lists every file in order and `interrupted_count` how often that
//!   happened. No active capture is a `409`.
//! - **`POST /api/video/record/{start,stop}`** — the same two legs on any
//!   profile, with no ground-station gate: the recorder taps this node's own
//!   mediamtx `main` path, which on a drone is its own camera.
//!
//! ## Why the recorder is held in-process here (the working write path)
//!
//! The start and stop come on separate HTTP requests, so the recorder process
//! (the ffmpeg child) must live ACROSS them — a per-request recorder would lose
//! the child between start and stop. The native front is the cross-profile
//! long-lived daemon that serves the ground-station routes, so it holds the one
//! [`GroundStationRecorder`] for the life of the process behind a `OnceLock`.
//! Both READ surfaces — the `/status` recording legs and the `/recording/list`
//! envelope — derive their live recording state from this same
//! handle through `recording_view`, so there is one answer to "is a capture in
//! flight" rather than one per route.
//!
//! The recorder lifecycle logic lives in the `ados-video` crate (it is the owner
//! of the media subprocesses' process-group teardown); this module is the thin
//! REST surface over `start()` / `stop()`.
//!
//! ## Error shape
//!
//! A recorder failure answers `{"detail": {"error": {"code", "message"}}}`, so
//! the 4xx/5xx bodies here use that error-OBJECT detail shape (NOT the
//! bare-string `{"detail"}` the rest of the front uses), and the profile-mismatch
//! 404 carries the `E_PROFILE_MISMATCH` object the sibling ground-station routes
//! serve.

use std::sync::{Arc, OnceLock};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use ados_video::recorder::{GroundStationRecorder, RecorderError};

use crate::state::AppState;

// ---------------------------------------------------------------------------
// Profile gate (the same shape gs_recording_list.rs + gs_network_write.rs emit).
// ---------------------------------------------------------------------------

/// True when the node resolves to the ground-station profile, via
/// `current_profile_and_role` (the same source of truth the node advertises on
/// the wire), so a `profile: auto` node that resolves to a ground station via
/// `profile.conf` passes the gate, matching the Python `_require_ground_profile`.
fn is_ground_station(state: &AppState) -> bool {
    let cfg = crate::config::PairingConfig::load_from(&state.pairing_paths.config);
    let (profile, _role) = crate::profile::current_profile_and_role(&cfg.agent.profile);
    profile == "ground-station"
}

/// The `404` profile-mismatch response, byte-identical to the FastAPI
/// `HTTPException(status_code=404, detail={"error": {"code": "E_PROFILE_MISMATCH"}})`
/// (FastAPI wraps the `detail` dict under a top-level `"detail"` key).
fn profile_mismatch() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({"detail": {"error": {"code": "E_PROFILE_MISMATCH"}}})),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// The process-wide recorder singleton.
// ---------------------------------------------------------------------------

/// The one recorder for the life of the front process. The start and stop are
/// separate HTTP requests, so the ffmpeg child must outlive a single request;
/// the front holds the recorder here (behind an `Arc` so each handler shares the
/// same instance + `Mutex`-guarded inner state). Built lazily at the first
/// start/stop/status read so a drone-profile front (which never records) never
/// constructs one.
///
/// `pub(crate)` because the ground-station status route reports the recorder's
/// live state and must read THIS instance: it previously hardcoded `false` on the
/// stated grounds that the front had no in-process recorder, which this module
/// contradicts, and an operator recording through the cockpit saw the status
/// surface deny it.
pub(crate) fn recorder() -> Arc<GroundStationRecorder> {
    static RECORDER: OnceLock<Arc<GroundStationRecorder>> = OnceLock::new();
    RECORDER
        .get_or_init(|| Arc::new(GroundStationRecorder::default_recorder()))
        .clone()
}

/// The `(recording, current_filename)` pair BOTH ground-station surfaces report:
/// the `/status` recording legs and the `/recording/list` envelope, derived from
/// the one live recorder this module owns.
///
/// ONE derivation, deliberately. The listing route hardcoded `recording: false`
/// on the stated grounds that the front has no in-process recorder — the same
/// false justification the status route once carried — so the two surfaces
/// answered differently about the same capture and one of them was known-false.
/// A second copy of this logic would be the same defect, so both
/// routes call THIS.
///
/// Takes the recorder rather than reading the `OnceLock` itself, so the
/// derivation is exercised without `AppState` or a process-wide singleton: each
/// handler's only remaining decision is *which* recorder to ask, and that must be
/// the one [`recorder`] holds.
///
/// The filename is reported ONLY while a capture is in flight. The recorder keeps
/// `current_path` after a stop (the stop reply needs it), so forwarding it
/// unconditionally would render the last completed capture as if it were still
/// running — the same class of frozen reading an honest surface must never show.
pub(crate) async fn recording_view(recorder: &GroundStationRecorder) -> (bool, Value) {
    if !recorder.is_active().await {
        return (false, Value::Null);
    }
    let filename = recorder
        .current_filename()
        .await
        .map(Value::from)
        .unwrap_or(Value::Null);
    (true, filename)
}

// ---------------------------------------------------------------------------
// Error mapping (the RecorderError code → HTTP status).
// ---------------------------------------------------------------------------

/// Map a `RecorderError` code to its HTTP status: the already/not-active codes
/// are `409`, the ffmpeg-missing / spawn-failed / dir-unwritable codes are `503`,
/// the disk-full code is `507`, and any other code degrades to `500`.
fn error_status(code: &str) -> StatusCode {
    match code {
        "E_RECORDING_ACTIVE" | "E_RECORDING_NOT_ACTIVE" => StatusCode::CONFLICT,
        "E_FFMPEG_NOT_FOUND" | "E_RECORDER_SPAWN_FAILED" | "E_RECORDING_DIR_UNWRITABLE" => {
            StatusCode::SERVICE_UNAVAILABLE
        }
        // 507 INSUFFICIENT_STORAGE is the WebDAV status for a full volume; axum
        // has no named constant, so use the code.
        "E_DISK_FULL" => StatusCode::from_u16(507).unwrap_or(StatusCode::INSUFFICIENT_STORAGE),
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// Build the recorder-error body: `(status, {"detail": {"error":
/// {"code", "message"}}})`. The recorder routes raise an error whose `detail` is
/// an error OBJECT (not the bare-string `detail`), so this reproduces that exact
/// nested shape with the recorder's own code + message.
fn recorder_error(err: &RecorderError) -> Response {
    (
        error_status(&err.code),
        Json(json!({"detail": {"error": {"code": err.code, "message": err.message}}})),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// POST /api/v1/ground-station/recording/start
// ---------------------------------------------------------------------------

/// The `POST .../recording/start` request body: an optional `filename_hint` (the recorder sanitises
/// + truncates it). Absent → `None`, recording with a bare timestamp name.
#[derive(Debug, Deserialize)]
pub struct RecordingStartRequest {
    #[serde(default)]
    pub filename_hint: Option<String>,
}

/// `POST .../recording/start` → `{filename, started_at, path}`.
///
/// `404` `E_PROFILE_MISMATCH` off a drone-profile node. On a ground station,
/// spawns the recorder ffmpeg and returns the start summary; a recorder failure
/// maps to the status for its code (`409` already recording, `503`
/// ffmpeg-missing / unwritable-dir / spawn-failed, `507` disk-full) with the
/// error-object detail body.
pub async fn post_recording_start(
    State(state): State<AppState>,
    req: Option<Json<RecordingStartRequest>>,
) -> Response {
    if !is_ground_station(&state) {
        return profile_mismatch();
    }
    start_recording(req).await
}

/// The start leg both recording surfaces share. The body is optional: a bare
/// `POST` (no `filename_hint`) records under a timestamp name.
async fn start_recording(req: Option<Json<RecordingStartRequest>>) -> Response {
    let hint = req.and_then(|Json(r)| r.filename_hint);
    match recorder().start(hint.as_deref()).await {
        Ok(body) => json_ok(body),
        Err(err) => {
            tracing::warn!(code = %err.code, message = %err.message, "recording start rejected");
            recorder_error(&err)
        }
    }
}

// ---------------------------------------------------------------------------
// POST /api/v1/ground-station/recording/stop
// ---------------------------------------------------------------------------

/// `POST .../recording/stop` → `{filename, stopped_at, duration_seconds,
/// size_bytes, segments, interrupted_count}`.
///
/// `404` `E_PROFILE_MISMATCH` off a drone-profile node. On a ground station,
/// stops the in-flight capture and returns the stop summary; no active capture
/// is the `409` `E_RECORDING_NOT_ACTIVE` error-object body.
pub async fn post_recording_stop(State(state): State<AppState>) -> Response {
    if !is_ground_station(&state) {
        return profile_mismatch();
    }
    stop_recording().await
}

/// The stop leg both recording surfaces share.
async fn stop_recording() -> Response {
    match recorder().stop().await {
        Ok(body) => json_ok(body),
        Err(err) => {
            tracing::warn!(code = %err.code, message = %err.message, "recording stop rejected");
            recorder_error(&err)
        }
    }
}

// ---------------------------------------------------------------------------
// POST /api/video/record/{start,stop} — the node-local recorder on any profile.
// ---------------------------------------------------------------------------

/// `POST /api/video/record/start` → `{filename, started_at, path}`.
///
/// The same recorder the ground-station routes drive, on any profile: it taps
/// this node's own mediamtx `main` path, which on a drone is its camera and on a
/// ground station the relayed drone. Same error mapping as the ground-station
/// start. The body is optional (`{filename_hint}`).
pub async fn post_video_record_start(req: Option<Json<RecordingStartRequest>>) -> Response {
    start_recording(req).await
}

/// `POST /api/video/record/stop` → `{filename, stopped_at, duration_seconds,
/// size_bytes, segments, interrupted_count}`; `409 E_RECORDING_NOT_ACTIVE`
/// when nothing is recording.
pub async fn post_video_record_stop() -> Response {
    stop_recording().await
}

/// A `200` JSON body. The recorder returns the body as a `serde_json::Value`
/// already in its wire shape, so this is a thin wrapper.
fn json_ok(body: Value) -> Response {
    (StatusCode::OK, Json(body)).into_response()
}

#[cfg(test)]
mod tests;
