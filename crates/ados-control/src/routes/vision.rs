//! The vision write routes (operator click-to-follow and detection leases) and
//! the engine read-backs.
//!
//! `POST /api/vision/designate` locks the vision engine's single-object tracker
//! for a camera onto a specific box (the box the operator clicked in the GCS),
//! overriding the auto-lock. The route builds a `vision.designate_track` request
//! and runs it against `/run/ados/vision.sock` through [`VisionIpcClient`]; an
//! absent socket (vision disabled, or the engine not up) maps to a 503, the same
//! shape the command route uses for a missing FC link, so a designate is never
//! silently dropped.
//!
//! `POST /api/vision/demand` takes, renews or releases an operator surface's
//! lease on one camera (`vision.request_detection`): `kind` is `detection`
//! (the default), `depth` (the depth grid) or `surfaces` (the landing zones).
//! The engine runs each only on a camera some consumer holds a lease of that
//! kind on.
//!
//! Auth: these are writes, so they sit outside the public set and the LAN edge
//! requires the pairing key when paired (the same posture as `/api/command`).

use ados_protocol::framebus::{ModelInfo, ModelKind};
use ados_protocol::vision_status::{VisionStatus, VISION_STATUS_FILE};
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

/// The `POST /api/vision/demand` body.
#[derive(Debug, Deserialize)]
pub struct DemandBody {
    /// What to run: `detection` (default), `depth` or `surfaces`.
    #[serde(default)]
    pub kind: Option<String>,
    /// The camera to run it on.
    pub camera_id: String,
    /// Seconds to hold it on, `0..=60`; `0` releases this owner's lease.
    pub hold_s: f64,
    /// Who holds the lease; one owner's renew or release never touches
    /// another's. Default `operator`.
    #[serde(default)]
    pub owner: Option<String>,
}

/// The lease owner when a `POST /api/vision/demand` names none.
const DEFAULT_DEMAND_OWNER: &str = "operator";

/// `POST /api/vision/demand` — hold (or release) detection, depth or surfaces
/// on a camera.
pub async fn demand(State(_state): State<AppState>, Json(req): Json<DemandBody>) -> Response {
    demand_via(&VisionIpcClient::default_socket(), req).await
}

/// The demand route against a given engine socket. A malformed body is a 400
/// before the engine is asked; the engine's refusal (a camera it does not
/// capture) is a 400; an unreachable engine is a 503.
async fn demand_via(client: &VisionIpcClient, req: DemandBody) -> Response {
    use ados_protocol::vision_rpc::{self, DemandKind, MAX_DETECTION_HOLD_MS};

    if req.camera_id.is_empty() {
        return detail(StatusCode::BAD_REQUEST, "camera_id is required");
    }
    let kind = match req.kind.as_deref() {
        None => DemandKind::Detection,
        Some(k) => match DemandKind::parse(k) {
            Some(kind) => kind,
            None => {
                return detail(
                    StatusCode::BAD_REQUEST,
                    "kind must be detection, depth or surfaces",
                )
            }
        },
    };
    let max_s = MAX_DETECTION_HOLD_MS as f64 / 1000.0;
    if !req.hold_s.is_finite() || !(0.0..=max_s).contains(&req.hold_s) {
        return detail(
            StatusCode::BAD_REQUEST,
            format!("hold_s must be between 0 and {max_s}"),
        );
    }
    let owner = req
        .owner
        .as_deref()
        .filter(|o| !o.is_empty())
        .unwrap_or(DEFAULT_DEMAND_OWNER);
    let hold_ms = (req.hold_s * 1000.0).round() as u64;
    let args = vision_rpc::with_owner(
        &vision_rpc::request_demand_args(kind, &req.camera_id, hold_ms),
        owner,
    );
    match client.request_detection(args).await {
        Ok(resp) => match vision_rpc::decode_request_detection_reply(&resp) {
            Ok(reply) => (
                StatusCode::OK,
                Json(json!({
                    "kind": reply.kind.as_str(),
                    "camera_id": reply.camera_id,
                    "detecting": reply.detecting,
                    "until_ms": reply.until_ms,
                })),
            )
                .into_response(),
            Err(e) => detail(
                StatusCode::BAD_GATEWAY,
                format!("vision engine reply unreadable: {e}"),
            ),
        },
        Err(VisionError::Rpc(msg)) => detail(StatusCode::BAD_REQUEST, msg),
        Err(e) => detail(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("vision engine unavailable: {e}"),
        ),
    }
}

