//! The flight-controller parameter profile ADOS Pilot depends on.
//!
//! One table serves the readiness check (`fc_profile` item) and the setup
//! wizard's `ados_profile` step. A row whose name and alternative names are all
//! absent from the FC's parameter table is skipped, not failed: firmware
//! versions differ in which of these exist. A row with an `absent_blocks`
//! reason is the exception: its absence means a feature the pilot relies on is
//! compiled out of the firmware, so readiness fails on it.
//!
//! The FC's proximity backstop rows (`backstop`) are a fallback under the
//! pilot's own brake: the wizard writes them, the `fc_backstop` readiness
//! item reports them as a warning, and they never block a flight by
//! themselves. An FC that has not had them written yet (one set up before
//! they joined the profile) flies as before, with the pilot's brake alone.
//! What still blocks is an FC avoider that would act before the pilot's
//! brake ([`avoid_conflict`]).

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
    /// In an open place the row accepts any value in this band (`value`
    /// is written when it is outside); a confined place does not check it.
    pub open_band: Option<(f32, f32)>,
    /// `value` is a bitmask: the row wants those bits set and leaves the
    /// operator's other bits as they are.
    pub bits: bool,
    /// The pilot sets the row itself before the flight that needs it (the
    /// supervised lift-off): the wizard writes it, readiness does not check
    /// it.
    pub pilot_sets: bool,
    /// The FC's proximity backstop: the wizard writes it, readiness warns
    /// when it differs (`fc_backstop`) and never blocks on it.
    pub backstop: bool,
}

/// `ATC_INPUT_TC` for camera flight in an open place, seconds: 0.15-0.2.
///
/// ArduCopter (fe92274, `AC_AttitudeControl`) defaults to 0.10 ("crisp";
/// range 0-1 s, 0.15 is ArduPlane's "medium"): the attitude target follows
/// a new lean demand with that time constant, so every change of commanded
/// acceleration the pilot's shaper did not smooth (an FC position
/// correction, a brake cut) tilts the body, and the fixed camera, within
/// 0.1 s. 0.15-0.2 rounds those edges. The cost is a slower response to a
/// brake cut: about 0.05-0.1 s more lag, inside the 0.8 s [`BRAKE_LAG_S`]
/// the brake plans for (measured 0.5-0.6 s at 0.10), so the open stopping
/// budget holds. `PSC_NE_JERK` keeps its 5 m/s³ default: the pilot's goal
/// shaper already holds goal flight to 1.2-5 m/s³ by profile
/// ([`crate::model::goal_limits`]), while the FC's jerk limit sets how fast
/// a brake cut bites (at 2.5 m/s³ the FC would take 1.2 s instead of 0.6 s
/// to unwind 3 m/s² of acceleration, past [`BRAKE_LAG_S`]). A confined place
/// keeps the FC's own value: its stops 0.1 m beyond the propeller tips were
/// flown and scored at the crisp default only.
///
/// [`BRAKE_LAG_S`]: crate::core::control::BRAKE_LAG_S
pub const OPEN_INPUT_TC_S: (f32, f32) = (0.15, 0.2);

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

/// The FC's proximity stop sits this much inside the pilot's obstacle
/// margin, metres: the pilot's brake stops first, the FC's only when the
/// pilot has not.
pub const FC_AVOID_INSIDE_M: f32 = 0.7;
/// `AVOID_MARGIN`'s documented minimum, metres (`AC_Avoid`: range 1-10).
pub const FC_AVOID_MIN_M: f32 = 1.0;
/// The least gap between the pilot's margin and the FC's stop, metres:
/// a pilot margin under [`FC_AVOID_MIN_M`] plus this leaves no room for
/// the FC's stop inside it, so its proximity avoidance is off.
pub const FC_AVOID_GAP_MIN_M: f32 = 0.3;
/// The row whose value follows the pilot's margin ([`fc_avoid_margin_m`]).
pub const AVOID_MARGIN: &str = "AVOID_MARGIN";
/// The row that turns the FC's proximity avoidance on (bit 1).
pub const AVOID_ENABLE: &str = "AVOID_ENABLE";
/// `AVOID_ENABLE` with proximity only.
const AVOID_PROXIMITY: f32 = 2.0;

