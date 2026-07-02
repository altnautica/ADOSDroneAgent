//! Atlas reconstruct-job cloud sync.
//!
//! `ados-compute` (the workstation/compute profile) writes a reconstruct-job
//! sidecar (`/run/ados/compute-jobs.json`); this loop reads it and POSTs each job
//! to `{convex}/agent/atlas-jobs` so Mission Control's `cmd_atlasJobs` mirrors the
//! node's world models. The GCS World Model tab reads the reconstruction
//! LOCAL-FIRST off the compute node over the LAN (Rule 39); this cloud sync is the
//! secondary/remote path.
//!
//! INERT by default: the loop no-ops unless the node is paired AND a cloud posture
//! is set (same gate as the heartbeat) AND the sidecar is present and fresh — so a
//! local-only, unpaired, or non-atlas node POSTs nothing. The upsert on the Convex
//! side is idempotent on `(computeNodeId, sessionId)`, so re-POSTing every tick
//! dedups.
//!
//! Auth-vs-attribution split: the POST authenticates the POSTER (this workstation,
//! a paired fleet node) via `posterDeviceId` + `X-ADOS-Key`, while
//! `cmd_atlasJobs.deviceId` is the DIFFERENT capturing-drone id the sidecar
//! carries. The reconstructor is co-located with this relay (both run on the
//! workstation), so `computeNodeId` is this node's fleet id too.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// The compute-jobs sidecar filename, resolved under the run dir. Honours the
/// `ADOS_RUN_DIR` override the producer writes under (default `/run/ados`), so a
/// dev / macOS run reads the same file the compute daemon wrote.
const COMPUTE_JOBS_FILE: &str = "compute-jobs.json";

/// A sidecar not re-written within this window is treated as absent, so a
/// dead/hung `ados-compute` (whose tmpfs file persists) never makes the relay
/// forward frozen job state forever (operating rule 44). 4x the producer's 5 s
/// write cadence.
const COMPUTE_JOBS_STALE_MS: i64 = 20_000;

/// How often the reconstruct-job sidecar is forwarded. Jobs are slow-moving and
/// the local-first path is primary, so a coarser cadence than the heartbeat is
/// fine; the idempotent upsert makes re-POSTs cheap.
pub const ATLAS_JOBS_INTERVAL: Duration = Duration::from_secs(15);

/// Resolve the compute-jobs sidecar path (`ADOS_RUN_DIR`-aware, default
/// `/run/ados`), matching the producer's [`ados_compute::compute_jobs_path`].
pub fn compute_jobs_path() -> PathBuf {
    let dir = std::env::var("ADOS_RUN_DIR").unwrap_or_else(|_| "/run/ados".to_string());
    Path::new(&dir).join(COMPUTE_JOBS_FILE)
}

/// Read + parse the compute-jobs sidecar at `path`, returning its job list only
/// when the file is present, parseable, carries a write time, and is FRESH
/// (within the staleness budget at `now_ms`). A stale / missing / malformed file
/// yields `None` so a dead producer stops asserting frozen jobs.
pub fn read_jobs_sidecar_from(path: &Path, now_ms: i64) -> Option<Vec<serde_json::Value>> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let generated = value.get("generatedAtMs").and_then(|v| v.as_i64())?;
    if now_ms.saturating_sub(generated) > COMPUTE_JOBS_STALE_MS {
        return None;
    }
    // Best-effort drift signal: warn (never reject) if the producer's sidecar
    // schema version differs from what this build expects (an older writer emits
    // no field ⇒ 0), then read the jobs anyway.
    let version = value.get("version").and_then(|v| v.as_u64()).unwrap_or(0) as u16;
    ados_protocol::sidecar::check_sidecar_version(
        "compute-jobs",
        version,
        ados_compute::COMPUTE_JOBS_SIDECAR_VERSION,
    );
    let jobs = value.get("jobs")?.as_array()?;
    Some(jobs.clone())
}

/// Build the `/agent/atlas-jobs` POST body for one sidecar job: forward the job's
/// fields verbatim and stamp this node's identity as BOTH the poster (auth) and
/// the compute node (attribution). Returns `None` for a malformed job (not an
/// object, or missing a non-empty `deviceId` / `sessionId`) so a bad entry can
/// never POST a wrong/empty attribution. `poster_device_id` is this workstation's
/// fleet device id.
pub fn build_atlas_job_post(
    job: &serde_json::Value,
    poster_device_id: &str,
) -> Option<serde_json::Value> {
    let obj = job.as_object()?;
    let non_empty = |key: &str| {
        obj.get(key)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
    };
    // The capturing drone + the session are the row's identity + upsert key; skip
    // a job missing either (an unattributable row).
    non_empty("deviceId")?;
    non_empty("sessionId")?;
    if poster_device_id.is_empty() {
        return None;
    }

    let mut body = obj.clone();
    // The reconstructor is co-located with this relay: the poster (auth) and the
    // compute node (attribution) are this same workstation's fleet id.
    body.insert(
        "posterDeviceId".to_string(),
        serde_json::Value::String(poster_device_id.to_string()),
    );
    body.insert(
        "computeNodeId".to_string(),
        serde_json::Value::String(poster_device_id.to_string()),
    );
    Some(serde_json::Value::Object(body))
}

