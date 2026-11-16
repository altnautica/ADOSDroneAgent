//! The confined route as the FC flies it: a skip or a resume never flies the
//! leg from where the vehicle stands without checking it, the clearance is
//! conservative against the map's cells, items the check cannot place are
//! named, σ is never forgotten into a smaller clearance, and the cap follows
//! the FC's own speed changes and returns.

use ados_protocol::pilot::{Owner, RouteState};
use serde_json::{json, Value};

use super::confined::{
    area, confined, err, map, map_payload, ok, on_pad, post, run_seen, seen, speed_changes,
    REQUIRED_M,
};
use super::harness::World;
use crate::core::control::{legs_of, Origin, RouteItem};
use crate::modes::copter;

/// A navigation waypoint `(seq, n, e)` in the local frame at 5 m.
pub(super) type Wp = (u32, f32, f32);

/// The route north along e = 0, as in the confined tests.
pub(super) const LINE: [Wp; 4] = [
    (1, 2.0, 0.0),
    (2, 10.0, 0.0),
    (3, 20.0, 0.0),
    (4, 25.0, 0.0),
];

pub(super) fn item(s: u32, n: f32, e: f32) -> Value {
    json!({"seq": s, "cmd": 16, "frame": 6, "lat_e7": 1, "lon_e7": 1, "alt_m": 5.0,
           "ned": [n, e, -5.0]})
}

/// The router's snapshot: `items`, the FC at `seq` in `state`, its target
/// at `target` (`None`: the router cannot place it), and `extra` mission
/// fields.
pub(super) fn feed(
    w: &mut World,
    state: &str,
    seq: u32,
    items: Vec<Value>,
    target: Option<[f32; 2]>,
    extra: Value,
) {
    let mut m = json!({
        "state": state, "seq": seq, "total": 5, "item_reached": seq.checked_sub(1),
        "id": null, "source": "mission_state", "age_ms": 100, "landing_seq": false,
        "rev": 1, "uploads_seen": 0, "items": items,
        "target": {"seq": seq, "cmd": 16, "ned": target.map(|t| [t[0], t[1], -5.0])}});
    if let (Some(o), Some(x)) = (m.as_object_mut(), extra.as_object()) {
        o.extend(x.clone());
    }
    let snapshot = json!({"armed": w.armed, "mission": m});
    let m = crate::router_state::mission_of(snapshot);
    w.p.on_feed(crate::router_state::MISSION_TOPIC, &m, w.now);
    w.collect();
}

pub(super) fn route(w: &mut World, state: &str, seq: u32, wps: &[Wp]) {
    let items = wps.iter().map(|&(s, n, e)| item(s, n, e)).collect();
    let target = wps.iter().find(|w| w.0 == seq).map(|w| [w.1, w.2]);
    feed(w, state, seq, items, target, json!({}));
}

/// A ground station's AUTO in a confined place at item 2 of `wps`, the
/// vehicle at n 9 on the way, with `occ` mapped: 1.1 s later, past the
/// confirmation an infeasible verdict needs to hold.
pub(super) fn flying_to_2(wps: &[Wp], occ: &dyn Fn(f32, f32) -> bool) -> World {
    let mut w = World::new();
    confined(&mut w);
    w.airborne(copter::AUTO);
    w.pos = [9.0, 0.0, -5.0];
    // The FC reports the position before the first pose places the map.
    w.run(100);
    map(&mut w, &area(occ));
    route(&mut w, "active", 2, wps);
    run_seen(&mut w, 1100);
    w
}

/// Held before the infeasible leg 2→3 with the skip counting down.
fn assert_held_on_2_to_3(w: &World) {
    assert_eq!(w.set_modes(copter::BRAKE), 1, "held once confirmed");
    let s = w.p.state(w.now);
    assert_eq!(s.hold_reason.as_deref(), Some("route-infeasible"));
    let in_s = s
        .mission_stall
        .and_then(|m| m.skip_in_s)
        .expect("countdown");
    assert!((43.0..=45.0).contains(&in_s), "{in_s}");
    assert_eq!(s.banner.expect("banner").args["leg"], "2→3");
}

