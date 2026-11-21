//! The battery failsafe on a copter, through the pilot: the
//! `battery_failsafe` readiness item and the `fc_profile` item agree, with
//! and without GNSS; Take control refuses on every blocking item it guards;
//! and the pilot's own low-battery rule lands a copter that may fly without
//! GNSS, announcing what it does.

use ados_protocol::mavlink::ardupilotmega::*;
use ados_protocol::pilot::{Owner, PlaceClass, Readiness, Severity};

use super::harness::*;
use crate::core::Reply;
use crate::modes::copter;

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

fn err(r: &Reply) -> Option<&str> {
    match r {
        Reply::Action(a) if !a.ok => a.err.as_deref(),
        _ => None,
    }
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
        "Battery failsafe does nothing: set low = SmartRTL or Land, critical = Land"
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
        "May fly without GNSS: a battery RTL climbs to the RTL altitude, into the roof indoors; set low = Land, critical = Land"
    );
}

#[test]
fn copter_with_gnss_wants_smart_rtl_or_land() {
    let w = world(4.0, 1.0, GPS);
    assert!(item(&w, "fc_profile").unwrap().ok);
    let batt = item(&w, "battery_failsafe").expect("a battery_failsafe item");
    assert!(batt.ok, "{batt:?}");
    // Land or RTL on low with GNSS differs from the profile: one item says so.
    for low in [1.0, 2.0] {
        let w = world(low, 1.0, GPS);
        let profile = item(&w, "fc_profile").unwrap();
        let batt = item(&w, "battery_failsafe").expect("a battery_failsafe item");
        assert!(profile.ok, "{profile:?}");
        assert_eq!((batt.ok, batt.severity), (false, Severity::Block), "{low}");
    }
}

/// The pilot's reserve and the FC's mAh levels both count the charge used
/// against `BATT_CAPACITY`: without it they never act, so the item warns.
#[test]
fn an_unset_capacity_warns() {
    for unset in [0.0, 10.0] {
        let mut w = world(4.0, 1.0, GPS);
        w.p.fc.params.insert("BATT_CAPACITY".into(), unset);
        let batt = item(&w, "battery_failsafe").expect("a battery_failsafe item");
        assert_eq!((batt.ok, batt.severity), (false, Severity::Warn), "{unset}");
        assert_eq!(
            batt.msg,
            "Battery capacity not set: set BATT_CAPACITY so the low and critical reserve count the charge used"
        );
    }
    let mut w = world(4.0, 1.0, GPS);
    w.p.fc.params.insert("BATT_CAPACITY".into(), 5000.0);
    assert!(item(&w, "battery_failsafe").unwrap().ok);
}

/// A confined place: the vehicle's tip radius set, as a confined launch
/// needs, and the FC's SmartRTL accuracy within the confined bound.
fn confined(w: &mut World) {
    w.p.settings.pilot.place_class = PlaceClass::Confined;
    w.p.settings.pilot.prop_tip_radius_m = Some(0.377);
    w.p.fc.params.insert("SRTL_ACCURACY".into(), 0.2);
    w.p.fc.params.insert("AVOID_ENABLE".into(), 0.0);
    w.run(100);
}

#[test]
fn confined_external_nav_copter_wants_smart_rtl_or_land() {
    // Landing in place came down on furniture next to the hover point: in a
    // confined place on external navigation the low battery retraces the
    // flown path, which the route check cleared.
    let mut w = world(4.0, 1.0, EXTERNAL_NAV);
    confined(&mut w);
    assert!(item(&w, "fc_profile").unwrap().ok);
    let batt = item(&w, "battery_failsafe").expect("a battery_failsafe item");
    assert!(batt.ok, "{batt:?}");
    // Land on low there is the profile's error, and the fix names SmartRTL.
    for low in [1.0, 2.0] {
        let mut w = world(low, 1.0, EXTERNAL_NAV);
        confined(&mut w);
        let batt = item(&w, "battery_failsafe").expect("a battery_failsafe item");
        assert_eq!((batt.ok, batt.severity), (false, Severity::Block), "{low}");
        assert_eq!(
            batt.msg,
            "Battery failsafe not set for ADOS: set low = SmartRTL or Land, critical = Land"
        );
    }
    // Critical stays Land.
    let mut w = world(4.0, 4.0, EXTERNAL_NAV);
    confined(&mut w);
    assert!(!item(&w, "battery_failsafe").unwrap().ok);
}