/// `AVOID_MARGIN` for a pilot obstacle margin of `pilot_margin_m`:
/// [`FC_AVOID_INSIDE_M`] inside it, at least [`FC_AVOID_MIN_M`]; `None`
/// when that is not [`FC_AVOID_GAP_MIN_M`] inside it (a confined place's
/// margin from the propeller tips): the FC's stop would act before the
/// pilot's brake, so the backstop is off.
pub fn fc_avoid_margin_m(pilot_margin_m: f32) -> Option<f32> {
    let m = (pilot_margin_m - FC_AVOID_INSIDE_M).max(FC_AVOID_MIN_M);
    (pilot_margin_m - m >= FC_AVOID_GAP_MIN_M - 1e-4).then_some(m)
}

/// The FC's own avoidance against the pilot's flight, from its parameters
/// (`lookup`) and the pilot's margin: `Some(why)` when it would act before
/// the pilot's brake: fence or beacon avoidance (an `AVOID_ENABLE` bit other
/// than proximity), proximity avoidance where its stop cannot fit inside the
/// pilot's margin ([`fc_avoid_margin_m`] `None`), or a proximity stop
/// (`AVOID_MARGIN`) less than [`FC_AVOID_GAP_MIN_M`] inside the margin.
/// `AVOID_ENABLE` 0 never conflicts, whatever the other backstop rows hold:
/// the vehicle flies with the pilot's brake alone, as it did before the
/// backstop. `None` while a row it needs is unread or absent.
pub fn avoid_conflict(pilot_margin_m: f32, lookup: impl Fn(&str) -> Option<f32>) -> Option<String> {
    let enable = lookup(AVOID_ENABLE)?;
    let bits = enable.max(0.0) as u32;
    let proximity = AVOID_PROXIMITY as u32;
    let fits = fc_avoid_margin_m(pilot_margin_m).is_some();
    if bits & !proximity != 0 || (bits & proximity != 0 && !fits) {
        let want = if fits { "0 or 2" } else { "0" };
        return Some(format!("{AVOID_ENABLE} {enable} (want {want})"));
    }
    if bits & proximity == 0 {
        return None;
    }
    let have = lookup(AVOID_MARGIN)?;
    // Centimetres, as the FC stores it.
    let most = ((pilot_margin_m - FC_AVOID_GAP_MIN_M) * 100.0 + 1e-3).floor() / 100.0;
    (have > most + 1e-4)
        .then(|| format!("{AVOID_MARGIN} {have} (want ≤ {most} with {AVOID_ENABLE} 2)"))
}

/// `row` as the pilot flying with margin `pilot_margin_m` wants it:
/// `AVOID_ENABLE` proximity only and [`AVOID_MARGIN`] at
/// [`fc_avoid_margin_m`] while the FC's stop fits inside the pilot's
/// margin; else `AVOID_ENABLE` 0 and any documented `AVOID_MARGIN`. Every
/// other row is unchanged.
pub fn for_margin(row: ProfileParam, pilot_margin_m: f32) -> ProfileParam {
    let fc = fc_avoid_margin_m(pilot_margin_m);
    match (row.name, fc) {
        // Rounded to the centimetre the FC stores.
        (AVOID_MARGIN, Some(m)) => ProfileParam {
            value: (m * 100.0).round() / 100.0,
            ..row
        },
        (AVOID_MARGIN, None) => ProfileParam {
            value: FC_AVOID_MIN_M,
            open_band: Some((FC_AVOID_MIN_M, 10.0)),
            ..row
        },
        (AVOID_ENABLE, Some(_)) => ProfileParam {
            value: AVOID_PROXIMITY,
            ..row
        },
        (AVOID_ENABLE, None) => ProfileParam { value: 0.0, ..row },
        _ => row,
    }
}

