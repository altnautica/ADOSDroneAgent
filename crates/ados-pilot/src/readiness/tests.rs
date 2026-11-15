use ados_protocol::mavlink::ardupilotmega::MavLandedState;
use ados_protocol::pilot::{ControlMode, Readiness, Severity};

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
    };
    items(fc, mode, false, &ctx, 2)
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
    ("FS_GCS_ENABLE", 1.0),
    ("FS_GCS_TIMEOUT", 3.0),
    ("GUID_TIMEOUT", 1.0),
    ("RC_OVERRIDE_TIME", 1.0),
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