/// After skipping item 2: still held as `pilot`, never AUTO, on the leg the
/// FC would fly from here to item 3 past the post, with the gap recorded.
fn assert_held_on_the_bridge(w: &World, gap_reason: &str) {
    assert_ne!(w.mode, copter::AUTO);
    assert_eq!(w.set_modes(copter::AUTO), 0, "AUTO never commanded");
    assert_eq!(w.p.owner(), Owner::Pilot);
    let s = w.p.state(w.now);
    assert_eq!(s.hold_reason.as_deref(), Some("route-infeasible"));
    let rc = s.route_check.expect("route_check");
    assert_eq!(rc.state, RouteState::Infeasible);
    let pinch = rc.pinch.expect("pinch");
    assert_eq!((pinch.from_seq, pinch.to_seq), (1, 3), "{pinch:?}");
    assert!(
        (pinch.ned[0] - 15.0).abs() < 0.1 && pinch.ned[1].abs() < 1e-3,
        "{pinch:?}"
    );
    let b = s.banner.expect("banner");
    assert_eq!(b.text_key, "pilot.banner.route_infeasible");
    assert_eq!(b.args["leg"], "1→3");
    let gaps = s.mission.expect("mission").gaps;
    assert!(
        gaps.iter().any(|g| g.seq == 2 && g.reason == gap_reason),
        "{gaps:?}"
    );
}

#[test]
fn the_automatic_skip_holds_before_the_bridging_leg_it_would_fly() {
    let mut w = flying_to_2(&LINE, &post(15.0));
    assert_held_on_2_to_3(&w);
    let t0 = w.now;
    while w.now < t0 + 45_500 {
        run_seen(&mut w, 100);
    }
    assert_held_on_the_bridge(&w, "stall_timeout");
    // The FC confirms item 3: the hold stays, counting down once more.
    route(&mut w, "active", 3, &LINE);
    run_seen(&mut w, 1000);
    assert_held_on_the_bridge(&w, "stall_timeout");
    let stall = w.p.state(w.now).mission_stall.expect("stall");
    assert_eq!(stall.seq, Some(3));
    assert!(stall.skip_in_s.is_some(), "one automatic skip of two");
}

#[test]
fn the_operator_skip_holds_before_the_bridging_leg_it_would_fly() {
    let mut w = flying_to_2(&LINE, &post(15.0));
    assert_held_on_2_to_3(&w);
    assert!(ok(&w.action("mission_skip")));
    assert_held_on_the_bridge(&w, "stall_operator");
    route(&mut w, "active", 3, &LINE);
    run_seen(&mut w, 1000);
    assert_held_on_the_bridge(&w, "stall_operator");
}

#[test]
fn a_skip_over_a_clear_bridge_carries_on_in_auto() {
    // Item 2 sits 2 m east: the post 0.5 m off the leg 2→3 is 1.5 m off the
    // line from the vehicle to item 3.
    let wps: [Wp; 4] = [
        (1, 2.0, 0.0),
        (2, 10.0, 2.0),
        (3, 20.0, 0.0),
        (4, 25.0, 0.0),
    ];
    let mut w = flying_to_2(&wps, &|n, e| {
        (n - 15.0).abs() < 0.1 && (1.5..1.7).contains(&e)
    });
    assert_held_on_2_to_3(&w);
    assert!(ok(&w.action("mission_skip")));
    assert_eq!(w.mode, copter::AUTO);
    assert_eq!(w.p.owner(), Owner::MissionFc);
    route(&mut w, "active", 3, &wps);
    run_seen(&mut w, 1500);
    assert_eq!(w.set_modes(copter::BRAKE), 1, "no second hold");
    assert_eq!(w.p.owner(), Owner::MissionFc);
    let rc = w.p.state(w.now).route_check.unwrap();
    assert_eq!(rc.state, RouteState::Feasible, "{rc:?}");
}