#[test]
fn open_external_nav_copter_keeps_land() {
    let w = world(4.0, 1.0, EXTERNAL_NAV);
    let batt = item(&w, "battery_failsafe").expect("a battery_failsafe item");
    assert_eq!((batt.ok, batt.severity), (false, Severity::Block));
    assert_eq!(
        batt.msg,
        "Battery failsafe not set for ADOS: may fly without GNSS, so set low = Land, critical = Land"
    );
    // A confined place positioned by optical flow alone (no external nav)
    // keeps Land too.
    let mut w = world(1.0, 1.0, 0.0);
    confined(&mut w);
    assert!(item(&w, "battery_failsafe").unwrap().ok);
}

/// An EKF report with no horizontal position: the `position` item blocks.
fn no_position(w: &mut World) {
    let m = MavMessage::EKF_STATUS_REPORT(EKF_STATUS_REPORT_DATA {
        flags: EkfStatusFlags::empty(),
        ..Default::default()
    });
    w.p.on_fc(FC_SYS, FC_COMP, &m, w.now);
}

/// Airborne in LOITER (the FC flies), the battery item blocking.
fn airborne_battery_blocked() -> World {
    let mut w = world(0.0, 0.0, GPS);
    w.airborne(copter::LOITER);
    w.run(100);
    assert!(!item(&w, "battery_failsafe").unwrap().ok);
    w
}

#[test]
fn take_control_refused_while_the_battery_item_hides_a_position_block() {
    let mut w = airborne_battery_blocked();
    no_position(&mut w);
    let pos = item(&w, "position").unwrap();
    assert_eq!((pos.ok, pos.severity), (false, Severity::Block), "{pos:?}");
    let r = w.action("take_control");
    assert_eq!(err(&r), Some("not_ready:position"), "{r:?}");
    assert_eq!(w.set_modes(copter::GUIDED), 0);
}

#[test]
fn take_control_refused_on_a_blocking_battery_item() {
    let mut w = airborne_battery_blocked();
    let r = w.action("take_control");
    assert_eq!(err(&r), Some("not_ready:battery_failsafe"), "{r:?}");
    assert_eq!(w.set_modes(copter::GUIDED), 0);
}

/// Flying as the pilot in GUIDED with the battery at `pct`, on `posxy`.
fn low_battery_flight(posxy: f32, pct: f32) -> World {
    let mut w = world(if posxy == GPS { 4.0 } else { 1.0 }, 1.0, posxy);
    // No surface frames follow Take control: keep the link-loss rule out.
    w.p.settings.pilot.link_loss_s = 3600;
    w.airborne(copter::GUIDED);
    w.run(100);
    assert!(matches!(w.action("take_control"), Reply::Action(a) if a.ok));
    w.run(100);
    assert_eq!(w.p.owner(), Owner::Pilot);
    w.p.fc.battery_pct = Some(pct);
    w
}

