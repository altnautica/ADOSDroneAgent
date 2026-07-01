//! The compute-node reconstruct-job sidecar.
//!
//! `ados-compute` snapshots its reconstruct jobs to a sidecar file so the native
//! cloud relay (`ados-cloud`) can forward them to Mission Control's cloud backend
//! (`cmd_atlasJobs`) — without `ados-cloud` depending on this crate. This is the
//! same Contract-E sidecar pattern as the heartbeat sidecar: the producer owns
//! its domain state (the jobs, in the wire shape the cloud route accepts), the
//! relay owns the transport (auth + POST). The GCS World Model tab reads the
//! reconstruction LOCAL-FIRST off the compute node over the LAN (Rule 39); this
//! cloud sync is the secondary/remote path.
//!
//! One representative entry per capture session: the newest COMPLETED reconstruct
//! (else the newest in-flight one), so the row is the latest world model for that
//! session and periodic live cycles collapse cleanly. A job with no capturing
//! drone id is skipped — the row is keyed to the drone that captured it
//! (`cmd_atlasJobs.deviceId`), so an unattributable job is never emitted with a
//! wrong/empty id.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{ComputeError, ComputeJobKind, ComputeJobState, JobRecord, JobStore};

/// The compute-jobs sidecar's default absolute path (the cross-process contract
/// anchor `ados-cloud` reads). The live path resolves through
/// [`compute_jobs_path`] so `ADOS_RUN_DIR` redirects it on a dev / macOS run.
pub const COMPUTE_JOBS_SIDECAR: &str = "/run/ados/compute-jobs.json";

/// The sidecar filename, joined onto the resolved run dir.
const COMPUTE_JOBS_FILE: &str = "compute-jobs.json";

/// Resolve the sidecar path, honouring the `ADOS_RUN_DIR` override (default
/// `/run/ados`) the sibling daemons resolve their run-dir sidecars under.
pub fn compute_jobs_path() -> PathBuf {
    let dir = std::env::var("ADOS_RUN_DIR").unwrap_or_else(|_| "/run/ados".to_string());
    Path::new(&dir).join(COMPUTE_JOBS_FILE)
}

/// One reconstruct job in the wire shape the `/agent/atlas-jobs` route accepts
/// (camelCase). `ados-cloud` folds these verbatim, adding only the poster +
/// compute-node identity (the relay owns the fleet identity, not this crate).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AtlasJobEntry {
    /// The capturing drone (`cmd_atlasJobs.deviceId`); never empty (a job without
    /// one is skipped upstream).
    pub device_id: String,
    /// The capture session the world model reconstructs (the upsert key).
    pub session_id: String,
    /// The job kind (always `reconstruct` for a world model).
    pub kind: String,
    /// The `cmd_atlasJobs` status vocabulary: `queued` / `running` / `done` /
    /// `error` / `cancelled` (mapped from the engine state).
    pub status: String,
    /// The dataset/bag the job ran on (lineage).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_bag: Option<String>,
    /// The reconstruction artifact URL when an output exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_url: Option<String>,
    /// Opaque metadata the GCS reads: `{ backend?, viewerHint?, gaussianCount? }`.
    /// `backend` drives the reconstruction-honesty badge (Rule 44).
    pub metadata: serde_json::Value,
    /// Job creation time (epoch ms).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<i64>,
    /// Job completion time (epoch ms) when the job reached a terminal state.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<i64>,
}

/// The sidecar: the representative reconstruct jobs plus the write time so the
/// relay can reject a stale file (a dead/hung producer whose tmpfs file persists).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AtlasJobsSidecar {
    pub jobs: Vec<AtlasJobEntry>,
    pub generated_at_ms: i64,
}

/// Map the engine's job state to the `cmd_atlasJobs` status vocabulary the GCS
/// World Model tab reads (`done`/`error`, not `completed`/`failed`).
fn cmd_atlas_status(state: ComputeJobState) -> &'static str {
    match state {
        ComputeJobState::Queued => "queued",
        ComputeJobState::Running => "running",
        ComputeJobState::Completed => "done",
        ComputeJobState::Failed => "error",
        ComputeJobState::Cancelled => "cancelled",
    }
}

