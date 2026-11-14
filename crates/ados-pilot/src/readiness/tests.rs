use ados_protocol::mavlink::ardupilotmega::MavLandedState;
use ados_protocol::pilot::{ControlMode, Readiness, Severity};

use super::items;
use crate::batt_failsafe::BattCtx;
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

const GNSS: BattCtx = BattCtx {
    nav_set: 1,
    ceiling_m: None,
};

fn all(fc: &FcState, mode: ControlMode, batt: &BattCtx) -> Vec<Readiness> {
    let lidar = LidarTrack::default();
    let mounts = FcSensors::default();
    let nav_imu = NavImuTrack::default();
    let ctx = SensorCtx {
        lidar: &lidar,
        avoid_on: false,
        mounts: &mounts,
        nav_imu: &nav_imu,
    };
    items(fc, mode, false, &ctx, batt, 2)
}

fn find(items: &[Readiness], id: &str) -> Option<Readiness> {
    items.iter().find(|r| r.id == id).cloned()
}

fn battery(fc: &FcState, mode: ControlMode) -> Option<Readiness> {
    find(&all(fc, mode, &GNSS), "battery_failsafe")
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
        "Battery failsafe does nothing: set low = RTL, critical = Land"
    );
    // Under ADOS control it blocks, as the FC failsafe profile does.
    let r = ardupilot(WARN_ONLY, ControlMode::Ados);
    assert_eq!((r.ok, r.severity), (false, Severity::Block));
}

#[test]
fn rtl_and_land_pass() {
    let p = with(
        WARN_ONLY,
        &[("BATT_FS_LOW_ACT", 2.0), ("BATT_FS_CRT_ACT", 1.0)],
    );
    let r = ardupilot(&p, ControlMode::Ados);
    assert!(r.ok, "{r:?}");
    assert_eq!(r.msg, text("ok"));
}

#[test]
fn one_action_off_fails_like_the_profile() {
    let p = with(WARN_ONLY, &[("BATT_FS_LOW_ACT", 2.0)]);
    let r = ardupilot(&p, ControlMode::Ados);
    assert_eq!((r.ok, r.severity), (false, Severity::Block));
    assert_eq!(r.msg, text("critical_none"));
    let p = with(WARN_ONLY, &[("BATT_FS_CRT_ACT", 1.0)]);
    let r = ardupilot(&p, ControlMode::FcManual);
    assert_eq!((r.ok, r.severity), (false, Severity::Warn));
    assert_eq!(r.msg, text("low_none"));
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
        "Battery failsafe not set for ADOS: set low = RTL, critical = Land"
    );
}

