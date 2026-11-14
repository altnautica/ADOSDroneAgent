//! The battery failsafe on a copter, through the pilot's readiness: the
//! `battery_failsafe` item and the `fc_profile` item agree, with and without
//! GNSS.

use ados_protocol::pilot::{Readiness, Severity};

use super::harness::World;

/// Monitor, both levels set, the given actions and position source.
fn world(low: f32, crt: f32, posxy: f32) -> World {
    let mut w = World::new();
    for (n, v) in [
        ("BATT_MONITOR", 4.0),
        ("BATT_LOW_VOLT", 14.0),
        ("BATT_LOW_MAH", 0.0),
        ("BATT_CRT_VOLT", 12.8),
        ("BATT_CRT_MAH", 0.0),
        ("BATT_FS_LOW_ACT", low),
        ("BATT_FS_CRT_ACT", crt),
        ("EK3_SRC1_POSXY", posxy),
    ] {
        w.p.fc.params.insert(n.into(), v);
    }
    w.run(100);
    w
}

fn item(w: &World, id: &str) -> Option<Readiness> {
    w.p.readiness(w.now).into_iter().find(|r| r.id == id)
}

const GPS: f32 = 3.0;
const EXTERNAL_NAV: f32 = 6.0;

#[test]
fn warn_only_actions_fail_readiness() {
    let w = world(0.0, 0.0, GPS);
    let r = item(&w, "battery_failsafe").expect("a battery_failsafe item");
    assert_eq!((r.ok, r.severity), (false, Severity::Block));
    assert_eq!(
        r.msg,
        "Battery failsafe does nothing: set low = RTL, critical = Land"
    );
}

#[test]
fn gnss_free_copter_with_land_passes_both_items() {
    let w = world(1.0, 1.0, EXTERNAL_NAV);
    let profile = item(&w, "fc_profile").unwrap();
    assert!(profile.ok, "{profile:?}");
    let batt = item(&w, "battery_failsafe").expect("a battery_failsafe item");
    assert!(batt.ok, "{batt:?}");
}

#[test]
fn gnss_free_copter_with_rtl_blocks_on_no_gnss() {
    let w = world(2.0, 1.0, EXTERNAL_NAV);
    assert!(item(&w, "fc_profile").unwrap().ok);
    let batt = item(&w, "battery_failsafe").expect("a battery_failsafe item");
    assert_eq!((batt.ok, batt.severity), (false, Severity::Block));
    assert_eq!(
        batt.msg,
        "No GNSS: a battery RTL climbs to the RTL altitude, into the roof indoors; set low = Land, critical = Land"
    );
}

#[test]
fn copter_with_gnss_keeps_rtl() {
    let w = world(2.0, 1.0, GPS);
    assert!(item(&w, "fc_profile").unwrap().ok);
    let batt = item(&w, "battery_failsafe").expect("a battery_failsafe item");
    assert!(batt.ok, "{batt:?}");
    // Land on low with GNSS differs from the profile: one item says so.
    let w = world(1.0, 1.0, GPS);
    let profile = item(&w, "fc_profile").unwrap();
    let batt = item(&w, "battery_failsafe").expect("a battery_failsafe item");
    assert!(profile.ok, "{profile:?}");
    assert_eq!((batt.ok, batt.severity), (false, Severity::Block));
}

#[test]
fn rtl_above_the_altitude_limit_warns() {
    let mut w = world(2.0, 1.0, GPS);
    w.p.fc.params.insert("RTL_ALT_M".into(), 15.0);
    w.p.settings.pilot.max_alt_agl_m = 8.0;
    let r = item(&w, "battery_failsafe").expect("a battery_failsafe item");
    assert_eq!((r.ok, r.severity), (false, Severity::Warn));
    assert_eq!(
        r.msg,
        "Battery RTL climbs to 15 m, above the 8 m altitude limit: lower the RTL altitude"
    );
}
