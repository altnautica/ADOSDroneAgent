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

/// The ground-station units the start step kicks with `--no-block` (the START
/// half of `enable_ground_station_units`; the ENABLE half ran in `systemd`).
const GROUND_STATION_START_UNITS: &[&str] = &[
    "ados-wfb-rx.service",
    "ados-mediamtx-gs.service",
    "ados-usb-gadget.service",
    "ados-oled.service",
    "ados-buttons.service",
    "ados-hostapd.service",
    "ados-dnsmasq-gs.service",
    "ados-setup-captive.service",
    "ados-kiosk.service",
    "ados-input.service",
    "ados-pic.service",
    "ados-uplink-router.service",
    "ados-modem.service",
    "ados-wifi-client.service",
    "ados-ethernet.service",
    "ados-cloud-relay.service",
];

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
        // restart above stopped it; bring it back unless the fallback marker
        // pins it off. The log-view endpoints read it, so a fresh box must come
        // up with it running and zero manual steps. Cross-profile.
        if !Path::new(CONFIG_DIR).join("logd-python-fallback").exists() {
            let _ = exec::run("systemctl", &["start", "--no-block", "ados-logd.service"]);
            tracing::info!(
                unit = "ados-logd.service",
                "logging store started (--no-block)"
            );
        }

        // The native plugin host owns the per-plugin sockets by default and is
        // PartOf the supervisor, so the restart above stopped it; bring it back
        // unless the fallback marker pins the packaged path. A fresh box must
        // come up with the native host serving the plugin sockets and zero
        // manual steps. Cross-profile (both profiles fetch the binary).
        if !Path::new(CONFIG_DIR)
            .join("plugin-host-python-fallback")
            .exists()
        {
            let _ = exec::run(
                "systemctl",
                &["start", "--no-block", "ados-plugin-host.service"],
            );
            tracing::info!(
                unit = "ados-plugin-host.service",
                "native plugin host started (--no-block)"
            );
        }

        // On a ground station, kick the GS unit set with --no-block. The
        // supervisor's PartOf= chain stops these on its restart above with
        // nothing subsequently re-starting them; this brings them back.
        // Best-effort: a unit that is not deployed / not enabled is a no-op.
        if ctx.profile == "ground_station" {
            for unit in GROUND_STATION_START_UNITS {
                let _ = exec::run("systemctl", &["start", "--no-block", unit]);
            }
            tracing::info!("ground-station unit set started (--no-block)");
        }

        StepOutcome::Ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gs_start_set_matches_the_enable_set_minus_gadget_setup() {
        // The start set must not include the supervisor (started above) and
        // must include the receive + AP + setup units.
        assert!(!GROUND_STATION_START_UNITS.contains(&"ados-supervisor.service"));
        assert!(GROUND_STATION_START_UNITS.contains(&"ados-wfb-rx.service"));
        assert!(GROUND_STATION_START_UNITS.contains(&"ados-hostapd.service"));
        assert!(GROUND_STATION_START_UNITS.contains(&"ados-setup-captive.service"));
    }
}
