//! The FC's proximity backstop rows: a warning (`fc_backstop`) that never
//! blocks a flight by itself, on a fresh FC and on one set up before the
//! backstop joined the profile.

use ados_protocol::pilot::{PlaceClass, RcReceiver, Readiness, Severity, VehicleClass};

use super::tests::{copter, profile_in};
use crate::fc::FcState;
use crate::fc_profile::Firmware;

fn backstop_in(fc: &FcState, place: PlaceClass, margin_m: f32) -> Readiness {
    super::backstop_item(
        fc,
        Firmware::Ardupilot,
        place,
        RcReceiver::Present,
        margin_m,
    )
    .expect("a copter has the backstop rows")
}

fn backstop(fc: &FcState) -> Readiness {
    backstop_in(fc, PlaceClass::Open, 1.5)
}

/// `fc` with `params` over it.
fn with(mut fc: FcState, params: &[(&str, f32)]) -> FcState {
    for (k, v) in params {
        fc.params.insert((*k).into(), *v);
    }
    fc
}

/// The backstop rows as an FC set up before they joined the profile holds
/// them: avoidance off (the old profile's `AVOID_ENABLE` 0) and ArduCopter's
/// defaults for the rest.
const BEFORE_BACKSTOP: &[(&str, f32)] = &[
    ("AVOID_ENABLE", 0.0),
    ("AVOID_MARGIN", 2.0),
    ("AVOID_BEHAVE", 0.0),
    ("PRX1_TYPE", 0.0),
];

#[test]
fn an_fc_set_up_before_the_backstop_flies_with_a_warning() {
    let fc = with(copter(), BEFORE_BACKSTOP);
    let p = profile_in(&fc, PlaceClass::Open);
    assert!(p.ok, "the backstop never blocks by itself: {}", p.msg);
    let b = backstop(&fc);
    assert_eq!((b.ok, b.severity), (false, Severity::Warn));
    assert_eq!(
        b.msg,
        "FC proximity backstop not set: AVOID_ENABLE 0 (want 2), AVOID_MARGIN 2 (want 1), \
         AVOID_BEHAVE 0 (want 1), PRX1_TYPE 0 (want 2, used after an FC reboot). The pilot's \
         brake flies alone until the ADOS profile is applied in setup"
    );
    // Each row alone is a warning too, never a block (but AVOID_MARGIN 2
    // under a proximity stop that is on: that stop acts before the pilot's
    // brake, see below).
    for (name, have) in BEFORE_BACKSTOP.iter().filter(|(n, _)| *n != "AVOID_MARGIN") {
        let fc = with(copter(), &[(name, *have)]);
        assert!(profile_in(&fc, PlaceClass::Open).ok, "{name}");
        let b = backstop(&fc);
        assert_eq!((b.ok, b.severity), (false, Severity::Warn), "{name}");
        assert!(
            b.msg.contains(&format!("{name} {have} (want ")),
            "{}",
            b.msg
        );
    }
}

#[test]
fn a_fresh_fc_after_the_ados_profile_step_has_the_backstop() {
    // What the wizard writes over ArduCopter's defaults (AVOID_ENABLE 3:
    // fence and proximity).
    let defaults = with(
        copter(),
        &[
            ("AVOID_ENABLE", 3.0),
            ("AVOID_MARGIN", 2.0),
            ("AVOID_BEHAVE", 0.0),
            ("PRX1_TYPE", 0.0),
        ],
    );
    let p = profile_in(&defaults, PlaceClass::Open);
    assert!(!p.ok, "fence avoidance would act before the pilot's brake");
    assert_eq!(p.msg, "FC parameters differ: AVOID_ENABLE 3 (want 0 or 2)");
    let mut fc = defaults.clone();
    let rows = crate::no_rc::profile(
        Firmware::Ardupilot,
        VehicleClass::Multirotor,
        RcReceiver::Present,
        1.5,
    );
    for row in rows.filter(|r| r.backstop) {
        let have = defaults.params.get(row.name).copied();
        fc.params
            .insert(row.name.into(), row.value_in(PlaceClass::Open, have));
    }
    assert!(profile_in(&fc, PlaceClass::Open).ok);
    let b = backstop(&fc);
    assert_eq!((b.ok, b.severity), (true, Severity::Warn));
    assert_eq!(b.msg, "FC proximity backstop set");
}

