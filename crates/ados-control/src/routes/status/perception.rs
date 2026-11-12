//! The perception capability and tier the status routes report.

use super::*;
use crate::ipc::VisionIpcClient;

/// The vision engine's backend name for its in-process ONNX runtime: the
/// detector runs on the companion's CPU.
const CPU_BACKEND: &str = "onnx";

/// Where the vision engine's live backend runs the detector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LiveDetector {
    /// No detector runs: the engine is unreachable (vision disabled, not up
    /// yet, or mid-restart), or its backend runs no real inference (the mock,
    /// or an unavailable backend).
    Off,
    /// A real backend on an accelerator (an NPU or accelerator sidecar).
    Accelerator,
    /// The in-process ONNX backend: the detector runs on the CPU.
    Cpu,
}

/// Derive the perception capability + tier from the board block (the same board
/// sidecar [`read_board`] returns) plus where the vision engine's live backend
/// runs its detector. `npu_tops` is the board's declared NPU throughput; the
/// tier is the canonical `ados_offload::pick_tier` decision (NOT a second
/// implementation), so `/api/status` reports the same tier the drone runs on.
///
/// The tier follows the detector the engine actually runs, never a board
/// declaration alone: a board can declare an NPU (`npu_tops > 0`) with no
/// in-tree backend for that silicon (e.g. the A733's VIP9000 has none), or a
/// CPU-ONNX profile on a binary built without the `onnx` feature, and
/// `select_backend` then resolves to `MockBackend`, which loads every model but
/// returns zero detections. So:
///
/// - a live accelerator backend reads `local`;
/// - a live ONNX backend reads `cpu`, whatever the board declares: the detector
///   runs on the CPU, so `none` would be false;
/// - no live detector reads `none`, or the offload tier when the reconciler's
///   offload link names a paired, reachable workstation.
///
/// The offload target stays null until a workstation is paired, never a
/// fabricated reach. Returns `(npu_tops, has_accelerator, tier,
/// offload_target)`. `has_accelerator` stays the raw hardware declaration: it
/// answers "does this board have NPU hardware", a distinct question from the
/// tier.
///
/// Shared with the consolidated `/api/status/full` route so both report the
/// same perception fields from the same live read.
pub(crate) fn perception_fields(
    board: &Value,
    detector: LiveDetector,
) -> (f64, bool, &'static str, Option<String>) {
    let npu_tops = board
        .get("npu_tops")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let has_accelerator = npu_tops > 0.0;
    // The live offload-link the reconciler writes: a paired, reachable
    // workstation flips compute_node_paired + bearer_acceptable true and names
    // the target. Absent / stale ⇒ no link (never a fabricated paired node).
    // Fed identically here and on the cloud heartbeat via
    // `TierInputs::for_drone`.
    let link = ados_protocol::offload_link::read_offload_link(now_epoch_ms());
    let (paired, bearer_ok) = link
        .as_ref()
        .map(|l| (l.paired, l.bearer_acceptable))
        .unwrap_or((false, false));
    let tier = ados_offload::pick_tier(&ados_offload::TierInputs::for_drone(
        detector == LiveDetector::Accelerator,
        detector == LiveDetector::Cpu,
        paired,
        bearer_ok,
    ));
    let tier_str = tier.map_or("none", ados_offload::PerceptionTier::as_str);
    // Surface the target only on an actual offload path (never a reach we are
    // not really using).
    let offload_target = link.filter(|l| l.is_offload_path()).and_then(|l| l.target);
    (npu_tops, has_accelerator, tier_str, offload_target)
}

/// Where the vision engine's loaded backend runs its detector, read LIVE over
/// `/run/ados/vision.sock`'s `vision.list_models` reply: the engine folds its
/// `backend` name and `backend_inference_capable` into that reply
/// unconditionally (see `VisionEngine::is_inference_capable`), so this is
/// answered even when no model is registered. An unreachable engine degrades
/// to [`LiveDetector::Off`]: with nothing loaded there is no live inference to
/// report, and a stale answer would be exactly the fabricated capability an
/// honest surface forbids.
///
/// Shared by `/api/status` and `/api/status/full` so both feed
/// [`perception_fields`] the same live signal.
pub(crate) async fn live_detector() -> LiveDetector {
    match VisionIpcClient::default_socket().list_models().await {
        Ok(reply) => detector_from_reply(&reply),
        Err(_) => LiveDetector::Off,
    }
}

/// The detector a `vision.list_models` reply describes.
pub(crate) fn detector_from_reply(reply: &rmpv::Value) -> LiveDetector {
    let field = |key: &str| {
        reply
            .as_map()
            .and_then(|m| m.iter().find(|(k, _)| k.as_str() == Some(key)))
            .map(|(_, v)| v)
    };
    let capable = field("backend_inference_capable")
        .and_then(rmpv::Value::as_bool)
        .unwrap_or(false);
    if !capable {
        return LiveDetector::Off;
    }
    match field("backend").and_then(rmpv::Value::as_str) {
        Some(CPU_BACKEND) => LiveDetector::Cpu,
        _ => LiveDetector::Accelerator,
    }
}