pub(crate) const fn p(
    name: &'static str,
    alt_names: &'static [&'static str],
    value: f32,
) -> ProfileParam {
    ProfileParam {
        name,
        alt_names,
        value,
        absent_blocks: None,
        reboot: false,
        confined_max: None,
        open_band: None,
        bits: false,
        pilot_sets: false,
        backstop: false,
    }
}

impl ProfileParam {
    const fn reboots(self) -> Self {
        ProfileParam {
            reboot: true,
            ..self
        }
    }

    pub(crate) const fn absent_blocks(self, why: &'static str) -> Self {
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

    const fn open_within(self, band: (f32, f32)) -> Self {
        ProfileParam {
            open_band: Some(band),
            ..self
        }
    }

    const fn bits_set(self) -> Self {
        ProfileParam { bits: true, ..self }
    }

    const fn set_by_pilot(self) -> Self {
        ProfileParam {
            pilot_sets: true,
            ..self
        }
    }

    const fn backstop(self) -> Self {
        ProfileParam {
            backstop: true,
            ..self
        }
    }

    /// What the row wants in `place`.
    pub fn want(&self, place: PlaceClass) -> Want {
        match (place, self.confined_max, self.open_band) {
            _ if self.bits => Want::Bits(self.value as u32),
            (PlaceClass::Confined, Some(max), _) => Want::Within(max),
            (PlaceClass::Confined, None, Some(_)) => Want::Any,
            (_, _, Some((lo, hi))) => Want::Between(lo, hi),
            _ => Want::Exactly(self.value),
        }
    }

    /// The value the setup wizard writes in `place` over `have`: `have`
    /// itself when it already satisfies the row there; for a bitmask row,
    /// `have` with the row's bits ORed in.
    pub fn value_in(&self, place: PlaceClass, have: Option<f32>) -> f32 {
        let want = self.want(place);
        match want {
            Want::Exactly(v) => v,
            Want::Within(max) => have.filter(|h| want.accepts(*h)).unwrap_or(max),
            Want::Between(..) | Want::Any => {
                have.filter(|h| want.accepts(*h)).unwrap_or(self.value)
            }
            Want::Bits(b) => (have.map_or(0, |h| h as u32) | b) as f32,
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
    /// From the first to the second, inclusive.
    Between(f32, f32),
    /// Whatever the FC has.
    Any,
    /// These bits set, whatever the others.
    Bits(u32),
}

impl Want {
    fn accepts(self, v: f32) -> bool {
        match self {
            Want::Exactly(want) => (v - want).abs() < 0.01,
            Want::Within(max) => v > 0.0 && v <= max + 1e-4,
            Want::Between(lo, hi) => v >= lo - 1e-4 && v <= hi + 1e-4,
            Want::Any => true,
            Want::Bits(b) => v >= 0.0 && (v as u32) & b == b,
        }
    }
}

impl fmt::Display for Want {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Want::Exactly(v) => write!(f, "{v}"),
            Want::Within(max) => write!(f, "> 0 and ≤ {max} in a confined place"),
            Want::Between(lo, hi) => write!(f, "{lo} to {hi}"),
            Want::Any => write!(f, "any value"),
            Want::Bits(b) => write!(f, "bits {b} set"),
        }
    }
}

/// ArduCopter. The pilot does its own obstacle avoidance, so the FC's path
/// planner (`OA_TYPE`) and its fence avoidance stay off: they would fight
/// the pilot's setpoints. Its proximity avoidance is on as a backstop under
/// the pilot's brake (`AVOID_ENABLE` 2, stop at `AVOID_MARGIN`, inside the
/// pilot's margin: [`fc_avoid_margin_m`]), so a pilot that stopped acting
/// still leaves the vehicle stopping short of what Navigation's
/// `OBSTACLE_DISTANCE` shows. Every FC-side return is SmartRTL with a Land
/// fallback, back along the flown path, never a straight line through what
/// the pilot flew around.
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
    // 2: proximity only; 1: stop, never slide. The values here are the
    // default margin's; the profile takes the flown one (for_margin), and
    // a margin too small for the FC's stop inside it turns it off. The
    // backstop never blocks a flight by itself (see the module note).
    p(AVOID_ENABLE, &[], AVOID_PROXIMITY).backstop(),
    p(AVOID_MARGIN, &[], 1.0).backstop(),
    p("AVOID_BEHAVE", &[], 1.0).backstop(),
    // 2: MAVLink: the proximity library reads Navigation's
    // OBSTACLE_DISTANCE (PRX_TYPE before ArduPilot 4.3).
    p("PRX1_TYPE", &["PRX_TYPE"], 2.0).reboots().backstop(),
    p("SRTL_POINTS", &[], 500.0)
        .reboots()
        .absent_blocks("SmartRTL not in this firmware: no return along the flown path"),
    // A confined place takes a tighter accuracy (see
    // CONFINED_SRTL_ACCURACY_MAX_M); an open one the firmware default.
    p("SRTL_ACCURACY", &[], 2.0).confined_at_most(CONFINED_SRTL_ACCURACY_MAX_M),
    // A softer attitude input for the fixed camera (see OPEN_INPUT_TC_S).
    p("ATC_INPUT_TC", &[], OPEN_INPUT_TC_S.0).open_within(OPEN_INPUT_TC_S),
    // The supervised lift-off: SET_ATTITUDE_TARGET thrust read as thrust,
    // and PID_TUNING for roll and pitch. Neither changes manual flight.
    p(
        "GUID_OPTIONS",
        &[],
        ados_protocol::mavlink::GUID_OPTIONS_THRUST as f32,
    )
    .bits_set()
    .set_by_pilot(),
    p(
        "GCS_PID_MASK",
        &[],
        crate::liftoff::GCS_PID_ROLL_PITCH as f32,
    )
    .bits_set()
    .set_by_pilot(),
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

