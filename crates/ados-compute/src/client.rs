//! The compute-offload client: the agent-side caller of a paired compute node's
//! job API.
//!
//! A plugin's offload submits a reconstruction or a perception / SLAM-offload job
//! through this client, uploads its dataset, and reads the status + result. It is
//! the drone/GCS half of the `compute.job.submit` / `compute.job.read` /
//! `compute.dataset.write` capability contract — the plugin host gates a plugin
//! on those caps, then routes the call here. Local-first: `base_url` is the paired
//! node's LAN address, the pairing key rides `X-ADOS-Key` for the off-box leg.

use std::time::Duration;

use reqwest::Client;
use serde::Serialize;

use crate::api::{CancelResponse, SubmitResponse};
use crate::{ComputeHeartbeat, ComputeJobKind, Dataset, JobRecord, Output};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// A failure calling the compute node's job API.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The request itself failed (connect / timeout / transport).
    #[error("request: {0}")]
    Request(String),
    /// The node returned a non-success status; the body is the error JSON.
    #[error("http {0}: {1}")]
    Http(u16, String),
    /// The response body did not decode to the expected type.
    #[error("decode: {0}")]
    Decode(String),
}

#[derive(Serialize)]
struct CreateDatasetBody<'a> {
    kind: &'a str,
    meta: serde_json::Value,
}

#[derive(Serialize)]
struct SubmitBody {
    /// An optional caller-chosen id. Reusing it on a retry makes the submit
    /// idempotent: a duplicate yields a `409 Conflict` instead of a second job.
    #[serde(skip_serializing_if = "Option::is_none")]
    job_id: Option<String>,
    kind: ComputeJobKind,
    dataset_id: Option<String>,
    params: serde_json::Value,
}

/// Percent-encode a value going into a URL path segment, so a job id with a
/// `/`, `?`, `#`, or whitespace cannot misroute the request. Server-generated
/// ids are already URL-safe, so this is identity for them; it guards an
/// arbitrary caller-supplied id.
fn encode_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// A client to one compute node's job API.
pub struct ComputeClient {
    http: Client,
    base_url: String,
    api_key: Option<String>,
}

