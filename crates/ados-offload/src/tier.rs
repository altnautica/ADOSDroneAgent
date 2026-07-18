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
    /// The board has no NPU but a CPU strong enough to run the detector locally
    /// via the in-process ONNX backend (declared by the board profile, e.g. a
    /// Cortex-A76-class SoC). A distinct signal from `has_accelerator`: it is a
    /// full local compute path, so it resolves to `Local` the same way an
    /// accelerator does — an NPU-less-but-CPU-strong board runs detection
    /// on-board rather than offloading. Only true when the board genuinely
    /// declares it (rule 44), so it never fabricates a local path.
    pub local_inference_capable: bool,
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

impl TierInputs {
    /// The tier inputs a drone-profile status surface builds each read: the local
    /// accelerator picture (an NPU, or a CPU strong enough for the ONNX detector)
    /// plus the offload-link signal (a paired, reachable workstation).
    /// `models_fit_locally` is held `true` for an accelerator board (an NPU board
    /// is assumed to fit its recommended detector — there is no model-fit probe
    /// yet, so this is a documented assumption, not a fabricated signal) and
    /// `can_run_light_local` `false` (no split light-detector + fast-VIO path runs
    /// today). Both status call sites (`/api/status`, the cloud heartbeat) use
    /// this so they feed `pick_tier` identically and cannot drift.
    pub fn for_drone(
        has_accelerator: bool,
        local_inference_capable: bool,
        compute_node_paired: bool,
        bearer_acceptable: bool,
    ) -> Self {
        TierInputs {
            has_accelerator,
            local_inference_capable,
            models_fit_locally: true,
            compute_node_paired,
            bearer_acceptable,
            can_run_light_local: false,
        }
    }
}

/// Pick the perception tier, or `None` when no heavy perception is available
/// (an NPU-less board with no usable compute node — it flies on bare odometry,
/// with no detection / tracking / map-based autonomy).
///
/// - A local compute path that fits the models ⇒ **Local** (the default): an
///   accelerator (NPU / GPU), or a CPU strong enough to run the detector via the
///   in-process ONNX backend (`local_inference_capable`). Either is a full
///   on-board path, so it wins over an available node (lowest latency).
/// - Otherwise, a paired node on an acceptable bearer ⇒ **Hybrid** when the
///   board can carry the light local path, else **Offload**.
/// - Otherwise, a board that can run a light local detector (no usable node) ⇒
///   **Local** (degraded: the small on-board detector only, no heavy remote).
/// - Otherwise ⇒ `None` (bare odometry, no detection / tracking / map autonomy).
pub fn pick_tier(inputs: &TierInputs) -> Option<PerceptionTier> {
    if (inputs.has_accelerator && inputs.models_fit_locally) || inputs.local_inference_capable {
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
            local_inference_capable: false,
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
    fn a_cpu_onnx_capable_board_runs_local_without_an_accelerator() {
        // A board with no NPU but a CPU strong enough for the in-process ONNX
        // detector is a full local path: it runs detection on-board.
        let mut i = inputs();
        i.has_accelerator = false;
        i.local_inference_capable = true;
        assert_eq!(pick_tier(&i), Some(PerceptionTier::Local));
        // Local wins even when a node is paired (lowest latency on-board).
        i.compute_node_paired = true;
        i.bearer_acceptable = true;
        assert_eq!(pick_tier(&i), Some(PerceptionTier::Local));
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

    #[test]
    fn for_drone_maps_the_offload_link_to_the_expected_tier() {
        // NPU-less, not CPU-inference-capable + a paired reachable workstation ⇒ offload.
        assert_eq!(
            pick_tier(&TierInputs::for_drone(false, false, true, true)),
            Some(PerceptionTier::Offload)
        );
        // NPU-less + no link ⇒ none (honest: no offload path).
        assert_eq!(
            pick_tier(&TierInputs::for_drone(false, false, false, false)),
            None
        );
        // An accelerator board runs local regardless of a link.
        assert_eq!(
            pick_tier(&TierInputs::for_drone(true, false, true, true)),
            Some(PerceptionTier::Local)
        );
        // A CPU-ONNX-capable board runs local regardless of a link (no offload).
        assert_eq!(
            pick_tier(&TierInputs::for_drone(false, true, true, true)),
            Some(PerceptionTier::Local)
        );
        assert_eq!(
            pick_tier(&TierInputs::for_drone(false, true, false, false)),
            Some(PerceptionTier::Local)
        );
        // A paired node on an unacceptable bearer is not an offload path.
        assert_eq!(
            pick_tier(&TierInputs::for_drone(false, false, true, false)),
            None
        );
    }
}
