//! The route hold's persistence and release, and what the planar scan can
//! judge: one infeasible check never holds the mission, a pinch the vehicle
//! could reach before a confirmed hold stopped it holds at once, a hold
//! resumes the mission once the route from where it holds stays clear
//! (clearing a stall it raised meanwhile), a pinch that stays holds for
//! good, a roof a tilted scan reaches is no obstacle at flight height while
//! a wall approached tilted still brakes, and a leg flown at another height
//! is judged only once the vehicle flies at it. A launched mission and a
//! ground station's go through the same code.

use ados_protocol::pilot::{Owner, RouteState};
use serde_json::{json, Value};

use super::confined::{area, confined, map, mission, ok, post, run_seen, seen, speed_changes};
use super::confined_route::feed;
use super::harness::{wall, World};
use crate::modes::copter;

/// A ground station's AUTO in a confined place toward item 2 of the route
/// north along e = 0 at 5 m, the vehicle at (0, 0), with `occ` mapped.
fn flying(occ: &dyn Fn(f32, f32) -> bool) -> World {
    let mut w = World::new();
    confined(&mut w);
    w.airborne(copter::AUTO);
    w.run(100);
    map(&mut w, &area(occ));
    mission(&mut w, "active", 2);
    w
}

fn held(w: &World) -> Option<String> {
    w.p.state(w.now).hold_reason
}

const SET_CURRENT: ados_protocol::mavlink::ardupilotmega::MavCmd =
    ados_protocol::mavlink::ardupilotmega::MavCmd::MAV_CMD_DO_SET_MISSION_CURRENT;

/// What a resume is measured against: the AUTO commands and mission item
/// changes sent so far.
fn sent_before(w: &World) -> (usize, usize) {
    (w.set_modes(copter::AUTO), w.commands(SET_CURRENT, None))
}

/// Resumed in AUTO at the FC's own item since `before`, the hold and any
/// stall gone.
fn assert_resumed(w: &World, before: (usize, usize)) {
    assert_eq!(
        w.set_modes(copter::AUTO),
        before.0 + 1,
        "AUTO commanded again"
    );
    assert_eq!(w.mode, copter::AUTO);
    assert_eq!(w.p.owner(), Owner::MissionFc);
    let s = w.p.state(w.now);
    assert_eq!(s.hold_reason, None);
    assert!(s.mission_stall.is_none(), "{:?}", s.mission_stall);
    assert_eq!(
        w.commands(SET_CURRENT, None),
        before.1,
        "resumed at its own item"
    );
}

#[test]
fn one_infeasible_check_does_not_hold_the_mission() {
    let mut w = flying(&|_, _| false);
    run_seen(&mut w, 1500);
    let rc = w.p.state(w.now).route_check.expect("route_check");
    assert_eq!(rc.state, RouteState::Feasible, "{rc:?}");
    // One check reads a post on leg 2→3, 15 m ahead (a cell the next map
    // no longer shows).
    map(&mut w, &area(&post(15.0)));
    let t0 = w.now;
    while w.p.state(w.now).route_check.unwrap().state != RouteState::Infeasible {
        w.run(20);
        assert!(w.now < t0 + 1500, "never checked");
    }
    map(&mut w, &area(&|_, _| false));
    run_seen(&mut w, 3000);
    assert_eq!(w.set_modes(copter::BRAKE), 0, "one check never holds");
    assert_eq!(w.mode, copter::AUTO);
    assert_eq!(w.p.owner(), Owner::MissionFc);
    let s = w.p.state(w.now);
    assert_eq!(s.hold_reason, None);
    assert!(s.mission_stall.is_none());
    assert_eq!(s.route_check.unwrap().state, RouteState::Feasible);
    // The FC was slowed while the verdict waited, and given its speed back.
    assert_eq!(speed_changes(&w).first(), Some(&0.5));
}

#[test]
fn a_pinch_within_the_stopping_reach_holds_at_once() {
    // Hovering 1.4 m short of the post's point on the leg: a hold that waited
    // a second could not stop clear of it.
    let mut w = World::new();
    confined(&mut w);
    w.airborne(copter::AUTO);
    w.pos = [13.6, 0.0, -5.0];
    w.run(100);
    map(&mut w, &area(&post(15.0)));
    mission(&mut w, "active", 3);
    run_seen(&mut w, 100);
    assert_eq!(w.set_modes(copter::BRAKE), 1, "held on the first check");
    assert_eq!(held(&w).as_deref(), Some("route-infeasible"));
    // 6 m short of it at 3 m/s: the reach grows with the speed.
    let mut w = World::new();
    confined(&mut w);
    w.airborne(copter::AUTO);
    w.pos = [9.0, 0.0, -5.0];
    w.vel = [3.0, 0.0, 0.0];
    w.run(100);
    map(&mut w, &area(&post(15.0)));
    mission(&mut w, "active", 2);
    run_seen(&mut w, 100);
    assert_eq!(w.set_modes(copter::BRAKE), 1, "held on the first check");
}