impl ComputeClient {
    /// A client targeting `base_url` (e.g. `http://compute.local:8092`). `api_key`
    /// is the pairing key, sent as `X-ADOS-Key` for the off-box leg; pass `None`
    /// on-box / when unpaired. The client carries connect + request timeouts so a
    /// hung node fails the call rather than parking the caller.
    pub fn new(base_url: impl Into<String>, api_key: Option<String>) -> Self {
        // build() only fails on a TLS-stack init error, which a plain-HTTP
        // (no-TLS) client never hits; `expect` keeps the timeout guarantee
        // (a silent fall back to a timeout-less default would let a hung node
        // park the caller — the exact thing the timeouts prevent).
        let http = Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .expect("build compute http client");
        Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    fn auth(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.api_key {
            Some(key) => rb.header("X-ADOS-Key", key),
            None => rb,
        }
    }

    async fn send_json<T: serde::de::DeserializeOwned>(
        &self,
        rb: reqwest::RequestBuilder,
    ) -> Result<T, ClientError> {
        let resp = self
            .auth(rb)
            .send()
            .await
            .map_err(|e| ClientError::Request(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ClientError::Http(status.as_u16(), body));
        }
        resp.json::<T>()
            .await
            .map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Read the node + cluster heartbeat.
    pub async fn status(&self) -> Result<ComputeHeartbeat, ClientError> {
        self.send_json(self.http.get(self.url("/api/compute/status")))
            .await
    }

    /// Register a dataset (`compute.dataset.write`). The heavy input rides a
    /// separate bulk/stream lane; this records the dataset the job consumes.
    pub async fn write_dataset(
        &self,
        kind: &str,
        meta: serde_json::Value,
    ) -> Result<Dataset, ClientError> {
        self.send_json(
            self.http
                .post(self.url("/api/compute/datasets"))
                .json(&CreateDatasetBody { kind, meta }),
        )
        .await
    }

    /// Submit a job (`compute.job.submit`): a reconstruction over a dataset, or a
    /// perception / SLAM offload whose frame rides `params`. Pass `job_id` to make
    /// the submit idempotent — reusing the same id on a retry (after a timeout
    /// over a lossy link) yields a `409 Conflict` rather than a duplicate job.
    pub async fn submit_job(
        &self,
        kind: ComputeJobKind,
        dataset_id: Option<String>,
        params: serde_json::Value,
        job_id: Option<String>,
    ) -> Result<SubmitResponse, ClientError> {
        self.send_json(
            self.http
                .post(self.url("/api/compute/jobs"))
                .json(&SubmitBody {
                    job_id,
                    kind,
                    dataset_id,
                    params,
                }),
        )
        .await
    }

    /// Read a job's status + progress (`compute.job.read`).
    pub async fn job_status(&self, id: &str) -> Result<JobRecord, ClientError> {
        let seg = encode_segment(id);
        self.send_json(self.http.get(self.url(&format!("/api/compute/jobs/{seg}"))))
            .await
    }

    /// Cancel a job. Returns whether it was a non-terminal job that was cancelled.
    pub async fn cancel_job(&self, id: &str) -> Result<bool, ClientError> {
        let seg = encode_segment(id);
        let resp: CancelResponse = self
            .send_json(
                self.http
                    .post(self.url(&format!("/api/compute/jobs/{seg}/cancel"))),
            )
            .await?;
        Ok(resp.cancelled)
    }

    /// Read a finished job's outputs (`compute.job.read`).
    pub async fn job_outputs(&self, id: &str) -> Result<Vec<Output>, ClientError> {
        let seg = encode_segment(id);
        self.send_json(
            self.http
                .get(self.url(&format!("/api/compute/jobs/{seg}/outputs"))),
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        build_router, Cluster, ComputeAuth, ComputeJobState, ComputeRole, Engine, JobStore,
        MockDetector, MockReconstructor, Scheduler,
    };
    use std::net::SocketAddr;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    /// Spin the real job-API router over a fresh engine on a loopback port, with
    /// the given auth. Returns the bound address + the engine handle (so a test
    /// can drive `tick` directly, the way the daemon's worker loop would).
    async fn spawn(auth: Arc<ComputeAuth>) -> (SocketAddr, Arc<Mutex<Engine>>) {
        let store = JobStore::open_in_memory().unwrap();
        let scheduler = Scheduler::new(store, Arc::new(MockReconstructor), Arc::new(MockDetector));
        let engine = Arc::new(Mutex::new(Engine::new(
            scheduler,
            Cluster::new_master("node-a"),
            2,
        )));
        let app = build_router(engine.clone(), auth);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });
        (addr, engine)
    }

    fn unpaired_auth() -> Arc<ComputeAuth> {
        Arc::new(ComputeAuth::new(
            "/nonexistent/ados-compute-client-test.json".into(),
        ))
    }

    #[tokio::test]
    async fn client_submits_a_job_and_reads_its_result() {
        let (addr, engine) = spawn(unpaired_auth()).await;
        let client = ComputeClient::new(format!("http://{addr}"), None);

        // Status reflects the master node.
        let hb = client.status().await.unwrap();
        assert_eq!(hb.role, ComputeRole::Master);

        // Register a dataset, submit a reconstruct job over it.
        let ds = client
            .write_dataset("bag", serde_json::json!({ "cameras": 1 }))
            .await
            .unwrap();
        let sub = client
            .submit_job(
                ComputeJobKind::Reconstruct,
                Some(ds.id.clone()),
                serde_json::json!({}),
                None,
            )
            .await
            .unwrap();
        assert_eq!(sub.state, ComputeJobState::Queued);

        // The daemon's worker loop runs the job; drive a tick directly here.
        engine.lock().await.tick(1).unwrap();

        // The job completed and produced the mock splat output.
        let job = client.job_status(&sub.job_id).await.unwrap();
        assert_eq!(job.state, ComputeJobState::Completed);
        let outputs = client.job_outputs(&sub.job_id).await.unwrap();
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].kind, "splat");
    }

    #[tokio::test]
    async fn a_perception_offload_returns_a_detection_artifact() {
        let (addr, engine) = spawn(unpaired_auth()).await;
        let client = ComputeClient::new(format!("http://{addr}"), None);
        // No dataset; the frame rides params.
        let sub = client
            .submit_job(
                ComputeJobKind::PerceptionOffload,
                None,
                serde_json::json!({ "frame": { "camera_id": "front", "width": 640, "height": 480, "ts_ms": 1 } }),
                None,
            )
            .await
            .unwrap();
        engine.lock().await.tick(1).unwrap();
        let outputs = client.job_outputs(&sub.job_id).await.unwrap();
        assert_eq!(outputs[0].kind, "detection");
    }

    #[tokio::test]
    async fn client_cancels_a_queued_job() {
        let (addr, _engine) = spawn(unpaired_auth()).await;
        let client = ComputeClient::new(format!("http://{addr}"), None);
        let ds = client
            .write_dataset("bag", serde_json::json!({}))
            .await
            .unwrap();
        let sub = client
            .submit_job(
                ComputeJobKind::Reconstruct,
                Some(ds.id),
                serde_json::json!({}),
                None,
            )
            .await
            .unwrap();
        // The job is still queued (no tick), so cancel succeeds.
        assert!(client.cancel_job(&sub.job_id).await.unwrap());
        assert_eq!(
            client.job_status(&sub.job_id).await.unwrap().state,
            ComputeJobState::Cancelled
        );
    }

    #[tokio::test]
    async fn a_reused_job_id_is_a_409_so_a_retry_is_idempotent() {
        let (addr, _engine) = spawn(unpaired_auth()).await;
        let client = ComputeClient::new(format!("http://{addr}"), None);
        let ds = client
            .write_dataset("bag", serde_json::json!({}))
            .await
            .unwrap();
        let id = Some("my-idempotent-job".to_string());
        client
            .submit_job(
                ComputeJobKind::Reconstruct,
                Some(ds.id.clone()),
                serde_json::json!({}),
                id.clone(),
            )
            .await
            .unwrap();
        // A retry with the same id does not create a second job.
        match client
            .submit_job(
                ComputeJobKind::Reconstruct,
                Some(ds.id),
                serde_json::json!({}),
                id,
            )
            .await
        {
            Err(ClientError::Http(409, _)) => {}
            other => panic!("expected Http(409) on a duplicate id, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_missing_job_is_a_404() {
        let (addr, _engine) = spawn(unpaired_auth()).await;
        let client = ComputeClient::new(format!("http://{addr}"), None);
        match client.job_status("nope").await {
            Err(ClientError::Http(404, _)) => {}
            other => panic!("expected Http(404), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn the_pairing_key_round_trips_against_a_paired_node() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pairing.json");
        std::fs::write(&path, r#"{"paired": true, "api_key": "ados_secret"}"#).unwrap();
        let (addr, _engine) = spawn(Arc::new(ComputeAuth::new(path))).await;
        // A loopback connect is on-box (open even when paired); the X-ADOS-Key
        // header is still sent and accepted. The off-box rejection path is
        // covered by the auth-gate unit tests, which can synthesize an off-box
        // peer; a real loopback client cannot.
        let client = ComputeClient::new(format!("http://{addr}"), Some("ados_secret".into()));
        assert!(client.status().await.is_ok());
    }
}
