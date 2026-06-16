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
//!   `{filename, stopped_at, duration_seconds, size_bytes}`. No active capture
//!   is a `409`.
//!
//! ## Why the recorder is held in-process here (the working write path)
//!
//! The start and stop come on separate HTTP requests, so the recorder process
//! (the ffmpeg child) must live ACROSS them — a per-request recorder would lose
//! the child between start and stop. The native front is the cross-profile
//! long-lived daemon that serves the ground-station routes, so it holds the one
//! [`GroundStationRecorder`] for the life of the process behind a `OnceLock`,
//! mirroring the Python `get_recorder()` module-level singleton the FastAPI
//! route read. The listing read ([`crate::routes::gs_recording_list`]) is
//! stateless (it scans the directory directly) and does not need this handle.
//!
//! The recorder lifecycle logic lives in the `ados-video` crate (it is the owner
//! of the media subprocesses' process-group teardown); this module is the thin
//! REST surface over `start()` / `stop()`.
//!
//! ## Error shape (parity with the FastAPI route)
//!
//! The FastAPI route raises `HTTPException(detail={"error": {"code", "message"}})`
//! on a recorder failure, so the 4xx/5xx bodies here use that error-OBJECT detail
//! shape (NOT the bare-string `{"detail"}` the rest of the front uses), and the
//! profile-mismatch 404 carries the `E_PROFILE_MISMATCH` object the sibling
//! ground-station routes serve.

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
/// same instance + `Mutex`-guarded inner state). Mirrors the Python
/// `get_recorder()` module-level singleton. Built lazily at the first start/stop
/// so a drone-profile front (which never records) never constructs one.
fn recorder() -> Arc<GroundStationRecorder> {
    static RECORDER: OnceLock<Arc<GroundStationRecorder>> = OnceLock::new();
    RECORDER
        .get_or_init(|| Arc::new(GroundStationRecorder::default_recorder()))
        .clone()
}

// ---------------------------------------------------------------------------
// Error mapping (the RecorderError code → HTTP status the FastAPI route used).
// ---------------------------------------------------------------------------

