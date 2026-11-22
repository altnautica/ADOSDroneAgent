use ados_protocol::mavlink::ardupilotmega::MavLandedState;
use ados_protocol::pilot::{ControlMode, PlaceClass, RcReceiver, Readiness, Severity};

use super::items;
use crate::config::FcSensors;
use crate::fc::FcState;
use crate::fc_profile::Firmware;
use crate::nav_imu::NavImuTrack;
use crate::sensor_health::LidarTrack;
use crate::sensor_readiness::SensorCtx;

/// A linked quad on the ground holding `params`.
fn quad(fw: Firmware, params: &[(&str, f32)]) -> FcState {
    let mut fc = FcState {
        sysid: 1,
        compid: 1,
        heartbeat_ms: 1,
        firmware: Some(fw),
        mav_type: 2,
        landed_state: Some(MavLandedState::MAV_LANDED_STATE_ON_GROUND),
        ..Default::default()
    };
    for (k, v) in params {
        fc.params.insert((*k).to_owned(), *v);
    }
    // The table is fully read: every profile row not given was answered
    // DOES_NOT_EXIST.
    if !params.is_empty() {
        let class = fc.vehicle_class();
        for row in crate::fc_profile::fc_profile(fw, class) {
            if !fc.params.contains_key(row.name) {
                fc.param_asks
                    .insert(row.name.into(), crate::fc::PARAM_ABSENT_ASKS);
            }
        }
    }
    fc
}

fn all(fc: &FcState, mode: ControlMode) -> Vec<Readiness> {
    let lidar = LidarTrack::default();
    let mounts = FcSensors::default();
    let nav_imu = NavImuTrack::default();
    let ctx = SensorCtx {
        lidar: &lidar,
        avoid_on: false,
        mounts: &mounts,
        nav_imu: &nav_imu,
        nav_set: None,
    };
    items(
        fc,
        mode,
        PlaceClass::Open,
        RcReceiver::Present,
        1.5,
        false,
        &ctx,
        2,
    )
}

fn find(items: &[Readiness], id: &str) -> Option<Readiness> {
    items.iter().find(|r| r.id == id).cloned()
}

fn battery(fc: &FcState, mode: ControlMode) -> Option<Readiness> {
    find(&all(fc, mode), "battery_failsafe")
}

fn ardupilot(params: &[(&str, f32)], mode: ControlMode) -> Readiness {
    battery(&quad(Firmware::Ardupilot, params), mode).expect("a battery_failsafe item")
}

/// The spec text `readiness.battery_failsafe.<key>`.
fn text(key: &str) -> String {
    let spec: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../web-shared/src/cockpit-spec/setup-strings.json"
    ))
    .unwrap();
    spec["strings"][format!("readiness.battery_failsafe.{key}")]
        .as_str()
        .unwrap()
        .to_owned()
}

/// ArduCopter's defaults on a 4S pack: a monitor, both levels set, both
/// actions 0 (warn only).
const WARN_ONLY: &[(&str, f32)] = &[
    ("BATT_MONITOR", 4.0),
    ("BATT_FS_LOW_ACT", 0.0),
    ("BATT_FS_CRT_ACT", 0.0),
    ("BATT_LOW_VOLT", 14.0),
    ("BATT_LOW_MAH", 0.0),
    ("BATT_CRT_VOLT", 12.8),
    ("BATT_CRT_MAH", 0.0),
];

/// The rest of the copter ADOS profile, matching.
const PROFILE_REST: &[(&str, f32)] = &[
    ("MAV_GCS_SYSID", 1.0),
    ("FS_GCS_ENABLE", 4.0),
    ("FS_GCS_TIMEOUT", 3.0),
    ("FS_THR_ENABLE", 5.0),
    ("GUID_TIMEOUT", 1.0),
    ("RC_OVERRIDE_TIME", 1.0),
    ("OA_TYPE", 0.0),
    ("AVOID_ENABLE", 2.0),
    ("AVOID_MARGIN", 1.0),
    ("AVOID_BEHAVE", 1.0),
    ("PRX1_TYPE", 2.0),
    ("SRTL_POINTS", 500.0),
    ("SRTL_ACCURACY", 2.0),
];