#[test]
fn unset_levels_warn() {
    // ArduCopter's default critical voltage is 0: that level never fires.
    let p = with(
        WARN_ONLY,
        &[
            ("BATT_FS_LOW_ACT", 2.0),
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
fn no_monitor_reads_unknown() {
    let p = with(WARN_ONLY, &[("BATT_MONITOR", 0.0)]);
    let r = ardupilot(&p, ControlMode::Ados);
    assert_eq!((r.ok, r.severity), (false, Severity::Warn));
    assert_eq!(r.msg, "Battery failsafe —: no battery monitor");
}

#[test]
fn unread_parameters_read_unknown() {
    let r = ardupilot(&[], ControlMode::Ados);
    assert_eq!((r.ok, r.severity), (false, Severity::Warn));
    assert_eq!(r.msg, text("unread"));
}

#[test]
fn px4_has_no_battery_failsafe_item() {
    assert!(battery(&quad(Firmware::Px4, WARN_ONLY), ControlMode::FcManual).is_none());
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
    let items = all(&gnss_free_copter(1.0, 1.0), ControlMode::Ados, &GNSS);
    let profile = find(&items, "fc_profile").unwrap();
    assert!(profile.ok, "{profile:?}");
    let batt = find(&items, "battery_failsafe").unwrap();
    assert!(batt.ok, "{batt:?}");
    assert!(super::first_block(&items)
        .is_none_or(|b| b.id != "fc_profile" && b.id != "battery_failsafe"));
}

#[test]
fn gnss_free_copter_with_rtl_blocks_on_the_battery_item_only() {
    let items = all(&gnss_free_copter(2.0, 1.0), ControlMode::Ados, &GNSS);
    assert!(find(&items, "fc_profile").unwrap().ok);
    let batt = find(&items, "battery_failsafe").unwrap();
    assert_eq!((batt.ok, batt.severity), (false, Severity::Block));
    assert_eq!(
        batt.msg,
        "No GNSS: a battery RTL climbs to the RTL altitude, into the roof indoors; set low = Land, critical = Land"
    );
    // Under FC control it warns.
    let items = all(&gnss_free_copter(2.0, 1.0), ControlMode::FcManual, &GNSS);
    let batt = find(&items, "battery_failsafe").unwrap();
    assert_eq!((batt.ok, batt.severity), (false, Severity::Warn));
}

#[test]
fn gnss_free_copter_recommends_land_when_off() {
    let r = find(
        &all(&gnss_free_copter(0.0, 0.0), ControlMode::Ados, &GNSS),
        "battery_failsafe",
    )
    .unwrap();
    assert_eq!(r.msg, text("none_confined"));
    let r = find(
        &all(&gnss_free_copter(0.0, 1.0), ControlMode::Ados, &GNSS),
        "battery_failsafe",
    )
    .unwrap();
    assert_eq!(
        r.msg,
        "Low battery does nothing: no GNSS, so set low = Land"
    );
}

#[test]
fn the_active_source_set_decides() {
    // GPS on set 1, external nav on set 2: RTL is right on set 1 only.
    let mut p = with(
        WARN_ONLY,
        &[("BATT_FS_LOW_ACT", 2.0), ("BATT_FS_CRT_ACT", 1.0)],
    );
    p.push(("EK3_SRC1_POSXY", 3.0));
    p.push(("EK3_SRC2_POSXY", 6.0));
    let fc = quad(Firmware::Ardupilot, &p);
    let on = |nav_set| {
        find(
            &all(
                &fc,
                ControlMode::Ados,
                &BattCtx {
                    nav_set,
                    ceiling_m: None,
                },
            ),
            "battery_failsafe",
        )
        .unwrap()
    };
    assert!(on(1).ok);
    assert_eq!(on(2).msg, text("confined_rtl"));
}

#[test]
fn rtl_above_the_altitude_limit_warns() {
    let p = with(
        WARN_ONLY,
        &[
            ("BATT_FS_LOW_ACT", 2.0),
            ("BATT_FS_CRT_ACT", 1.0),
            ("RTL_ALT_M", 15.0),
        ],
    );
    let fc = quad(Firmware::Ardupilot, &p);
    let at = |ceiling_m| {
        find(
            &all(
                &fc,
                ControlMode::Ados,
                &BattCtx {
                    nav_set: 1,
                    ceiling_m: Some(ceiling_m),
                },
            ),
            "battery_failsafe",
        )
        .unwrap()
    };
    let r = at(8.0);
    assert_eq!((r.ok, r.severity), (false, Severity::Warn));
    assert_eq!(
        r.msg,
        "Battery RTL climbs to 15 m, above the 8 m altitude limit: lower the RTL altitude"
    );
    assert!(at(120.0).ok);
    // Before ArduCopter 4.7: RTL_ALT in centimetres.
    let p = with(&p, &[("BATT_FS_LOW_ACT", 2.0)])
        .into_iter()
        .filter(|(k, _)| *k != "RTL_ALT_M")
        .chain([("RTL_ALT", 1500.0)])
        .collect::<Vec<_>>();
    let fc = quad(Firmware::Ardupilot, &p);
    let r = find(
        &all(
            &fc,
            ControlMode::Ados,
            &BattCtx {
                nav_set: 1,
                ceiling_m: Some(8.0),
            },
        ),
        "battery_failsafe",
    )
    .unwrap();
    assert!(!r.ok);
}
