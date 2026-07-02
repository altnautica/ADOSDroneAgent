//! The compute-node heartbeat sidecar.
//!
//! ados-compute writes its cluster + queue state to a sidecar file so the
//! native cloud relay (`ados-cloud`) can fold the compute fields into the agent
//! heartbeat it POSTs to Convex — without `ados-cloud` depending on this crate.
//! The sidecar keys are the camelCase `cmd_droneStatus` field names the GCS
//! reads (`computeRole`, `computeClusterSlaves`, …), so `ados-cloud` folds them
//! verbatim. This is the Contract-E sidecar pattern (mirrors the ground-station
//! uplink sidecar): the producer owns its domain state, the relay owns the wire.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::engine::ComputeHeartbeat;
use crate::{ComputeGpu, ComputeRole};

/// The compute-node heartbeat sidecar's default absolute path. The cross-process
/// contract anchor (`ados-cloud` folds this file onto the cloud heartbeat); the
/// live path is resolved through [`compute_heartbeat_path`] so `ADOS_RUN_DIR`
/// redirects it on a dev / macOS run.
pub const COMPUTE_HEARTBEAT_SIDECAR: &str = "/run/ados/compute-heartbeat.json";

/// Schema version stamped on the [`COMPUTE_HEARTBEAT_SIDECAR`] file by its writer
/// and checked (best-effort) by its readers (the cloud relay fold + the local
/// `/api/compute/status` route). Held equal to the `compute-heartbeat` entry in
/// the sidecar registry (see [`ados_protocol::contracts`]); a drift warns, never
/// rejects.
pub const COMPUTE_HEARTBEAT_SIDECAR_VERSION: u16 = 1;

/// The heartbeat sidecar filename, joined onto the resolved run dir.
const COMPUTE_HEARTBEAT_FILE: &str = "compute-heartbeat.json";

/// Resolve the heartbeat sidecar path, honouring the `ADOS_RUN_DIR` override
/// (default `/run/ados`) the sibling daemons resolve their run-dir sidecars
/// under. Byte-identical to [`COMPUTE_HEARTBEAT_SIDECAR`] when the override is
/// unset, so a board with no override is unchanged.
pub fn compute_heartbeat_path() -> PathBuf {
    let dir = std::env::var("ADOS_RUN_DIR").unwrap_or_else(|_| "/run/ados".to_string());
    Path::new(&dir).join(COMPUTE_HEARTBEAT_FILE)
}

/// One slave node's capacity, in the camelCase shape the GCS expects under
/// `cmd_droneStatus.computeClusterSlaves`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SlaveEntry {
    pub node_id: String,
    pub accelerators: Vec<String>,
    pub workers_idle: u32,
    pub queue_depth: u32,
}

/// The flat, camelCase heartbeat sidecar — exactly the `cmd_droneStatus`
/// `compute*` field names, so `ados-cloud` folds it onto the heartbeat with no
/// remapping.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ComputeHeartbeatSidecar {
    /// Sidecar schema version, stamped [`COMPUTE_HEARTBEAT_SIDECAR_VERSION`] on
    /// write so a reader can detect a producer/reader drift. NOT a heartbeat
    /// field — a consumer folding the sidecar onto the cloud heartbeat never
    /// forwards it.
    pub version: u16,
    /// `ComputeRole` serializes lowercase ("master" / "slave").
    pub compute_role: ComputeRole,
    pub compute_cluster_master_id: String,
    pub compute_queue_depth: u32,
    pub compute_active_jobs: u32,
    pub compute_workers_idle: u32,
    pub compute_cluster_aggregate_workers_idle: u32,
    pub compute_cluster_slaves: Vec<SlaveEntry>,
    /// The host GPU block (identity + live utilisation). Serialized under `gpu`;
    /// its own snake_case keys (`unified_memory_mb`, `utilization_pct`) are the
    /// wire shape the GCS compute card reads. All-`null` on a non-macOS node.
    pub gpu: ComputeGpu,
    /// Epoch ms this sidecar was written, so the relay can reject a stale file
    /// (the producer died/hung but the tmpfs file persists). NOT a heartbeat
    /// field — the relay consumes it for the freshness gate, never folds it.
    pub generated_at_ms: i64,
}

impl ComputeHeartbeatSidecar {
    /// Map the engine's heartbeat + a host-GPU sample to the wire sidecar shape.
    /// `now_ms` is the local epoch-ms write time, stamped so the relay can
    /// age-gate the file.
    pub fn from_heartbeat(hb: &ComputeHeartbeat, gpu: ComputeGpu, now_ms: i64) -> Self {
        Self {
            version: COMPUTE_HEARTBEAT_SIDECAR_VERSION,
            generated_at_ms: now_ms,
            compute_role: hb.role,
            compute_cluster_master_id: hb.cluster.master_id.clone(),
            compute_queue_depth: hb.queue_depth,
            compute_active_jobs: hb.active_jobs,
            compute_workers_idle: hb.workers_idle,
            compute_cluster_aggregate_workers_idle: hb.cluster.aggregate_workers_idle,
            compute_cluster_slaves: hb
                .cluster
                .slaves
                .iter()
                .map(|s| SlaveEntry {
                    node_id: s.node_id.clone(),
                    accelerators: s.accelerators.clone(),
                    workers_idle: s.workers_idle,
                    queue_depth: s.queue_depth,
                })
                .collect(),
            gpu,
        }
    }
}

