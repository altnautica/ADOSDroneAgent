//! Start: start the supervisor unit (which brings up the profile's service
//! set), and on a ground station also `--no-block` start the GS unit set.
//! Required. Runs only after the units are installed AND the binaries they
//! exec are present.
//!
//! THE ORDERING INVARIANT lives in this step's `requires`: it depends on both
//! `systemd` (units deployed + enabled) AND `fetch_binaries` (the prebuilt
//! `ados-supervisor` binary on disk). The bash installer's bug was starting the
//! supervisor inside `install_systemd_service` BEFORE the binaries were
//! guaranteed present; splitting the restart into this separately-gated step is
//! the fix. This is the ONLY place the supervisor is started.

use std::path::Path;

use crate::ctx::Ctx;
use crate::env::{CONFIG_DIR, SERVICE_NAME};
use crate::exec;
use crate::graph::{Step, StepKind, StepOutcome};

/// Start the top-level supervisor unit (+ the GS unit set on a ground station).
pub struct Start;

impl Step for Start {
    fn id(&self) -> &str {
        "start"
    }
    fn requires(&self) -> &[&str] {
        &["systemd", "fetch_binaries"]
    }
    fn checkpoint(&self) -> Option<&str> {
        None
    }
    fn kind(&self) -> StepKind {
        StepKind::Required
    }
    fn run(&self, ctx: &mut Ctx) -> StepOutcome {
        // Enable is idempotent (the systemd step already enabled it); re-affirm
        // so a re-run from a partial state self-heals.
        let _ = exec::run("systemctl", &["enable", SERVICE_NAME]);

        // The ONLY supervisor start in the whole install. The binary is
        // guaranteed present because this step requires fetch_binaries.
        let res = exec::run("systemctl", &["restart", SERVICE_NAME]);
        if !res.success() {
            if !res.spawned {
                return StepOutcome::Failed(
                    "systemctl not available to start the supervisor".to_string(),
                );
            }
            return StepOutcome::Failed(format!(
                "starting {SERVICE_NAME} failed: {}",
                res.stderr.trim()
            ));
        }
        tracing::info!(unit = SERVICE_NAME, "supervisor started");

        // The logging and telemetry store is PartOf the supervisor, so the
        // restart above stopped it; bring it back when `logging.store.enabled`
        // asks for it (the systemd step masks it otherwise). Cross-profile.
        let config_body = std::fs::read_to_string(Path::new(CONFIG_DIR).join("config.yaml")).ok();
        if crate::steps::systemd::logd_unit_wanted(config_body.as_deref()) {
            let _ = exec::run("systemctl", &["start", "--no-block", "ados-logd.service"]);
            tracing::info!(
                unit = "ados-logd.service",
                "logging store started (--no-block)"
            );
        }

        // Kick the profile's start set (the unit table's `start_on_install`
        // rows) with --no-block:
        // - the native plugin host and the control front are PartOf the
        //   supervisor, so the restart above cycles them, but a unit left
        //   inactive before this install (or stopped on its way out by the
        //   previous release's supervisor, which stopped every unit it had
        //   adopted) needs the explicit start; the health gate waits for the
        //   control front;
        // - on a ground station, the GS unit set: several are
        //   WantedBy=multi-user.target, which a supervisor restart never pulls
        //   in, so this brings them up now rather than at the next boot.
        // Best-effort: a unit that is not deployed / not enabled is a no-op,
        // and a ConditionPathExists keeps a box whose binary has not landed
        // inactive rather than restart-looping.
        for unit in crate::steps::systemd::start_units(&ctx.profile) {
            let _ = exec::run("systemctl", &["start", "--no-block", unit]);
            tracing::info!(unit, "unit started (--no-block)");
        }

        StepOutcome::Ok
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn gs_start_set_has_the_receive_ap_and_pairing_units() {
        let gs = crate::steps::systemd::start_units("ground_station");
        // The supervisor is restarted above, never kicked again here.
        assert!(!gs.contains(&"ados-supervisor.service"));
        assert!(gs.contains(&"ados-wfb-rx.service"));
        assert!(gs.contains(&"ados-hostapd.service"));
        assert!(gs.contains(&"ados-setup-captive.service"));
        // Every REST pairing call reaches this daemon over its socket; a
        // ground station that never starts it cannot pair anything.
        assert!(gs.contains(&"ados-mesh-pairing.service"));
        // The front and plugin host come back on every profile.
        for profile in ["drone", "ground_station", "workstation", "compute"] {
            let set = crate::steps::systemd::start_units(profile);
            assert!(set.contains(&"ados-control.service"), "{profile}");
            assert!(set.contains(&"ados-plugin-host.service"), "{profile}");
        }
    }
}