fn with<'a>(base: &[(&'a str, f32)], over: &[(&'a str, f32)]) -> Vec<(&'a str, f32)> {
    let mut v = base.to_vec();
    for (k, x) in over {
        v.retain(|(n, _)| n != k);
        v.push((k, *x));
    }
    v
}

#[test]
fn warn_only_battery_failsafe_fails_readiness() {
    let r = ardupilot(WARN_ONLY, ControlMode::FcManual);
    assert_eq!((r.ok, r.severity), (false, Severity::Warn));
    assert_eq!(
        r.msg,
        "Battery failsafe does nothing: set low = SmartRTL or Land, critical = Land"
    );
    // Under ADOS control it blocks, as the FC failsafe profile does.
    let r = ardupilot(WARN_ONLY, ControlMode::Ados);
    assert_eq!((r.ok, r.severity), (false, Severity::Block));
}

#[test]
fn smart_rtl_or_land_and_land_pass_with_gnss() {
    let p = with(
        WARN_ONLY,
        &[("BATT_FS_LOW_ACT", 4.0), ("BATT_FS_CRT_ACT", 1.0)],
    );
    let r = ardupilot(&p, ControlMode::Ados);
    assert!(r.ok, "{r:?}");
    assert_eq!(r.msg, text("ok"));
}

#[test]
fn plain_rtl_or_smart_rtl_on_low_with_gnss_differs() {
    for low in [2.0, 3.0] {
        let p = with(
            WARN_ONLY,
            &[("BATT_FS_LOW_ACT", low), ("BATT_FS_CRT_ACT", 1.0)],
        );
        let r = ardupilot(&p, ControlMode::Ados);
        assert_eq!((r.ok, r.severity), (false, Severity::Block), "{low}");
        assert_eq!(r.msg, text("differs"));
    }
}

#[test]
fn one_action_off_fails_like_the_profile() {
    let p = with(WARN_ONLY, &[("BATT_FS_LOW_ACT", 4.0)]);
    let r = ardupilot(&p, ControlMode::Ados);
    assert_eq!((r.ok, r.severity), (false, Severity::Block));
    assert_eq!(r.msg, text("critical_none"));
    let p = with(WARN_ONLY, &[("BATT_FS_CRT_ACT", 1.0)]);
    let r = ardupilot(&p, ControlMode::FcManual);
    assert_eq!((r.ok, r.severity), (false, Severity::Warn));
    assert_eq!(
        r.msg,
        "Low battery does nothing: set low = SmartRTL or Land"
    );
}

#[test]
fn land_on_low_with_gnss_differs_from_the_profile() {
    let p = with(
        WARN_ONLY,
        &[("BATT_FS_LOW_ACT", 1.0), ("BATT_FS_CRT_ACT", 1.0)],
    );
    let r = ardupilot(&p, ControlMode::Ados);
    assert_eq!((r.ok, r.severity), (false, Severity::Block));
    assert_eq!(
        r.msg,
        "Battery failsafe not set for ADOS: set low = SmartRTL or Land, critical = Land"
    );
}

#[test]
fn unset_levels_warn() {
    // ArduCopter's default critical voltage is 0: that level never fires.
    let p = with(
        WARN_ONLY,
        &[
            ("BATT_FS_LOW_ACT", 4.0),
            ("BATT_FS_CRT_ACT", 1.0),
            ("BATT_CRT_VOLT", 0.0),
        ],
    );
    let r = ardupilot(&p, ControlMode::Ados);
    assert_eq!((r.ok, r.severity), (false, Severity::Warn));
    assert_eq!(
        r.msg,
        "Battery failsafe levels not set: set the low and critical voltage"
    );
    // A capacity threshold counts as a level.
    let p = with(&p, &[("BATT_CRT_MAH", 300.0)]);
    assert!(ardupilot(&p, ControlMode::Ados).ok);
}

#[test]
fn no_monitor_blocks_under_ados() {
    let p = with(WARN_ONLY, &[("BATT_MONITOR", 0.0)]);
    let r = ardupilot(&p, ControlMode::Ados);
    assert_eq!((r.ok, r.severity), (false, Severity::Block));
    assert_eq!(
        r.msg,
        "No battery monitor: set BATT_MONITOR so the battery failsafe can act"
    );
    let r = ardupilot(&p, ControlMode::FcManual);
    assert_eq!((r.ok, r.severity), (false, Severity::Warn));
}

#[test]
fn unread_parameters_read_unknown() {
    let r = ardupilot(&[], ControlMode::Ados);
    assert_eq!((r.ok, r.severity), (false, Severity::Warn));
    assert_eq!(r.msg, "Reading battery failsafe parameters");
}

#[test]
fn px4_has_no_battery_failsafe_item() {
    assert!(battery(&quad(Firmware::Px4, WARN_ONLY), ControlMode::FcManual).is_none());
}

#[test]
fn a_plane_with_no_low_action_is_told_rtl_only() {
    let mut fc = quad(Firmware::Ardupilot, WARN_ONLY);
    fc.mav_type = 1;
    let r = battery(&fc, ControlMode::Ados).unwrap();
    assert_eq!((r.ok, r.severity), (false, Severity::Block));
    assert_eq!(r.msg, "Low battery does nothing: set low = RTL");
}

/// A copter navigating by external nav (`EK3_SRC1_POSXY` 6) with the
/// battery actions `low`, `crt` and the rest of the profile matching.
fn gnss_free_copter(low: f32, crt: f32) -> FcState {
    let mut p = with(
        WARN_ONLY,
        &[("BATT_FS_LOW_ACT", low), ("BATT_FS_CRT_ACT", crt)],
    );
    p.extend_from_slice(PROFILE_REST);
    p.push(("EK3_SRC1_POSXY", 6.0));
    quad(Firmware::Ardupilot, &p)
}

#[test]
fn gnss_free_copter_with_land_passes_both_items() {
    let items = all(&gnss_free_copter(1.0, 1.0), ControlMode::Ados);
    let profile = find(&items, "fc_profile").unwrap();
    assert!(profile.ok, "{profile:?}");
    let batt = find(&items, "battery_failsafe").unwrap();
    assert!(batt.ok, "{batt:?}");
    assert!(super::first_block(&items)
        .is_none_or(|b| b.id != "fc_profile" && b.id != "battery_failsafe"));
}

#[test]
fn gnss_free_copter_with_rtl_blocks_on_the_battery_item_only() {
    let items = all(&gnss_free_copter(2.0, 1.0), ControlMode::Ados);
    assert!(find(&items, "fc_profile").unwrap().ok);
    let batt = find(&items, "battery_failsafe").unwrap();
    assert_eq!((batt.ok, batt.severity), (false, Severity::Block));
    assert_eq!(
        batt.msg,
        "May fly without GNSS: a battery RTL climbs to the RTL altitude, into the roof indoors; set low = Land, critical = Land"
    );
    // Under FC control it warns.
    let items = all(&gnss_free_copter(2.0, 1.0), ControlMode::FcManual);
    let batt = find(&items, "battery_failsafe").unwrap();
    assert_eq!((batt.ok, batt.severity), (false, Severity::Warn));
}

#[test]
fn gnss_free_copter_recommends_land() {
    let r = find(
        &all(&gnss_free_copter(0.0, 0.0), ControlMode::Ados),
        "battery_failsafe",
    )
    .unwrap();
    assert_eq!(r.msg, text("none_confined"));
    let r = find(
        &all(&gnss_free_copter(0.0, 1.0), ControlMode::Ados),
        "battery_failsafe",
    )
    .unwrap();
    assert_eq!(
        r.msg,
        "Low battery does nothing: may fly without GNSS, so set low = Land"
    );
    // SmartRTL-or-Land is the GNSS action: without GNSS it differs.
    let r = find(
        &all(&gnss_free_copter(4.0, 1.0), ControlMode::Ados),
        "battery_failsafe",
    )
    .unwrap();
    assert_eq!(r.msg, text("differs_confined"));
}

#[test]
fn any_source_set_without_gnss_decides() {
    // GPS on set 1, external nav on set 2: the FC may switch to set 2 in
    // flight, so the battery action must be safe there too.
    let on = |low: f32| {
        let mut p = with(
            WARN_ONLY,
            &[("BATT_FS_LOW_ACT", low), ("BATT_FS_CRT_ACT", 1.0)],
        );
        p.push(("EK3_SRC1_POSXY", 3.0));
        p.push(("EK3_SRC2_POSXY", 6.0));
        ardupilot(&p, ControlMode::Ados)
    };
    assert_eq!(on(2.0).msg, text("confined_rtl"));
    assert_eq!(on(4.0).msg, text("differs_confined"));
    assert!(on(1.0).ok);
}

/// An ArduCopter whose parameters match the whole profile.
pub(super) fn copter() -> FcState {
    let mut fc = FcState {
        firmware: Some(Firmware::Ardupilot),
        ..FcState::default()
    };
    let rows = crate::fc_profile::fc_profile(
        Firmware::Ardupilot,
        ados_protocol::pilot::VehicleClass::Multirotor,
    );
    for row in rows {
        fc.params.insert(row.name.into(), row.value);
    }
    fc
}

fn profile(fc: &FcState) -> Readiness {
    profile_in(fc, PlaceClass::Open)
}

pub(super) fn profile_in(fc: &FcState, place: PlaceClass) -> Readiness {
    super::profile_item(
        fc,
        Firmware::Ardupilot,
        ControlMode::Ados,
        place,
        RcReceiver::Present,
        1.5,
    )
}

#[test]
fn a_confined_place_accepts_a_tight_smartrtl_accuracy_an_open_one_does_not() {
    let with_accuracy = |v: f32| {
        let mut fc = copter();
        fc.params.insert("SRTL_ACCURACY".into(), v);
        fc
    };
    let tight = with_accuracy(0.2);
    assert!(profile_in(&tight, PlaceClass::Confined).ok);
    let r = profile_in(&tight, PlaceClass::Open);
    assert!(!r.ok);
    assert_eq!(r.msg, "FC parameters differ: SRTL_ACCURACY 0.2 (want 2)");
    // Coarser than the bound, the firmware default, and 0 (SmartRTL off)
    // block there.
    for wrong in [0.25, 2.0, 0.0] {
        let r = profile_in(&with_accuracy(wrong), PlaceClass::Confined);
        assert_eq!((r.ok, r.severity), (false, Severity::Block), "{wrong}");
        assert_eq!(
            r.msg,
            format!(
                "FC parameters differ: SRTL_ACCURACY {wrong} (want > 0 and ≤ 0.2 in a confined place)"
            )
        );
    }
    // Every other row still wants its own value in a confined place.
    let mut fc = with_accuracy(0.2);
    fc.params.insert("FS_THR_ENABLE".into(), 1.0);
    assert_eq!(
        profile_in(&fc, PlaceClass::Confined).msg,
        "FC parameters differ: FS_THR_ENABLE 1 (want 5)"
    );
}

/// The proximity backstop rows are `fc_backstop`'s and never block by
/// themselves (`backstop_tests`).
#[test]
fn each_avoidance_and_return_row_blocks_when_it_differs() {
    assert!(profile(&copter()).ok);
    for (name, want, wrong) in [
        ("OA_TYPE", 0.0, 1.0),
        ("FS_GCS_ENABLE", 4.0, 1.0),
        ("FS_THR_ENABLE", 5.0, 1.0),
        ("SRTL_POINTS", 500.0, 300.0),
        ("SRTL_ACCURACY", 2.0, 1.0),
    ] {
        let mut fc = copter();
        assert_eq!(fc.params.get(name), Some(&want), "{name} in the profile");
        fc.params.insert(name.into(), wrong);
        let r = profile(&fc);
        assert!(!r.ok, "{name} {wrong} passes");
        assert_eq!(r.severity, Severity::Block, "{name}");
        assert!(
            r.msg.contains(&format!("{name} {wrong} (want {want})")),
            "{name}: {}",
            r.msg
        );
    }
}

/// `name` asked for by name until the pilot takes it as absent.
fn asked_out(fc: &mut FcState, name: &str) {
    fc.params.remove(name);
    fc.param_asks
        .insert(name.into(), crate::fc::PARAM_ABSENT_ASKS);
}

#[test]
fn absent_smartrtl_blocks_while_another_absent_row_is_skipped() {
    let mut fc = copter();
    asked_out(&mut fc, "GUID_TIMEOUT");
    asked_out(&mut fc, "OA_TYPE");
    assert!(profile(&fc).ok, "{}", profile(&fc).msg);

    let mut fc = copter();
    asked_out(&mut fc, "SRTL_POINTS");
    let r = profile(&fc);
    assert!(!r.ok);
    assert_eq!(r.severity, Severity::Block);
    assert!(r.msg.contains("SRTL_POINTS absent"), "{}", r.msg);
}

#[test]
fn a_parameter_not_answered_yet_is_still_being_read_not_absent() {
    // ArduPilot drops by-name reads past its 20-deep request queue: on the
    // bench SRTL_POINTS stayed unanswered for the whole 60 s readiness wait
    // while the rest of the table arrived, and readiness called it "not in
    // this firmware" and refused the flight.
    let mut fc = copter();
    fc.params.remove("SRTL_POINTS");
    fc.param_asks.insert("SRTL_POINTS".into(), 1);
    let r = profile(&fc);
    assert!(!r.ok);
    assert_eq!(r.msg, "Reading flight controller failsafe parameters");
    // A real mismatch is not hidden behind the reading state.
    fc.params.insert("GUID_TIMEOUT".into(), 99.0);
    assert!(profile(&fc).msg.starts_with("FC parameters differ"));
}

/// The lift-off rows are the pilot's to set before a supervised lift-off:
/// an FC with them clear still passes `fc_profile`.
#[test]
fn the_lift_off_bits_never_fail_the_profile() {
    let mut fc = gnss_free_copter(1.0, 1.0);
    fc.params.insert("GUID_OPTIONS".into(), 1.0);
    fc.params.insert("GCS_PID_MASK".into(), 0.0);
    let items = all(&fc, ControlMode::Ados);
    let profile = find(&items, "fc_profile").unwrap();
    assert!(profile.ok, "{profile:?}");
}
