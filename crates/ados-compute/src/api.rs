//! The compute node's REST job API (native Rust, axum over tokio). A drone or
//! GCS submits reconstruction and offload jobs here and reads their status and
//! results. Handlers lock the engine (a single-writer SQLite store) briefly per
//! request. This is the local-first control surface, gated by the pairing
//! posture (see [`crate::auth`]): unpaired ⇒ open, paired + on-box ⇒ open,
//! paired + off-box ⇒ `X-ADOS-Key`, with an off-box rate limiter. mDNS discovery
//! wraps it later.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::artifacts::rewrite_artifact_host;
use crate::auth::{require_pairing, ComputeAuth};
use crate::{ComputeError, ComputeJobKind, ComputeJobState, Dataset, Engine, JobRecord};

/// Shared engine handle. One mutex serializes access to the single-writer store,
/// shared by the API handlers and the worker loop.
pub type ApiState = Arc<Mutex<Engine>>;

/// Build the job-API router over a shared engine, gated by the pairing posture.
///
/// Every route passes through [`require_pairing`]: unpaired ⇒ open, paired +
/// on-box ⇒ open, paired + off-box ⇒ `X-ADOS-Key` required, with an off-box rate
/// limiter. The peer address the gate reads comes from `ConnectInfo`, so the
/// daemon serves the router with `into_make_service_with_connect_info::<SocketAddr>()`.
/// Build the router with a default public base (loopback). The daemon uses
/// [`build_router_with_base`] with its live base; this shorthand is for callers
/// (the on-box control-front mount, tests) where artifact-host rewriting is a
/// no-op on the URLs they exercise.
pub fn build_router(state: ApiState, auth: Arc<ComputeAuth>) -> Router {
    build_router_with_base(state, auth, Arc::from("http://127.0.0.1:8092"))
}

