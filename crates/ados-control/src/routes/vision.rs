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

use ados_protocol::framebus::{ModelInfo, ModelKind};
use axum::extract::{Query, State};
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

/// `GET /api/vision/status` — the engine's registered-model read-back, so the
/// GCS vision hub shows every model loaded on the drone (task, execution,
/// backend-loaded), not only the ones actively publishing detections. An
/// unreachable engine (vision off / not up) is a 503, never a silent empty.
pub async fn engine_status(State(state): State<AppState>) -> Response {
    let client = VisionIpcClient::default_socket();
    // The node NPU utilization from the logging store's hardware signals. `None`
    // when no sampler feeds it (a board with no NPU, or debugfs not readable) —
    // surfaced as `null`, never a fabricated 0 (Rule 44).
    let npu_util = state
        .logd
        .latest_hw_signals()
        .await
        .and_then(|s| s.get("npu.load_pct").and_then(|v| v.as_f64()));
    match client.list_models().await {
        Ok(resp) => {
            let models = decode_models(&resp);
            let model_count = models.len();
            (
                StatusCode::OK,
                Json(json!({
                    "models": models,
                    "modelCount": model_count,
                    "npuUtilizationPct": npu_util,
                })),
            )
                .into_response()
        }
        Err(e) => detail(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("vision engine unavailable: {e}"),
        ),
    }
}

/// Decode the engine's `list_models` reply (`{models: Binary(msgpack)}`) into
/// the model set. Shared by the status + capabilities reads.
fn decode_models(resp: &Value) -> Vec<ModelInfo> {
    let map = resp.as_map().map(|m| m.to_vec()).unwrap_or_default();
    let bytes = map
        .iter()
        .find(|(k, _)| k.as_str() == Some("models"))
        .and_then(|(_, v)| v.as_slice());
    match bytes {
        Some(b) => rmp_serde::from_slice(b).unwrap_or_default(),
        None => Vec::new(),
    }
}

/// The model kinds in a fixed display order, so the grouped read-back is
/// deterministic.
const ALL_KINDS: [ModelKind; 7] = [
    ModelKind::Detection,
    ModelKind::Segmentation,
    ModelKind::Classification,
    ModelKind::Tracking,
    ModelKind::Pose,
    ModelKind::Depth,
    ModelKind::Reid,
];

/// Parse a `?kind=` value into a [`ModelKind`] (lowercase, the wire form).
/// `None` for an unknown kind so the route can answer `400`.
fn parse_kind(s: &str) -> Option<ModelKind> {
    match s.to_ascii_lowercase().as_str() {
        "detection" => Some(ModelKind::Detection),
        "segmentation" => Some(ModelKind::Segmentation),
        "classification" => Some(ModelKind::Classification),
        "tracking" => Some(ModelKind::Tracking),
        "pose" => Some(ModelKind::Pose),
        "depth" => Some(ModelKind::Depth),
        "reid" => Some(ModelKind::Reid),
        _ => None,
    }
}

/// Resolve a capability from a model set: the first inference-capable model of
/// `kind` whose classes cover `class` (any of that kind when `class` is
/// `None`). Mirrors the engine's `resolve_capability`; the model set the engine
/// returns is id-sorted, so the first match is deterministic.
fn resolve_capability<'a>(
    models: &'a [ModelInfo],
    kind: ModelKind,
    class: Option<&str>,
) -> Option<&'a ModelInfo> {
    models.iter().find(|m| {
        m.kind == kind
            && m.is_inference_capable
            && match class {
                Some(c) => m.output_classes.iter().any(|oc| oc == c),
                None => true,
            }
    })
}

/// Build the `/api/vision/capabilities` response body from a model set + query.
/// Pure, so the route's grouping + resolution logic is unit-testable without
/// the engine socket. `Err((status, msg))` is a bad request.
fn capabilities_body(
    models: &[ModelInfo],
    kind: Option<&str>,
    class: Option<&str>,
) -> Result<serde_json::Value, (StatusCode, String)> {
    // Query mode: resolve one capability.
    if let Some(kind_str) = kind {
        let Some(k) = parse_kind(kind_str) else {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("unknown model kind: {kind_str}"),
            ));
        };
        let resolved = resolve_capability(models, k, class);
        return Ok(json!({
            "available": resolved.is_some(),
            "kind": k,
            "class": class,
            "model": resolved,
        }));
    }
    // A class without a kind is meaningless.
    if class.is_some() {
        return Err((StatusCode::BAD_REQUEST, "class requires kind".into()));
    }

    // List mode: group by task (only kinds with a registered model).
    let mut capabilities = Vec::new();
    for k in ALL_KINDS {
        let of_kind: Vec<&ModelInfo> = models.iter().filter(|m| m.kind == k).collect();
        if of_kind.is_empty() {
            continue;
        }
        let mut classes: Vec<String> = Vec::new();
        for m in &of_kind {
            for c in &m.output_classes {
                if !classes.contains(c) {
                    classes.push(c.clone());
                }
            }
        }
        classes.sort();
        let is_inference_capable = of_kind.iter().any(|m| m.is_inference_capable);
        capabilities.push(json!({
            "kind": k,
            "isInferenceCapable": is_inference_capable,
            "classes": classes,
            "models": of_kind,
        }));
    }
    Ok(json!({ "capabilities": capabilities }))
}