    /// Camera flight in an open place wants the softer attitude input:
    /// ArduCopter's crisp 0.10 default fails, 0.15-0.2 passes and is kept,
    /// and the wizard writes 0.15 over anything else. A confined place keeps
    /// whatever the FC has.
    #[test]
    fn an_open_place_wants_a_soft_attitude_input_and_a_confined_one_keeps_its_own() {
        let row = COPTER
            .iter()
            .find(|r| r.name == "ATC_INPUT_TC")
            .expect("the copter profile has an ATC_INPUT_TC row");
        let check = |place, v: f32| check_row(row, place, |_| Some(v));
        for ok in [0.15, 0.18, 0.2] {
            assert_eq!(check(PlaceClass::Open, ok), RowCheck::Ok, "{ok}");
        }
        for wrong in [0.0, 0.1, 0.25, 0.5] {
            assert!(
                matches!(check(PlaceClass::Open, wrong), RowCheck::Mismatch { .. }),
                "{wrong}"
            );
        }
        for any in [0.05, 0.1, 0.15, 0.5] {
            assert_eq!(check(PlaceClass::Confined, any), RowCheck::Ok, "{any}");
        }
        assert_eq!(row.value_in(PlaceClass::Open, Some(0.1)), 0.15);
        assert_eq!(row.value_in(PlaceClass::Open, Some(0.18)), 0.18);
        assert_eq!(row.value_in(PlaceClass::Open, None), 0.15);
        assert_eq!(row.value_in(PlaceClass::Confined, Some(0.1)), 0.1);
    }