pub fn build_router_with_base(
    state: ApiState,
    auth: Arc<ComputeAuth>,
    public_base: Arc<str>,
) -> Router {
    Router::new()
        .route("/api/compute/status", get(status))
        .route("/api/compute/datasets", post(create_dataset))
        .route("/api/compute/jobs", get(list_jobs).post(submit_job))
        .route("/api/compute/jobs/:id", get(job_status))
        .route("/api/compute/jobs/:id/cancel", post(cancel_job))
        .route("/api/compute/jobs/:id/outputs", get(job_outputs))
        .layer(axum::middleware::from_fn_with_state(auth, require_pairing))
        // The live public base rewrites each stored artifact URL's host on read,
        // so a URL frozen at an earlier (drifting) hostname stays reachable.
        .layer(Extension(public_base))
        .with_state(state)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn next_id(prefix: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{nanos}-{n}")
}

// A ComputeError renders as a JSON error with a fitting status. NotFound is a
// 404, a wrong kind is a 400, everything else is a 500.
impl IntoResponse for ComputeError {
    fn into_response(self) -> Response {
        let status = match self {
            ComputeError::NotFound(_) => StatusCode::NOT_FOUND,
            ComputeError::Conflict(_) => StatusCode::CONFLICT,
            ComputeError::WrongKind(_) => StatusCode::BAD_REQUEST,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let body = Json(serde_json::json!({ "error": self.to_string() }));
        (status, body).into_response()
    }
}

#[derive(Debug, Deserialize)]
struct CreateDatasetRequest {
    id: Option<String>,
    kind: String,
    meta: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct SubmitRequest {
    job_id: Option<String>,
    kind: ComputeJobKind,
    dataset_id: Option<String>,
    params: Option<serde_json::Value>,
}

/// The reply to a job submission (shared with the offload client).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubmitResponse {
    pub job_id: String,
    pub state: ComputeJobState,
}

/// The reply to a cancel (shared with the offload client).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelResponse {
    pub cancelled: bool,
}

/// A job as served over the REST API: every [`JobRecord`] field plus a top-level
/// `session_id` lifted from the job's `params` (the capturing session a
/// reconstruct job belongs to). The GCS correlates a world-model artifact to a
/// drone's active session by `session_id` without re-parsing the opaque params or
/// the dataset/job id format. Omitted when the job carries no session — an offload
/// job, or a reconstruct job written by an agent before the session was tagged —
/// so the surface stays backward-compatible.
#[derive(Debug, Serialize)]
struct JobView {
    #[serde(flatten)]
    job: JobRecord,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
}

impl JobView {
    fn of(job: JobRecord) -> Self {
        let session_id = job
            .params
            .get("session_id")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        Self { job, session_id }
    }
}

async fn status(State(state): State<ApiState>) -> Result<Response, ComputeError> {
    let engine = state.lock().await;
    Ok(Json(engine.heartbeat()?).into_response())
}

async fn create_dataset(
    State(state): State<ApiState>,
    Json(req): Json<CreateDatasetRequest>,
) -> Result<Response, ComputeError> {
    let dataset = Dataset {
        id: req.id.unwrap_or_else(|| next_id("ds")),
        kind: req.kind,
        created_ms: now_ms(),
        meta: req.meta.unwrap_or(serde_json::Value::Null),
    };
    let engine = state.lock().await;
    engine.scheduler().store().insert_dataset(&dataset)?;
    Ok((StatusCode::CREATED, Json(dataset)).into_response())
}

async fn submit_job(
    State(state): State<ApiState>,
    Json(req): Json<SubmitRequest>,
) -> Result<Response, ComputeError> {
    let now = now_ms();
    let job = JobRecord {
        id: req.job_id.unwrap_or_else(|| next_id("job")),
        kind: req.kind,
        dataset_id: req.dataset_id,
        state: ComputeJobState::Queued,
        progress: 0.0,
        params: req.params.unwrap_or(serde_json::Value::Null),
        result_ref: None,
        error: None,
        created_ms: now,
        updated_ms: now,
    };
    let engine = state.lock().await;
    engine.scheduler().store().submit_job(&job)?;
    Ok((
        StatusCode::CREATED,
        Json(SubmitResponse {
            job_id: job.id,
            state: job.state,
        }),
    )
        .into_response())
}

/// Rewrite a job record's `result_ref` artifact host to the live public base.
fn rehost_job(mut job: JobRecord, public_base: &str) -> JobRecord {
    if let Some(r) = job.result_ref.take() {
        job.result_ref = Some(rewrite_artifact_host(&r, public_base));
    }
    job
}

async fn list_jobs(
    State(state): State<ApiState>,
    Extension(public_base): Extension<Arc<str>>,
) -> Result<Response, ComputeError> {
    let engine = state.lock().await;
    let jobs = engine.scheduler().store().list_jobs()?;
    let views: Vec<JobView> = jobs
        .into_iter()
        .map(|job| JobView::of(rehost_job(job, &public_base)))
        .collect();
    Ok(Json(views).into_response())
}

async fn job_status(
    State(state): State<ApiState>,
    Extension(public_base): Extension<Arc<str>>,
    Path(id): Path<String>,
) -> Result<Response, ComputeError> {
    let engine = state.lock().await;
    match engine.scheduler().store().get_job(&id)? {
        Some(job) => Ok(Json(JobView::of(rehost_job(job, &public_base))).into_response()),
        None => Err(ComputeError::NotFound(format!("job {id}"))),
    }
}

async fn cancel_job(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Response, ComputeError> {
    let engine = state.lock().await;
    let cancelled = engine.scheduler().store().cancel_job(&id, now_ms())?;
    Ok(Json(CancelResponse { cancelled }).into_response())
}

async fn job_outputs(
    State(state): State<ApiState>,
    Extension(public_base): Extension<Arc<str>>,
    Path(id): Path<String>,
) -> Result<Response, ComputeError> {
    let engine = state.lock().await;
    let mut outputs = engine.scheduler().store().outputs_for_job(&id)?;
    for o in &mut outputs {
        o.uri = rewrite_artifact_host(&o.uri, &public_base);
    }
    Ok(Json(outputs).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cluster, JobStore, MockDetector, MockReconstructor, Output, Scheduler};
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn test_state() -> ApiState {
        let store = JobStore::open_in_memory().unwrap();
        let scheduler = Scheduler::new(store, Arc::new(MockReconstructor), Arc::new(MockDetector));
        let engine = Engine::new(scheduler, Cluster::new_master("node-a"), 2);
        Arc::new(Mutex::new(engine))
    }

    /// A nonexistent pairing file reads as Unpaired (open), the posture the
    /// job-flow tests run under.
    fn unpaired_auth() -> Arc<ComputeAuth> {
        Arc::new(ComputeAuth::new(
            "/nonexistent/ados-compute-test-pairing.json".into(),
        ))
    }

    /// Auth over a temp pairing.json that is paired with `ados_secret`.
    fn paired_auth(dir: &std::path::Path) -> Arc<ComputeAuth> {
        let path = dir.join("pairing.json");
        std::fs::write(&path, r#"{"paired": true, "api_key": "ados_secret"}"#).unwrap();
        Arc::new(ComputeAuth::new(path))
    }

    const OFFBOX: &str = "192.168.1.50:55000";
    const ONBOX: &str = "127.0.0.1:55000";

    /// GET /api/compute/status from `peer`, optionally presenting a key.
    async fn send_auth(
        router: &Router,
        peer: std::net::SocketAddr,
        key: Option<&str>,
    ) -> StatusCode {
        let mut builder = Request::builder().method("GET").uri("/api/compute/status");
        if let Some(k) = key {
            builder = builder.header("x-ados-key", k);
        }
        let mut req = builder.body(Body::empty()).unwrap();
        req.extensions_mut()
            .insert(axum::extract::ConnectInfo(peer));
        router.clone().oneshot(req).await.unwrap().status()
    }

    async fn send(
        router: &Router,
        method: &str,
        path: &str,
        body: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        let req = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        let resp = router.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json = if bytes.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
        };
        (status, json)
    }

    #[tokio::test]
    async fn submit_reconstruct_then_run_then_read_completed() {
        let state = test_state();
        let router = build_router(state.clone(), unpaired_auth());

        // Create a dataset.
        let (st, ds) = send(
            &router,
            "POST",
            "/api/compute/datasets",
            serde_json::json!({ "id": "ds-1", "kind": "bag", "meta": { "cameras": 1 } }),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);
        assert_eq!(ds["id"], "ds-1");

        // Submit a reconstruct job.
        let (st, sub) = send(
            &router,
            "POST",
            "/api/compute/jobs",
            serde_json::json!({ "job_id": "job-1", "kind": "reconstruct", "dataset_id": "ds-1" }),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);
        assert_eq!(sub["job_id"], "job-1");
        assert_eq!(sub["state"], "queued");

        // It is listed as queued.
        let (_, jobs) = send(&router, "GET", "/api/compute/jobs", serde_json::Value::Null).await;
        assert_eq!(jobs.as_array().unwrap().len(), 1);

        // Run one tick (the worker).
        state.lock().await.tick(now_ms()).unwrap();

        // Now it reads as completed with the splat result.
        let (st, job) = send(
            &router,
            "GET",
            "/api/compute/jobs/job-1",
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(job["state"], "completed");
        assert_eq!(job["result_ref"], "mock://splat/ds-1");

        // The output is recorded.
        let (_, outs) = send(
            &router,
            "GET",
            "/api/compute/jobs/job-1/outputs",
            serde_json::Value::Null,
        )
        .await;
        let outs: Vec<Output> = serde_json::from_value(outs).unwrap();
        assert_eq!(outs.len(), 1);
        assert_eq!(outs[0].kind, "splat");
    }

    #[tokio::test]
    async fn submit_offload_then_run_completes() {
        let state = test_state();
        let router = build_router(state.clone(), unpaired_auth());
        let (st, _) = send(
            &router,
            "POST",
            "/api/compute/jobs",
            serde_json::json!({ "job_id": "job-off", "kind": "perception_offload",
                "params": { "frame": { "camera_id": "front", "width": 640, "height": 480, "ts_ms": 5 } } }),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);
        state.lock().await.tick(now_ms()).unwrap();
        let (_, job) = send(
            &router,
            "GET",
            "/api/compute/jobs/job-off",
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(job["state"], "completed");
        assert_eq!(job["result_ref"], "offload://detection/job-off");
        // The offload must have produced a detection ARTIFACT, not just a
        // terminal state. The worker discards JobOutcome.detections, so the
        // recorded Output is the only evidence through the REST surface.
        let (_, outs) = send(
            &router,
            "GET",
            "/api/compute/jobs/job-off/outputs",
            serde_json::Value::Null,
        )
        .await;
        let outs: Vec<Output> = serde_json::from_value(outs).unwrap();
        assert_eq!(outs.len(), 1);
        assert_eq!(outs[0].kind, "detection");
    }

    #[tokio::test]
    async fn job_api_surfaces_session_id_from_params() {
        let router = build_router(test_state(), unpaired_auth());

        // A reconstruct job tagged with its capturing session (the shape the
        // capture ingest submits). The session must appear top-level on both the
        // list and single-job reads so the GCS can correlate the artifact.
        let (st, _) = send(
            &router,
            "POST",
            "/api/compute/jobs",
            serde_json::json!({ "job_id": "recon-s1", "kind": "reconstruct",
                "dataset_id": "ds-s1", "params": { "session_id": "s1", "backend": "brush" } }),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);

        let (_, jobs) = send(&router, "GET", "/api/compute/jobs", serde_json::Value::Null).await;
        let arr = jobs.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["session_id"], "s1");
        // The flattened JobRecord fields are still all present alongside it.
        assert_eq!(arr[0]["id"], "recon-s1");
        assert_eq!(arr[0]["state"], "queued");

        let (_, job) = send(
            &router,
            "GET",
            "/api/compute/jobs/recon-s1",
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(job["session_id"], "s1");

        // A job that carries no session (an offload job) omits the field entirely,
        // so the surface stays backward-compatible.
        send(
            &router,
            "POST",
            "/api/compute/jobs",
            serde_json::json!({ "job_id": "off-1", "kind": "perception_offload" }),
        )
        .await;
        let (_, off) = send(
            &router,
            "GET",
            "/api/compute/jobs/off-1",
            serde_json::Value::Null,
        )
        .await;
        assert!(
            off.get("session_id").is_none(),
            "a sessionless job omits session_id, got: {off}"
        );
    }

    #[tokio::test]
    async fn duplicate_job_id_is_a_409() {
        let router = build_router(test_state(), unpaired_auth());
        let body =
            serde_json::json!({ "job_id": "dup", "kind": "reconstruct", "dataset_id": "ds-x" });
        let (st1, _) = send(&router, "POST", "/api/compute/jobs", body.clone()).await;
        assert_eq!(st1, StatusCode::CREATED);
        let (st2, _) = send(&router, "POST", "/api/compute/jobs", body).await;
        assert_eq!(st2, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn unknown_job_is_404() {
        let router = build_router(test_state(), unpaired_auth());
        let (st, body) = send(
            &router,
            "GET",
            "/api/compute/jobs/ghost",
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        assert!(body["error"].as_str().unwrap().contains("ghost"));
    }

    #[tokio::test]
    async fn cancel_a_queued_job() {
        let router = build_router(test_state(), unpaired_auth());
        send(
            &router,
            "POST",
            "/api/compute/jobs",
            serde_json::json!({ "job_id": "job-c", "kind": "reconstruct", "dataset_id": "ds-x" }),
        )
        .await;
        let (st, body) = send(
            &router,
            "POST",
            "/api/compute/jobs/job-c/cancel",
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(body["cancelled"], true);
    }

    #[tokio::test]
    async fn status_reports_master_role() {
        let router = build_router(test_state(), unpaired_auth());
        let (st, body) = send(
            &router,
            "GET",
            "/api/compute/status",
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(body["role"], "master");
        assert_eq!(body["workers_idle"], 2);
        assert_eq!(body["cluster"]["master_id"], "node-a");
    }

    #[tokio::test]
    async fn unpaired_admits_an_offbox_caller_with_no_key() {
        let router = build_router(test_state(), unpaired_auth());
        assert_eq!(
            send_auth(&router, OFFBOX.parse().unwrap(), None).await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn paired_rejects_an_offbox_caller_with_no_key() {
        let dir = tempfile::tempdir().unwrap();
        let router = build_router(test_state(), paired_auth(dir.path()));
        assert_eq!(
            send_auth(&router, OFFBOX.parse().unwrap(), None).await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn paired_admits_an_offbox_caller_with_the_key_and_rejects_a_wrong_one() {
        let dir = tempfile::tempdir().unwrap();
        let router = build_router(test_state(), paired_auth(dir.path()));
        assert_eq!(
            send_auth(&router, OFFBOX.parse().unwrap(), Some("ados_secret")).await,
            StatusCode::OK
        );
        assert_eq!(
            send_auth(&router, OFFBOX.parse().unwrap(), Some("wrong")).await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn paired_admits_an_onbox_caller_without_a_key() {
        let dir = tempfile::tempdir().unwrap();
        let router = build_router(test_state(), paired_auth(dir.path()));
        assert_eq!(
            send_auth(&router, ONBOX.parse().unwrap(), None).await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn a_forwarded_loopback_caller_is_not_trusted_on_box() {
        // A tunnel terminating on 127.0.0.1 carries a forwarding header; it must
        // NOT get on-box trust, so a paired node still demands the key.
        let dir = tempfile::tempdir().unwrap();
        let router = build_router(test_state(), paired_auth(dir.path()));
        let mut req = Request::builder()
            .method("GET")
            .uri("/api/compute/status")
            .header("x-forwarded-for", "203.0.113.7")
            .body(Body::empty())
            .unwrap();
        req.extensions_mut()
            .insert(axum::extract::ConnectInfo::<std::net::SocketAddr>(
                ONBOX.parse().unwrap(),
            ));
        let st = router.clone().oneshot(req).await.unwrap().status();
        assert_eq!(st, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn the_gate_covers_non_status_routes_too() {
        // The auth layer wraps the WHOLE router, not just /status: a paired node
        // rejects an off-box keyless POST to the job route.
        let dir = tempfile::tempdir().unwrap();
        let router = build_router(test_state(), paired_auth(dir.path()));
        let mut req = Request::builder()
            .method("POST")
            .uri("/api/compute/jobs")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::to_vec(&serde_json::json!({ "kind": "reconstruct" })).unwrap(),
            ))
            .unwrap();
        req.extensions_mut()
            .insert(axum::extract::ConnectInfo::<std::net::SocketAddr>(
                OFFBOX.parse().unwrap(),
            ));
        let st = router.clone().oneshot(req).await.unwrap().status();
        assert_eq!(st, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn paired_rejects_an_empty_key() {
        let dir = tempfile::tempdir().unwrap();
        let router = build_router(test_state(), paired_auth(dir.path()));
        assert_eq!(
            send_auth(&router, OFFBOX.parse().unwrap(), Some("")).await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn serve_path_attaches_connect_info_so_onbox_is_trusted() {
        // Prove the PRODUCTION wiring, not just the middleware logic: serving with
        // into_make_service_with_connect_info attaches the peer, so a real loopback
        // request is on-box and admitted keyless even on a PAIRED node. A refactor
        // that drops the connect-info wiring would default every caller to off-box
        // and this would 401.
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::{TcpListener, TcpStream};

        let dir = tempfile::tempdir().unwrap();
        let router = build_router(test_state(), paired_auth(dir.path()));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await;
        });

        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(b"GET /api/compute/status HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.unwrap();
        let head = String::from_utf8_lossy(&buf);
        assert!(
            head.starts_with("HTTP/1.1 200"),
            "on-box trusted through the real serve path; got: {}",
            &head[..head.len().min(40)]
        );
        server.abort();
    }
}
