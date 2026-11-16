//! The flight-controller parameter profile ADOS Pilot depends on.
//!
//! One table serves the readiness check (`fc_profile` item) and the setup
//! wizard's `ados_profile` step. A row whose name and alternative names are all
//! absent from the FC's parameter table is skipped, not failed: firmware
//! versions differ in which of these exist. A row with an `absent_blocks`
//! reason is the exception: its absence means a feature the pilot relies on is
//! compiled out of the firmware, so readiness fails on it.

use std::fmt;

use ados_protocol::pilot::{PlaceClass, VehicleClass};
use serde::Serialize;

use crate::config::CONFINED_TIP_EXTRA_M;

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
    /// Why the row's absence fails readiness; `None` skips an absent row.
    pub absent_blocks: Option<&'static str>,
    /// The FC reads the parameter at boot only: a change needs a reboot.
    pub reboot: bool,
    /// In a confined place the row accepts any value above 0 and at most
    /// this one instead of `value`.
    pub confined_max: Option<f32>,
}

/// The largest `SRTL_ACCURACY` a confined place accepts, metres: 0.2.
///
/// SmartRTL (`libraries/AP_SmartRTL`, ArduPilot fe92274) keeps a point once
/// the vehicle is `SRTL_ACCURACY` from the last one, simplifies the path to
/// within half the accuracy (`SMARTRTL_SIMPLIFY_EPSILON`), and cuts a loop
/// where two path segments pass within 0.99 times the accuracy
/// (`SMARTRTL_PRUNING_DELTA`), returning through the midpoint between them.
/// Its return so leaves the flown path by up to about half the accuracy:
/// 1 m at the default 2 m, into the wall of a room or a tunnel bore. The
/// clearance a confined place keeps whatever the pose uncertainty is the
/// margin's [`CONFINED_TIP_EXTRA_M`] beyond the propeller tips (the route
/// check's extra term covers pose error, which the return shares, and flown
/// legs off the checked route have none), so half the accuracy fits at
/// 0.2 m. 0 turns SmartRTL off (`AP_SmartRTL::enabled`), so a confined place
/// needs it above 0. A finer accuracy keeps more points: the path buffer
/// holds at most 500 (`SRTL_POINTS`), and a flight that fills it loses
/// SmartRTL, so its FC-side return lands in place.
pub const CONFINED_SRTL_ACCURACY_MAX_M: f32 = 2.0 * CONFINED_TIP_EXTRA_M;

const fn p(name: &'static str, alt_names: &'static [&'static str], value: f32) -> ProfileParam {
    ProfileParam {
        name,
        alt_names,
        value,
        absent_blocks: None,
        reboot: false,
        confined_max: None,
    }
}

impl ProfileParam {
    const fn reboots(self) -> Self {
        ProfileParam {
            reboot: true,
            ..self
        }
    }

    const fn absent_blocks(self, why: &'static str) -> Self {
        ProfileParam {
            absent_blocks: Some(why),
            ..self
        }
    }

    const fn confined_at_most(self, max: f32) -> Self {
        ProfileParam {
            confined_max: Some(max),
            ..self
        }
    }

    /// What the row wants in `place`.
    pub fn want(&self, place: PlaceClass) -> Want {
        match (place, self.confined_max) {
            (PlaceClass::Confined, Some(max)) => Want::Within(max),
            _ => Want::Exactly(self.value),
        }
    }

    /// The value the setup wizard writes in `place` over `have`: `have`
    /// itself when it already satisfies the row there.
    pub fn value_in(&self, place: PlaceClass, have: Option<f32>) -> f32 {
        match self.want(place) {
            Want::Exactly(v) => v,
            Want::Within(max) => have
                .filter(|h| Want::Within(max).accepts(*h))
                .unwrap_or(max),
        }
    }
}

/// What a row accepts.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Want {
    /// This value.
    Exactly(f32),
    /// Above 0 and at most this.
    Within(f32),
}

impl Want {
    fn accepts(self, v: f32) -> bool {
        match self {
            Want::Exactly(want) => (v - want).abs() < 0.01,
            Want::Within(max) => v > 0.0 && v <= max + 1e-4,
        }
    }
}

impl fmt::Display for Want {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Want::Exactly(v) => write!(f, "{v}"),
            Want::Within(max) => write!(f, "> 0 and ≤ {max} in a confined place"),
        }
    }
}

