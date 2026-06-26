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
mod cluster;
mod engine;
mod offload;
mod reconstructor;
mod scheduler;
mod store;

pub use api::{build_router, ApiState};
pub use cluster::Cluster;
pub use engine::{ComputeHeartbeat, Engine};
pub use offload::{Detection, Detector, FrameRef, MockDetector};
pub use reconstructor::{MockReconstructor, ReconstructOutput, Reconstructor};
pub use scheduler::{JobOutcome, Scheduler};
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
