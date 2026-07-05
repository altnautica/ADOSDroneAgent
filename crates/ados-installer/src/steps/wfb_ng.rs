//! wfb-ng userspace build + bind-artifact provisioning.
//!
//! Delegates to `scripts/drivers/install-wfb-ng.sh` (mirrors how [`super::dkms`]
//! delegates to `install-rtl8812eu.sh`): the heavy build + provisioning is leaf
//! OS shell work the installer ORCHESTRATES, not reimplements. This step OWNS the
//! order (after `venv_agent`, which clones the source tree; before the radio
//! units `systemd` enables) + the VERIFY: it confirms the real outcome (the
//! wfb-ng binaries on PATH + the bind artifacts on disk), not the script's exit
//! code — the script is deliberately lenient on a librga-less BSP.
//!
//! Required: the supervisor's local-radio bind FSM
//! (`crates/ados-supervisor/src/bind/`) cannot start without `/etc/bind.key` +
//! `/etc/bind.yaml` + the wfb-ng binaries, so a fresh rig that skips this never
//! auto-pairs (the local-radio auto-bind fails with "upstream wfb-ng artifact
//! missing: /etc/bind.key"). This restores the radio-stack provisioning a prior
//! installer refactor dropped, so it now fails loudly here.

use std::path::Path;

use crate::ctx::Ctx;
use crate::env;
use crate::exec;
use crate::graph::{Step, StepKind, StepOutcome};
use crate::ui::activity;

/// The userspace binaries the bind protocol + radio services need on PATH.
const REQUIRED_BINS: &[&str] = &["wfb_tx", "wfb_rx", "wfb_keygen", "wfb-server"];
/// The bind artifacts the supervisor's bind FSM requires before it can run.
const BIND_ARTIFACTS: &[&str] = &["/etc/bind.key", "/etc/bind.yaml"];

/// True when `bin` resolves on PATH or in the two dirs `setup.py install` writes
/// console scripts to. Replaces a `command -v` shell-out (a builtin, not a
/// binary) so the verify never depends on `which` being present.
fn on_path(bin: &str) -> bool {
    if let Ok(path) = std::env::var("PATH") {
        if std::env::split_paths(&path).any(|dir| dir.join(bin).is_file()) {
            return true;
        }
    }
    Path::new("/usr/bin").join(bin).is_file() || Path::new("/usr/local/bin").join(bin).is_file()
}

/// wfb-ng userspace build + bind-artifact provisioning (delegated).
pub struct WfbNg;

impl Step for WfbNg {
    fn id(&self) -> &str {
        "wfb_ng"
    }
    fn requires(&self) -> &[&str] {
        // `venv_agent` clones the source tree (vendor/wfb-ng + the driver
        // script) and records `ctx.source_dir`; it transitively requires `deps`,
        // which installs the build toolchain (build-essential, python3-dev,
        // libpcap-dev, libsodium-dev).
        &["venv_agent"]
    }
    fn checkpoint(&self) -> Option<&str> {
        // No checkpoint: like `config_identity`, this runs on every install AND
        // upgrade. The delegated script is internally idempotent (it skips the
        // rebuild when wfb-ng is already at the vendored commit) and must
        // re-assert the bind artifacts on every run so an upgrade lands them.
        None
    }
    fn kind(&self) -> StepKind {
        // Hard gate: a rig without the radio stack cannot auto-pair, so a fresh
        // install that fails here must report `failed`, not a silent `ok`.
        StepKind::Required
    }
    fn run(&self, ctx: &mut Ctx) -> StepOutcome {
        let source = match env::resolve_source_dir(ctx.source_dir.as_deref()) {
            Some(s) => s,
            None => {
                return StepOutcome::Failed(
                    "source tree not resolved for the wfb-ng build".to_string(),
                )
            }
        };
        let script = source.join("scripts/drivers/install-wfb-ng.sh");
        if !script.is_file() {
            return StepOutcome::Failed(format!(
                "wfb-ng installer script not present at {}",
                script.display()
            ));
        }

        tracing::info!("building + provisioning wfb-ng (delegated)");
        let sink = ctx.progress.clone();
        let script_s = script.to_string_lossy();
        let res = exec::run_streamed("bash", &[script_s.as_ref()], |line| {
            sink.sub_log("wfb_ng", line);
            if let Some(a) = activity::wfb_activity(line) {
                sink.activity("wfb_ng", a);
            }
        });
        if !res.spawned {
            return StepOutcome::Failed(
                "bash not available to run the wfb-ng installer".to_string(),
            );
        }

        // Verify the REAL outcome, not the script's exit code (it returns 0 even
        // on a best-effort partial). The bind FSM needs all of these present.
        let missing_bins: Vec<&str> = REQUIRED_BINS
            .iter()
            .copied()
            .filter(|b| !on_path(b))
            .collect();
        let missing_artifacts: Vec<&str> = BIND_ARTIFACTS
            .iter()
            .copied()
            .filter(|p| !Path::new(p).exists())
            .collect();

        if missing_bins.is_empty() && missing_artifacts.is_empty() {
            tracing::info!("wfb-ng userspace + bind artifacts present");
            StepOutcome::Ok
        } else {
            StepOutcome::Failed(format!(
                "wfb-ng provisioning incomplete: missing binaries {:?}, missing artifacts {:?} \
                 (see /tmp/wfb-ng-build.log + /tmp/wfb-ng-install.log)",
                missing_bins, missing_artifacts
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn on_path_finds_a_ubiquitous_binary() {
        // `sh` is on PATH (or in /usr/bin) on every CI runner + dev host.
        assert!(on_path("sh"));
    }

    #[test]
    fn on_path_rejects_a_nonexistent_binary() {
        assert!(!on_path("definitely-not-a-real-binary-xyz"));
    }

    #[test]
    fn required_set_covers_the_bind_protocol_tools() {
        // wfb-server (the bind orchestrator) + wfb_keygen (per-pair key mint)
        // are the two the C-only partial-install trap was missing.
        assert!(REQUIRED_BINS.contains(&"wfb-server"));
        assert!(REQUIRED_BINS.contains(&"wfb_keygen"));
        assert!(BIND_ARTIFACTS.contains(&"/etc/bind.key"));
    }
}