    fn row(name: &str) -> &'static ProfileParam {
        COPTER.iter().find(|r| r.name == name).unwrap()
    }

    #[test]
    fn the_lift_off_rows_want_their_bits_set_and_keep_the_operators_others() {
        let guid = row("GUID_OPTIONS");
        let pid = row("GCS_PID_MASK");
        assert_eq!(guid.want(PlaceClass::Open), Want::Bits(8));
        assert_eq!(pid.want(PlaceClass::Confined), Want::Bits(3));
        assert!(guid.pilot_sets && pid.pilot_sets);
        // The wizard ORs the bits in.
        for place in [PlaceClass::Open, PlaceClass::Confined] {
            assert_eq!(guid.value_in(place, None), 8.0);
            assert_eq!(guid.value_in(place, Some(0.0)), 8.0);
            assert_eq!(guid.value_in(place, Some(1.0)), 9.0);
            assert_eq!(guid.value_in(place, Some(9.0)), 9.0);
            assert_eq!(pid.value_in(place, Some(4.0)), 7.0);
        }
        // Set with other bits passes; missing a bit does not.
        let check = |r, v: f32| check_row(r, PlaceClass::Open, |_| Some(v));
        assert_eq!(check(guid, 9.0), RowCheck::Ok);
        assert_eq!(check(pid, 7.0), RowCheck::Ok);
        assert_eq!(
            check(pid, 1.0),
            RowCheck::Mismatch {
                name: "GCS_PID_MASK",
                have: 1.0,
                want: Want::Bits(3)
            }
        );
        assert_eq!(Want::Bits(8).to_string(), "bits 8 set");
    }
}

#[cfg(test)]
mod backstop_tests {
    use super::*;

    fn copter_row(name: &str) -> ProfileParam {
        *fc_profile(Firmware::Ardupilot, VehicleClass::Multirotor)
            .iter()
            .find(|r| r.name == name)
            .unwrap()
    }

    #[test]
    fn the_fc_proximity_stop_is_a_backstop_inside_the_pilot_margin() {
        assert_eq!(copter_row("AVOID_ENABLE").value, 2.0, "proximity only");
        assert_eq!(copter_row("AVOID_BEHAVE").value, 1.0, "stop");
        assert_eq!(copter_row("OA_TYPE").value, 0.0, "no FC path planner");
        let prx = copter_row("PRX1_TYPE");
        assert_eq!(
            (prx.value, prx.alt_names, prx.reboot),
            (2.0, &["PRX_TYPE"][..], true)
        );
        // The table's values are the default margin's.
        let open = crate::config::PilotSection::default().effective_margin_m();
        assert_eq!(
            fc_avoid_margin_m(open),
            Some(copter_row(AVOID_MARGIN).value)
        );
        // 0.7 m inside the pilot's margin, never under the documented 1 m,
        // and always at least 0.3 m inside it: else the backstop is off.
        for (pilot, fc) in [(1.5, Some(1.0)), (2.0, Some(1.3)), (1.3, Some(1.0))] {
            assert_eq!(
                fc_avoid_margin_m(pilot).map(|m| (m * 100.0).round()),
                fc.map(|m: f32| (m * 100.0).round()),
                "{pilot}"
            );
            let row = for_margin(copter_row(AVOID_MARGIN), pilot);
            assert!(row.value < pilot - 0.29, "{pilot}: {}", row.value);
            assert_eq!(for_margin(copter_row(AVOID_ENABLE), pilot).value, 2.0);
        }
        // A confined margin (tips + 0.1 m) or a small open one: off.
        for pilot in [0.48, 0.5, 1.0, 1.25] {
            assert_eq!(fc_avoid_margin_m(pilot), None, "{pilot}");
            assert_eq!(for_margin(copter_row(AVOID_ENABLE), pilot).value, 0.0);
            let m = for_margin(copter_row(AVOID_MARGIN), pilot);
            assert_eq!(
                check_row(&m, PlaceClass::Confined, |_| Some(2.0)),
                RowCheck::Ok
            );
        }
        // Other rows keep their value.
        let guid = copter_row("GUID_TIMEOUT");
        assert_eq!(for_margin(guid, 3.0), guid);
        // The declared profile carries the flown margin's value.
        let rows: Vec<_> = crate::no_rc::profile(
            Firmware::Ardupilot,
            VehicleClass::Multirotor,
            ados_protocol::pilot::RcReceiver::Present,
            2.5,
        )
        .collect();
        let m = rows.iter().find(|r| r.name == AVOID_MARGIN).unwrap();
        assert!((m.value - 1.8).abs() < 1e-4);
        assert_eq!(
            check_row(m, PlaceClass::Open, |_| Some(0.8)),
            RowCheck::Mismatch {
                name: AVOID_MARGIN,
                have: 0.8,
                want: Want::Exactly(1.8)
            }
        );
    }
}
