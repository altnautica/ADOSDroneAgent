//! NPU provision: on an RKNN-class board, install the NPU inference runtime,
//! provision the detector model, turn on the vision config, and enable the
//! NPU sidecar unit — so a fresh install brings the on-board vision pipeline up
//! with zero manual steps. Optional — a board without an NPU is a clean
//! `Skipped`, and any sub-step failure on an NPU board degrades (not aborts) the
//! install. Checkpoint `npu-provision`. Runs after `venv_agent` (the sidecar
//! wheel installs into the agent venv).
//!
//! What this step OWNS on an NPU board:
//!   1. the rknn-toolkit-lite2 wheel installed into the agent venv (the Python
//!      side of the sidecar; the wheel carries only the Python API),
//!   2. the librknnrt runtime library placed at `/usr/lib/librknnrt.so` (a
//!      separate artifact the wheel does NOT bundle; the sidecar unit gates on
//!      its presence),
//!   3. the configured detector model fetched into the model cache (best-effort
//!      — when no detector is configured yet, the runtime + config still
//!      provision and the model is deferred, surfaced honestly in the log),
//!   4. the vision config turned on (`vision.enabled = true`,
//!      `vision.backend = rknn`) via a real config load → merge → save so no
//!      other config field is dropped,
//!   5. the `ados-vision-rknn.service` unit ENABLED (the START is the
//!      supervisor's job; the unit is PartOf the supervisor and self-gates on
//!      the runtime library).
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
pub(crate) use rknn::read_board_model;
use rknn::{install_rknn_wheel, provision_librknnrt};
pub use rknn::{is_npu_board, librknnrt_url, wheel_cp_tag, wheel_filename, wheel_url};

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

/// Provision the on-board NPU inference path on RKNN-class boards.
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
    fn run(&self, _ctx: &mut Ctx) -> StepOutcome {
        let model = read_board_model();
        if !is_npu_board(&model) {
            tracing::info!(
                board = %model,
                "no NPU detected; skipping NPU runtime provisioning"
            );
            return StepOutcome::Skipped;
        }
        tracing::info!(board = %model, "NPU board detected; provisioning the inference runtime");

        // 1. The Python side of the sidecar (the rknn-toolkit-lite2 wheel).
        if let Err(e) = install_rknn_wheel() {
            return StepOutcome::Failed(e);
        }
        // 2. The runtime library the sidecar unit gates on.
        if let Err(e) = provision_librknnrt() {
            return StepOutcome::Failed(e);
        }
        // 3. The detector model (best-effort; a missing config is deferred, not
        //    a failure — the runtime + config still provision).
        if let Err(e) = fetch_detector_model() {
            return StepOutcome::Failed(e);
        }
        // 4. Turn the vision config on (load → merge → save, no field dropped).
        if let Err(e) = enable_vision_config() {
            return StepOutcome::Failed(e);
        }
        // 5. Enable the sidecar unit (the supervisor starts it).
        if let Err(e) = enable_sidecar_unit() {
            return StepOutcome::Failed(e);
        }

        tracing::info!("NPU inference runtime provisioned");
        StepOutcome::Ok
    }
}

#[cfg(test)]
mod tests;