/// The `GET /api/vision/capabilities` query. Optional `kind` (with an optional
/// `class`) turns the read into a single-capability resolution.
#[derive(Debug, Deserialize)]
pub struct CapabilitiesQuery {
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub class: Option<String>,
}

/// `GET /api/vision/capabilities` — the perception this node actually offers.
///
/// With no query, returns the registered models grouped by task (`kind`) with
/// the union of their classes and whether any is inference-capable, so the GCS
/// or a plugin sees the node's real capabilities (a mock-backed model is not
/// inference-capable, Rule 44). With `?kind=detection[&class=person]` it
/// resolves a single capability and returns the matched model or
/// `{available:false}`. An unreachable engine is a 503, never a silent empty.
pub async fn engine_capabilities(
    State(_state): State<AppState>,
    Query(q): Query<CapabilitiesQuery>,
) -> Response {
    let client = VisionIpcClient::default_socket();
    let models = match client.list_models().await {
        Ok(resp) => decode_models(&resp),
        Err(e) => {
            return detail(
                StatusCode::SERVICE_UNAVAILABLE,
                format!("vision engine unavailable: {e}"),
            )
        }
    };
    match capabilities_body(&models, q.kind.as_deref(), q.class.as_deref()) {
        Ok(body) => (StatusCode::OK, Json(body)).into_response(),
        Err((status, msg)) => detail(status, msg),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_protocol::framebus::ModelExecution;

    fn mi(id: &str, kind: ModelKind, classes: &[&str], capable: bool) -> ModelInfo {
        ModelInfo {
            id: id.into(),
            kind,
            execution: ModelExecution::EngineRun,
            backend_loaded: true,
            output_classes: classes.iter().map(|s| s.to_string()).collect(),
            fps: 0.0,
            latency_ms: 0.0,
            is_inference_capable: capable,
        }
    }

    #[test]
    fn parse_kind_reads_the_wire_form() {
        assert_eq!(parse_kind("detection"), Some(ModelKind::Detection));
        assert_eq!(parse_kind("REID"), Some(ModelKind::Reid));
        assert!(parse_kind("bogus").is_none());
    }

    #[test]
    fn resolve_matches_kind_class_and_capability() {
        let models = vec![
            mi("person-det", ModelKind::Detection, &["person", "car"], true),
            mi("depth-mock", ModelKind::Depth, &["_"], false),
        ];
        assert_eq!(
            resolve_capability(&models, ModelKind::Detection, Some("person"))
                .map(|m| m.id.as_str()),
            Some("person-det")
        );
        assert!(resolve_capability(&models, ModelKind::Detection, None).is_some());
        // An unlisted class does not resolve.
        assert!(resolve_capability(&models, ModelKind::Detection, Some("boat")).is_none());
        // A non-inference-capable depth model does not resolve (Rule 44).
        assert!(resolve_capability(&models, ModelKind::Depth, None).is_none());
    }

    #[test]
    fn query_mode_reports_available_or_not() {
        let models = vec![mi("person-det", ModelKind::Detection, &["person"], true)];
        let body = capabilities_body(&models, Some("detection"), Some("person")).unwrap();
        assert_eq!(body["available"], json!(true));
        assert_eq!(body["kind"], json!("detection"));
        assert_eq!(body["model"]["id"], json!("person-det"));

        // A missing capability is honestly unavailable, not an error.
        let none = capabilities_body(&models, Some("depth"), None).unwrap();
        assert_eq!(none["available"], json!(false));
        assert_eq!(none["model"], json!(null));
    }

    #[test]
    fn query_mode_rejects_an_unknown_kind_and_a_lone_class() {
        let models: Vec<ModelInfo> = vec![];
        assert_eq!(
            capabilities_body(&models, Some("bogus"), None)
                .unwrap_err()
                .0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            capabilities_body(&models, None, Some("person"))
                .unwrap_err()
                .0,
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn list_mode_groups_by_kind_with_class_union_and_capability() {
        let models = vec![
            mi("det-a", ModelKind::Detection, &["person"], true),
            mi("det-b", ModelKind::Detection, &["car"], false),
            mi("depth", ModelKind::Depth, &["_"], false),
        ];
        let body = capabilities_body(&models, None, None).unwrap();
        let caps = body["capabilities"].as_array().unwrap();
        // Two kinds present; detection first by the fixed order.
        assert_eq!(caps.len(), 2);
        assert_eq!(caps[0]["kind"], json!("detection"));
        // Class union across both detectors, sorted.
        assert_eq!(caps[0]["classes"], json!(["car", "person"]));
        // Any inference-capable detector makes the kind capable.
        assert_eq!(caps[0]["isInferenceCapable"], json!(true));
        assert_eq!(caps[1]["kind"], json!("depth"));
        assert_eq!(caps[1]["isInferenceCapable"], json!(false));
    }
}
