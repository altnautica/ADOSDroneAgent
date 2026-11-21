//! NPU provision: on an RKNN-class board, install the NPU inference runtime,
//! provision the detector model, turn on the vision config, and enable the
//! NPU sidecar unit — so a fresh install brings the on-board vision pipeline up
//! with zero manual steps. On a CUDA-GPU board (a Jetson) it provisions the
//! detector model and names the `onnx-cuda` vision backend, with no NPU runtime
//! and no sidecar. Optional — a board without either is a clean `Skipped`, and
//! any sub-step failure on an accelerator board degrades (not aborts) the
//! install. Checkpoint `npu-provision`. Runs after `venv_agent` (the sidecar
//! wheel installs into the agent venv).
//!
//! What this step OWNS on an NPU board:
//!   1. the rknn-toolkit-lite2 wheel installed into the agent venv (the Python
//!      side of the sidecar; the wheel carries only the Python API),
//!   2. the librknnrt runtime library placed at `/usr/lib/librknnrt.so` (a
//!      separate artifact the wheel does NOT bundle; the sidecar unit gates on
//!      its presence),
//!   3. the detector model fetched into the model cache (best-effort — when no
//!      detector runs on this board yet, the runtime + config still provision
//!      and the model is deferred, surfaced honestly in the log). A `.rknn`
//!      file built for the board's RKNN platform is preferred; a platform the
//!      registry has no `.rknn` file for takes the ONNX detector on the CPU,
//!      never no detector,
//!   4. the vision config turned on (`vision.enabled = true`, `vision.backend`
//!      = the fetched detector's runtime) via a real config load → merge → save
//!      so no other config field is dropped; the fetch step reads that backend
//!      to install the onnx-enabled vision build for an ONNX fallback,
//!   5. the `ados-vision-rknn.service` unit ENABLED when the backend is `rknn`
//!      (the START is the supervisor's job; the unit is PartOf the supervisor
//!      and self-gates on the runtime library).
//!
//! On a CUDA board only steps 3 and 4 run: the ONNX detector, and
//! `vision.backend = onnx-cuda`, which the vision engine runs on ONNX Runtime's
//! CUDA execution provider, or on the CPU when the runtime library has none.
//! The fetch step installs the onnx-enabled vision build for it.
//!
//! The detection (board substring match) and the URL builders are pure so a
//! unit test exercises them without a board, the network, or an interpreter.

use std::path::Path;

use ados_term::exec;

use crate::ctx::Ctx;
use crate::graph::{Step, StepKind, StepOutcome};

mod detector;
mod rknn;

use detector::{enable_vision_config, fetch_detector_model};
pub(crate) use rknn::read_board_identity;
use rknn::{install_rknn_wheel, provision_librknnrt};
pub use rknn::{is_npu_board, librknnrt_url, wheel_cp_tag, wheel_filename, wheel_url};

/// The inference accelerator a board's detector is provisioned for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Accelerator {
    /// A Rockchip NPU, driven through the RKNN sidecar.
    Rknn,
    /// An NVIDIA CUDA GPU (a Jetson), driven through the in-process ONNX
    /// backend's CUDA execution provider.
    Cuda,
}

impl Accelerator {
    /// The `vision.backend` a detector fetched for model `runtime` (`rknn` or
    /// `onnx`) runs on: an NPU board runs a `.rknn` file on the sidecar and
    /// falls back to the ONNX CPU backend for an `.onnx` file; a CUDA board
    /// runs every `.onnx` file on `onnx-cuda`. Pure.
    pub fn detector_backend(self, runtime: &str) -> &'static str {
        match (self, runtime) {
            (Accelerator::Rknn, "rknn") => "rknn",
            (Accelerator::Rknn, _) => "onnx",
            (Accelerator::Cuda, _) => "onnx-cuda",
        }
    }

    /// The `vision.backend` written when no detector was written now and the
    /// config names none.
    pub fn default_backend(self) -> &'static str {
        match self {
            Accelerator::Rknn => "rknn",
            Accelerator::Cuda => "onnx-cuda",
        }
    }
}

