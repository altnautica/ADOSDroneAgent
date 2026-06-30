//! Compute-node offload contract: the master/slave cluster shape and the job
//! interface any plugin uses to push work to the compute node.
//!
//! The compute node is a shared heavy-compute substrate. It runs as a
//! master/slave cluster: there is always one master (the single logical compute
//! endpoint a drone or GCS pairs with, and the scheduler), and extra nodes
//! slave to it and offer their workers. A plugin submits a job (reconstruction,
//! or a perception/SLAM offload session) with the `compute.job.submit`
//! capability and reads its status and result with `compute.job.read`. The
//! heavy result is delivered out-of-band (a shared-data topic or a stream-lane
//! url); the job interface carries the small request and status only.
//!
//! The wire structs are reserved here so the compute service, the offload
//! session, and the cluster heartbeat all speak one contract.

use serde::{Deserialize, Serialize};

/// A node's role in the compute cluster. A lone node is the master.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ComputeRole {
    #[default]
    Master,
    Slave,
}

/// What a compute job does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComputeJobKind {
    /// Reconstruct a world model from a keyframe bag (splat / cloud / mesh).
    Reconstruct,
    /// A streaming session: frames in, detections back (for an NPU-less drone).
    PerceptionOffload,
    /// A streaming session: frames in, poses back (offloaded SLAM).
    SlamOffload,
}

/// Lifecycle of a compute job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ComputeJobState {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl ComputeJobState {
    /// Whether the job has reached a terminal state.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            ComputeJobState::Completed | ComputeJobState::Failed | ComputeJobState::Cancelled
        )
    }
}

/// A job submission. `dataset_ref` names the input (a bag handle, or a live
/// session id for a streaming offload); `params` carries job-specific options.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComputeJobRequest {
    pub job_id: String,
    pub kind: ComputeJobKind,
    pub dataset_ref: Option<String>,
    pub params: serde_json::Value,
}

/// The status of a job, polled with [`Capability::ComputeJobRead`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComputeJobStatus {
    pub job_id: String,
    pub kind: ComputeJobKind,
    pub state: ComputeJobState,
    /// Progress in `0.0..=1.0` while running.
    pub progress: f32,
    /// Where the finished artifact can be fetched (stream-lane url or handle).
    pub result_ref: Option<String>,
    /// Failure detail when `state` is [`ComputeJobState::Failed`].
    pub error: Option<String>,
}

/// The host GPU block carried on a compute node's status: identity (name, core
/// count, Metal support, unified-memory size) plus a live utilisation sample.
///
/// Every field is optional and honest (Rule 44): an unknown field is `null`,
/// never fabricated. On a non-macOS host (or when a probe tool is missing / a
/// `powermetrics` sample needs sudo it does not have) the relevant field is
/// `null`, so the whole block degrades to all-`null` rather than reporting a
/// guessed value. The field names are the snake_case wire keys the GCS compute
/// card reads (`unified_memory_mb`, `utilization_pct`).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ComputeGpu {
    /// GPU model name, e.g. `Apple M1 Pro`.
    pub name: Option<String>,
    /// GPU core count.
    pub cores: Option<u32>,
    /// Unified / total graphics memory in MB (on Apple Silicon the shared system
    /// memory the GPU draws from).
    pub unified_memory_mb: Option<u64>,
    /// Metal support label, e.g. `Metal 3`.
    pub metal: Option<String>,
    /// Live GPU utilisation percentage (`0.0..=100.0`); `null` when not sampleable.
    pub utilization_pct: Option<f32>,
}

/// One slave node's capacity, advertised to the master.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SlaveDescriptor {
    pub node_id: String,
    /// Accelerator labels the slave offers (e.g. `cuda:0`, `mps`, `cpu`).
    pub accelerators: Vec<String>,
    pub workers_idle: u32,
    pub queue_depth: u32,
}

/// The cluster as the master sees it, surfaced on the compute node heartbeat.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClusterDescriptor {
    pub master_id: String,
    pub slaves: Vec<SlaveDescriptor>,
    /// Total idle workers across the master and every slave.
    pub aggregate_workers_idle: u32,
}

macro_rules! impl_msgpack {
    ($($t:ty),+ $(,)?) => {
        $(impl $t {
            /// Encode as a msgpack map with named keys.
            pub fn to_msgpack(&self) -> Result<Vec<u8>, rmp_serde::encode::Error> {
                rmp_serde::to_vec_named(self)
            }
            /// Decode from a msgpack map.
            pub fn from_msgpack(bytes: &[u8]) -> Result<Self, rmp_serde::decode::Error> {
                rmp_serde::from_slice(bytes)
            }
        })+
    };
}

impl_msgpack!(ComputeJobRequest, ComputeJobStatus, ClusterDescriptor);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_default_is_master() {
        assert_eq!(ComputeRole::default(), ComputeRole::Master);
    }

    #[test]
    fn job_state_terminal() {
        assert!(ComputeJobState::Completed.is_terminal());
        assert!(ComputeJobState::Failed.is_terminal());
        assert!(ComputeJobState::Cancelled.is_terminal());
        assert!(!ComputeJobState::Queued.is_terminal());
        assert!(!ComputeJobState::Running.is_terminal());
    }

    #[test]
    fn job_request_round_trips() {
        let req = ComputeJobRequest {
            job_id: "job-1".into(),
            kind: ComputeJobKind::PerceptionOffload,
            dataset_ref: Some("live-sess-1".into()),
            params: serde_json::json!({ "model": "yolov8n", "fps": 6 }),
        };
        let back = ComputeJobRequest::from_msgpack(&req.to_msgpack().unwrap()).unwrap();
        assert_eq!(req, back);
        assert_eq!(back.kind, ComputeJobKind::PerceptionOffload);
    }

    #[test]
    fn gpu_default_is_all_null_with_snake_case_keys() {
        let v = serde_json::to_value(ComputeGpu::default()).unwrap();
        for key in [
            "name",
            "cores",
            "unified_memory_mb",
            "metal",
            "utilization_pct",
        ] {
            assert!(v.get(key).is_some(), "{key} key present");
            assert!(v[key].is_null(), "{key} is null by default");
        }
    }

    #[test]
    fn gpu_round_trips_a_populated_block() {
        let gpu = ComputeGpu {
            name: Some("Apple M1 Pro".into()),
            cores: Some(16),
            unified_memory_mb: Some(32768),
            metal: Some("Metal 3".into()),
            utilization_pct: Some(12.5),
        };
        let v = serde_json::to_value(&gpu).unwrap();
        assert_eq!(v["name"], "Apple M1 Pro");
        assert_eq!(v["cores"], 16);
        assert_eq!(v["unified_memory_mb"], 32768);
        assert_eq!(v["metal"], "Metal 3");
        assert_eq!(v["utilization_pct"], 12.5);
        let back: ComputeGpu = serde_json::from_value(v).unwrap();
        assert_eq!(back, gpu);
    }

    #[test]
    fn cluster_descriptor_round_trips() {
        let cluster = ClusterDescriptor {
            master_id: "node-master".into(),
            slaves: vec![SlaveDescriptor {
                node_id: "node-slave-1".into(),
                accelerators: vec!["cuda:0".into()],
                workers_idle: 2,
                queue_depth: 0,
            }],
            aggregate_workers_idle: 3,
        };
        let back = ClusterDescriptor::from_msgpack(&cluster.to_msgpack().unwrap()).unwrap();
        assert_eq!(cluster, back);
    }
}
