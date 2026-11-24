//! Pruning retired units and udev rules, and tearing down the units of the
//! other profile.

use super::*;

/// Prune units retired in a prior release: stop, disable (while the file still
/// exists so systemd can read its `[Install]` section and drop the `.wants`
/// symlinks), reset any failed state, then delete the unit file and any drop-in
/// dir. Idempotent — a unit already gone is a clean no-op. Must run before the
/// `daemon-reload` so the removal is visible to systemd in the same install.
pub(super) fn prune_retired_units() {
    prune_retired_units_in(Path::new(SYSTEMD_DIR), &mut |args| {
        let _ = exec::run("systemctl", args);
    });
}

/// [`prune_retired_units`] against an injected unit dir and `systemctl` runner,
/// so the prune is testable without touching the host's systemd.
pub(super) fn prune_retired_units_in(systemd_dir: &Path, systemctl: &mut dyn FnMut(&[&str])) {
    for &unit in RETIRED_UNITS {
        let path = systemd_dir.join(unit);
        let dropin = systemd_dir.join(format!("{unit}.d"));
        let present = path.exists() || dropin.exists();
        systemctl(&["stop", unit]);
        systemctl(&["disable", unit]);
        systemctl(&["reset-failed", unit]);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&dropin);
        if present {
            tracing::info!(unit, "pruned retired systemd unit");
        }
    }
}

/// The unit states in which a retired unit still holds a process.
const LIVE_STATES: [&str; 4] = ["active", "activating", "deactivating", "reloading"];

/// Clear what the prune could not: a retired unit still running, or stuck
/// stopping, when the prune's `stop` was issued is killed outright, then every
/// retired unit's failed state is reset, so none stays listed as failed after
/// the install. Run after the start step, when no supervisor restarts them.
pub(crate) fn reap_retired_units() {
    reap_retired_units_in(
        &mut |unit| {
            exec::run("systemctl", &["show", "-p", "ActiveState", "--value", unit])
                .stdout
                .trim()
                .to_string()
        },
        &mut |args| {
            let _ = exec::run("systemctl", args);
        },
        &mut || std::thread::sleep(std::time::Duration::from_secs(1)),
    );
}

/// [`reap_retired_units`] against an injected state probe, `systemctl` runner
/// and sleep.
pub(crate) fn reap_retired_units_in(
    state: &mut dyn FnMut(&str) -> String,
    systemctl: &mut dyn FnMut(&[&str]),
    sleep: &mut dyn FnMut(),
) {
    for &unit in RETIRED_UNITS {
        if LIVE_STATES.contains(&state(unit).as_str()) {
            systemctl(&["kill", "--signal=SIGKILL", unit]);
            for _ in 0..10 {
                if !LIVE_STATES.contains(&state(unit).as_str()) {
                    break;
                }
                sleep();
            }
            tracing::info!(unit, "killed a retired unit still running");
        }
        systemctl(&["reset-failed", unit]);
    }
}

/// Prune udev rules retired in a prior release, then reload so the removal takes
/// effect in the same install. Idempotent — a rule already gone is a no-op.
pub(super) fn prune_retired_udev_rules() {
    let mut removed = false;
    for rule in RETIRED_UDEV_RULES {
        let path = Path::new(UDEV_RULES_DIR).join(rule);
        if std::fs::remove_file(&path).is_ok() {
            tracing::info!(rule, "pruned retired udev rule");
            removed = true;
        }
    }
    if removed {
        let _ = exec::run("udevadm", &["control", "--reload"]);
    }
}

/// Unlink the units belonging to the other profile: `disable` drops their
/// `.wants` links, so neither the supervisor's start nor a boot pulls them in
/// again. Stopping them is the `start` step's job, done while the supervisor
/// is down: the supervisor still running here is the previous profile's, and
/// it restarts any unit it supervises the moment that unit dies, so a stop
/// issued now is undone within one monitor pass.
pub(super) fn disable_other_profile_units(profile: &str, host: ados_protocol::host_kind::HostKind) {
    for unit in other_profile_units(profile, host) {
        let _ = exec::run("systemctl", &["disable", unit]);
    }
}

/// Stop + disable + mask the Debian-default dnsmasq/hostapd so they cannot
/// fight the access-point profiles (ground station, visor) for ports 53/67 or
/// wlan0 (`mask_conflicting_standalone_services`).
pub(super) fn mask_conflicting_standalone_services() {
    for action in ["stop", "disable", "mask"] {
        let _ = exec::run("systemctl", &[action, "dnsmasq.service", "hostapd.service"]);
    }
}
