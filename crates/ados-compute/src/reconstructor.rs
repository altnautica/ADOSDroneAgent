//! The reconstructor backend: turns a keyframe-bag dataset into a world-model
//! artifact (gaussian splat / point cloud / mesh). Real backends shell out to
//! third-party tools (Brush / nerfstudio / COLMAP / WebODM); the mock keeps the
//! engine testable with no GPU.

use serde::{Deserialize, Serialize};

use crate::{ComputeError, Dataset};

/// What a reconstruction produced.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReconstructOutput {
    /// Artifact kind (e.g. `splat`, `pointcloud`, `mesh`).
    pub kind: String,
    /// Where the artifact can be fetched.
    pub uri: String,
    /// Gaussian count for a splat (0 for non-splat artifacts).
    pub gaussian_count: u64,
    /// The concrete backend that produced this artifact. `mock` is a
    /// deterministic placeholder (no GPU, CI / no-backend node) and is NEVER a
    /// real world model; a real reconstruction carries its tool name
    /// (`brush` / `nerfstudio` / `colmap` / `webodm`). Stamped by the backend
    /// that actually ran so it survives the selecting-reconstructor indirection
    /// and reaches the client for an honest-placeholder badge.
    pub backend: String,
}

/// A reconstruction backend. Implementations are `Send + Sync` so a worker
/// pool can share one behind a trait object.
pub trait Reconstructor: Send + Sync {
    /// A stable name for logs and the job result.
    fn name(&self) -> &str;

    /// Reconstruct `dataset` into an artifact. `params` carries backend options
    /// (quality preset, output format).
    fn reconstruct(
        &self,
        dataset: &Dataset,
        params: &serde_json::Value,
    ) -> Result<ReconstructOutput, ComputeError>;
}

/// A no-GPU reconstructor that returns a deterministic fake splat. Used in CI
/// and on a node with no real backend installed, so the queue, scheduler, and
/// output paths are exercised end to end without a GPU.
#[derive(Debug, Default, Clone, Copy)]
pub struct MockReconstructor;

impl Reconstructor for MockReconstructor {
    fn name(&self) -> &str {
        "mock"
    }

    fn reconstruct(
        &self,
        dataset: &Dataset,
        _params: &serde_json::Value,
    ) -> Result<ReconstructOutput, ComputeError> {
        Ok(ReconstructOutput {
            kind: "splat".into(),
            uri: format!("mock://splat/{}", dataset.id),
            gaussian_count: 1000,
            backend: "mock".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_produces_a_deterministic_splat() {
        let ds = Dataset {
            id: "ds-7".into(),
            kind: "bag".into(),
            created_ms: 0,
            meta: serde_json::json!({}),
        };
        let out = MockReconstructor
            .reconstruct(&ds, &serde_json::json!({}))
            .unwrap();
        assert_eq!(out.kind, "splat");
        assert_eq!(out.uri, "mock://splat/ds-7");
        assert_eq!(out.gaussian_count, 1000);
        // The mock stamps its backend so a client can badge it as a placeholder,
        // never mistaking the deterministic fake for a real world model.
        assert_eq!(out.backend, "mock");
        assert_eq!(MockReconstructor.name(), "mock");
    }
}
