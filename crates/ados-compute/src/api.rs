//! The compute node's REST job API (native Rust, axum over tokio). A drone or
//! GCS submits reconstruction and offload jobs here over the LAN and reads their
//! status and results. Handlers lock the engine (a single-writer SQLite store)
//! briefly per request. This is the local-first control surface; mDNS discovery
//! and the pairing auth wrap it, and the daemon serves it on a unix socket plus
//! a LAN TCP port.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::{ComputeError, ComputeJobKind, ComputeJobState, Dataset, Engine, JobRecord};

/// Shared engine handle. One mutex serializes access to the single-writer store,
/// shared by the API handlers and the worker loop.
pub type ApiState = Arc<Mutex<Engine>>;

/// Build the job-API router over a shared engine.
pub fn build_router(state: ApiState) -> Router {
    Router::new()
        .route("/api/compute/status", get(status))
        .route("/api/compute/datasets", post(create_dataset))
        .route("/api/compute/jobs", get(list_jobs).post(submit_job))
        .route("/api/compute/jobs/:id", get(job_status))
        .route("/api/compute/jobs/:id/cancel", post(cancel_job))
        .route("/api/compute/jobs/:id/outputs", get(job_outputs))
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

#[derive(Debug, Serialize)]
struct SubmitResponse {
    job_id: String,
    state: ComputeJobState,
}

#[derive(Debug, Serialize)]
struct CancelResponse {
    cancelled: bool,
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

async fn list_jobs(State(state): State<ApiState>) -> Result<Response, ComputeError> {
    let engine = state.lock().await;
    let jobs = engine.scheduler().store().list_jobs()?;
    Ok(Json(jobs).into_response())
}

async fn job_status(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Response, ComputeError> {
    let engine = state.lock().await;
    match engine.scheduler().store().get_job(&id)? {
        Some(job) => Ok(Json(job).into_response()),
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
    Path(id): Path<String>,
) -> Result<Response, ComputeError> {
    let engine = state.lock().await;
    let outputs = engine.scheduler().store().outputs_for_job(&id)?;
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
        let scheduler = Scheduler::new(store, Box::new(MockReconstructor), Box::new(MockDetector));
        let engine = Engine::new(scheduler, Cluster::new_master("node-a"), 2);
        Arc::new(Mutex::new(engine))
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
        let router = build_router(state.clone());

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
        let router = build_router(state.clone());
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
    }

    #[tokio::test]
    async fn unknown_job_is_404() {
        let router = build_router(test_state());
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
        let router = build_router(test_state());
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
        let router = build_router(test_state());
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
}