/// ArduCopter. The pilot does its own obstacle avoidance, so the FC's path
/// planner (`OA_TYPE`) and its fence and proximity avoidance (`AVOID_ENABLE`)
/// stay off: they would fight the pilot's setpoints. Every FC-side return is
/// SmartRTL with a Land fallback, back along the flown path, never a straight
/// line through what the pilot flew around.
const COPTER: &[ProfileParam] = &[
    p("MAV_GCS_SYSID", &["SYSID_MYGCS"], 1.0),
    // 4: SmartRTL or Land.
    p("FS_GCS_ENABLE", &[], 4.0),
    p("FS_GCS_TIMEOUT", &[], 3.0),
    // 5: SmartRTL or Land.
    p("FS_THR_ENABLE", &[], 5.0),
    p("GUID_TIMEOUT", &[], 1.0),
    p("RC_OVERRIDE_TIME", &[], 1.0),
    p("BATT_FS_LOW_ACT", &[], 2.0),
    p("BATT_FS_CRT_ACT", &[], 1.0),
    p("OA_TYPE", &[], 0.0).reboots(),
    p("AVOID_ENABLE", &[], 0.0),
    p("SRTL_POINTS", &[], 500.0)
        .reboots()
        .absent_blocks("SmartRTL not in this firmware: no return along the flown path"),
    // A confined place takes a tighter accuracy (see
    // CONFINED_SRTL_ACCURACY_MAX_M); an open one the firmware default.
    p("SRTL_ACCURACY", &[], 2.0).confined_at_most(CONFINED_SRTL_ACCURACY_MAX_M),
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
        want: Want,
    },
}

/// Check a row in `place` with `lookup(name) -> Option<value>`.
pub fn check_row(
    row: &ProfileParam,
    place: PlaceClass,
    lookup: impl Fn(&str) -> Option<f32>,
) -> RowCheck {
    let found = std::iter::once(row.name)
        .chain(row.alt_names.iter().copied())
        .find_map(|n| lookup(n).map(|v| (n, v)));
    let want = row.want(place);
    match found {
        None => RowCheck::Absent,
        Some((_, v)) if want.accepts(v) => RowCheck::Ok,
        Some((name, have)) => RowCheck::Mismatch { name, have, want },
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
        let open = PlaceClass::Open;
        assert_eq!(
            check_row(row, open, |n| (n == "SYSID_MYGCS").then_some(1.0)),
            RowCheck::Ok
        );
        assert_eq!(check_row(row, open, |_| None), RowCheck::Absent);
        assert_eq!(
            check_row(row, open, |n| (n == "MAV_GCS_SYSID").then_some(255.0)),
            RowCheck::Mismatch {
                name: "MAV_GCS_SYSID",
                have: 255.0,
                want: Want::Exactly(1.0)
            }
        );
    }

    fn srtl_accuracy() -> &'static ProfileParam {
        COPTER.iter().find(|r| r.name == "SRTL_ACCURACY").unwrap()
    }

    #[test]
    fn the_confined_smartrtl_accuracy_bound_is_twice_the_clearance_past_the_tips() {
        // The margin keeps the tips 0.1 m clear; the return may leave the
        // flown path by half the accuracy.
        assert!((CONFINED_SRTL_ACCURACY_MAX_M - 0.2).abs() < 1e-6);
    }

    #[test]
    fn a_confined_place_accepts_a_tighter_smartrtl_accuracy_above_zero() {
        let row = srtl_accuracy();
        let check = |place, v: f32| check_row(row, place, |_| Some(v));
        for ok in [0.1, 0.15, 0.2] {
            assert_eq!(check(PlaceClass::Confined, ok), RowCheck::Ok, "{ok}");
        }
        for wrong in [0.0, 0.21, 0.25, 2.0] {
            assert_eq!(
                check(PlaceClass::Confined, wrong),
                RowCheck::Mismatch {
                    name: "SRTL_ACCURACY",
                    have: wrong,
                    want: Want::Within(CONFINED_SRTL_ACCURACY_MAX_M)
                },
                "{wrong}"
            );
        }
        // An open place keeps the firmware default.
        assert_eq!(check(PlaceClass::Open, 2.0), RowCheck::Ok);
        assert!(matches!(
            check(PlaceClass::Open, 0.25),
            RowCheck::Mismatch { want: Want::Exactly(w), .. } if w == 2.0
        ));
    }

    #[test]
    fn the_wizard_keeps_a_confined_accuracy_that_passes_and_writes_the_bound_otherwise() {
        let row = srtl_accuracy();
        assert_eq!(row.value_in(PlaceClass::Confined, Some(0.15)), 0.15);
        assert_eq!(
            row.value_in(PlaceClass::Confined, Some(0.25)),
            CONFINED_SRTL_ACCURACY_MAX_M
        );
        assert_eq!(
            row.value_in(PlaceClass::Confined, Some(2.0)),
            CONFINED_SRTL_ACCURACY_MAX_M
        );
        assert_eq!(
            row.value_in(PlaceClass::Confined, None),
            CONFINED_SRTL_ACCURACY_MAX_M
        );
        assert_eq!(row.value_in(PlaceClass::Open, Some(0.25)), 2.0);
        // Other rows want their own value in either place.
        assert_eq!(COPTER[0].value_in(PlaceClass::Confined, Some(255.0)), 1.0);
    }

    #[test]
    fn a_confined_mismatch_reads_the_bound() {
        assert_eq!(
            Want::Within(CONFINED_SRTL_ACCURACY_MAX_M).to_string(),
            "> 0 and ≤ 0.2 in a confined place"
        );
        assert_eq!(Want::Exactly(2.0).to_string(), "2");
    }
}
