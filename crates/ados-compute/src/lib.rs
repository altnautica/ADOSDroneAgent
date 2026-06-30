//! Compute-node engine.
//!
//! The Rust core of the compute profile: a SQLite-backed job store, a queue and
//! scheduler with a worker model, the reconstructor and perception-offload
//! traits (with mock backends used in CI), and the master/slave cluster. It is
//! the heavy-compute substrate a drone or GCS pairs with to run reconstruction
//! (gaussian splat / point cloud / mesh / occupancy) and perception offload for
//! NPU-less drones.
//!
//! Real reconstructors and detectors are third-party binaries the workers shell
//! out to, behind the [`Reconstructor`] and [`Detector`] traits. The mock
//! backends keep the whole engine testable with no GPU, no camera, and no
//! network. The job and cluster wire types live in
//! [`ados_protocol::compute`]; this crate owns the store, the scheduler, and
//! the backends.

mod api;
mod auth;
mod backends;
mod client;
mod cluster;
mod engine;
mod heartbeat_sidecar;
mod ingest;
/// mDNS advertise (the compute node) + resolve (a drone-side caller browsing for
/// a `profile=workstation` node). Public so a consumer crate can reach
/// `ados_compute::mdns::resolve_compute` directly, mirroring
/// `ados_groundlink::mdns`.
pub mod mdns;
mod offload;
mod pipeline;
mod reconstructor;
mod rerun_log;
mod scheduler;
mod session;
mod store;

pub use api::{build_router, ApiState, CancelResponse, SubmitResponse};
pub use auth::{require_pairing, ComputeAuth, PairingGate, RateLimiter, DEFAULT_PAIRING_PATH};
pub use backends::{
    file_uri_to_path, is_tool_available, parse_gaussian_count, path_to_file_uri,
    select_reconstructor, CliReconstructor, ReconstructCommand, ReconstructorKind,
};
pub use client::{ClientError, ComputeClient};
pub use cluster::Cluster;
pub use engine::{ComputeHeartbeat, Engine};
pub use heartbeat_sidecar::{
    write_compute_heartbeat, write_compute_heartbeat_to, ComputeHeartbeatSidecar, SlaveEntry,
    COMPUTE_HEARTBEAT_SIDECAR,
};
pub use ingest::AtlasIngest;
pub use mdns::{advertise_compute, resolve_compute, ComputeAdvert};
pub use offload::{Detection, Detector, FrameRef, MockDetector};
pub use pipeline::{stage_index_of, Pipeline, PipelineRunner, PipelineStage};
pub use reconstructor::{MockReconstructor, ReconstructOutput, Reconstructor};
pub use rerun_log::{
    log_keyframe, log_mesh, log_occupancy, log_pointcloud, log_splat, RerunArchetype,
    RerunLogEntry, RerunRecording,
};
pub use scheduler::{BackendResult, JobOutcome, Prepared, PreparedInput, Scheduler};
pub use session::{DeltaProducer, LiveSession, LiveSessionState, MockDeltaProducer, SplatDelta};
pub use store::{Dataset, JobRecord, JobStore, Output};

// Re-export the shared wire contract so callers get one import surface.
pub use ados_protocol::compute::{
    ClusterDescriptor, ComputeJobKind, ComputeJobRequest, ComputeJobState, ComputeJobStatus,
    ComputeRole, SlaveDescriptor,
};

/// Errors from the compute engine.
#[derive(Debug, thiserror::Error)]
pub enum ComputeError {
    /// The job store failed.
    #[error("store: {0}")]
    Store(#[from] rusqlite::Error),
    /// A params or result value failed to (de)serialize.
    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),
    /// A reconstructor or detector backend failed.
    #[error("backend {backend}: {message}")]
    Backend { backend: String, message: String },
    /// A job, dataset, or output id was not found.
    #[error("not found: {0}")]
    NotFound(String),
    /// An id already exists (a duplicate submit) — distinct from a store fault.
    #[error("conflict: {0}")]
    Conflict(String),
    /// The job kind does not match the backend it was dispatched to.
    #[error("wrong job kind for {0}")]
    WrongKind(String),
}