#[test]
fn a_route_hold_resumes_the_mission_once_the_route_stays_clear() {
    let mut w = flying(&post(15.0));
    run_seen(&mut w, 1100);
    assert_eq!(w.set_modes(copter::BRAKE), 1, "held once confirmed");
    run_seen(&mut w, 1600);
    assert_eq!(w.mode, copter::GUIDED);
    assert_eq!(w.p.owner(), Owner::Pilot);
    let s = w.p.state(w.now);
    assert_eq!(s.hold_reason.as_deref(), Some("route-infeasible"));
    assert!(s.mission_stall.and_then(|m| m.skip_in_s).is_some());
    // The next maps no longer show the post.
    let before = sent_before(&w);
    map(&mut w, &area(&|_, _| false));
    run_seen(&mut w, 2000);
    assert_eq!(w.mode, copter::GUIDED, "not before the dwell");
    assert_eq!(w.set_modes(copter::AUTO), before.0);
    run_seen(&mut w, 2500);
    assert_resumed(&w, before);
    run_seen(&mut w, 2000);
    assert_eq!(w.set_modes(copter::BRAKE), 1, "flies on");
}

#[test]
fn a_stalled_route_hold_resumes_and_clears_the_stall() {
    let mut w = flying(&post(15.0));
    run_seen(&mut w, 2700);
    assert_eq!(held(&w).as_deref(), Some("route-infeasible"));
    // The FC moved on to item 3 as the hold began: the question about item 2
    // ends, and with no progress the stall rule raises its own.
    mission(&mut w, "active", 3);
    run_seen(&mut w, 16_000);
    let s = w.p.state(w.now);
    assert_eq!(s.hold_reason.as_deref(), Some("mission-stalled"));
    assert!(s.mission_stall.and_then(|m| m.skip_in_s).is_some());
    let before = sent_before(&w);
    assert_eq!(before.0, 0);
    map(&mut w, &area(&|_, _| false));
    run_seen(&mut w, 4500);
    assert_resumed(&w, before);
    let gaps = w.p.state(w.now).mission.expect("mission").gaps;
    assert!(gaps.is_empty(), "nothing skipped: {gaps:?}");
}

#[test]
fn a_route_hold_resumes_only_on_fresh_obstacle_data() {
    let mut w = flying(&post(15.0));
    run_seen(&mut w, 2700);
    assert_eq!(held(&w).as_deref(), Some("route-infeasible"));
    // The scans stop: the route reads clear on the map alone, yet nothing
    // would brake in AUTO.
    let before = sent_before(&w);
    w.lidar = false;
    map(&mut w, &area(&|_, _| false));
    run_seen(&mut w, 6000);
    assert_eq!(
        w.set_modes(copter::AUTO),
        before.0,
        "no AUTO on stale scans"
    );
    assert_ne!(w.mode, copter::AUTO);
    // Scans again: the stale checks never counted, so the full dwell runs
    // from the first fresh one.
    w.lidar = true;
    run_seen(&mut w, 1500);
    assert_eq!(w.set_modes(copter::AUTO), before.0, "not before the dwell");
    run_seen(&mut w, 3000);
    assert_resumed(&w, before);
}

#[test]
fn a_bore_that_narrows_toward_its_pinch_holds_from_where_it_turns_tight() {
    // Walls either side of e = 0 close in from 0.60 m at n 5 to 0.45 m at
    // n 15: the pinch is at n 10 on leg 1→2, 4.4 m off, but the leg turns
    // too tight about n 6.7, 1.1 m ahead of the hovering vehicle.
    let walls = |n: f32, e: f32| {
        let half = 0.60 - 0.015 * (n - 5.0);
        (5.0..15.0).contains(&n) && (half..half + 0.06).contains(&e.abs())
    };
    let mut w = World::new();
    confined(&mut w);
    w.airborne(copter::AUTO);
    w.pos = [5.6, 0.0, -5.0];
    w.run(100);
    map(&mut w, &area(&walls));
    mission(&mut w, "active", 2);
    run_seen(&mut w, 100);
    let rc = w.p.state(w.now).route_check.expect("route_check");
    assert_eq!(rc.state, RouteState::Infeasible, "{rc:?}");
    assert!(rc.pinch.as_ref().expect("pinch").ned[0] > 9.0, "{rc:?}");
    assert_eq!(w.set_modes(copter::BRAKE), 1, "held on the first check");
    assert_eq!(held(&w).as_deref(), Some("route-infeasible"));
}