/// Map a `RecorderError` code to its HTTP status, matching the FastAPI
/// `_error_status_code`: the already/not-active codes are `409`, the
/// ffmpeg-missing / spawn-failed / dir-unwritable codes are `503`, the
/// disk-full code is `507`, and any other code degrades to `500`.
fn error_status(code: &str) -> StatusCode {
    match code {
        "E_RECORDING_ACTIVE" | "E_RECORDING_NOT_ACTIVE" => StatusCode::CONFLICT,
        "E_FFMPEG_NOT_FOUND" | "E_RECORDER_SPAWN_FAILED" | "E_RECORDING_DIR_UNWRITABLE" => {
            StatusCode::SERVICE_UNAVAILABLE
        }
        // 507 INSUFFICIENT_STORAGE is the WebDAV status the FastAPI route raises
        // for a full volume; axum has no named constant, so use the code.
        "E_DISK_FULL" => StatusCode::from_u16(507).unwrap_or(StatusCode::INSUFFICIENT_STORAGE),
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// Build the FastAPI recorder-error body: `(status, {"detail": {"error":
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

/// The `POST .../recording/start` request body. Mirrors the FastAPI
/// `RecordingStartRequest`: an optional `filename_hint` (the recorder sanitises
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
/// maps to the FastAPI status for its code (`409` already recording, `503`
/// ffmpeg-missing / unwritable-dir / spawn-failed, `507` disk-full) with the
/// error-object detail body.
pub async fn post_recording_start(
    State(state): State<AppState>,
    Json(req): Json<RecordingStartRequest>,
) -> Response {
    if !is_ground_station(&state) {
        return profile_mismatch();
    }
    match recorder().start(req.filename_hint.as_deref()).await {
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
/// size_bytes}`.
///
/// `404` `E_PROFILE_MISMATCH` off a drone-profile node. On a ground station,
/// stops the in-flight capture and returns the stop summary; no active capture
/// is the FastAPI `409` `E_RECORDING_NOT_ACTIVE` error-object body.
pub async fn post_recording_stop(State(state): State<AppState>) -> Response {
    if !is_ground_station(&state) {
        return profile_mismatch();
    }
    match recorder().stop().await {
        Ok(body) => json_ok(body),
        Err(err) => {
            tracing::warn!(code = %err.code, message = %err.message, "recording stop rejected");
            recorder_error(&err)
        }
    }
}

/// A `200` JSON body. The recorder returns the body as a `serde_json::Value`
/// already shaped exactly as the FastAPI route's dict, so this is a thin wrapper.
fn json_ok(body: Value) -> Response {
    (StatusCode::OK, Json(body)).into_response()
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

    #[test]
    fn error_status_maps_every_recorder_code() {
        // 409: the already/not-active codes.
        assert_eq!(error_status("E_RECORDING_ACTIVE"), StatusCode::CONFLICT);
        assert_eq!(error_status("E_RECORDING_NOT_ACTIVE"), StatusCode::CONFLICT);
        // 503: ffmpeg-missing / spawn-failed / unwritable-dir.
        assert_eq!(
            error_status("E_FFMPEG_NOT_FOUND"),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            error_status("E_RECORDER_SPAWN_FAILED"),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            error_status("E_RECORDING_DIR_UNWRITABLE"),
            StatusCode::SERVICE_UNAVAILABLE
        );
        // 507: disk full.
        assert_eq!(error_status("E_DISK_FULL").as_u16(), 507);
        // Anything else degrades to 500.
        assert_eq!(
            error_status("E_SOMETHING_ELSE"),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[tokio::test]
    async fn recorder_error_body_is_the_error_object_detail_shape() {
        // The 409 already-recording body the start route returns.
        let resp = recorder_error(&RecorderError {
            code: "E_RECORDING_ACTIVE".to_string(),
            message: "a recording is already in progress".to_string(),
        });
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let body = body_json(resp).await;
        assert_eq!(
            body,
            json!({"detail": {"error": {
                "code": "E_RECORDING_ACTIVE",
                "message": "a recording is already in progress",
            }}})
        );
    }

    #[tokio::test]
    async fn disk_full_error_body_is_a_507() {
        let resp = recorder_error(&RecorderError {
            code: "E_DISK_FULL".to_string(),
            message: "less than 64 MiB free on the recordings volume".to_string(),
        });
        assert_eq!(resp.status().as_u16(), 507);
        let body = body_json(resp).await;
        assert_eq!(body["detail"]["error"]["code"], json!("E_DISK_FULL"));
    }

    #[tokio::test]
    async fn ffmpeg_missing_error_body_is_a_503() {
        let resp = recorder_error(&RecorderError {
            code: "E_FFMPEG_NOT_FOUND".to_string(),
            message: "ffmpeg binary not on PATH".to_string(),
        });
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = body_json(resp).await;
        assert_eq!(
            body["detail"]["error"]["message"],
            json!("ffmpeg binary not on PATH")
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
    fn start_request_filename_hint_defaults_to_none() {
        // An empty body parses with no hint (the recorder then uses a bare
        // timestamp filename), matching the FastAPI optional field.
        let req: RecordingStartRequest = serde_json::from_str("{}").unwrap();
        assert_eq!(req.filename_hint, None);
        let req2: RecordingStartRequest =
            serde_json::from_str(r#"{"filename_hint":"pipe-run"}"#).unwrap();
        assert_eq!(req2.filename_hint.as_deref(), Some("pipe-run"));
    }

    #[test]
    fn the_singleton_returns_the_same_instance() {
        // Two calls hand back the same recorder (the Arc is cloned, not rebuilt),
        // so a start on one request and a stop on another share the same child.
        let a = recorder();
        let b = recorder();
        assert!(
            Arc::ptr_eq(&a, &b),
            "the recorder singleton must be one instance across calls"
        );
    }
}