/// POST one job to `{convex}/agent/atlas-jobs` with `X-ADOS-Key` auth.
/// Best-effort: a transport error or non-2xx is logged, never fatal (mirrors
/// [`crate::loops::heartbeat::post_heartbeat`]).
pub async fn post_atlas_job(
    client: &reqwest::Client,
    convex_url: &str,
    api_key: &str,
    body: &serde_json::Value,
) {
    let url = format!("{}/agent/atlas-jobs", convex_url.trim_end_matches('/'));
    match client
        .post(&url)
        .header("X-ADOS-Key", api_key)
        .json(body)
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            tracing::debug!("atlas job synced to cloud");
        }
        Ok(resp) => {
            tracing::warn!(status = resp.status().as_u16(), "atlas job sync rejected");
        }
        Err(e) => {
            tracing::debug!(error = %e, "atlas job sync failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_sidecar(dir: &std::path::Path, body: serde_json::Value) -> std::path::PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join("compute-jobs.json");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(body.to_string().as_bytes())
            .unwrap();
        path
    }

    #[test]
    fn a_fresh_sidecar_yields_jobs_but_a_stale_or_missing_one_does_not() {
        let dir = std::env::temp_dir().join(format!("ados-atlas-jobs-{}", std::process::id()));
        let path = write_sidecar(
            &dir,
            serde_json::json!({
                "generatedAtMs": 1_000_000,
                "jobs": [ { "deviceId": "drone-1", "sessionId": "s1", "status": "done" } ]
            }),
        );
        // Fresh (within the 20 s budget) → the jobs are returned.
        let jobs = read_jobs_sidecar_from(&path, 1_000_000 + 5_000).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0]["deviceId"], "drone-1");
        // Stale (past the budget) → None.
        assert!(read_jobs_sidecar_from(&path, 1_000_000 + 25_000).is_none());
        // Missing file → None.
        assert!(read_jobs_sidecar_from(&dir.join("nope.json"), 1_000_000).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_sidecar_without_a_write_time_is_treated_as_absent() {
        let dir = std::env::temp_dir().join(format!("ados-atlas-jobs-nots-{}", std::process::id()));
        let path = write_sidecar(&dir, serde_json::json!({ "jobs": [] }));
        assert!(read_jobs_sidecar_from(&path, 1_000_000).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_valid_job_gets_the_poster_and_compute_node_stamped() {
        let job = serde_json::json!({
            "deviceId": "drone-1",
            "sessionId": "s1",
            "kind": "reconstruct",
            "status": "done",
            "outputUrl": "http://node:8092/artifacts/s1/world.spz",
            "metadata": { "backend": "brush" }
        });
        let body = build_atlas_job_post(&job, "workstation-7").unwrap();
        // Auth identity + attribution are BOTH this node's fleet id.
        assert_eq!(body["posterDeviceId"], "workstation-7");
        assert_eq!(body["computeNodeId"], "workstation-7");
        // The capturing drone + the job fields ride through verbatim.
        assert_eq!(body["deviceId"], "drone-1");
        assert_eq!(body["sessionId"], "s1");
        assert_eq!(body["status"], "done");
        assert_eq!(body["metadata"]["backend"], "brush");
    }

    #[test]
    fn a_job_missing_the_drone_or_session_is_never_posted() {
        // No deviceId → skipped (never a wrong/empty attribution).
        let no_device = serde_json::json!({ "sessionId": "s1", "status": "done" });
        assert!(build_atlas_job_post(&no_device, "workstation-7").is_none());
        // No sessionId → skipped (no stable upsert key).
        let no_session = serde_json::json!({ "deviceId": "drone-1", "status": "done" });
        assert!(build_atlas_job_post(&no_session, "workstation-7").is_none());
        // Empty deviceId → skipped.
        let empty_device = serde_json::json!({ "deviceId": "", "sessionId": "s1" });
        assert!(build_atlas_job_post(&empty_device, "workstation-7").is_none());
        // Non-object → skipped.
        assert!(build_atlas_job_post(&serde_json::json!("nope"), "workstation-7").is_none());
        // Empty poster id → skipped (an unpaired/unconfigured node never posts).
        let ok = serde_json::json!({ "deviceId": "drone-1", "sessionId": "s1" });
        assert!(build_atlas_job_post(&ok, "").is_none());
    }
}
