//! The confined route across flights and feeds: a mission the router has
//! not mirrored (or mirrored stale) is never launched unchecked, a new
//! flight checks the legs an earlier one skipped, the leg from the vehicle
//! is checked however the FC entered AUTO, and no AUTO is commanded without
//! the position that leg starts from.

use ados_protocol::pilot::RouteState;
use serde_json::json;

use super::confined::{area, confined, err, map, ok, post, run_seen};
use super::confined_route::{feed, flying_to_2, item, route, Wp, LINE};
use super::harness::World;
use crate::modes::copter;

#[test]
fn a_mission_the_feed_may_not_hold_refuses_the_launch() {
    let mut w = World::new();
    confined(&mut w);
    map(&mut w, &area(&|_, _| false));
    // The FC holds a mission the router has not mirrored yet.
    feed(&mut w, "loaded", 0, vec![], None, json!({}));
    run_seen(&mut w, 200);
    let rc = w.p.state(w.now).route_check.expect("route_check");
    assert_eq!(rc.state, RouteState::Unverified);
    let r = w.action_args("launch", json!({"after": "mission"}));
    assert_eq!(err(&r).as_deref(), Some("route_unchecked"));
    // Mirrored, but the router says the mirror may not hold the FC's plan.
    let items = || LINE.iter().map(|&(s, n, e)| item(s, n, e)).collect();
    feed(
        &mut w,
        "loaded",
        0,
        items(),
        None,
        json!({"route_stale": true}),
    );
    run_seen(&mut w, 200);
    let rc = w.p.state(w.now).route_check.expect("route_check");
    assert_eq!(rc.state, RouteState::Unverified);
    let r = w.action_args("launch", json!({"after": "mission"}));
    assert_eq!(err(&r).as_deref(), Some("route_unchecked"));
    assert!(!w.armed);
    // In step with the FC: the same route reads feasible.
    feed(&mut w, "loaded", 0, items(), None, json!({}));
    run_seen(&mut w, 200);
    let rc = w.p.state(w.now).route_check.expect("route_check");
    assert_eq!(rc.state, RouteState::Feasible, "{rc:?}");
}

/// Item 2 sits 2 m east: a post 0.5 m off the leg 2→3 is 1.5 m off the
/// line 1→3.
const DOGLEG: [Wp; 4] = [
    (1, 2.0, 0.0),
    (2, 10.0, 2.0),
    (3, 20.0, 0.0),
    (4, 25.0, 0.0),
];

fn dogleg_post(n: f32, e: f32) -> bool {
    (n - 15.0).abs() < 0.1 && (1.5..1.7).contains(&e)
}

#[test]
fn a_new_flight_checks_the_legs_an_earlier_flight_skipped() {
    let mut w = flying_to_2(&DOGLEG, &dogleg_post);
    assert!(ok(&w.action("mission_skip")));
    assert_eq!(w.mode, copter::AUTO, "the bridge 1→3 is clear");
    // Landed and disarmed; the same mission is launched again.
    w.armed = false;
    w.landed = true;
    w.mode = copter::LOITER;
    w.run(500);
    route(&mut w, "loaded", 0, &DOGLEG);
    run_seen(&mut w, 200);
    let r = w.action_args("launch", json!({"after": "mission"}));
    assert_eq!(err(&r).as_deref(), Some("route_infeasible"));
    let s = w.p.state(w.now);
    let pinch = s.route_check.expect("route_check").pinch.expect("pinch");
    assert_eq!((pinch.from_seq, pinch.to_seq), (2, 3));
    assert!(s.mission.is_none_or(|m| m.gaps.is_empty()));
}

#[test]
fn a_ground_station_jump_is_checked_from_where_the_vehicle_is() {
    // The FC jumps from the way to item 2 straight to item 4: it flies
    // (9, 0) → (25, 0) past the post at n 15, though the legs between the
    // items after it are clear.
    let mut w = World::new();
    confined(&mut w);
    w.airborne(copter::AUTO);
    w.pos = [9.0, 0.0, -5.0];
    w.run(100);
    map(&mut w, &area(&post(15.0)));
    route(&mut w, "active", 4, &LINE);
    run_seen(&mut w, 1500);
    assert_eq!(w.set_modes(copter::BRAKE), 1, "held");
    let s = w.p.state(w.now);
    assert_eq!(s.hold_reason.as_deref(), Some("route-infeasible"));
    assert_eq!(s.banner.expect("banner").args["leg"], "3→4");
}

#[test]
fn no_auto_without_the_position_the_leg_from_here_starts_at() {
    let mut w = flying_to_2(&LINE, &post(15.0));
    assert_eq!(
        w.p.state(w.now).hold_reason.as_deref(),
        Some("route-infeasible")
    );
    // The FC's local position stops; the automatic skip's deadline passes.
    w.local = false;
    let t0 = w.now;
    while w.now < t0 + 45_500 {
        run_seen(&mut w, 100);
    }
    let s = w.p.state(w.now);
    let gaps = s.mission.expect("mission").gaps;
    assert!(
        gaps.iter()
            .any(|g| g.seq == 2 && g.reason == "stall_timeout"),
        "{gaps:?}"
    );
    assert_eq!(w.set_modes(copter::AUTO), 0, "AUTO never commanded");
    assert_ne!(w.mode, copter::AUTO);
}
