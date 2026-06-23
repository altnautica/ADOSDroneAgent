//! The vision designate route: operator click-to-follow.
//!
//! `POST /api/vision/designate` locks the vision engine's single-object tracker
//! for a camera onto a specific box (the box the operator clicked in the GCS),
//! overriding the auto-lock. The route builds a `vision.designate_track` request
//! and runs it against `/run/ados/vision.sock` through [`VisionIpcClient`]; an
//! absent socket (vision disabled, or the engine not up) maps to a 503, the same
//! shape the command route uses for a missing FC link, so a designate is never
//! silently dropped.
//!
//! Auth: this is a write, so it sits outside the public set and the LAN edge
//! requires the pairing key when paired (the same posture as `/api/command`).

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use rmpv::Value;
use serde::Deserialize;
use serde_json::json;

use crate::ipc::vision_client::VisionError;
use crate::ipc::VisionIpcClient;
use crate::routes::detail;
use crate::state::AppState;

/// A pixel-space box the operator picked (origin top-left, in the frame's own
/// resolution), matching `ados_protocol::framebus::BoundingBox`.
#[derive(Debug, Deserialize)]
pub struct BboxBody {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// The `POST /api/vision/designate` body.
#[derive(Debug, Deserialize)]
pub struct DesignateBody {
    /// The camera whose tracker to lock.
    pub camera_id: String,
    /// The box to lock onto.
    pub bbox: BboxBody,
    /// Optional label/confidence carried through to the seeded detection. The
    /// operator's pick overrides the auto-lock regardless of confidence.
    #[serde(default)]
    pub class_label: Option<String>,
    #[serde(default)]
    pub confidence: Option<f32>,
}

fn mv(s: &str) -> Value {
    Value::from(s)
}

fn f32v(v: f32) -> Value {
    Value::F32(v)
}

/// `POST /api/vision/designate` — lock the camera's follow target onto a box.
pub async fn designate(State(_state): State<AppState>, Json(req): Json<DesignateBody>) -> Response {
    let mut args = vec![
        (mv("camera_id"), mv(&req.camera_id)),
        (
            mv("bbox"),
            Value::Map(vec![
                (mv("x"), f32v(req.bbox.x)),
                (mv("y"), f32v(req.bbox.y)),
                (mv("width"), f32v(req.bbox.width)),
                (mv("height"), f32v(req.bbox.height)),
            ]),
        ),
    ];
    if let Some(label) = &req.class_label {
        args.push((mv("class_label"), mv(label)));
    }
    if let Some(conf) = req.confidence {
        args.push((mv("confidence"), f32v(conf)));
    }

    let client = VisionIpcClient::default_socket();
    match client.designate_track(Value::Map(args)).await {
        Ok(resp) => {
            // The engine returns {designated, track_id, camera_id}. Project it
            // into the FastAPI-shaped JSON the GCS reads.
            let map = resp.as_map().map(|m| m.to_vec()).unwrap_or_default();
            let get = |key: &str| {
                map.iter()
                    .find(|(k, _)| k.as_str() == Some(key))
                    .map(|(_, v)| v)
            };
            let designated = get("designated").and_then(|v| v.as_bool()).unwrap_or(false);
            let track_id = get("track_id").and_then(|v| v.as_u64());
            (
                StatusCode::OK,
                Json(json!({
                    "designated": designated,
                    "track_id": track_id,
                    "camera_id": req.camera_id,
                })),
            )
                .into_response()
        }
        // A bad request (the engine rejected it) is a 400; an unreachable engine
        // (vision off / not up) is a 503 — a designate is never silently dropped.
        Err(VisionError::Rpc(msg)) => detail(StatusCode::BAD_REQUEST, msg),
        Err(e) => detail(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("vision engine unavailable: {e}"),
        ),
    }
}