#[test]
fn the_countdown_acts_on_an_item_the_router_cannot_place() {
    // The router has no position for the current item's target: no stall
    // distance. The route hold's countdown still ends in the skip it shows.
    let mut w = World::new();
    confined(&mut w);
    w.airborne(copter::AUTO);
    w.pos = [9.0, 0.0, -5.0];
    w.run(100);
    map(&mut w, &area(&post(15.0)));
    let items = LINE.iter().map(|&(s, n, e)| item(s, n, e)).collect();
    feed(&mut w, "active", 2, items, None, json!({}));
    run_seen(&mut w, 1100);
    assert_held_on_2_to_3(&w);
    let t0 = w.now;
    while w.now < t0 + 45_500 {
        run_seen(&mut w, 100);
    }
    let gaps = w.p.state(w.now).mission.expect("mission").gaps;
    assert!(
        gaps.iter()
            .any(|g| g.seq == 2 && g.reason == "stall_timeout"),
        "{gaps:?}"
    );
}

/// The x500's required clearance less 3 mm, as a wall's true edge distance.
#[test]
fn a_pinch_a_few_millimetres_under_the_clearance_never_reads_feasible() {
    let mut w = World::new();
    confined(&mut w);
    on_pad(&mut w);
    // A wall whose map cells span e 0.55…0.60, the map placed 0.024 m east:
    // its edge is 0.574 m from the route along e = 0, 3 mm under 0.577 m.
    // Its nearest cell centre is 0.599 m off.
    let wall = map_payload(-6.4, 32.0, -9.6, 9.6, &|n, e| {
        (12.0..18.0).contains(&n) && (0.55..0.6).contains(&e)
    });
    w.p.on_feed("plugin.world-engine.occupancy", &wall, w.now);
    let pose = json!({"pose": {"t": [w.pos[0], w.pos[1] - 0.024, w.pos[2]]}, "sigma_xy_m": 0.02});
    w.p.on_feed("plugin.world-engine.pose", &pose, w.now);
    route(&mut w, "loaded", 0, &LINE);
    w.run(200);
    let r = w.action_args("launch", json!({"after": "mission"}));
    assert_eq!(err(&r).as_deref(), Some("route_infeasible"));
    let rc = w.p.state(w.now).route_check.unwrap();
    assert_eq!(rc.state, RouteState::Infeasible);
    let d = rc.pinch.unwrap().clearance_m;
    // Never above the wall's true edge distance (so under the required).
    assert!(d < 0.574, "{d}");
}

#[test]
fn items_the_check_cannot_place_are_named_and_refuse_the_launch() {
    let mut w = World::new();
    confined(&mut w);
    map(&mut w, &area(&|_, _| false));
    // Item 3 has no position (a terrain frame, say); item 4 is "here".
    let items = vec![
        item(1, 2.0, 0.0),
        item(2, 10.0, 0.0),
        json!({"seq": 3, "cmd": 16, "frame": 10, "lat_e7": 1, "lon_e7": 1, "alt_m": 5.0, "ned": null}),
        item(4, 25.0, 0.0),
        json!({"seq": 5, "cmd": 21, "frame": 3, "lat_e7": 0, "lon_e7": 0, "alt_m": 0.0, "ned": null}),
    ];
    feed(&mut w, "loaded", 0, items, None, json!({}));
    w.run(200);
    let rc = w.p.state(w.now).route_check.expect("route_check");
    assert_eq!(rc.state, RouteState::Unverified);
    assert_eq!(rc.unchecked, vec![3], "the land here is placed");
    let r = w.action_args("launch", json!({"after": "mission"}));
    assert_eq!(err(&r).as_deref(), Some("route_unchecked"));
    assert!(!w.armed);
    // A mission longer than the feed carries is not checked to its end.
    feed(
        &mut w,
        "loaded",
        0,
        LINE.iter().map(|&(s, n, e)| item(s, n, e)).collect(),
        None,
        json!({"items_truncated": true}),
    );
    w.run(200);
    let rc = w.p.state(w.now).route_check.unwrap();
    assert_eq!((rc.state, rc.truncated), (RouteState::Unverified, true));
    let r = w.action_args("launch", json!({"after": "mission"}));
    assert_eq!(err(&r).as_deref(), Some("route_unchecked"));
    // No item placed at all still reports, never goes silent.
    let unplaced = (1..=3)
        .map(|s| json!({"seq": s, "cmd": 16, "frame": 6, "lat_e7": 1, "lon_e7": 1, "alt_m": 5.0, "ned": null}))
        .collect();
    feed(&mut w, "loaded", 0, unplaced, None, json!({}));
    w.run(200);
    let rc = w.p.state(w.now).route_check.expect("reported");
    assert_eq!(
        (rc.state, rc.unchecked),
        (RouteState::Unverified, vec![1, 2, 3])
    );
}

