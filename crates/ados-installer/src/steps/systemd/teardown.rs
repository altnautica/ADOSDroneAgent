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

/// Tear down (stop + disable) units belonging to the other profile.
pub(super) fn disable_other_profile_units(profile: &str) {
    for unit in other_profile_units(profile) {
        let _ = exec::run("systemctl", &["stop", unit]);
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