#[test]
fn low_battery_lands_a_gnss_free_copter_and_says_so() {
    let mut w = low_battery_flight(EXTERNAL_NAV, 19.4);
    w.run(1_000);
    let s = w.p.state(w.now);
    assert_eq!(s.low_battery_action.as_deref(), Some("land"));
    assert!(s.rth_countdown_s.is_some());
    // The countdown says it will land, never "Returning".
    assert_eq!(
        s.banner.as_ref().map(|b| b.text_key.as_str()),
        Some("pilot.banner.low_battery_land_in")
    );
    // Whole percentages on the wire: every surface prints "19%", never
    // "19.0%".
    let args = &s.banner.as_ref().expect("banner").args;
    assert_eq!(args["pct"].as_i64(), Some(19), "{args:?}");
    assert!(args["need_pct"].is_i64(), "{args:?}");
    w.run(10_000);
    assert_eq!(w.set_modes(copter::RTL), 0, "never RTL without GNSS");
    assert_eq!(
        w.set_modes(copter::SMART_RTL),
        0,
        "never SmartRTL without GNSS"
    );
    assert!(w.p.landing, "the pilot lands where it is");
    let s = w.p.state(w.now);
    assert_eq!(s.low_battery_action.as_deref(), Some("land"));
    assert!(s.rth_countdown_s.is_none());
    assert_eq!(
        s.banner.as_ref().map(|b| b.text_key.as_str()),
        Some("pilot.banner.low_battery_land")
    );
    let args = &s.banner.as_ref().expect("banner").args;
    assert_eq!(args["pct"].as_i64(), Some(19), "{args:?}");
    assert!(
        s.statustext
            .iter()
            .any(|t| t.text == "Low battery: landing" && t.severity == 2),
        "{:?}",
        s.statustext
    );
}

#[test]
fn low_battery_returns_a_gnss_copter_by_smart_rtl_and_says_so() {
    let mut w = low_battery_flight(GPS, 19.0);
    w.run(1_000);
    assert_eq!(
        w.p.state(w.now).banner.map(|b| b.text_key),
        Some("pilot.banner.low_battery_rth".to_string())
    );
    w.run(10_000);
    assert_eq!(w.mode, copter::SMART_RTL);
    assert_eq!(w.set_modes(copter::RTL), 0, "never a plain RTL climb");
    let s = w.p.state(w.now);
    assert_eq!(s.low_battery_action.as_deref(), Some("rth"));
    assert_eq!(
        s.banner.as_ref().map(|b| b.text_key.as_str()),
        Some("pilot.banner.low_battery_smart_rtl")
    );
    assert!(s
        .statustext
        .iter()
        .any(|t| t.text == "Low battery: returning home" && t.severity == 2));
}

/// With no charge level on the FC (`BATT_LOW_MAH` 0) the pilot's reserve
/// acts at the wizard's 30 % floor, not at 20 %.
#[test]
fn low_battery_without_a_charge_level_acts_at_the_wizard_floor() {
    let mut w = low_battery_flight(GPS, 29.0);
    w.run(1_000);
    let s = w.p.state(w.now);
    assert_eq!(s.low_battery_action.as_deref(), Some("rth"));
    assert!(s.rth_countdown_s.is_some());
    let mut w = low_battery_flight(GPS, 31.0);
    w.run(1_000);
    assert_eq!(w.p.state(w.now).low_battery_action, None);
}

#[test]
fn low_battery_returns_a_confined_external_nav_copter_by_smart_rtl() {
    let mut w = low_battery_flight(EXTERNAL_NAV, 19.0);
    w.p.fc.params.insert("BATT_FS_LOW_ACT".into(), 4.0);
    confined(&mut w);
    w.run(1_000);
    let s = w.p.state(w.now);
    assert_eq!(s.low_battery_action.as_deref(), Some("rth"));
    assert_eq!(
        s.banner.map(|b| b.text_key),
        Some("pilot.banner.low_battery_rth".to_string())
    );
    w.run(10_000);
    assert!(!w.p.landing, "never lands in place first");
    assert_eq!(w.mode, copter::SMART_RTL);
    assert_eq!(w.set_modes(copter::RTL), 0, "never a plain RTL climb");
    let s = w.p.state(w.now);
    assert_eq!(
        s.banner.as_ref().map(|b| b.text_key.as_str()),
        Some("pilot.banner.low_battery_smart_rtl")
    );
}

#[test]
fn low_battery_still_lands_an_open_external_nav_copter() {
    let mut w = low_battery_flight(EXTERNAL_NAV, 19.0);
    w.run(11_000);
    assert_eq!(w.set_modes(copter::SMART_RTL), 0);
    assert!(w.p.landing);
}

