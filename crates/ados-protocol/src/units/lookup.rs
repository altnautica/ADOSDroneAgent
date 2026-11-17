//! Lookups over [`UNITS`]: by name, by tier and by profile, and the per-profile
//! unit lists a profile install or a runtime profile switch acts on. Every list
//! follows table order.
//!
//! Every list a caller acts on takes the [`HostKind`]: on a personal computer
//! the [`UnitSpec::appliance_only`] rows are dropped, because each of them takes
//! over something the machine's owner already owns (the console, the display,
//! the Wi-Fi station, the routing table, the USB port's mode or the mesh). The
//! appliance lists are unchanged.

use super::{HostKind, Install, UnitSpec, UNITS};

/// Whether `unit` belongs on `host`.
fn on_host(unit: &UnitSpec, host: HostKind) -> bool {
    !(host.is_personal() && unit.appliance_only)
}

/// The row for `name`, given as the unit file name (`ados-wfb.service`) or
/// its stem (`ados-wfb`).
pub fn find(name: &str) -> Option<&'static UnitSpec> {
    let stem = name.strip_suffix(".service").unwrap_or(name);
    UNITS.iter().find(|u| u.stem() == stem)
}

/// The rows the supervisor manages, in catalog order.
pub fn supervised() -> impl Iterator<Item = &'static UnitSpec> {
    UNITS.iter().filter(|u| u.category.is_some())
}

/// Whether the ados-net uplink daemon (`ados-uplink-router`) runs on
/// `profile`, and so owns the Wi-Fi command socket. Accepts the underscore or
/// the hyphen spelling. Every other node serves that socket from the control
/// surface. Never on a personal computer: the uplink daemon would take the
/// machine's own Wi-Fi, so this process serves the socket instead.
pub fn net_daemon_owns_wifi(profile: &str, host: HostKind) -> bool {
    let profile = profile.replace('-', "_");
    match find("ados-uplink-router") {
        Some(u) => u.on_profile(&profile) && on_host(u, host),
        None => false,
    }
}

/// The units installed on `profile`, in table order.
pub fn units_for_profile(
    profile: &str,
    host: HostKind,
) -> impl Iterator<Item = &'static UnitSpec> + '_ {
    UNITS
        .iter()
        .filter(move |u| u.on_profile(profile) && on_host(u, host))
}

/// Units enable-linked whatever the profile: the supervisor, the control front
/// and the self-gating cross-profile set.
pub fn universal_units(host: HostKind) -> Vec<&'static str> {
    UNITS
        .iter()
        .filter(|u| u.install == Install::Universal && on_host(u, host))
        .map(|u| u.name)
        .collect()
}

/// The units enable-linked for `profile` (underscore form) on top of the
/// universal set.
pub fn profile_enable_units(profile: &str, host: HostKind) -> Vec<&'static str> {
    UNITS
        .iter()
        .filter(|u| u.install == Install::Profile && u.profiles.contains(&profile))
        .filter(|u| on_host(u, host))
        .map(|u| u.name)
        .collect()
}

/// The units a switch to `profile` stops and disables: a ground station drops
/// the air-side units, any other profile the ground-station units.
///
/// On a personal computer this also returns the profile's own appliance-only
/// units: they never run here, so a host that used to be an appliance has to
/// have them stopped and disabled rather than merely left out of the new lists.
pub fn other_profile_units(profile: &str, host: HostKind) -> Vec<&'static str> {
    UNITS
        .iter()
        .filter(|u| u.teardown && !u.on_profile(profile))
        .chain(
            UNITS
                .iter()
                .filter(move |u| u.profiles.contains(&profile) && !on_host(u, host)),
        )
        .map(|u| u.name)
        .collect()
}

/// The units kicked with `--no-block` once the supervisor is up on `profile`.
pub fn start_units(profile: &str, host: HostKind) -> Vec<&'static str> {
    UNITS
        .iter()
        .filter(|u| u.start_on_install && u.on_profile(profile) && on_host(u, host))
        .map(|u| u.name)
        .collect()
}

/// The units an install's health gate requires enabled on `profile`.
pub fn health_units(profile: &str, host: HostKind) -> Vec<&'static str> {
    UNITS
        .iter()
        .filter(|u| u.health_check && u.on_profile(profile) && on_host(u, host))
        .map(|u| u.name)
        .collect()
}

/// The appliance-only units an install of `profile` must stop and disable
/// because the host is personal. Empty on an appliance, and empty for a
/// profile that ships none of them.
pub fn appliance_teardown_units(profile: &str, host: HostKind) -> Vec<&'static str> {
    if !host.is_personal() {
        return Vec::new();
    }
    UNITS
        .iter()
        .filter(|u| u.appliance_only && u.profiles.contains(&profile))
        .map(|u| u.name)
        .collect()
}