#[test]
fn a_ground_station_mission_toward_an_unplaced_item_is_capped() {
    let mut w = World::new();
    confined(&mut w);
    w.p.fc.params.insert("WP_SPD".into(), 5.0);
    w.airborne(copter::AUTO);
    map(&mut w, &area(&|_, _| false));
    let mut items: Vec<Value> = LINE.iter().map(|&(s, n, e)| item(s, n, e)).collect();
    items[1]["ned"] = Value::Null;
    feed(&mut w, "active", 2, items, None, json!({}));
    run_seen(&mut w, 600);
    assert_eq!(speed_changes(&w), vec![0.5]);
    let s = w.p.state(w.now);
    assert_eq!(s.route_check.unwrap().unchecked, vec![2]);
    assert_eq!(s.speed_cap.unwrap().reason, "confined-near");
}

#[test]
fn a_stale_pose_keeps_its_sigma_and_never_lowers_the_clearance() {
    let mut w = World::new();
    confined(&mut w);
    on_pad(&mut w);
    map(&mut w, &area(&|_, _| false));
    // σ 0.1 m: the 3σ term (0.3 m) is above its 0.1 m floor.
    let pose = json!({"pose": {"t": [w.pos[0], w.pos[1], w.pos[2]]}, "sigma_xy_m": 0.1});
    w.p.on_feed("plugin.world-engine.pose", &pose, w.now);
    route(&mut w, "loaded", 0, &LINE);
    w.run(200);
    let rc = w.p.state(w.now).route_check.unwrap();
    assert_eq!(
        (rc.state, rc.required_m, rc.sigma_m),
        (RouteState::Feasible, 0.777, Some(0.1))
    );
    // The pose stops: its σ holds for 5 s.
    w.run(3000);
    let rc = w.p.state(w.now).route_check.unwrap();
    assert_eq!(
        (rc.state, rc.required_m, rc.sigma_m),
        (RouteState::Feasible, 0.777, Some(0.1))
    );
    // Aged out: unverified, still 3σ from the last σ, never margin + 0.1.
    w.run(3000);
    let rc = w.p.state(w.now).route_check.unwrap();
    assert_eq!(
        (rc.state, rc.required_m, rc.sigma_m),
        (RouteState::Unverified, 0.777, None)
    );
    // A fresh small σ lowers it again.
    seen(&mut w);
    w.run(1100);
    assert_eq!(w.p.state(w.now).route_check.unwrap().required_m, REQUIRED_M);
}