#[test]
fn a_refused_smart_rtl_lands_and_says_so() {
    let mut w = low_battery_flight(GPS, 19.0);
    // No recorded path: the FC refuses SmartRTL and stays in GUIDED.
    w.refuse_mode = Some((
        copter::SMART_RTL,
        MavResult::MAV_RESULT_FAILED,
        Some("SmartRTL Unavailable, Trying RTL Mode"),
    ));
    w.run(10_500);
    assert!(w.set_modes(copter::SMART_RTL) >= 1, "SmartRTL asked first");
    w.run(2_500);
    assert_eq!(w.mode, copter::LAND);
    assert_eq!(w.set_modes(copter::RTL), 0, "never a plain RTL climb");
    let s = w.p.state(w.now);
    assert_eq!(s.low_battery_action.as_deref(), Some("land"));
    assert_eq!(
        s.banner.as_ref().map(|b| b.text_key.as_str()),
        Some("pilot.banner.low_battery_land_no_srtl")
    );
    assert!(s
        .statustext
        .iter()
        .any(|t| t.text == "Low battery: landing" && t.severity == 2));
}

/// A GNSS copter whose low-battery SmartRTL request the FC has not taken
/// (it ignores it), 10.5 s into the low battery.
fn smart_rtl_pending() -> World {
    let mut w = low_battery_flight(GPS, 19.0);
    w.refuse_mode = Some((copter::SMART_RTL, MavResult::MAV_RESULT_FAILED, None));
    w.run(10_500);
    assert!(w.set_modes(copter::SMART_RTL) >= 1);
    w
}

#[test]
fn an_fc_mode_change_during_the_smart_rtl_request_is_never_overridden() {
    let mut w = smart_rtl_pending();
    // The FC's own failsafe (or an RC switch) puts it in LOITER.
    w.mode = copter::LOITER;
    w.run(3_000);
    assert_eq!(w.mode, copter::LOITER);
    assert_eq!(w.set_modes(copter::LAND), 0);
}

#[test]
fn an_operator_pause_during_the_smart_rtl_request_is_never_overridden() {
    let mut w = smart_rtl_pending();
    let _ = w.action("pause");
    w.run(3_000);
    assert_ne!(w.mode, copter::LAND);
    assert_eq!(w.set_modes(copter::LAND), 0);
}

#[test]
fn a_cancelled_countdown_clears_the_action() {
    let mut w = low_battery_flight(EXTERNAL_NAV, 19.0);
    w.run(1_000);
    assert!(w.p.state(w.now).low_battery_action.is_some());
    assert!(matches!(w.action("rth_cancel"), Reply::Action(a) if a.ok));
    w.run(11_000);
    assert!(w.p.state(w.now).low_battery_action.is_none());
    assert!(!w.p.landing);
}

/// Airborne in LOITER (the FC flies) with every guarded item passing but
/// `id`, which blocks.
fn airborne_blocked_on(id: &str) -> World {
    let (low, crt) = if id == "battery_failsafe" {
        (0.0, 0.0)
    } else {
        (4.0, 1.0)
    };
    let mut w = world(low, crt, GPS);
    if id == "fc_profile" {
        w.p.fc.params.insert("FS_GCS_ENABLE".into(), 1.0);
    }
    w.airborne(copter::LOITER);
    w.run(100);
    match id {
        "position" => no_position(&mut w),
        // Firmware not identified: the next heartbeat would name it again,
        // so the action is asked before any tick.
        "firmware" => w.p.fc.firmware = None,
        _ => {}
    }
    w
}

