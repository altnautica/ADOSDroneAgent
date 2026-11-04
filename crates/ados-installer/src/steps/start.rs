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
use crate::graph::{Step, StepKind, StepOutcome};
use ados_term::exec::{self, CmdResult};

/// Bring the supervisor up on `profile`'s unit set: stop it, stop the other
/// profile's units, then start it. Returns the start's result. The runner is
/// injected so the order is testable without a host systemd.
///
/// Never a `restart`, for two reasons that together kept a ground station's
/// media server running on a node reinstalled as a drone, holding the ports
/// the drone's own media server binds:
/// - the supervisor running until this step is the previous profile's, and it
///   restarts any unit it supervises the moment that unit dies, so the other
///   profile's units can only be stopped for good once it is down;
/// - a restart propagates to every `PartOf=` unit as a restart of whatever is
///   still active, so a unit revived in that window survives it, and the new
///   supervisor never stops a unit outside its profile.
///
/// The stop propagates to every `PartOf=` unit; the explicit stops cover the
/// other profile's units that are not `PartOf=` the supervisor. The start pulls
/// in only what is enabled, and the systemd step has already disabled them.
pub(crate) fn cycle_supervisor(
    profile: &str,
    systemctl: &mut dyn FnMut(&[&str]) -> CmdResult,
) -> CmdResult {
    let _ = systemctl(&["stop", SERVICE_NAME]);
    for unit in crate::steps::systemd::other_profile_units(profile) {
        let _ = systemctl(&["stop", unit]);
    }
    systemctl(&["start", SERVICE_NAME])
}

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
        let res = cycle_supervisor(&ctx.profile, &mut |args| exec::run("systemctl", args));
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
        // stop above stopped it; bring it back when `logging.store.enabled`
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
        //   supervisor, so the cycle above brings them back down and up, but a
        //   unit left inactive before this install (or stopped on its way out
        //   by the previous release's supervisor, which stopped every unit it
        //   had adopted) needs the explicit start; the health gate waits for
        //   the control front;
        // - on a ground station, the GS unit set: several are
        //   WantedBy=multi-user.target, which a supervisor start never pulls
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
    use super::*;

    const PROFILES: [&str; 5] = ["drone", "ground_station", "workstation", "compute", "visor"];

    fn ok() -> CmdResult {
        CmdResult {
            code: Some(0),
            stdout: String::new(),
            stderr: String::new(),
            spawned: true,
        }
    }

    /// The `systemctl` calls the cycle issues for `profile`, in order.
    fn cycle_calls(profile: &str) -> Vec<Vec<String>> {
        let mut calls = Vec::new();
        let res = cycle_supervisor(profile, &mut |args| {
            calls.push(args.iter().map(|a| a.to_string()).collect());
            ok()
        });
        assert!(res.success());
        calls
    }

    #[test]
    fn gs_start_set_has_the_receive_ap_and_pairing_units() {
        let gs = crate::steps::systemd::start_units("ground_station");
        // The supervisor is cycled above, never kicked again here.
        assert!(!gs.contains(&"ados-supervisor.service"));
        assert!(gs.contains(&"ados-wfb-rx.service"));
        assert!(gs.contains(&"ados-hostapd.service"));
        assert!(gs.contains(&"ados-setup-captive.service"));
        // Every REST pairing call reaches this daemon over its socket; a
        // ground station that never starts it cannot pair anything.
        assert!(gs.contains(&"ados-mesh-pairing.service"));
        // The front and plugin host come back on every profile.
        for profile in PROFILES {
            let set = crate::steps::systemd::start_units(profile);
            assert!(set.contains(&"ados-control.service"), "{profile}");
            assert!(set.contains(&"ados-plugin-host.service"), "{profile}");
        }
    }

    /// Regression: a ground station reinstalled as a drone kept its media
    /// server running. The old supervisor revived it after the systemd step
    /// stopped it, and the supervisor `restart` then carried it over to the new
    /// profile, where it held the ports the drone's media server binds. The
    /// other profile's units are stopped only once the supervisor is down, and
    /// the supervisor is stopped and started, never restarted.
    #[test]
    fn the_other_profile_is_stopped_while_the_supervisor_is_down() {
        for profile in PROFILES {
            let calls = cycle_calls(profile);
            let other = crate::steps::systemd::other_profile_units(profile);
            assert_eq!(calls.first().unwrap(), &["stop", SERVICE_NAME], "{profile}");
            assert_eq!(calls.last().unwrap(), &["start", SERVICE_NAME], "{profile}");
            let middle: Vec<&Vec<String>> = calls[1..calls.len() - 1].iter().collect();
            assert_eq!(middle.len(), other.len(), "{profile}");
            for (call, unit) in middle.iter().zip(&other) {
                assert_eq!(call.as_slice(), ["stop", *unit], "{profile}");
            }
            assert!(
                calls.iter().all(|c| c[0] != "restart"),
                "{profile}: a restart carries revived units over"
            );
            // Nothing stopped here is started again by the start set.
            let started = crate::steps::systemd::start_units(profile);
            assert!(other.iter().all(|u| !started.contains(u)), "{profile}");
        }
        // Both directions of the switch that showed it.
        assert!(cycle_calls("drone").contains(&vec![
            "stop".to_string(),
            "ados-mediamtx-gs.service".to_string()
        ]));
        assert!(cycle_calls("ground_station")
            .contains(&vec!["stop".to_string(), "ados-video.service".to_string()]));
    }

    #[test]
    fn the_cycle_reports_the_supervisor_start() {
        let res = cycle_supervisor("drone", &mut |args| {
            if args == ["start", SERVICE_NAME] {
                CmdResult {
                    code: Some(1),
                    stderr: "start failed".to_string(),
                    ..ok()
                }
            } else {
                ok()
            }
        });
        assert!(!res.success());
        assert_eq!(res.stderr, "start failed");
    }
}