#[test]
fn an_fc_avoider_acting_before_the_pilots_brake_still_blocks() {
    // Fence or beacon avoidance, any margin.
    for enable in [1.0, 3.0, 4.0, 7.0] {
        let p = profile_in(
            &with(copter(), &[("AVOID_ENABLE", enable)]),
            PlaceClass::Open,
        );
        assert_eq!((p.ok, p.severity), (false, Severity::Block), "{enable}");
        assert_eq!(
            p.msg,
            format!("FC parameters differ: AVOID_ENABLE {enable} (want 0 or 2)")
        );
    }
    // A proximity stop less than 0.3 m inside the pilot's 1.5 m margin.
    let p = profile_in(&with(copter(), &[("AVOID_MARGIN", 1.3)]), PlaceClass::Open);
    assert_eq!((p.ok, p.severity), (false, Severity::Block));
    assert_eq!(
        p.msg,
        "FC parameters differ: AVOID_MARGIN 1.3 (want ≤ 1.2 with AVOID_ENABLE 2)"
    );
    // Inside it but not the profile's value: a warning only.
    let fc = with(copter(), &[("AVOID_MARGIN", 1.2)]);
    assert!(profile_in(&fc, PlaceClass::Open).ok);
    assert!(!backstop(&fc).ok);
    // With the backstop off, AVOID_MARGIN is moot.
    let fc = with(copter(), &[("AVOID_ENABLE", 0.0), ("AVOID_MARGIN", 5.0)]);
    assert!(profile_in(&fc, PlaceClass::Open).ok);
}

#[test]
fn a_confined_margin_wants_the_backstop_off_and_blocks_it_on() {
    // A confined margin from the propeller tips leaves no room for the FC's
    // stop inside it.
    let tips = 0.48;
    let item = |fc: &FcState| {
        super::profile_item(
            fc,
            Firmware::Ardupilot,
            ados_protocol::pilot::ControlMode::Ados,
            PlaceClass::Confined,
            RcReceiver::Present,
            tips,
        )
    };
    let confined = with(copter(), &[("SRTL_ACCURACY", 0.2)]);
    let p = item(&confined);
    assert_eq!((p.ok, p.severity), (false, Severity::Block));
    assert_eq!(p.msg, "FC parameters differ: AVOID_ENABLE 2 (want 0)");
    let off = with(confined, &[("AVOID_ENABLE", 0.0)]);
    assert!(item(&off).ok, "{}", item(&off).msg);
    let b = backstop_in(&off, PlaceClass::Confined, tips);
    assert_eq!(
        (b.ok, b.msg.as_str()),
        (
            true,
            "FC proximity backstop off: its stop does not fit inside the pilot's margin"
        )
    );
}

#[test]
fn backstop_rows_still_being_read_wait_only_for_the_avoidance_switch() {
    // AVOID_ENABLE decides whether the FC acts before the pilot: the
    // blocking item waits for it.
    let mut fc = copter();
    fc.params.remove("AVOID_ENABLE");
    fc.param_asks.insert("AVOID_ENABLE".into(), 1);
    assert_eq!(
        profile_in(&fc, PlaceClass::Open).msg,
        "Reading flight controller failsafe parameters"
    );
    // The other rows are the warning's to wait for.
    let mut fc = copter();
    for n in ["AVOID_BEHAVE", "PRX1_TYPE"] {
        fc.params.remove(n);
        fc.param_asks.insert(n.into(), 1);
    }
    assert!(profile_in(&fc, PlaceClass::Open).ok);
    let b = backstop(&fc);
    assert_eq!(
        (b.ok, b.severity, b.msg.as_str()),
        (
            false,
            Severity::Warn,
            "Reading the FC proximity backstop parameters"
        )
    );
    // A firmware without avoidance: the warning says so, nothing blocks.
    let mut fc = copter();
    for n in ["AVOID_ENABLE", "AVOID_MARGIN", "AVOID_BEHAVE"] {
        fc.params.remove(n);
        fc.param_asks.insert(n.into(), crate::fc::PARAM_ABSENT_ASKS);
    }
    assert!(profile_in(&fc, PlaceClass::Open).ok);
    assert_eq!(
        backstop(&fc).msg,
        "FC proximity backstop not in this firmware: AVOID_ENABLE, AVOID_MARGIN, AVOID_BEHAVE absent"
    );
}

#[test]
fn only_the_copter_profile_carries_the_backstop() {
    let mut fc = copter();
    fc.mav_type = 1; // fixed wing
    assert!(super::backstop_item(
        &fc,
        Firmware::Ardupilot,
        PlaceClass::Open,
        RcReceiver::Present,
        1.5
    )
    .is_none());
    assert!(super::backstop_item(
        &copter(),
        Firmware::Px4,
        PlaceClass::Open,
        RcReceiver::Present,
        1.5
    )
    .is_none());
}