#[test]
fn take_control_is_refused_on_every_guarded_block() {
    for id in ["firmware", "position", "fc_profile", "battery_failsafe"] {
        let mut w = airborne_blocked_on(id);
        let items = w.p.readiness(w.now);
        for other in ["firmware", "position", "fc_profile", "battery_failsafe"] {
            let blocks = items
                .iter()
                .any(|r| r.id == other && !r.ok && r.severity == Severity::Block);
            assert_eq!(blocks, other == id, "{id}: {other} {items:?}");
        }
        let owner = w.p.owner();
        let sent = w.sent.len();
        let r = w.action("take_control");
        assert_eq!(err(&r), Some(format!("not_ready:{id}").as_str()), "{r:?}");
        assert_eq!(w.p.owner(), owner, "{id}: nothing changes hands");
        assert_eq!(w.sent.len(), sent, "{id}: nothing sent to the FC");
        assert_eq!(w.mode, copter::LOITER);
    }
}

#[test]
fn a_smart_rtl_request_is_dropped_by_an_operator_motion_action() {
    for action in ["take_control", "goto", "land"] {
        let mut w = smart_rtl_pending();
        let before = w.set_modes(copter::LAND);
        let r = if action == "goto" {
            let m = MavMessage::HOME_POSITION(HOME_POSITION_DATA {
                latitude: 120_000_000,
                longitude: 770_000_000,
                ..Default::default()
            });
            w.p.on_fc(FC_SYS, FC_COMP, &m, w.now);
            w.action_args(
                "goto",
                serde_json::json!({"lat": 12.0009, "lon": 77.0, "alt_m": 6.0}),
            )
        } else {
            w.action(action)
        };
        assert!(matches!(&r, Reply::Action(a) if a.ok), "{action}: {r:?}");
        // `land` while the FC holds the refused request: the operator's own
        // LAND, once; no other action commands one.
        let own = usize::from(action == "land");
        assert_eq!(w.set_modes(copter::LAND), before + own, "{action}");
        w.run(3_000);
        assert_eq!(
            w.set_modes(copter::LAND),
            before + own,
            "{action}: no Land fallback after the operator's action"
        );
        match action {
            "land" => assert_eq!(w.mode, copter::LAND),
            "goto" => assert_eq!(w.p.owner(), Owner::Goto),
            _ => assert_eq!(w.p.owner(), Owner::Pilot),
        }
    }
}

/// A GNSS copter with a 5000 mAh pack flown by the pilot in GUIDED at `pos`
/// (NED, home at the origin), the FC reporting `pct` and `current_a` (no
/// current measured when negative) every 100 ms for `ms`.
fn energy_flight(pos: [f32; 3], pct: i8, current_a: f32, ms: u64) -> World {
    let mut w = world(4.0, 1.0, GPS);
    w.p.fc.params.insert("BATT_CAPACITY".into(), 5000.0);
    w.p.settings.pilot.link_loss_s = 3600;
    w.airborne_at(copter::GUIDED, pos);
    w.run(100);
    assert!(matches!(w.action("take_control"), Reply::Action(a) if a.ok));
    report(&mut w, pct, current_a, ms);
    w
}

fn report(w: &mut World, pct: i8, current_a: f32, ms: u64) {
    for _ in 0..ms / 100 {
        w.inbox.push(MavMessage::SYS_STATUS(SYS_STATUS_DATA {
            battery_remaining: pct,
            current_battery: if current_a < 0.0 {
                -1
            } else {
                (current_a * 100.0) as i16
            },
            ..Default::default()
        }));
        w.run(100);
    }
}

const FAR: [f32; 3] = [1000.0, 0.0, -30.0];

#[test]
fn the_energy_estimate_returns_above_the_floor_on_a_long_way_home() {
    // 1 km out at 20 A: about 250 s home, 2.5 Ah with the margin and the
    // critical reserve: half the pack, well above the 30 % floor.
    let mut w = energy_flight(FAR, 45, 20.0, 11_000);
    let s = w.p.state(w.now);
    let r = s.battery_reserve.clone().expect("a reserve airborne");
    assert_eq!(r.basis, "energy", "{r:?}");
    assert!(r.need_pct > 45, "{r:?}");
    assert!(r.return_s > 200, "{r:?}");
    assert_eq!(s.low_battery_action.as_deref(), Some("rth"));
    let b = s.banner.expect("a banner");
    assert_eq!(b.text_key, "pilot.banner.low_battery_rth_energy");
    // The banner names the need the countdown began at; the reserve counts
    // only the countdown still to run, so it reads up to a point lower.
    let named = b.args["need_pct"].as_i64().expect("need_pct");
    let now = i64::from(r.need_pct);
    assert!(
        (now..=now + 1).contains(&named),
        "banner {named}, reserve {now}"
    );
    report(&mut w, 45, 20.0, 10_000);
    assert_eq!(w.mode, copter::SMART_RTL);
    // On the ground the reserve is not published.
    w.landed = true;
    w.armed = false;
    w.run(200);
    assert!(w.p.state(w.now).battery_reserve.is_none());
}