#[test]
fn the_cap_survives_the_mission_speed_items_and_restores_the_mission_speed() {
    let mut w = World::new();
    confined(&mut w);
    w.p.fc.params.insert("WP_SPD".into(), 5.0);
    w.airborne(copter::AUTO);
    map(&mut w, &area(&|n, e| e >= 1.6 && n < 6.0));
    let items = || {
        LINE.iter()
            .map(|&(s, n, e)| item(s, n, e))
            .collect::<Vec<_>>()
    };
    let speeds = json!({"speeds": [{"seq": 1, "mps": 2.0}]});
    feed(
        &mut w,
        "active",
        2,
        items(),
        Some([10.0, 0.0]),
        speeds.clone(),
    );
    run_seen(&mut w, 600);
    assert_eq!(speed_changes(&w), vec![0.5]);
    // The FC moves on: a DO_CHANGE_SPEED on the way would undo the cap, so
    // it is sent again.
    feed(
        &mut w,
        "active",
        3,
        items(),
        Some([20.0, 0.0]),
        speeds.clone(),
    );
    run_seen(&mut w, 300);
    assert_eq!(speed_changes(&w), vec![0.5, 0.5]);
    // Clear of the wall: the mission's own 2 m/s, not the 5 m/s default.
    w.pos = [12.0, 0.0, -5.0];
    run_seen(&mut w, 600);
    assert_eq!(speed_changes(&w), vec![0.5, 0.5, 2.0]);
}

#[test]
fn a_return_near_a_wall_is_capped_and_restored() {
    let mut w = World::new();
    confined(&mut w);
    w.p.fc.params.insert("WP_SPD".into(), 5.0);
    w.airborne(copter::GUIDED);
    map(&mut w, &area(&|n, e| e >= 1.6 && n < 6.0));
    assert!(ok(&w.action("rth")));
    assert_eq!(w.mode, copter::SMART_RTL);
    assert_eq!(w.p.owner(), Owner::FcMode);
    run_seen(&mut w, 600);
    assert_eq!(
        speed_changes(&w),
        vec![0.5],
        "SmartRTL slowed near the wall"
    );
    w.pos = [12.0, 0.0, -5.0];
    run_seen(&mut w, 600);
    assert_eq!(speed_changes(&w), vec![0.5, 5.0]);
}

#[test]
fn the_legs_follow_the_fc_through_skips_unplaced_items_and_an_origin() {
    let it = |seq, ned: Option<[f32; 3]>, here| RouteItem {
        seq,
        cmd: 16,
        ned,
        here,
        up_m: None,
    };
    let p = |n: f32| Some([n, 0.0, -5.0]);
    let items = [
        it(1, p(2.0), false),
        it(2, p(10.0), false),
        it(3, None, false),
        it(4, p(25.0), false),
        it(5, None, true),
    ];
    let none = |_: u32| false;
    let l = legs_of(&items, [0.0, 0.0], &none, None, None);
    let pairs: Vec<(u32, u32)> = l.legs.iter().map(|l| (l.0, l.2)).collect();
    assert_eq!(
        pairs,
        vec![(1, 2), (4, 5)],
        "no leg through the unplaced item"
    );
    assert_eq!(l.unchecked, vec![3]);
    // Flying to 4: the leg into it from the unplaced 3 is not checked.
    let l = legs_of(&items, [0.0, 0.0], &none, Some(4), None);
    assert_eq!((l.legs.len(), l.unchecked.clone()), (1, vec![3]));
    // Skipped 2, flown from the vehicle to 4.
    let o = Origin {
        from_seq: 1,
        to_seq: 4,
        ned: [9.0, 0.0, -5.0],
    };
    let skipped = |s: u32| s == 2 || s == 3;
    let l = legs_of(&items, [0.0, 0.0], &skipped, Some(2), Some(&o));
    assert_eq!(l.legs[0], (1, [9.0, 0.0, -5.0], 4, [25.0, 0.0, -5.0]));
    assert!(l.unchecked.is_empty());
    // An origin set to an unplaced item: the leg on is not checked.
    let o = Origin { to_seq: 3, ..o };
    let l = legs_of(&items, [0.0, 0.0], &|s| s == 2, Some(2), Some(&o));
    assert_eq!(l.unchecked, vec![3]);
    assert_eq!(l.legs.iter().map(|l| l.2).collect::<Vec<_>>(), vec![5]);
}
