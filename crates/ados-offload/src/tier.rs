//! The perception tier the agent picks: where detection / tracking / SLAM runs.
//!
//! NPU-less boards are first-class: a board with no accelerator runs an
//! autonomous mission by offloading the heavy perception to a paired compute
//! node, not as a degraded mode. The agent picks the tier from three inputs —
//! the HAL accelerator capability, whether a compute node is paired on an
//! acceptable bearer, and whether the board can carry a light local path.

use serde::{Deserialize, Serialize};

/// Where the heavy perception (detection / tracking / SLAM) runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PerceptionTier {
    /// On the drone's own NPU / GPU (lowest latency).
    Local,
    /// Entirely on the paired compute node (NPU-less board, near-real-time).
    Offload,
    /// Light/fast work local (a small detector + fast VIO for the control loop),
    /// heavy work (the big detector + drift-corrected SLAM) on the node.
    Hybrid,
}

/// The inputs the agent reads to pick a perception tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TierInputs {
    /// The board has an NPU / GPU (`infer-capabilities`).
    pub has_accelerator: bool,
    /// The required models fit + run on the local accelerator.
    pub models_fit_locally: bool,
    /// A compute node is paired (local-first, over the LAN or a relay).
    pub compute_node_paired: bool,
    /// The bearer's latency is inside the offload budget for the mission.
    pub bearer_acceptable: bool,
    /// The board can run a light local detector + fast VIO (the control-loop
    /// path) even though it cannot run the heavy models.
    pub can_run_light_local: bool,
}

/// Pick the perception tier, or `None` when no heavy perception is available
/// (an NPU-less board with no usable compute node — it flies on bare odometry,
/// with no detection / tracking / map-based autonomy).
///
/// - An accelerator that fits the models ⇒ **Local** (the default).
/// - Otherwise, a paired node on an acceptable bearer ⇒ **Hybrid** when the
///   board can carry the light local path, else **Offload**.
/// - Otherwise, a board that can run a light local detector (no usable node) ⇒
///   **Local** (degraded: the small on-board detector only, no heavy remote).
/// - Otherwise ⇒ `None` (bare odometry, no detection / tracking / map autonomy).
pub fn pick_tier(inputs: &TierInputs) -> Option<PerceptionTier> {
    if inputs.has_accelerator && inputs.models_fit_locally {
        return Some(PerceptionTier::Local);
    }
    if inputs.compute_node_paired && inputs.bearer_acceptable {
        return Some(if inputs.can_run_light_local {
            PerceptionTier::Hybrid
        } else {
            PerceptionTier::Offload
        });
    }
    // No usable node: a board that can run a small local detector still has
    // some local perception (degraded local), not none.
    if inputs.can_run_light_local {
        return Some(PerceptionTier::Local);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs() -> TierInputs {
        TierInputs {
            has_accelerator: false,
            models_fit_locally: false,
            compute_node_paired: false,
            bearer_acceptable: false,
            can_run_light_local: false,
        }
    }

    #[test]
    fn an_accelerator_that_fits_the_models_is_local() {
        let mut i = inputs();
        i.has_accelerator = true;
        i.models_fit_locally = true;
        // Local wins even when a node is paired (lowest latency on-board).
        i.compute_node_paired = true;
        i.bearer_acceptable = true;
        assert_eq!(pick_tier(&i), Some(PerceptionTier::Local));
    }

    #[test]
    fn an_accelerator_whose_models_do_not_fit_offloads_when_a_node_is_paired() {
        let mut i = inputs();
        i.has_accelerator = true;
        i.models_fit_locally = false; // models too heavy for this accelerator
        i.compute_node_paired = true;
        i.bearer_acceptable = true;
        assert_eq!(pick_tier(&i), Some(PerceptionTier::Offload));
    }

    #[test]
    fn an_npu_less_board_with_a_paired_node_offloads() {
        let mut i = inputs();
        i.compute_node_paired = true;
        i.bearer_acceptable = true;
        assert_eq!(pick_tier(&i), Some(PerceptionTier::Offload));
    }

    #[test]
    fn a_board_that_can_carry_the_light_path_is_hybrid() {
        let mut i = inputs();
        i.compute_node_paired = true;
        i.bearer_acceptable = true;
        i.can_run_light_local = true;
        assert_eq!(pick_tier(&i), Some(PerceptionTier::Hybrid));
    }

    #[test]
    fn an_unacceptable_bearer_is_not_an_offload_path() {
        let mut i = inputs();
        i.compute_node_paired = true;
        i.bearer_acceptable = false; // too slow / saturated
        assert_eq!(pick_tier(&i), None);
    }

    #[test]
    fn a_light_local_board_with_no_node_runs_local_not_none() {
        let mut i = inputs();
        i.can_run_light_local = true; // a small on-board detector, no node
        assert_eq!(pick_tier(&i), Some(PerceptionTier::Local));
    }

    #[test]
    fn no_accelerator_no_node_no_light_local_is_none() {
        // Bare odometry only: no detection / tracking / map-based autonomy.
        assert_eq!(pick_tier(&inputs()), None);
    }
}
