//! Lookups over [`UNITS`]: by name, by tier and by profile.

use super::{UnitSpec, UNITS};

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