#[test]
fn a_persistent_pinch_holds_and_never_flies_into_it() {
    let mut w = flying(&post(15.0));
    run_seen(&mut w, 1100);
    assert_eq!(w.set_modes(copter::BRAKE), 1);
    // Up to the automatic skip's deadline: held, never AUTO.
    for _ in 0..40 {
        run_seen(&mut w, 1000);
        assert_eq!(held(&w).as_deref(), Some("route-infeasible"));
        assert_ne!(w.mode, copter::AUTO);
    }
    assert_eq!(w.set_modes(copter::AUTO), 0);
    assert_eq!(w.set_modes(copter::BRAKE), 1);
    // Past the deadline item 2 is skipped, but the leg on to item 3 still
    // passes the post: held again, never AUTO.
    run_seen(&mut w, 6000);
    let s = w.p.state(w.now);
    let gaps = s.mission.expect("mission").gaps;
    assert!(gaps.iter().any(|g| g.seq == 2), "{gaps:?}");
    assert_eq!(s.hold_reason.as_deref(), Some("route-infeasible"));
    assert_eq!(w.set_modes(copter::AUTO), 0);
    assert_ne!(w.mode, copter::AUTO);
}

#[test]
fn a_roof_a_nose_up_scan_reaches_is_no_obstacle_on_the_legs_ahead() {
    let mut w = flying(&|_, _| false);
    // Nose up 4° (slowing): the bin dead ahead meets the roof 7 m out, 0.49 m
    // over the scan plane, right above leg 1→2.
    let mut ranges = wall(270, 8.0);
    ranges[0] = 7.0;
    w.scan_ranges = ranges;
    w.pitch = 4f32.to_radians();
    run_seen(&mut w, 1500);
    let rc = w.p.state(w.now).route_check.expect("route_check");
    assert_eq!(rc.state, RouteState::Feasible, "{rc:?}");
    assert_eq!(w.set_modes(copter::BRAKE), 0);
    // The brake still reads it: an obstacle over the slice, not a pinch.
    let hits = w.p.obstacles.hits(w.now);
    let roof: Vec<_> = hits.iter().filter(|h| h.range_m < 7.5).collect();
    assert!(
        roof.len() == 1 && roof[0].overhead && (roof[0].ne[0] - 6.98).abs() < 0.05,
        "{hits:?}"
    );
    // Level, the same return is a wall at flight height on the leg.
    w.pitch = 0.0;
    run_seen(&mut w, 1100);
    let rc = w.p.state(w.now).route_check.expect("route_check");
    assert_eq!(rc.state, RouteState::Infeasible, "{rc:?}");
    let pinch = rc.pinch.expect("pinch");
    assert!((pinch.ned[0] - 7.0).abs() < 0.05, "{pinch:?}");
}

#[test]
fn the_brake_still_stops_for_a_wall_approached_nose_up() {
    let mut w = flying(&|_, _| false);
    run_seen(&mut w, 500);
    // North at 2 m/s, nose up 5°: a wall 2 m ahead (bins ±10) lifted 0.17 m
    // over the scan plane, outside the slice, and in no map.
    let mut ranges = wall(270, 8.0);
    for d in 0..=20 {
        ranges[(350 + d) % 360] = 2.0;
    }
    w.scan_ranges = ranges;
    w.pitch = 5f32.to_radians();
    w.vel = [2.0, 0.0, 0.0];
    w.run(200);
    let hits = w.p.obstacles.hits(w.now);
    let wall_hits: Vec<_> = hits.iter().filter(|h| h.range_m < 7.5).collect();
    assert!(
        wall_hits.len() == 21 && wall_hits.iter().all(|h| h.overhead),
        "{hits:?}"
    );
    assert_eq!(w.set_modes(copter::BRAKE), 1, "braked short of the wall");
    assert_eq!(held(&w).as_deref(), Some("obstacle-close"));
}

/// An item `(seq, n, e)` at local down `d`.
fn item_at(s: u32, n: f32, e: f32, d: f32) -> Value {
    json!({"seq": s, "cmd": 16, "frame": 6, "lat_e7": 1, "lon_e7": 1, "alt_m": -d,
           "ned": [n, e, d]})
}

/// The bore 0.5 m either side of e = 0 at n 18–22, as the scan sees it
/// from the height it flies.
fn narrow(n: f32, e: f32) -> bool {
    (18.0..22.0).contains(&n) && (0.5..0.6).contains(&e.abs())
}

