//! The flight-controller parameter profile ADOS Pilot depends on.
//!
//! One table serves the readiness check (`fc_profile` item) and the setup
//! wizard's `ados_profile` step. A row whose name and alternative names are all
//! absent from the FC's parameter table is skipped, not failed: firmware
//! versions differ in which of these exist.

use ados_protocol::pilot::VehicleClass;
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Firmware {
    Ardupilot,
    Px4,
}

/// One required parameter value.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProfileParam {
    pub name: &'static str,
    /// Names the same parameter carries on other firmware versions.
    pub alt_names: &'static [&'static str],
    pub value: f32,
}

const fn p(name: &'static str, alt_names: &'static [&'static str], value: f32) -> ProfileParam {
    ProfileParam {
        name,
        alt_names,
        value,
    }
}

const COPTER: &[ProfileParam] = &[
    p("MAV_GCS_SYSID", &["SYSID_MYGCS"], 1.0),
    p("FS_GCS_ENABLE", &[], 1.0),
    p("FS_GCS_TIMEOUT", &[], 3.0),
    p("GUID_TIMEOUT", &[], 1.0),
    p("RC_OVERRIDE_TIME", &[], 1.0),
    p("BATT_FS_LOW_ACT", &[], 2.0),
    p("BATT_FS_CRT_ACT", &[], 1.0),
];

const PLANE: &[ProfileParam] = &[
    p("MAV_GCS_SYSID", &["SYSID_MYGCS"], 1.0),
    p("FS_GCS_ENABL", &[], 1.0),
    p("FS_LONG_TIMEOUT", &[], 3.0),
    p("FS_LONG_ACTN", &[], 1.0),
    p("BATT_FS_LOW_ACT", &[], 1.0),
    p("RC_OVERRIDE_TIME", &[], 1.0),
];

const VTOL: &[ProfileParam] = &[
    p("MAV_GCS_SYSID", &["SYSID_MYGCS"], 1.0),
    p("FS_GCS_ENABL", &[], 1.0),
    p("FS_LONG_TIMEOUT", &[], 3.0),
    p("FS_LONG_ACTN", &[], 1.0),
    p("BATT_FS_LOW_ACT", &[], 1.0),
    p("RC_OVERRIDE_TIME", &[], 1.0),
    p("Q_GUIDED_MODE", &[], 1.0),
];

const PX4: &[ProfileParam] = &[
    p("COM_OF_LOSS_T", &[], 1.0),
    p("COM_OBL_RC_ACT", &[], 5.0),
    p("NAV_DLL_ACT", &[], 2.0),
    p("COM_DL_LOSS_T", &[], 10.0),
];

/// The profile for a firmware and vehicle class.
pub fn fc_profile(fw: Firmware, class: VehicleClass) -> &'static [ProfileParam] {
    match (fw, class) {
        (Firmware::Px4, _) => PX4,
        (Firmware::Ardupilot, VehicleClass::Multirotor) => COPTER,
        (Firmware::Ardupilot, VehicleClass::Fixedwing) => PLANE,
        (Firmware::Ardupilot, VehicleClass::Vtol) => VTOL,
    }
}

/// Outcome of checking one row against the FC's parameters.
#[derive(Debug, Clone, PartialEq)]
pub enum RowCheck {
    Ok,
    Absent,
    Mismatch {
        name: &'static str,
        have: f32,
        want: f32,
    },
}

/// Check a row with `lookup(name) -> Option<value>`.
pub fn check_row(row: &ProfileParam, lookup: impl Fn(&str) -> Option<f32>) -> RowCheck {
    let found = std::iter::once(row.name)
        .chain(row.alt_names.iter().copied())
        .find_map(|n| lookup(n).map(|v| (n, v)));
    match found {
        None => RowCheck::Absent,
        Some((_, v)) if (v - row.value).abs() < 0.01 => RowCheck::Ok,
        Some((name, have)) => RowCheck::Mismatch {
            name,
            have,
            want: row.value,
        },
    }
}

/// Every parameter name the profile may read (primary and alternatives).
pub fn all_names(fw: Firmware, class: VehicleClass) -> impl Iterator<Item = &'static str> {
    fc_profile(fw, class)
        .iter()
        .flat_map(|r| std::iter::once(r.name).chain(r.alt_names.iter().copied()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alternative_name_satisfies_a_row() {
        let row = &COPTER[0];
        assert_eq!(
            check_row(row, |n| (n == "SYSID_MYGCS").then_some(1.0)),
            RowCheck::Ok
        );
        assert_eq!(check_row(row, |_| None), RowCheck::Absent);
        assert_eq!(
            check_row(row, |n| (n == "MAV_GCS_SYSID").then_some(255.0)),
            RowCheck::Mismatch {
                name: "MAV_GCS_SYSID",
                have: 255.0,
                want: 1.0
            }
        );
    }
}