/// `GET /api/vision/status` — the engine's registered-model read-back, so the
/// GCS vision hub shows every model loaded on the drone (task, execution,
/// backend-loaded), not only the ones actively publishing detections, plus the
/// inference backend and its state: `backend_state` is `ready`, `mock` (the
/// explicitly configured mock) or `unavailable` (the requested backend cannot
/// run here; `backend_reason` says why), and the detector's demand state
/// (`detection`: mode, and per camera whether it detects and who holds it on)
/// and the depth layer's (`depth`: model state, pace, and per camera whether
/// the grid and the landing zones run, their leases and the last scale source).
/// An unreachable engine (vision off / not up) is a 503, never a silent empty.
pub async fn engine_status(State(state): State<AppState>) -> Response {
    let client = VisionIpcClient::default_socket();
    // The node NPU utilization from the logging store's hardware signals. `None`
    // when no sampler feeds it (a board with no NPU, or debugfs not readable) —
    // surfaced as `null`, never a fabricated 0.
    let npu_util = state
        .logd
        .latest_hw_signals()
        .await
        .and_then(|s| s.get("npu.load_pct").and_then(|v| v.as_f64()));
    match client.list_models().await {
        Ok(resp) => {
            let models = match decode_models(&resp) {
                Ok(m) => m,
                Err(msg) => return detail(StatusCode::BAD_GATEWAY, msg),
            };
            let sidecar = read_vision_status(&vision_status_path()).await;
            let body = status_body(&resp, models, npu_util, sidecar.as_ref());
            (StatusCode::OK, Json(body)).into_response()
        }
        Err(e) => detail(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("vision engine unavailable: {e}"),
        ),
    }
}

/// The engine's work sidecar (`vision-status.json` under the run directory).
fn vision_status_path() -> std::path::PathBuf {
    ados_protocol::paths::run_dir().join(VISION_STATUS_FILE)
}

/// Read the engine's work sidecar. `None` when it is absent or unreadable: the
/// per-camera source state is then unknown, never an error for the route.
async fn read_vision_status(path: &std::path::Path) -> Option<VisionStatus> {
    let text = tokio::fs::read_to_string(path).await.ok()?;
    serde_json::from_str(&text).ok()
}

/// The `/api/vision/status` body from the engine's `list_models` reply and its
/// work sidecar. An engine reply without a `backend_state` (none ships without
/// one) reads as `unavailable` rather than a claimed `ready`. `source_errors`
/// lists each camera whose frame source is failing (or silent) with the reason,
/// empty without a sidecar; `live_inputs`, `detection` and `depth` are null
/// without one.
/// Every key is snake_case, the form the GCS client reads.
fn status_body(
    resp: &Value,
    models: Vec<ModelInfo>,
    npu_util: Option<f64>,
    sidecar: Option<&VisionStatus>,
) -> serde_json::Value {
    let field = |key: &str| -> Option<&str> {
        resp.as_map()
            .and_then(|m| m.iter().find(|(k, _)| k.as_str() == Some(key)))
            .and_then(|(_, v)| v.as_str())
    };
    let model_count = models.len();
    let models: Vec<serde_json::Value> = models.iter().map(model_json).collect();
    json!({
        "models": models,
        "model_count": model_count,
        "npu_utilization_pct": npu_util,
        "backend": field("backend"),
        "backend_state": field("backend_state").unwrap_or("unavailable"),
        "backend_reason": field("backend_reason"),
        "source_errors": sidecar.map(|s| s.source_errors.as_slice()).unwrap_or_default(),
        "live_inputs": sidecar.map(|s| s.live_inputs),
        "detection": sidecar.and_then(|s| s.detection.as_ref()),
        "depth": sidecar.and_then(|s| s.depth.as_ref()),
    })
}