/// Board-model substrings that identify a CUDA-GPU board (the Jetson Orin
/// Nano), matched case-insensitively against the board string: its display
/// name, its profile stem (the `board_override` grammar) and the Tegra234 SoC
/// token in the device-tree `compatible`.
const CUDA_BOARD_SUBSTRINGS: &[&str] = &["orin nano", "jetson-orin-nano", "tegra234"];

/// True when the board string names a CUDA-GPU board. Pure.
pub fn is_cuda_board(model: &str) -> bool {
    let m = model.to_lowercase();
    CUDA_BOARD_SUBSTRINGS.iter().any(|k| m.contains(k))
}

/// The accelerator the board's detector is provisioned for, if any. Pure.
pub fn board_accelerator(model: &str) -> Option<Accelerator> {
    if is_npu_board(model) {
        Some(Accelerator::Rknn)
    } else if is_cuda_board(model) {
        Some(Accelerator::Cuda)
    } else {
        None
    }
}

/// Enable the NPU sidecar unit (the START is the supervisor's job; the unit is
/// PartOf the supervisor and self-gates on the runtime library). Returns an
/// actionable error string when the enable fails.
fn enable_sidecar_unit() -> Result<(), String> {
    const UNIT: &str = "ados-vision-rknn.service";
    let res = exec::run("systemctl", &["enable", UNIT]);
    if res.success() {
        Ok(())
    } else if !res.spawned {
        Err("systemctl is not available to enable the NPU sidecar unit".to_string())
    } else {
        Err(format!("enabling {UNIT} failed: {}", res.stderr.trim()))
    }
}

/// Set a file's permission bits (Unix only; a no-op elsewhere).
fn set_mode(path: &Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
}

/// Provision the on-board inference path on RKNN-class NPU boards and CUDA-GPU
/// boards.
pub struct NpuProvision;

impl Step for NpuProvision {
    fn id(&self) -> &str {
        "npu_provision"
    }
    fn requires(&self) -> &[&str] {
        &["venv_agent"]
    }
    fn checkpoint(&self) -> Option<&str> {
        Some("npu-provision")
    }
    fn kind(&self) -> StepKind {
        StepKind::Optional
    }
    fn run(&self, ctx: &mut Ctx) -> StepOutcome {
        if let Some(skip) = crate::host::personal_skip(ctx, "npu_provision") {
            return skip;
        }
        let model = read_board_identity();
        let Some(accel) = board_accelerator(&model) else {
            tracing::info!(
                board = %model,
                "no NPU or CUDA GPU detected; skipping inference runtime provisioning"
            );
            return StepOutcome::Skipped;
        };
        tracing::info!(
            board = %model,
            accelerator = ?accel,
            "accelerator board detected; provisioning the inference path"
        );

        if accel == Accelerator::Rknn {
            // 1. The Python side of the sidecar (the rknn-toolkit-lite2 wheel).
            if let Err(e) = install_rknn_wheel() {
                return StepOutcome::Failed(e);
            }
            // 2. The runtime library the sidecar unit gates on.
            if let Err(e) = provision_librknnrt() {
                return StepOutcome::Failed(e);
            }
        }
        // 3. The detector model (best-effort; nothing runnable is deferred, not
        //    a failure — the runtime + config still provision).
        let detector_backend = match fetch_detector_model(&model) {
            Ok(runtime) => runtime.map(|r| accel.detector_backend(&r)),
            Err(e) => return StepOutcome::Failed(e),
        };
        // 4. Turn the vision config on (load → merge → save, no field dropped).
        let backend = match enable_vision_config(detector_backend, accel.default_backend()) {
            Ok(b) => b,
            Err(e) => return StepOutcome::Failed(e),
        };
        // 5. Enable the sidecar unit (the supervisor starts it) when the
        //    detector runs on the NPU (`auto` resolves to it on an NPU board).
        if accel == Accelerator::Rknn && matches!(backend.as_str(), "rknn" | "auto") {
            if let Err(e) = enable_sidecar_unit() {
                return StepOutcome::Failed(e);
            }
        } else {
            tracing::info!(
                backend = %backend,
                "the vision backend does not use the NPU sidecar; the sidecar stays disabled"
            );
        }

        tracing::info!(accelerator = ?accel, "inference path provisioned");
        StepOutcome::Ok
    }
}

#[cfg(test)]
mod tests;