/// The viewer a completed artifact prefers, keyed on the output kind, matching the
/// GCS `viewerForKind` mapping. `None` lets the GCS fall back to its default world
/// viewer (an unrecognized hint is dropped by the GCS anyway).
fn viewer_hint_for_kind(kind: &str) -> Option<&'static str> {
    match kind {
        "splat" => Some("splat"),
        "cloud" | "ply" | "pointcloud" => Some("cloud"),
        _ => None,
    }
}

/// A non-empty string param, or `None`.
fn str_param<'a>(job: &'a JobRecord, key: &str) -> Option<&'a str> {
    job.params
        .get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
}

/// Prefer a completed job over a non-completed one; among equal completeness, the
/// newer update wins. So a session's row reflects its latest completed world model
/// (else the latest in-flight job, so the operator still sees progress).
fn is_better(new: &JobRecord, cur: &JobRecord) -> bool {
    let rank = |s: ComputeJobState| (s == ComputeJobState::Completed) as u8;
    match rank(new.state).cmp(&rank(cur.state)) {
        std::cmp::Ordering::Greater => true,
        std::cmp::Ordering::Less => false,
        std::cmp::Ordering::Equal => new.updated_ms >= cur.updated_ms,
    }
}

/// Build the reconstruct-job sidecar from the store: one representative entry per
/// session that carries a capturing drone id, with the honest backend + artifact
/// lifted from the job's first output. `now_ms` is the write time stamped for the
/// relay's staleness gate.
pub fn build_atlas_jobs_sidecar(
    store: &JobStore,
    now_ms: i64,
) -> Result<AtlasJobsSidecar, ComputeError> {
    // Collapse each session to one representative reconstruct job.
    let mut by_session: HashMap<String, JobRecord> = HashMap::new();
    for job in store.list_jobs()? {
        if job.kind != ComputeJobKind::Reconstruct {
            continue;
        }
        let Some(session) = str_param(&job, "session_id").map(str::to_string) else {
            continue;
        };
        // Never emit a wrong/empty deviceId: a job with no capturing drone is not
        // surfaced to the cloud (it would be unattributable in cmd_atlasJobs).
        if str_param(&job, "device_id").is_none() {
            continue;
        }
        match by_session.get(&session) {
            Some(cur) if !is_better(&job, cur) => {}
            _ => {
                by_session.insert(session, job);
            }
        }
    }

    let mut jobs = Vec::with_capacity(by_session.len());
    for (session_id, job) in by_session {
        let device_id = str_param(&job, "device_id").unwrap_or_default().to_string();

        // The reconstruction artifact (the first output, mirroring the local
        // path's `outputs[0]`) carries the honest backend + gaussian count; a job
        // with no output yet has none.
        let outputs = store.outputs_for_job(&job.id)?;
        let first = outputs.first();
        let output_url = first.map(|o| o.uri.clone());

        let mut metadata = serde_json::Map::new();
        // The honest reconstruction backend (Rule 44): the output's stamped
        // backend, else the requested hint before an output exists.
        let backend = first
            .and_then(|o| o.meta.get("backend").and_then(|v| v.as_str()))
            .or_else(|| job.params.get("backend").and_then(|v| v.as_str()));
        if let Some(b) = backend {
            metadata.insert("backend".into(), serde_json::Value::String(b.to_string()));
        }
        if let Some(vh) = first.and_then(|o| viewer_hint_for_kind(&o.kind)) {
            metadata.insert(
                "viewerHint".into(),
                serde_json::Value::String(vh.to_string()),
            );
        }
        if let Some(gc) = first.and_then(|o| o.meta.get("gaussian_count").and_then(|v| v.as_u64()))
        {
            metadata.insert("gaussianCount".into(), serde_json::json!(gc));
        }

        let terminal = job.state.is_terminal();
        jobs.push(AtlasJobEntry {
            device_id,
            session_id,
            kind: "reconstruct".into(),
            status: cmd_atlas_status(job.state).into(),
            input_bag: job.dataset_id.clone(),
            output_url,
            metadata: serde_json::Value::Object(metadata),
            started_at: Some(job.created_ms),
            finished_at: terminal.then_some(job.updated_ms),
        });
    }
    // Deterministic order (a stable POST order + deterministic tests).
    jobs.sort_by(|a, b| a.session_id.cmp(&b.session_id));
    Ok(AtlasJobsSidecar {
        jobs,
        generated_at_ms: now_ms,
    })
}

