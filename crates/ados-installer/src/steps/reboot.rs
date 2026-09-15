//! Pending-reboot reconciliation: the consumer of `/run/ados/reboot-required`.
//!
//! Some provisioning cannot take effect without a reboot, because u-boot reads
//! the device tree only at boot: the CSI camera overlay
//! (`scripts/drivers/install-camera-overlay.sh`), the display overlay, and
//! `dtparam=i2c_arm=on` for the I2C OLED. Each of those writes a line to
//! `/run/ados/reboot-required` saying so.
//!
//! Nothing read that file. The install printed "installed", the operator was
//! never told a reboot was pending, and the camera simply did not work until
//! somebody rebooted for unrelated reasons — a manual post-install step, which
//! is a bug in the product and not a documented workaround.
//!
//! This step is that reader. It runs LAST (after `health`, like `watchdog`) and
//! does two things:
//!
//! * records the reasons on the context so the closing summary NAMES them, and
//! * decides whether the installer performs the single automatic reboot itself.
//!
//! The reboot is not issued here. A step that rebooted mid-graph would kill the
//! renderer before the operator saw the summary; the binary performs it after
//! the closing card is drawn, from [`crate::ctx::Ctx::pending_reboot`].
//!
//! `--no-reboot` defers it. In that case the step reports Failed so the install
//! lands `degraded` with the staged provisioning named, rather than reporting a
//! clean success for a node that is not yet in its provisioned state.

use std::path::Path;

use crate::ctx::Ctx;
use crate::graph::{Step, StepKind, StepOutcome};

/// Where the provisioners signal that their change needs a boot to take effect.
/// On tmpfs, so it is per-boot by construction: a reboot clears it.
pub const REBOOT_REQUIRED: &str = "/run/ados/reboot-required";

/// The reasons recorded in a `reboot-required` body, in file order, de-duplicated
/// and trimmed. Pure so the parse is testable without the run dir.
pub fn parse_reasons(body: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if !out.iter().any(|existing| existing == line) {
            out.push(line.to_string());
        }
    }
    out
}

/// Read the reasons from `path`. An absent or unreadable file is "nothing
/// pending", which is the normal case on a node with no overlay to stage.
pub fn read_reasons(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .map(|body| parse_reasons(&body))
        .unwrap_or_default()
}

/// Record that `reason` needs a reboot to take effect, appending one line to
/// [`REBOOT_REQUIRED`] (de-duplicated, so a re-run does not say it twice).
///
/// The same signal the camera/display overlay provisioners write from bash, so
/// every producer lands in one file with one reader. Best-effort: a run dir that
/// is not writable must not fail the step that was doing the real provisioning.
pub fn signal_required(reason: &str) {
    let path = Path::new(REBOOT_REQUIRED);
    if read_reasons(path).iter().any(|r| r == reason) {
        return;
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut body = std::fs::read_to_string(path).unwrap_or_default();
    if !body.is_empty() && !body.ends_with('\n') {
        body.push('\n');
    }
    body.push_str(reason);
    body.push('\n');
    if let Err(e) = std::fs::write(path, body) {
        tracing::warn!(error = %e, path = REBOOT_REQUIRED, reason, "could not signal the pending reboot");
    }
}

/// Classify a pending-reboot reading (pure). `Ok` when nothing is pending or
/// the installer will perform the reboot itself; `Failed` when `--no-reboot`
/// defers it, so the install lands `degraded` with the staged provisioning
/// named instead of reporting a clean success for a node that is not yet in
/// its provisioned state.
pub fn classify(reasons: &[String], no_reboot: bool) -> StepOutcome {
    if reasons.is_empty() {
        return StepOutcome::Ok;
    }
    if no_reboot {
        return StepOutcome::Failed(format!(
            "reboot deferred (--no-reboot); staged: {}",
            reasons.join(", ")
        ));
    }
    StepOutcome::Ok
}

/// Pending-reboot reconciliation step.
pub struct Reboot;

impl Step for Reboot {
    fn id(&self) -> &str {
        "reboot"
    }
    fn requires(&self) -> &[&str] {
        // After health: the reboot is the last thing that happens to the box, and
        // the health gate must have had its say on a running agent first.
        &["health"]
    }
    fn checkpoint(&self) -> Option<&str> {
        // No checkpoint: whether a reboot is pending is a per-run fact.
        None
    }
    fn kind(&self) -> StepKind {
        // Optional: a deferred reboot degrades the install (and is named), it
        // never aborts it.
        StepKind::Optional
    }
    fn run(&self, ctx: &mut Ctx) -> StepOutcome {
        let reasons = read_reasons(Path::new(REBOOT_REQUIRED));
        if reasons.is_empty() {
            return StepOutcome::Ok;
        }
        ctx.pending_reboot = reasons.clone();
        let outcome = classify(&reasons, ctx.args.no_reboot);
        if ctx.args.no_reboot {
            tracing::warn!(
                reasons = %reasons.join(", "),
                "provisioning staged that needs a reboot; --no-reboot given, deferring"
            );
        } else {
            // The binary reboots after the summary is drawn; see run_install.
            tracing::info!(
                reasons = %reasons.join(", "),
                "provisioning staged that needs a reboot; the installer will perform it"
            );
        }
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasons_are_trimmed_deduplicated_and_comment_free() {
        let body = "# staged by the provisioner\ncamera-overlay radxa-camera-13m-214\n\n  i2c_arm \n camera-overlay radxa-camera-13m-214\n";
        assert_eq!(
            parse_reasons(body),
            vec![
                "camera-overlay radxa-camera-13m-214".to_string(),
                "i2c_arm".to_string()
            ],
            "a re-run that appends the same line twice must not say it twice"
        );
    }

    #[test]
    fn an_absent_file_is_nothing_pending() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_reasons(&dir.path().join("reboot-required")).is_empty());
    }

    #[test]
    fn an_empty_file_is_nothing_pending() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reboot-required");
        std::fs::write(&path, "\n\n# nothing\n").unwrap();
        assert!(read_reasons(&path).is_empty());
    }

    #[test]
    fn nothing_pending_is_a_silent_success() {
        assert_eq!(classify(&[], false), StepOutcome::Ok);
        assert_eq!(classify(&[], true), StepOutcome::Ok);
    }

    #[test]
    fn a_pending_reboot_the_installer_will_perform_is_a_success() {
        let reasons = vec!["camera-overlay radxa-camera-13m-214".to_string()];
        assert_eq!(classify(&reasons, false), StepOutcome::Ok);
    }

    #[test]
    fn a_deferred_reboot_degrades_and_names_what_is_staged() {
        // An operator who passed --no-reboot must not be told the install is
        // clean while the camera overlay is staged and unbound.
        let reasons = vec![
            "camera-overlay radxa-camera-13m-214".to_string(),
            "i2c_arm".to_string(),
        ];
        match classify(&reasons, true) {
            StepOutcome::Failed(msg) => {
                assert!(msg.contains("camera-overlay radxa-camera-13m-214"), "{msg}");
                assert!(msg.contains("i2c_arm"), "{msg}");
                assert!(msg.contains("--no-reboot"), "{msg}");
            }
            other => panic!("a deferred reboot must degrade the install, got {other:?}"),
        }
    }

    #[test]
    fn the_step_runs_after_health_and_never_aborts_the_install() {
        assert_eq!(Reboot.requires(), &["health"]);
        assert_eq!(Reboot.kind(), StepKind::Optional);
        assert_eq!(Reboot.checkpoint(), None);
    }
}
