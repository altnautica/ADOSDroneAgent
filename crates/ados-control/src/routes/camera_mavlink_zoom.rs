//! `POST /api/v1/camera/mavlink/zoom`: the zoom types, the request, and the
//! deadman that stops a moving zoom whose caller went quiet.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use ados_protocol::mavlink::ardupilotmega::{MavCmd, COMMAND_LONG_DATA};

use super::{camera_command, resolve, send, Needs, ZOOM_TYPE_CONTINUOUS, ZOOM_TYPE_RANGE};
use crate::routes::detail;
use crate::state::AppState;

/// How a zoom `value` is read. No step: ArduPilot's camera library refuses it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ZoomType {
    /// A zoom rate until stopped: -1..1, 0 stops.
    Continuous,
    /// A position: 0..100 percent of the camera's zoom range.
    Range,
}

/// The `POST /api/v1/camera/mavlink/zoom` body.
#[derive(Debug, Deserialize)]
pub struct ZoomRequest {
    pub component_id: u8,
    #[serde(rename = "type")]
    pub kind: ZoomType,
    pub value: f32,
    /// Keep the moving zoom of `component_id` alive; nothing is sent.
    #[serde(default)]
    pub keepalive: bool,
}

/// The `(type, value)` params of a zoom, or why the value is refused.
fn zoom_params(kind: ZoomType, value: f32) -> Result<(f32, f32), String> {
    let (zoom_type, lo, hi) = match kind {
        ZoomType::Continuous => (ZOOM_TYPE_CONTINUOUS, -1.0, 1.0),
        ZoomType::Range => (ZOOM_TYPE_RANGE, 0.0, 100.0),
    };
    if !value.is_finite() || value < lo || value > hi {
        return Err(format!("zoom value must be within {lo}..{hi}"));
    }
    Ok((zoom_type, value))
}

/// `MAV_CMD_SET_CAMERA_ZOOM`: the type, the value, every camera of the
/// addressed component (id 0).
fn zoom_command(target: (u8, u8), zoom_type: f32, value: f32) -> COMMAND_LONG_DATA {
    camera_command(
        target,
        MavCmd::MAV_CMD_SET_CAMERA_ZOOM,
        [zoom_type, value, 0.0, 0.0, 0.0, 0.0, 0.0],
    )
}

/// Stop `component`'s zoom with rate 0 once `generation` has gone the
/// deadman's span with no newer zoom or keepalive.
fn arm_zoom_deadman(state: &AppState, component: u8, generation: u64) {
    let state = state.clone();
    tokio::spawn(async move {
        let registry = &state.mavlink_cameras;
        tokio::time::sleep(registry.timing.zoom_deadman).await;
        let Some(system) = registry.expire_zoom(component, generation) else {
            return;
        };
        tracing::warn!(component, "camera zoom kept no keepalive; stopping it");
        let stop = zoom_command((system, component), ZOOM_TYPE_CONTINUOUS, 0.0);
        let _ = send(&state, stop, true).await;
    });
}

/// `POST /api/v1/camera/mavlink/zoom`: zoom the camera, or keep its moving
/// zoom alive.
pub async fn post_zoom(State(state): State<AppState>, Json(req): Json<ZoomRequest>) -> Response {
    let (zoom_type, value) = match zoom_params(req.kind, req.value) {
        Ok(p) => p,
        Err(why) => return detail(StatusCode::BAD_REQUEST, why),
    };
    if req.keepalive {
        return match state.mavlink_cameras.keep_zoom(req.component_id) {
            Some(generation) => {
                arm_zoom_deadman(&state, req.component_id, generation);
                Json(json!({ "ok": true, "result": "KEEPALIVE", "component_id": req.component_id }))
                    .into_response()
            }
            None => detail(
                StatusCode::CONFLICT,
                format!("no zoom is moving at component {}", req.component_id),
            ),
        };
    }
    let system = match resolve(&state, req.component_id, Needs::Zoom).await {
        Ok(system) => system,
        Err((status, why)) => return detail(status, why),
    };
    let moving = req.kind == ZoomType::Continuous && value != 0.0;
    if let Some(generation) = state
        .mavlink_cameras
        .arm_zoom(system, req.component_id, moving)
    {
        arm_zoom_deadman(&state, req.component_id, generation);
    }
    send(
        &state,
        zoom_command((system, req.component_id), zoom_type, value),
        // A rate or a position set twice is the same zoom, so a resend after
        // a lost ACK is harmless.
        true,
    )
    .await
}