/// Serialize + atomically write the sidecar to its resolved path
/// (`ADOS_RUN_DIR`-aware). Reuses the heartbeat sidecar's atomic writer.
pub fn write_atlas_jobs_sidecar(sidecar: &AtlasJobsSidecar) -> std::io::Result<()> {
    let body = serde_json::to_vec(sidecar).map_err(std::io::Error::other)?;
    crate::heartbeat_sidecar::write_atomic(&compute_jobs_path(), &body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Output;

    fn recon_job(id: &str, session: &str, device: &str, state: ComputeJobState) -> JobRecord {
        JobRecord {
            id: id.into(),
            kind: ComputeJobKind::Reconstruct,
            dataset_id: Some(format!("ds-{session}")),
            state,
            progress: if state == ComputeJobState::Completed {
                1.0
            } else {
                0.0
            },
            params: serde_json::json!({
                "backend": "brush",
                "session_id": session,
                "device_id": device,
            }),
            result_ref: None,
            error: None,
            created_ms: 100,
            updated_ms: 200,
        }
    }

    fn store_with(jobs: &[JobRecord]) -> JobStore {
        let s = JobStore::open_in_memory().unwrap();
        for j in jobs {
            s.submit_job(j).unwrap();
            // Move it to its intended terminal/running state (submit inserts queued).
            if j.state != ComputeJobState::Queued {
                s.set_job_state(
                    &j.id,
                    j.state,
                    j.progress,
                    j.result_ref.as_deref(),
                    None,
                    j.updated_ms,
                )
                .unwrap();
            }
        }
        s
    }

    #[test]
    fn a_completed_reconstruct_becomes_a_done_entry_with_backend_and_output() {
        let store = store_with(&[recon_job(
            "recon-s1",
            "s1",
            "drone-1",
            ComputeJobState::Completed,
        )]);
        // The real backend + artifact ride the job's first output.
        let mut out = Output::new(
            "o1".into(),
            "recon-s1".into(),
            "splat".into(),
            "http://node:8092/artifacts/s1/world.spz".into(),
            200,
        );
        out.meta = serde_json::json!({ "gaussian_count": 250000, "backend": "brush" });
        store.insert_output(&out).unwrap();

        let sidecar = build_atlas_jobs_sidecar(&store, 1_700).unwrap();
        assert_eq!(sidecar.generated_at_ms, 1_700);
        assert_eq!(sidecar.jobs.len(), 1);
        let e = &sidecar.jobs[0];
        assert_eq!(e.device_id, "drone-1");
        assert_eq!(e.session_id, "s1");
        assert_eq!(e.kind, "reconstruct");
        // completed → done (the cmd_atlasJobs vocabulary the GCS reads).
        assert_eq!(e.status, "done");
        assert_eq!(e.input_bag.as_deref(), Some("ds-s1"));
        assert_eq!(
            e.output_url.as_deref(),
            Some("http://node:8092/artifacts/s1/world.spz")
        );
        // The honest backend badge (Rule 44) + viewer hint + gaussian count.
        assert_eq!(e.metadata["backend"], "brush");
        assert_eq!(e.metadata["viewerHint"], "splat");
        assert_eq!(e.metadata["gaussianCount"], 250000);
        assert_eq!(e.finished_at, Some(200));
    }

    #[test]
    fn the_camelcase_wire_shape_matches_the_route() {
        let store = store_with(&[recon_job(
            "recon-s1",
            "s1",
            "drone-1",
            ComputeJobState::Running,
        )]);
        let sidecar = build_atlas_jobs_sidecar(&store, 1).unwrap();
        let v = serde_json::to_value(&sidecar).unwrap();
        let job = &v["jobs"][0];
        // The keys the /agent/atlas-jobs route + upsertJob read, camelCased.
        assert_eq!(job["deviceId"], "drone-1");
        assert_eq!(job["sessionId"], "s1");
        assert_eq!(job["status"], "running");
        assert_eq!(job["inputBag"], "ds-s1");
        assert_eq!(v["generatedAtMs"], 1);
        // No output yet → no outputUrl key; backend falls back to the requested hint.
        assert!(job.get("outputUrl").is_none());
        assert_eq!(job["metadata"]["backend"], "brush");
    }

    #[test]
    fn a_job_without_a_capturing_drone_is_never_emitted() {
        // No device_id in params → unattributable → skipped (never a wrong/empty id).
        let mut job = recon_job("recon-x", "sx", "drone-x", ComputeJobState::Completed);
        job.params = serde_json::json!({ "backend": "brush", "session_id": "sx" });
        let store = store_with(&[job]);
        assert!(build_atlas_jobs_sidecar(&store, 1).unwrap().jobs.is_empty());
    }

    #[test]
    fn offload_jobs_and_sessionless_jobs_are_excluded() {
        let store = JobStore::open_in_memory().unwrap();
        // An offload job (no world model, no session) is not a reconstruct.
        let offload = JobRecord {
            id: "off-1".into(),
            kind: ComputeJobKind::PerceptionOffload,
            dataset_id: None,
            state: ComputeJobState::Completed,
            progress: 1.0,
            params: serde_json::json!({ "device_id": "drone-1" }),
            result_ref: None,
            error: None,
            created_ms: 1,
            updated_ms: 2,
        };
        store.submit_job(&offload).unwrap();
        assert!(build_atlas_jobs_sidecar(&store, 1).unwrap().jobs.is_empty());
    }

    #[test]
    fn the_representative_prefers_the_completed_cycle_over_a_running_one() {
        // A live session with two cycles: c0 completed, c1 running. The row shows
        // the completed world model, not the in-flight cycle (no done→running
        // regression).
        let mut c0 = recon_job("recon-s-c0", "s", "drone-1", ComputeJobState::Completed);
        c0.updated_ms = 300;
        let mut c1 = recon_job("recon-s-c1", "s", "drone-1", ComputeJobState::Running);
        c1.updated_ms = 400; // newer, but not completed
        let store = store_with(&[c0, c1]);
        let sidecar = build_atlas_jobs_sidecar(&store, 1).unwrap();
        assert_eq!(sidecar.jobs.len(), 1, "one row per session");
        assert_eq!(sidecar.jobs[0].status, "done");
    }

    #[test]
    fn a_failed_job_maps_to_error_and_carries_no_finished_regression() {
        let store = store_with(&[recon_job(
            "recon-f",
            "sf",
            "drone-1",
            ComputeJobState::Failed,
        )]);
        let sidecar = build_atlas_jobs_sidecar(&store, 1).unwrap();
        assert_eq!(sidecar.jobs[0].status, "error");
        assert_eq!(sidecar.jobs[0].finished_at, Some(200));
    }

    #[test]
    fn write_round_trips_through_the_run_dir_override() {
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: single-threaded mutation in this test.
        unsafe {
            std::env::set_var("ADOS_RUN_DIR", dir.path());
        }
        let store = store_with(&[recon_job(
            "recon-s1",
            "s1",
            "drone-1",
            ComputeJobState::Queued,
        )]);
        let sidecar = build_atlas_jobs_sidecar(&store, 42).unwrap();
        write_atlas_jobs_sidecar(&sidecar).unwrap();
        let path = dir.path().join("compute-jobs.json");
        let text = std::fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["generatedAtMs"], 42);
        assert_eq!(v["jobs"][0]["deviceId"], "drone-1");
        assert_eq!(v["jobs"][0]["status"], "queued");
        assert!(!dir.path().join("compute-jobs.json.tmp").exists());
        unsafe {
            std::env::remove_var("ADOS_RUN_DIR");
        }
    }
}
