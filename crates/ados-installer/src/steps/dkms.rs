//! DKMS: build + install the RTL8812EU WFB radio kernel module by delegating
//! to the battle-tested `scripts/drivers/install-rtl8812eu.sh`. Optional —
//! matches the bash installer (a rig with no RTL adapter, or a kernel with no
//! headers, degrades rather than failing). Checkpoint `radio-driver`.
//!
//! The driver script carries the load-bearing `ulimit -s unlimited` fix (the
//! BSP-header parse stack overflow), the DKMS fast-path, the mesh-enable patch,
//! and the arch translation (`uname -m` aarch64 → kernel `arm64`). Re-porting
//! that is exactly the kind of heavy OS logic Rust delegates rather than
//! rewrites. We only OWN the ORDER (this step runs after `deps`) + the verify:
//! the checkpoint marks only when the module is actually present afterwards,
//! mirroring the bash `run_health_gate` radio verify
//! (`lsmod | grep 8812eu || modinfo 8812eu`).

use std::path::Path;

use crate::ctx::Ctx;
use crate::env;
use crate::exec;
use crate::graph::{Step, StepKind, StepOutcome};
use crate::ui::activity;

/// The RTL8812EU module name the driver build exposes.
const MODULE_NAME: &str = "8812eu";

/// The path the driver script writes the source sentinel to on success
/// (`prebuilt` | `dkms`). The result builder reads this back.
const WFB_MODULE_SOURCE_FILE: &str = "/run/ados/wfb-module-source";

/// True when the RTL8812EU module is present — loaded (`lsmod`) or at least
/// resolvable on disk (`modinfo`). Mirrors the bash health-gate radio verify.
fn module_present() -> bool {
    let lsmod = exec::run("lsmod", &[]);
    if lsmod.success()
        && lsmod
            .stdout
            .lines()
            .any(|l| l.split_whitespace().next() == Some(MODULE_NAME))
    {
        return true;
    }
    exec::run_ok("modinfo", &[MODULE_NAME])
}

/// RTL8812EU DKMS build + install (delegated).
pub struct Dkms;

impl Step for Dkms {
    fn id(&self) -> &str {
        "dkms"
    }
    fn requires(&self) -> &[&str] {
        &["deps"]
    }
    fn checkpoint(&self) -> Option<&str> {
        Some("radio-driver")
    }
    fn kind(&self) -> StepKind {
        StepKind::Optional
    }
    fn run(&self, ctx: &mut Ctx) -> StepOutcome {
        // Opt-out: a node with no long-range radio (workstation / compute) or an
        // explicit operator choice skips the driver build. Returning Skipped keeps
        // the rtl_regulatory dependency satisfied; because the graph marks the
        // `radio-driver` checkpoint on a skip too, a later opt-IN re-run needs
        // `--upgrade`/`--force` (both clear checkpoints).
        if !ctx.install_rtl8812eu {
            tracing::info!("RTL8812EU driver install skipped (--no-rtl-driver)");
            return StepOutcome::Skipped;
        }
        // Resolve the driver script under the source tree the clone recorded.
        let source = match env::resolve_source_dir(ctx.source_dir.as_deref()) {
            Some(s) => s,
            None => {
                tracing::warn!("no source dir resolved; skipping RTL8812EU driver build");
                // Optional: degrade, not abort. Returning Failed records an
                // optional failure so the result shows the radio is absent.
                return StepOutcome::Failed("driver source tree not found".to_string());
            }
        };
        let script = source.join("scripts/drivers/install-rtl8812eu.sh");
        if !script.is_file() {
            tracing::warn!(
                script = %script.display(),
                "RTL8812EU installer not found; skipping driver build"
            );
            return StepOutcome::Failed("RTL8812EU installer script not present".to_string());
        }

        // Delegate. The script sets ARCH itself from `uname -m`, applies the
        // mesh patch, runs DKMS under `ulimit -s unlimited`, and writes the
        // wfb-module-source sentinel on success. We do not pass extra env.
        let script_s = script.to_string_lossy();
        tracing::info!("running RTL8812EU DKMS installer");
        let sink = ctx.progress.clone();
        let res = exec::run_streamed("bash", &[script_s.as_ref()], |line| {
            sink.sub_log("dkms", line);
            if let Some(a) = activity::dkms_activity(line) {
                sink.activity("dkms", a);
            }
        });
        if !res.spawned {
            return StepOutcome::Failed("bash not available to run the driver script".to_string());
        }

        // Verify the real outcome, not just the exit code: the bash health
        // gate trusts the module presence, not the script's return.
        if module_present() {
            tracing::info!("RTL8812EU module present after install");
            // Belt-and-suspenders: if the script did not drop the sentinel (an
            // older script, or it built but did not write), record `dkms` so
            // the result's wfbModuleSource is accurate.
            ensure_module_source_sentinel();
            StepOutcome::Ok
        } else {
            tracing::warn!(
                code = ?res.code,
                "RTL8812EU module not present after driver install; recording optional failure"
            );
            // Drop a stale/optimistic sentinel so the result reports the radio
            // as absent (wfbModuleSource empty) rather than a phantom value.
            let _ = std::fs::remove_file(WFB_MODULE_SOURCE_FILE);
            StepOutcome::Failed("RTL8812EU kernel module not present after install".to_string())
        }
    }
}

/// Ensure `/run/ados/wfb-module-source` exists when the module is present.
/// The driver script normally writes it; this only backfills `dkms` when it is
/// missing so the result's `wfbModuleSource` field is never blank on a built rig.
fn ensure_module_source_sentinel() {
    let path = Path::new(WFB_MODULE_SOURCE_FILE);
    if path.exists() {
        return;
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, "dkms\n");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checkpoint::Checkpoint;
    use crate::cli::Args;
    use crate::env::EnvInfo;

    #[test]
    fn opt_out_skips_the_driver_build() {
        // `--no-rtl-driver` makes the step return Skipped before touching the
        // source tree, so the rtl_regulatory dependency stays satisfied and no
        // driver is built on a node with no long-range radio.
        let args = Args {
            no_rtl_driver: true,
            ..Args::default()
        };
        let mut ctx = Ctx::from_args(args, EnvInfo::probe(), Checkpoint::new());
        assert!(!ctx.install_rtl8812eu);
        assert_eq!(Dkms.run(&mut ctx), StepOutcome::Skipped);
    }
}
