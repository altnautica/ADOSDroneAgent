//! `GET /api/compute/status` — this compute node's latest cluster status.
//!
//! The compute daemon writes its cluster + queue status to a heartbeat sidecar
//! (the same file `ados-cloud` folds onto the cloud heartbeat, in the exact
//! `cmd_droneStatus` `compute*` camelCase shape). This route serves it to a
//! LAN-paired GCS so the compute-cluster card renders local-first (Rule 39),
//! fresher than the cloud heartbeat. An absent / stale / unreadable sidecar is a
//! `404` (the node is not a compute profile, or its daemon is not running) —
//! never a `500`.
//!
//! Served with the front's native auth posture (key-gated when the agent is
//! paired), the same as the plugin-state read.
//!
//! The served body includes the host `gpu` block (identity + live utilisation)
//! that the compute daemon writes into the sidecar; the route guarantees the key
//! is present (an all-`null` block when the producer supplied none) so the GCS
//! compute card can always read `gpu.*`.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use ados_protocol::compute::ComputeGpu;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::Value;

use crate::routes::detail;

/// The compute daemon's heartbeat sidecar filename under the run dir (matches
/// `compute_heartbeat_path()` in `ados-compute`).
const SIDECAR_FILE: &str = "compute-heartbeat.json";

/// How stale the sidecar may be before the route treats it as absent. The
/// daemon rewrites it every ~5 s; beyond this window it is no longer reporting.
const STALE_AFTER: Duration = Duration::from_secs(20);

/// Resolve the compute heartbeat sidecar path, honouring the `ADOS_RUN_DIR`
/// override (default `/run/ados`) the daemon resolves it under, so the dev /
/// macOS run path finds the sidecar the local compute daemon writes.
fn sidecar_path() -> PathBuf {
    let dir = std::env::var("ADOS_RUN_DIR").unwrap_or_else(|_| "/run/ados".to_string());
    PathBuf::from(dir).join(SIDECAR_FILE)
}

/// `GET /api/compute/status` → the compute node's latest cluster status sidecar.
pub async fn get_compute_status() -> Response {
    read_compute_status(&sidecar_path(), SystemTime::now())
}

/// The read logic against an explicit path + a reference "now", so a test can
/// point it at a temp file and drive the staleness check deterministically.
fn read_compute_status(path: &std::path::Path, now: SystemTime) -> Response {
    let Ok(meta) = std::fs::metadata(path) else {
        return not_found();
    };
    if let Ok(modified) = meta.modified() {
        if let Ok(age) = now.duration_since(modified) {
            if age > STALE_AFTER {
                return not_found();
            }
        }
    }
    let Ok(text) = std::fs::read_to_string(path) else {
        return not_found();
    };
    let Ok(mut doc) = serde_json::from_str::<Value>(&text) else {
        return not_found();
    };
    let Some(obj) = doc.as_object_mut() else {
        return not_found();
    };
    // Guarantee the host gpu block is present so the GCS compute card can always
    // read `gpu.*` (all-null when the producing node supplied none). The macOS
    // workstation daemon writes a populated block; this only fills an absent one.
    if !obj.contains_key("gpu") {
        let null_gpu = serde_json::to_value(ComputeGpu::default()).unwrap_or(Value::Null);
        obj.insert("gpu".to_string(), null_gpu);
    }
    (StatusCode::OK, Json(doc)).into_response()
}

fn not_found() -> Response {
    detail(
        StatusCode::NOT_FOUND,
        "no compute status (not a compute node, or the compute daemon is not running)".to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    async fn body_json(resp: Response) -> Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn serves_a_fresh_sidecar_unchanged() {
        // A sidecar that already carries a gpu block passes through verbatim (the
        // guarantee only fills an absent gpu, so a present one is untouched).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("compute-heartbeat.json");
        let body = json!({
            "computeRole": "master",
            "computeClusterMasterId": "node-a",
            "computeQueueDepth": 2,
            "computeActiveJobs": 1,
            "computeWorkersIdle": 3,
            "computeClusterAggregateWorkersIdle": 3,
            "computeClusterSlaves": [],
            "gpu": {
                "name": "Apple M1 Pro",
                "cores": 16,
                "unified_memory_mb": 32768,
                "metal": "Metal 3",
                "utilization_pct": 12.5,
            },
            "generatedAtMs": 1234,
        });
        std::fs::write(&path, serde_json::to_string(&body).unwrap()).unwrap();
        let resp = read_compute_status(&path, SystemTime::now());
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_json(resp).await, body);
    }

    #[tokio::test]
    async fn injects_an_all_null_gpu_block_when_absent() {
        // A producer that wrote no gpu block still serves a `gpu` key (all-null) so
        // the GCS can always read `gpu.*`.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("compute-heartbeat.json");
        std::fs::write(&path, r#"{"computeRole":"master"}"#).unwrap();
        let resp = read_compute_status(&path, SystemTime::now());
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert!(body["gpu"].is_object());
        for key in [
            "name",
            "cores",
            "unified_memory_mb",
            "metal",
            "utilization_pct",
        ] {
            assert!(body["gpu"][key].is_null(), "{key} is null");
        }
    }

    #[tokio::test]
    async fn an_absent_sidecar_is_a_404() {
        let dir = tempfile::tempdir().unwrap();
        let resp = read_compute_status(&dir.path().join("nope.json"), SystemTime::now());
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_stale_sidecar_is_a_404() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("compute-heartbeat.json");
        std::fs::write(&path, r#"{"computeRole":"master"}"#).unwrap();
        let future = SystemTime::now() + STALE_AFTER + Duration::from_secs(5);
        let resp = read_compute_status(&path, future);
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_malformed_sidecar_is_a_404_not_a_500() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("compute-heartbeat.json");
        std::fs::write(&path, b"not json {{{").unwrap();
        let resp = read_compute_status(&path, SystemTime::now());
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
