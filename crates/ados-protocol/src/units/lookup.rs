//! Lookups over [`UNITS`]: by name, by tier and by profile, and the per-profile
//! unit lists a profile install or a runtime profile switch acts on. Every list
//! follows table order.

use super::{Install, UnitSpec, UNITS};

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
/// surface.
pub fn net_daemon_owns_wifi(profile: &str) -> bool {
    let profile = profile.replace('-', "_");
    find("ados-uplink-router").is_some_and(|u| u.on_profile(&profile))
}

/// The units installed on `profile`, in table order.
pub fn units_for_profile(profile: &str) -> impl Iterator<Item = &'static UnitSpec> + '_ {
    UNITS.iter().filter(move |u| u.on_profile(profile))
}

/// Units enable-linked whatever the profile: the supervisor, the control front
/// and the self-gating cross-profile set.
pub fn universal_units() -> Vec<&'static str> {
    UNITS
        .iter()
        .filter(|u| u.install == Install::Universal)
        .map(|u| u.name)
        .collect()
}

/// The units enable-linked for `profile` (underscore form) on top of the
/// universal set.
pub fn profile_enable_units(profile: &str) -> Vec<&'static str> {
    UNITS
        .iter()
        .filter(|u| u.install == Install::Profile && u.profiles.contains(&profile))
        .map(|u| u.name)
        .collect()
}

/// The units a switch to `profile` stops and disables: a ground station drops
/// the air-side units, any other profile the ground-station units.
pub fn other_profile_units(profile: &str) -> Vec<&'static str> {
    UNITS
        .iter()
        .filter(|u| u.teardown && !u.on_profile(profile))
        .map(|u| u.name)
        .collect()
}

/// The units kicked with `--no-block` once the supervisor is up on `profile`.
pub fn start_units(profile: &str) -> Vec<&'static str> {
    UNITS
        .iter()
        .filter(|u| u.start_on_install && u.on_profile(profile))
        .map(|u| u.name)
        .collect()
}

/// The units an install's health gate requires enabled on `profile`.
pub fn health_units(profile: &str) -> Vec<&'static str> {
    UNITS
        .iter()
        .filter(|u| u.health_check && u.on_profile(profile))
        .map(|u| u.name)
        .collect()
}