/// One registered model as the vision routes serve it, every key snake_case.
/// The engine reports 0.0 fps/latency for a model that has not run yet; that
/// is "no data", so it goes out as null rather than a measured zero.
fn model_json(m: &ModelInfo) -> serde_json::Value {
    let measured = |v: f32| (v > 0.0).then_some(v);
    json!({
        "id": m.id,
        "kind": m.kind,
        "execution": m.execution,
        "backend_loaded": m.backend_loaded,
        "output_classes": m.output_classes,
        "fps": measured(m.fps),
        "latency_ms": measured(m.latency_ms),
        "is_inference_capable": m.is_inference_capable,
    })
}

/// Decode the engine's `list_models` reply (`{models: Binary(msgpack)}`) into
/// the model set. Shared by the status + capabilities reads.
///
/// A reply that does not carry a decodable `models` list is an error, not an
/// empty set: an engine with no models and an answer nobody can read must not
/// look the same to the GCS.
fn decode_models(resp: &Value) -> Result<Vec<ModelInfo>, String> {
    let bytes = resp
        .as_map()
        .and_then(|m| m.iter().find(|(k, _)| k.as_str() == Some("models")))
        .and_then(|(_, v)| v.as_slice())
        .ok_or_else(|| "the vision engine reply carried no model list".to_string())?;
    rmp_serde::from_slice(bytes)
        .map_err(|e| format!("the vision engine model list did not decode: {e}"))
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

/// Resolve a capability from a model set: the first inference-capable model of `kind`
/// whose classes cover `class` (any of that kind when `class` is `None`).
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
            "model": resolved.map(model_json),
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
        let of_kind: Vec<serde_json::Value> = of_kind.into_iter().map(model_json).collect();
        capabilities.push(json!({
            "kind": k,
            "is_inference_capable": is_inference_capable,
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
/// inference-capable). With `?kind=detection[&class=person]` it
/// resolves a single capability and returns the matched model or
/// `{available:false}`. An unreachable engine is a 503, never a silent empty.
pub async fn engine_capabilities(
    State(_state): State<AppState>,
    Query(q): Query<CapabilitiesQuery>,
) -> Response {
    let client = VisionIpcClient::default_socket();
    let models = match client.list_models().await {
        Ok(resp) => match decode_models(&resp) {
            Ok(m) => m,
            Err(msg) => return detail(StatusCode::BAD_GATEWAY, msg),
        },
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

    #[test]
    fn a_reply_without_a_readable_model_list_is_an_error_not_an_empty_set() {
        let no_key = Value::Map(vec![(Value::from("ok"), Value::Boolean(true))]);
        assert!(decode_models(&no_key).is_err());
        let garbage = Value::Map(vec![(Value::from("models"), Value::Binary(vec![0xc1]))]);
        assert!(decode_models(&garbage).is_err());
        let empty = rmp_serde::to_vec(&Vec::<ModelInfo>::new()).unwrap();
        let ok = Value::Map(vec![(Value::from("models"), Value::Binary(empty))]);
        assert_eq!(decode_models(&ok).unwrap().len(), 0);
    }

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
    fn status_reports_the_backend_state_and_reason() {
        let reply = Value::Map(vec![
            (Value::from("backend"), Value::from("onnx")),
            (Value::from("backend_state"), Value::from("unavailable")),
            (
                Value::from("backend_reason"),
                Value::from("this build does not include the onnx inference runtime"),
            ),
        ]);
        let body = status_body(
            &reply,
            vec![mi("det", ModelKind::Detection, &[], false)],
            None,
            None,
        );
        assert_eq!(body["backend"], "onnx");
        assert_eq!(body["backend_state"], "unavailable");
        assert!(body["backend_reason"].as_str().unwrap().contains("onnx"));
        assert_eq!(body["model_count"], 1);
        assert!(body["npu_utilization_pct"].is_null());
        // Model fields are snake_case, the form the GCS client reads.
        let m = &body["models"][0];
        assert_eq!(m["id"], "det");
        assert_eq!(m["is_inference_capable"], false);
        assert!(m.get("latency_ms").is_some());
        assert!(m.get("latencyMs").is_none());

        let ready = Value::Map(vec![
            (Value::from("backend"), Value::from("rknn")),
            (Value::from("backend_state"), Value::from("ready")),
        ]);
        let body = status_body(&ready, Vec::new(), Some(42.5), None);
        assert_eq!(body["backend_state"], "ready");
        assert!(body["backend_reason"].is_null());
        assert_eq!(body["npu_utilization_pct"], 42.5);
    }

    #[test]
    fn a_reply_without_a_backend_state_is_not_claimed_ready() {
        let body = status_body(&Value::Map(vec![]), Vec::new(), None, None);
        assert_eq!(body["backend_state"], "unavailable");
        assert!(body["backend"].is_null());
    }

    #[tokio::test]
    async fn a_camera_source_error_in_the_sidecar_reaches_the_status_body() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(VISION_STATUS_FILE);
        let sidecar = r#"{"version":1,"frames_consumed":7,"live_inputs":0,
            "source_errors":[{"camera":"cam_fr","error":"delivered no frame for 10 s"}]}"#;
        std::fs::write(&path, sidecar).unwrap();
        let status = read_vision_status(&path).await;
        let body = status_body(&Value::Map(vec![]), Vec::new(), None, status.as_ref());
        assert_eq!(body["source_errors"][0]["camera"], "cam_fr");
        assert_eq!(
            body["source_errors"][0]["error"],
            "delivered no frame for 10 s"
        );
        assert_eq!(body["live_inputs"], 0);
    }

    #[tokio::test]
    async fn a_missing_sidecar_reads_as_no_source_errors() {
        let dir = tempfile::tempdir().unwrap();
        let status = read_vision_status(&dir.path().join(VISION_STATUS_FILE)).await;
        assert!(status.is_none());
        let body = status_body(&Value::Map(vec![]), Vec::new(), None, status.as_ref());
        assert_eq!(body["source_errors"], serde_json::json!([]));
        assert!(body["live_inputs"].is_null());
        assert!(body["detection"].is_null());
    }

    #[tokio::test]
    async fn the_sidecars_detection_block_reaches_the_status_body() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(VISION_STATUS_FILE);
        let sidecar = r#"{"version":1,"frames_consumed":7,"live_inputs":2,
            "detection":{"mode":"on_demand","cameras":{
              "main":{"detecting":false,"leases":[]},
              "belly":{"detecting":true,"leases":[{"owner":"operator","remaining_ms":4200}]}}}}"#;
        std::fs::write(&path, sidecar).unwrap();
        let status = read_vision_status(&path).await;
        let body = status_body(&Value::Map(vec![]), Vec::new(), None, status.as_ref());
        let det = &body["detection"];
        assert_eq!(det["mode"], "on_demand");
        assert_eq!(det["cameras"]["main"]["detecting"], false);
        assert_eq!(det["cameras"]["belly"]["detecting"], true);
        assert_eq!(det["cameras"]["belly"]["leases"][0]["owner"], "operator");
        assert_eq!(det["cameras"]["belly"]["leases"][0]["remaining_ms"], 4200);
    }

    /// A one-request engine on `path` that answers `vision.request_detection`
    /// from the request it got (detecting while the hold is non-zero), and
    /// hands the decoded request back for the test to inspect.
    fn demand_engine(
        path: &std::path::Path,
    ) -> tokio::task::JoinHandle<ados_protocol::vision_rpc::RequestDetection> {
        use ados_protocol::frame::{decode_len, HEADER_SIZE, PLUGIN_MAX_FRAME};
        use ados_protocol::plugin::{Envelope, PROTOCOL_VERSION};
        use ados_protocol::vision_rpc;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::UnixListener::bind(path).unwrap();
        tokio::spawn(async move {
            let (mut conn, _addr) = listener.accept().await.unwrap();
            let mut header = [0u8; HEADER_SIZE];
            conn.read_exact(&mut header).await.unwrap();
            let len = decode_len(header, PLUGIN_MAX_FRAME, false).unwrap();
            let mut body = vec![0u8; len];
            conn.read_exact(&mut body).await.unwrap();
            let req = Envelope::from_msgpack(&body).unwrap();
            assert_eq!(
                req.method,
                ados_protocol::framebus::methods::REQUEST_DETECTION
            );
            let decoded = vision_rpc::decode_request_detection(&req.args).unwrap();
            let held = decoded.hold_ms > 0;
            let reply = Envelope {
                version: PROTOCOL_VERSION,
                kind: "response".to_string(),
                method: req.method,
                capability: String::new(),
                args: vision_rpc::request_detection_reply(&vision_rpc::DetectionDemandReply {
                    kind: decoded.kind,
                    camera_id: decoded.camera_id.clone(),
                    detecting: held,
                    until_ms: held.then_some(1_000 + decoded.hold_ms),
                }),
                request_id: req.request_id,
                token: String::new(),
                error: None,
            };
            conn.write_all(&reply.encode_frame().unwrap())
                .await
                .unwrap();
            conn.flush().await.unwrap();
            decoded
        })
    }

    async fn body_json(resp: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 16)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn demand_body(camera_id: &str, hold_s: f64, owner: Option<&str>) -> DemandBody {
        DemandBody {
            kind: None,
            camera_id: camera_id.into(),
            hold_s,
            owner: owner.map(str::to_string),
        }
    }

    #[tokio::test]
    async fn demand_leases_the_camera_as_the_operator_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vision.sock");
        let engine = demand_engine(&path);
        let resp = demand_via(&VisionIpcClient::new(path), demand_body("belly", 6.0, None)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(
            body,
            json!({"kind": "detection", "camera_id": "belly", "detecting": true, "until_ms": 7000})
        );
        let sent = engine.await.unwrap();
        assert_eq!(sent.owner, DEFAULT_DEMAND_OWNER);
        assert_eq!(sent.hold_ms, 6_000);
    }

    #[tokio::test]
    async fn demand_with_a_zero_hold_releases_the_named_owner() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vision.sock");
        let engine = demand_engine(&path);
        let resp = demand_via(
            &VisionIpcClient::new(path),
            demand_body("main", 0.0, Some("gcs-overlay")),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["detecting"], false);
        assert!(body["until_ms"].is_null());
        let sent = engine.await.unwrap();
        assert_eq!((sent.owner.as_str(), sent.hold_ms), ("gcs-overlay", 0));
    }

    #[tokio::test]
    async fn a_depth_lease_carries_its_kind_to_the_engine_and_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vision.sock");
        let engine = demand_engine(&path);
        let body = DemandBody {
            kind: Some("surfaces".into()),
            ..demand_body("main", 6.0, None)
        };
        let resp = demand_via(&VisionIpcClient::new(path), body).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_json(resp).await["kind"], "surfaces");
        let sent = engine.await.unwrap();
        assert_eq!(sent.kind, ados_protocol::vision_rpc::DemandKind::Surfaces);
    }

    #[tokio::test]
    async fn demand_refuses_a_bad_body_and_reports_an_absent_engine() {
        let dir = tempfile::tempdir().unwrap();
        let absent = VisionIpcClient::new(dir.path().join("absent.sock"));
        for bad in [
            demand_body("", 6.0, None),
            demand_body("main", 61.0, None),
            demand_body("main", -1.0, None),
            demand_body("main", f64::NAN, None),
            DemandBody {
                kind: Some("segmentation".into()),
                ..demand_body("main", 6.0, None)
            },
        ] {
            assert_eq!(
                demand_via(&absent, bad).await.status(),
                StatusCode::BAD_REQUEST
            );
        }
        assert_eq!(
            demand_via(&absent, demand_body("main", 6.0, None))
                .await
                .status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
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
        // A non-inference-capable depth model does not resolve.
        assert!(resolve_capability(&models, ModelKind::Depth, None).is_none());
    }

    #[test]
    fn query_mode_reports_available_or_not() {
        let models = vec![mi("person-det", ModelKind::Detection, &["person"], true)];
        let body = capabilities_body(&models, Some("detection"), Some("person")).unwrap();
        assert_eq!(body["available"], json!(true));
        assert_eq!(body["kind"], json!("detection"));
        assert_eq!(body["model"]["id"], json!("person-det"));
        assert_eq!(body["model"]["is_inference_capable"], json!(true));
        assert!(body["model"].get("isInferenceCapable").is_none());

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
        assert_eq!(caps[0]["is_inference_capable"], json!(true));
        assert_eq!(caps[1]["kind"], json!("depth"));
        assert_eq!(caps[1]["is_inference_capable"], json!(false));
        // Every key snake_case, down to the grouped models.
        assert!(caps[0].get("isInferenceCapable").is_none());
        assert_eq!(caps[0]["models"][0]["output_classes"], json!(["person"]));
        assert!(caps[0]["models"][0].get("latencyMs").is_none());
    }
}