/// Write `body` to `path` atomically: write a `.tmp` sibling then rename, so a
/// reader never sees a half-written file. Creates the parent dir if absent.
/// Shared with the sibling compute-jobs sidecar producer.
pub(crate) fn write_atomic(path: &Path, body: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)
}

/// Serialize + write the compute heartbeat sidecar to its resolved path
/// (`ADOS_RUN_DIR`-aware), stamped with the local write time `now_ms`. `gpu` is
/// the host-GPU sample folded onto the heartbeat (all-`null` off macOS).
pub fn write_compute_heartbeat(
    hb: &ComputeHeartbeat,
    gpu: ComputeGpu,
    now_ms: i64,
) -> std::io::Result<()> {
    write_compute_heartbeat_to(&compute_heartbeat_path(), hb, gpu, now_ms)
}

/// Write to an explicit path (for tests).
pub fn write_compute_heartbeat_to(
    path: &Path,
    hb: &ComputeHeartbeat,
    gpu: ComputeGpu,
    now_ms: i64,
) -> std::io::Result<()> {
    let sidecar = ComputeHeartbeatSidecar::from_heartbeat(hb, gpu, now_ms);
    let body = serde_json::to_vec(&sidecar)?;
    write_atomic(path, &body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::ComputeHeartbeat;
    use crate::{ClusterDescriptor, SlaveDescriptor};

    fn heartbeat() -> ComputeHeartbeat {
        ComputeHeartbeat {
            role: ComputeRole::Master,
            cluster: ClusterDescriptor {
                master_id: "node-master".into(),
                slaves: vec![SlaveDescriptor {
                    node_id: "node-slave-1".into(),
                    accelerators: vec!["cuda:0".into()],
                    workers_idle: 2,
                    queue_depth: 1,
                }],
                aggregate_workers_idle: 6,
            },
            queue_depth: 3,
            active_jobs: 1,
            workers_idle: 4,
        }
    }

    fn sample_gpu() -> ComputeGpu {
        ComputeGpu {
            name: Some("Apple M1 Pro".into()),
            cores: Some(16),
            unified_memory_mb: Some(32768),
            metal: Some("Metal 3".into()),
            utilization_pct: Some(12.5),
        }
    }

    #[test]
    fn the_sidecar_uses_the_camelcase_cmd_drone_status_field_names() {
        let v = serde_json::to_value(ComputeHeartbeatSidecar::from_heartbeat(
            &heartbeat(),
            sample_gpu(),
            1700,
        ))
        .unwrap();
        assert_eq!(v["computeRole"], "master");
        assert_eq!(v["computeClusterMasterId"], "node-master");
        assert_eq!(v["computeQueueDepth"], 3);
        assert_eq!(v["computeActiveJobs"], 1);
        assert_eq!(v["computeWorkersIdle"], 4);
        assert_eq!(v["computeClusterAggregateWorkersIdle"], 6);
        assert_eq!(v["generatedAtMs"], 1700);
        assert_eq!(v["version"], COMPUTE_HEARTBEAT_SIDECAR_VERSION);
        let slave = &v["computeClusterSlaves"][0];
        assert_eq!(slave["nodeId"], "node-slave-1");
        assert_eq!(slave["accelerators"][0], "cuda:0");
        assert_eq!(slave["workersIdle"], 2);
        assert_eq!(slave["queueDepth"], 1);
        // The gpu block rides under `gpu` with its own snake_case wire keys (it is
        // NOT camelCased by the sidecar's rename — nested types keep their own).
        assert_eq!(v["gpu"]["name"], "Apple M1 Pro");
        assert_eq!(v["gpu"]["cores"], 16);
        assert_eq!(v["gpu"]["unified_memory_mb"], 32768);
        assert_eq!(v["gpu"]["metal"], "Metal 3");
        assert_eq!(v["gpu"]["utilization_pct"], 12.5);
    }

    #[test]
    fn compute_heartbeat_path_defaults_and_honours_run_dir_override() {
        // The only env-mutating test in this crate, so the set/remove is safe
        // under the parallel runner. SAFETY: single-threaded mutation here.
        unsafe {
            std::env::remove_var("ADOS_RUN_DIR");
        }
        assert_eq!(
            compute_heartbeat_path(),
            Path::new(COMPUTE_HEARTBEAT_SIDECAR)
        );
        unsafe {
            std::env::set_var("ADOS_RUN_DIR", "/tmp/ados-compute-test-run");
        }
        assert_eq!(
            compute_heartbeat_path(),
            Path::new("/tmp/ados-compute-test-run/compute-heartbeat.json")
        );
        unsafe {
            std::env::remove_var("ADOS_RUN_DIR");
        }
    }

    #[test]
    fn write_then_read_round_trips_the_fields() {
        let dir = std::env::temp_dir().join(format!("ados-compute-hb-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("compute-heartbeat.json");
        write_compute_heartbeat_to(&path, &heartbeat(), sample_gpu(), 1700).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["computeRole"], "master");
        assert_eq!(v["computeWorkersIdle"], 4);
        assert_eq!(v["gpu"]["cores"], 16);
        assert_eq!(v["generatedAtMs"], 1700);
        // No leftover .tmp sibling.
        assert!(!dir.join("compute-heartbeat.json.tmp").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn version_matches_registry() {
        // The per-file constant and the sidecar registry are the two sources of
        // truth for the compute-heartbeat version; catch a drift between them here.
        assert_eq!(
            COMPUTE_HEARTBEAT_SIDECAR_VERSION,
            ados_protocol::contracts::sidecar_version("compute-heartbeat").unwrap()
        );
    }
}