#[test]
fn the_estimate_waits_ten_seconds_airborne() {
    let w = energy_flight(FAR, 45, 20.0, 8_000);
    let s = w.p.state(w.now);
    assert_eq!(s.battery_reserve.map(|r| r.basis).as_deref(), Some("floor"));
    assert_eq!(s.low_battery_action, None);
}

#[test]
fn a_short_way_home_needs_little_above_the_fc_low_level() {
    // 5 m out the need is the low level kept at touchdown (no BATT_LOW_MAH:
    // the 30 % floor) and a few points for the way home.
    let w = energy_flight([5.0, 0.0, -5.0], 45, 20.0, 12_000);
    let s = w.p.state(w.now);
    let r = s.battery_reserve.expect("a reserve");
    assert!(
        r.basis == "energy" && (31..=36).contains(&r.need_pct),
        "{r:?}"
    );
    assert_eq!(s.low_battery_action, None);
}

#[test]
fn no_current_measured_keeps_the_floor() {
    let w = energy_flight(FAR, 45, -1.0, 12_000);
    let s = w.p.state(w.now);
    let r = s.battery_reserve.expect("a reserve");
    assert_eq!((r.basis.as_str(), r.need_pct), ("floor", 30), "{r:?}");
    assert_eq!(s.low_battery_action, None);
    // The floor still acts.
    let mut w = w;
    report(&mut w, 29, -1.0, 1_000);
    let s = w.p.state(w.now);
    assert_eq!(s.low_battery_action.as_deref(), Some("rth"));
    assert_eq!(
        s.banner.map(|b| b.text_key).as_deref(),
        Some("pilot.banner.low_battery_rth")
    );
}

#[test]
fn the_countdown_counts_in_the_time_home() {
    let a = energy_flight(FAR, 80, 20.0, 12_000);
    let mut b = world(4.0, 1.0, GPS);
    b.p.settings.pilot.low_battery_countdown_s = 30;
    b.p.fc.params.insert("BATT_CAPACITY".into(), 5000.0);
    b.p.settings.pilot.link_loss_s = 3600;
    b.airborne_at(copter::GUIDED, FAR);
    report(&mut b, 80, 20.0, 12_000);
    let ra = a.p.state(a.now).battery_reserve.expect("a").return_s;
    let rb = b.p.state(b.now).battery_reserve.expect("b").return_s;
    assert_eq!(rb, ra + 20, "{ra} {rb}");
}

#[test]
fn a_cancelled_energy_return_rearms_five_percent_above_its_need() {
    let mut w = energy_flight(FAR, 45, 20.0, 11_000);
    let banner = w.p.state(w.now).banner.expect("a banner");
    let need = banner.args["need_pct"]
        .as_i64()
        .expect("the need it fired at");
    assert!(matches!(w.action("rth_cancel"), Reply::Action(a) if a.ok));
    report(&mut w, 45, 20.0, 12_000);
    assert_eq!(w.p.state(w.now).low_battery_action, None);
    assert_ne!(w.mode, copter::SMART_RTL, "a cancel holds");
    // The charge recovers past the need and 5 %: the next fall acts again.
    report(&mut w, (need + 6) as i8, 0.0, 200);
    report(&mut w, 45, 20.0, 1_000);
    assert_eq!(w.p.state(w.now).low_battery_action.as_deref(), Some("rth"));
}