#[test]
fn a_far_leg_tight_from_the_wrong_height_is_judged_at_its_own() {
    // Items 1–2 at d −5, then down to d −4 from item 3 on. The bore's roof
    // narrows it at d −5 over n 18–22, where leg 3→4 flies at d −4 and is
    // wide.
    let items = || {
        vec![
            item_at(1, 2.0, 0.0, -5.0),
            item_at(2, 10.0, 0.0, -5.0),
            item_at(3, 14.0, 0.0, -4.0),
            item_at(4, 25.0, 0.0, -4.0),
        ]
    };
    let mut w = World::new();
    confined(&mut w);
    w.airborne(copter::AUTO);
    w.run(100);
    map(&mut w, &area(&narrow));
    feed(&mut w, "active", 2, items(), Some([10.0, 0.0]), json!({}));
    run_seen(&mut w, 3000);
    assert_eq!(
        w.set_modes(copter::BRAKE),
        0,
        "no hold from the wrong height"
    );
    let rc = w.p.state(w.now).route_check.expect("route_check");
    assert_eq!(rc.state, RouteState::Unverified, "{rc:?}");
    assert_eq!(rc.pinch, None);
    assert!(rc.unverified_legs.contains(&4), "{rc:?}");
    // Down at item 3: the scan maps the bore at d −4, wide, and leg 3→4 is
    // judged clear.
    w.pos = [12.0, 0.0, -4.5];
    feed(&mut w, "active", 3, items(), Some([14.0, 0.0]), json!({}));
    run_seen(&mut w, 500);
    w.pos = [14.0, 0.0, -4.0];
    map(&mut w, &area(&|_, _| false));
    feed(&mut w, "active", 4, items(), Some([25.0, 0.0]), json!({}));
    run_seen(&mut w, 2000);
    assert_eq!(w.set_modes(copter::BRAKE), 0);
    let rc = w.p.state(w.now).route_check.expect("route_check");
    assert_eq!(rc.state, RouteState::Feasible, "{rc:?}");
    // Narrow at its own height too: judged there, and held once confirmed.
    map(&mut w, &area(&narrow));
    run_seen(&mut w, 2200);
    assert_eq!(w.set_modes(copter::BRAKE), 1);
    assert_eq!(held(&w).as_deref(), Some("route-infeasible"));
}

#[test]
fn an_unverified_leg_within_reach_slows_the_fc() {
    // Leg 1→2 is level and clear; leg 2→3 climbs 2 m, off the scan's slice
    // past its first 0.65 m.
    let mut w = World::new();
    confined(&mut w);
    w.p.fc.params.insert("WP_SPD".into(), 5.0);
    w.airborne(copter::AUTO);
    w.pos = [4.0, 0.0, -5.0];
    w.run(100);
    map(&mut w, &area(&|_, _| false));
    let items = vec![
        item_at(1, 2.0, 0.0, -5.0),
        item_at(2, 10.0, 0.0, -5.0),
        item_at(3, 20.0, 0.0, -3.0),
        item_at(4, 25.0, 0.0, -3.0),
    ];
    feed(&mut w, "active", 2, items, Some([10.0, 0.0]), json!({}));
    run_seen(&mut w, 600);
    assert!(speed_changes(&w).is_empty(), "6 m short: out of reach");
    w.pos = [9.0, 0.0, -5.0];
    run_seen(&mut w, 600);
    // An 11° climb judged 0.65 m ahead: the crawl ([`sloped_leg`]).
    assert_eq!(speed_changes(&w), vec![0.2]);
    let cap = w.p.state(w.now).speed_cap.expect("cap");
    assert_eq!(cap.reason, "confined-near");
}

#[test]
fn a_launched_mission_holds_and_resumes_through_the_same_code() {
    let mut w = World::new();
    confined(&mut w);
    map(&mut w, &area(&post(15.0)));
    mission(&mut w, "loaded", 0);
    w.run(200);
    // From the floor the post at flight height is not judged: the launch
    // goes ahead.
    let rc = w.p.state(w.now).route_check.expect("route_check");
    assert_eq!((rc.state, rc.pinch), (RouteState::Unverified, None));
    assert!(ok(&w.action_args(
        "launch",
        json!({"after": "mission", "alt_m": 5.0})
    )));
    let t0 = w.now;
    while w.set_modes(copter::AUTO) == 0 {
        run_seen(&mut w, 100);
        assert!(w.now < t0 + 10_000, "never in AUTO");
    }
    // At flight height the post is judged; held once confirmed.
    mission(&mut w, "active", 2);
    let t1 = w.now;
    while w.set_modes(copter::BRAKE) == 0 {
        run_seen(&mut w, 100);
        assert!(w.now < t1 + 2500, "never held");
    }
    run_seen(&mut w, 1600);
    assert_eq!(held(&w).as_deref(), Some("route-infeasible"));
    let before = sent_before(&w);
    map(&mut w, &area(&|_, _| false));
    seen(&mut w);
    run_seen(&mut w, 4500);
    assert_resumed(&w, before);
}
