//! The confined-place profile: the place class and its readiness, the route
//! check of the FC mission against the live occupancy, the launch refusal, the
//! hold before an infeasible leg, and the 0.5 m/s cap near the clearance
//! limit. An open place flies as before.

use ados_protocol::mavlink::ardupilotmega::{MavCmd, MavMessage};
use ados_protocol::pilot::{Owner, PlaceClass, RouteState};
use serde_json::{json, Value};

use super::harness::{wall, World};
use crate::core::{Out, Reply};
use crate::modes::copter;
use crate::obstacles::{encode_tile_for_test, Bar, Obstacles};

/// The x500's propeller-tip radius.
pub(super) const TIP_M: f32 = 0.377;
/// Its confined margin (tip + 0.1) plus the 0.1 m floor of the 3σ term.
pub(super) const REQUIRED_M: f64 = 0.577;
const TILE_SPAN: f32 = 3.2;

pub(super) fn ok(r: &Reply) -> bool {
    matches!(r, Reply::Action(a) if a.ok)
}

pub(super) fn err(r: &Reply) -> Option<String> {
    match r {
        Reply::Action(a) => a.err.clone(),
        _ => None,
    }
}

/// The occupancy tiles covering `[n0, n1) × [e0, e1)` (local = map frame):
/// `occ(n, e)` cells occupied, the rest of the rectangle free, cells of the
/// tiles outside it unseen.
pub(super) fn map_payload(
    n0: f32,
    n1: f32,
    e0: f32,
    e1: f32,
    occ: &dyn Fn(f32, f32) -> bool,
) -> Value {
    let mut tiles = Vec::new();
    let idx = |v: f32| (v / TILE_SPAN).floor() as i32;
    for ix in idx(n0)..=idx(n1 - 1e-3) {
        for iy in idx(e0)..=idx(e1 - 1e-3) {
            let mut px = vec![205u8; 64 * 64];
            for lx in 0..64 {
                for ly in 0..64 {
                    let n = ix as f32 * TILE_SPAN + (lx as f32 + 0.5) * 0.05;
                    let e = iy as f32 * TILE_SPAN + (ly as f32 + 0.5) * 0.05;
                    if n >= n0 && n < n1 && e >= e0 && e < e1 {
                        px[(63 - lx) * 64 + ly] = if occ(n, e) { 0 } else { 254 };
                    }
                }
            }
            tiles.push(json!({"ix": ix, "iy": iy,
                "png_b64": encode_tile_for_test(&px, 64, png::FilterType::NoFilter)}));
        }
    }
    json!({"res_m": 0.05, "tile_cells": 64, "tiles": tiles})
}

/// A post 0.2 m square centred 0.6 m east of the line north at `n`: 0.525 m
/// from it.
pub(super) fn post(n: f32) -> impl Fn(f32, f32) -> bool {
    move |pn: f32, pe: f32| (pn - n).abs() < 0.1 && (0.5..0.7).contains(&pe)
}

/// The pose that places the map (map = local) with σ 0.02 m.
pub(super) fn seen(w: &mut World) {
    let p = json!({"pose": {"t": [w.pos[0], w.pos[1], w.pos[2]]}, "sigma_xy_m": 0.02});
    w.p.on_feed("plugin.world-engine.pose", &p, w.now);
}

/// Run `ms` with the pose arriving every 100 ms.
pub(super) fn run_seen(w: &mut World, ms: u64) {
    for _ in 0..ms / 100 {
        seen(w);
        w.run(100);
    }
}

pub(super) fn map(w: &mut World, payload: &Value) {
    w.p.on_feed("plugin.world-engine.occupancy", payload, w.now);
    seen(w);
}

/// A confined place for the x500 (margin 0.477 m), the open margin left at
/// its 1.5 m default, and an FC set up for it (`SRTL_ACCURACY` 0.2). A
/// confined place has walls, so the lidar sees one 8 m to the left (west,
/// off the routes north along e = 0): an empty scan there is not obstacle
/// data.
pub(super) fn confined(w: &mut World) {
    let p = &mut w.p.settings.pilot;
    p.place_class = PlaceClass::Confined;
    p.prop_tip_radius_m = Some(TIP_M);
    w.p.fc.params.insert("SRTL_ACCURACY".into(), 0.2);
    w.scan_ranges = wall(270, 8.0);
}

/// The route north along e = 0: items 1–4 at n 2, 10, 20, 25.
const ROUTE: [(u32, f32); 4] = [(1, 2.0), (2, 10.0), (3, 20.0), (4, 25.0)];

/// The vehicle on the ground on a pad at the routes' height (d −5): its
/// scan plane is the legs' own, so a check before the launch judges them.
/// From the floor (d 0) the planar scan judges no leg flown at 5 m.
pub(super) fn on_pad(w: &mut World) {
    w.pos = [0.0, 0.0, -5.0];
    w.feed();
}

/// The router's snapshot with the route's items in the local frame, the FC
/// at `seq` in `state`.
pub(super) fn mission(w: &mut World, state: &str, seq: u32) {
    let items: Vec<Value> = ROUTE
        .iter()
        .map(|&(s, n)| {
            json!({"seq": s, "cmd": 16, "frame": 6, "lat_e7": 1, "lon_e7": 1,
                              "alt_m": 5.0, "ned": [n, 0.0, -5.0]})
        })
        .collect();
    let target_n = ROUTE.iter().find(|r| r.0 == seq).map_or(0.0, |r| r.1);
    let snapshot = json!({"armed": w.armed, "mission": {
        "state": state, "seq": seq, "total": 5, "item_reached": seq.checked_sub(1),
        "id": null, "source": "mission_state", "age_ms": 100, "landing_seq": false,
        "rev": 1, "uploads_seen": 0, "items": items,
        "target": {"seq": seq, "cmd": 16, "ned": [target_n, 0.0, -5.0]}}});
    let m = crate::router_state::mission_of(snapshot);
    w.p.on_feed(crate::router_state::MISSION_TOPIC, &m, w.now);
    w.collect();
}

/// The open area n −6.4…32, e −9.6…9.6 with `occ` in it.
pub(super) fn area(occ: &dyn Fn(f32, f32) -> bool) -> Value {
    map_payload(-6.4, 32.0, -9.6, 9.6, occ)
}

pub(super) fn speed_changes(w: &World) -> Vec<f32> {
    w.sent
        .iter()
        .filter_map(|m| match m {
            MavMessage::COMMAND_LONG(c) if c.command == MavCmd::MAV_CMD_DO_CHANGE_SPEED => {
                Some(c.param2)
            }
            _ => None,
        })
        .collect()
}

#[test]
fn the_clearance_is_measured_to_the_leg_and_to_the_cell_edge() {
    let mut o = Obstacles::new();
    o.ingest_occupancy(&map_payload(-3.2, 12.8, -3.2, 3.2, &post(4.5)));
    o.ingest_pose(
        &json!({"pose": {"t": [0.0, 0.0, 0.0]}}),
        Some([0.0, 0.0]),
        0,
    );
    let req = &Bar::flat(REQUIRED_M as f32);
    let legs = [(1, [0.0, 0.0, -5.0], 2, [10.0, 0.0, -5.0])];
    let r = crate::core::control::check_legs(&o, &legs, req, Some(-5.0), 0);
    assert_eq!(r[0].state, RouteState::Infeasible, "{r:?}");
    let (p, d) = r[0].pinch.unwrap();
    assert!((p[0] - 4.5).abs() < 0.1 && p[1].abs() < 1e-3, "{p:?}");
    // The post's nearest cell centre is 0.525 m off the leg; its nearest
    // possible point is half a cell diagonal (0.035 m) closer.
    assert!(
        (d - (0.525 - 0.05 * std::f32::consts::FRAC_1_SQRT_2)).abs() < 1e-3,
        "{d}"
    );
    // A leg leaving the map is unverified; one with no obstacle near it is
    // feasible.
    let out = [(1, [0.0, -2.0, -5.0], 2, [20.0, -2.0, -5.0])];
    let r = crate::core::control::check_legs(&o, &out, req, Some(-5.0), 0);
    assert_eq!(r[0].state, RouteState::Unverified);
    let clear = [(1, [0.0, -2.0, -5.0], 2, [10.0, -2.0, -5.0])];
    let r = crate::core::control::check_legs(&o, &clear, req, Some(-5.0), 0);
    assert_eq!(r[0].state, RouteState::Feasible);
    // No pose offset: the map cannot be placed, everything is unseen.
    let mut o = Obstacles::new();
    o.ingest_occupancy(&map_payload(-3.2, 12.8, -3.2, 3.2, &|_, _| false));
    let r = crate::core::control::check_legs(&o, &clear, req, Some(-5.0), 0);
    assert_eq!(r[0].state, RouteState::Unverified);
}

#[test]
fn a_leg_is_judged_only_where_it_runs_in_the_scan_slice() {
    // The post 0.525 m off the leg at n 4.5 reads too tight from the leg's
    // own height; from 1 m above or below it the planar slice says nothing
    // about the leg, and a leg climbing through the slice is judged only
    // where it crosses it.
    let mut o = Obstacles::new();
    o.ingest_occupancy(&map_payload(-3.2, 12.8, -3.2, 3.2, &post(4.5)));
    o.ingest_pose(
        &json!({"pose": {"t": [0.0, 0.0, 0.0]}}),
        Some([0.0, 0.0]),
        0,
    );
    let req = &Bar::flat(REQUIRED_M as f32);
    let check = |legs: &[(u32, [f32; 3], u32, [f32; 3])], d: Option<f32>| {
        crate::core::control::check_legs(&o, legs, req, d, 0)[0]
    };
    let level = [(1, [0.0, 0.0, -5.0], 2, [10.0, 0.0, -5.0])];
    assert_eq!(check(&level, Some(-5.0)).state, RouteState::Infeasible);
    for d in [Some(-6.0), Some(-4.0), None] {
        let r = check(&level, d);
        assert_eq!(r.state, RouteState::Unverified, "{d:?}");
        assert_eq!(r.pinch, None, "{d:?}");
    }
    // From d −5 to −6 over 10 m: within 0.13 m of d −5 for its first 1.3 m,
    // short of the post at n 4.5.
    let climb = [(1, [0.0, 0.0, -5.0], 2, [10.0, 0.0, -6.0])];
    let r = check(&climb, Some(-5.0));
    assert_eq!(r.state, RouteState::Unverified, "{r:?}");
    // From the plane at d −5.45 the slice holds n 3.2 to 5.8: the post.
    let r = check(&climb, Some(-5.45));
    assert_eq!(r.state, RouteState::Infeasible, "{r:?}");
    let (p, _) = r.pinch.unwrap();
    assert!((p[0] - 4.5).abs() < 0.1, "{p:?}");
    assert!((p[2] + 5.45).abs() < 0.02, "{p:?}");
    // A clear leg judged only in part is unverified, never feasible.
    let clear = [(1, [0.0, -2.0, -5.0], 2, [10.0, -2.0, -6.0])];
    assert_eq!(check(&clear, Some(-5.45)).state, RouteState::Unverified);
    let clear_level = [(1, [0.0, -2.0, -5.0], 2, [10.0, -2.0, -5.0])];
    assert_eq!(check(&clear_level, Some(-5.1)).state, RouteState::Feasible);
}

#[test]
fn confined_without_a_tip_radius_blocks_readiness_and_refuses_the_launch() {
    let mut w = World::new();
    w.run(200);
    let item = |w: &World| {
        w.p.state(w.now)
            .readiness
            .into_iter()
            .find(|r| r.id == "place_class")
    };
    assert_eq!(item(&w), None, "an open place has no place_class item");
    w.p.settings.pilot.place_class = PlaceClass::Confined;
    w.p.fc.params.insert("SRTL_ACCURACY".into(), 0.2);
    let it = item(&w).expect("place_class item");
    assert!(!it.ok);
    assert_eq!(it.severity, ados_protocol::pilot::Severity::Block);
    assert!(it.msg.contains("pilot.prop_tip_radius_m"), "{}", it.msg);
    assert_eq!(
        err(&w.action("arm")).as_deref(),
        Some("not_ready:place_class")
    );
    let r = w.action_args("launch", json!({"after": "hold"}));
    assert_eq!(err(&r).as_deref(), Some("no_prop_radius"));
    assert!(!w.armed);
    // With the radius the item passes and names the margin.
    w.p.settings.pilot.prop_tip_radius_m = Some(TIP_M);
    let it = item(&w).unwrap();
    assert!(it.ok, "{it:?}");
    assert_eq!(it.msg, "Confined place: 0.48 m from obstacles");
}

#[test]
fn a_confined_place_takes_a_tight_smartrtl_accuracy_an_open_one_flags_it() {
    let mut w = World::new();
    w.p.fc.params.insert("SRTL_ACCURACY".into(), 0.2);
    w.run(200);
    let item = |w: &World| {
        w.p.state(w.now)
            .readiness
            .into_iter()
            .find(|r| r.id == "fc_profile")
            .expect("fc_profile item")
    };
    let it = item(&w);
    assert!(!it.ok, "an open place keeps the 2 m accuracy");
    assert_eq!(it.msg, "FC parameters differ: SRTL_ACCURACY 0.2 (want 2)");
    confined(&mut w);
    let it = item(&w);
    assert!(it.ok, "{it:?}");
    assert_ne!(
        err(&w.action("arm")).as_deref(),
        Some("not_ready:fc_profile")
    );
    // Coarser than the confined bound flags again.
    w.p.fc.params.insert("SRTL_ACCURACY".into(), 0.25);
    let it = item(&w);
    assert!(!it.ok);
    assert_eq!(
        it.msg,
        "FC parameters differ: SRTL_ACCURACY 0.25 (want > 0 and ≤ 0.2 in a confined place)"
    );
}

#[test]
fn the_place_class_action_persists_and_is_refused_while_armed() {
    let mut w = World::new();
    w.run(200);
    let r = w.req_now(ados_protocol::pilot::Request::Action(
        ados_protocol::pilot::ActionReq {
            name: "place_class:confined".into(),
            src: "mc".into(),
            args: None,
            id: None,
        },
    ));
    assert!(ok(&r), "{r:?}");
    assert_eq!(w.p.settings.pilot.place_class, PlaceClass::Confined);
    let v = serde_json::to_value(w.p.state(w.now)).unwrap();
    assert_eq!(v["place_class"], "confined");
    assert_eq!(
        err(&w.action("place_class:tunnel")).as_deref(),
        Some("bad_value")
    );
    assert!(ok(&w.action("place_class:open")));
    let v = serde_json::to_value(w.p.state(w.now)).unwrap();
    assert!(v.get("place_class").is_none(), "open is omitted: {v}");
    // The persisted value.
    w.p.on_request(
        ados_protocol::pilot::Request::Action(ados_protocol::pilot::ActionReq {
            name: "place_class:confined".into(),
            src: "mc".into(),
            args: None,
            id: None,
        }),
        w.now,
    );
    let persisted = w.p.drain().into_iter().any(|o| {
        matches!(o, Out::Persist { section: "pilot", key: "place_class", ref value } if value == "confined")
    });
    assert!(persisted);
    w.airborne(copter::GUIDED);
    assert_eq!(err(&w.action("place_class:open")).as_deref(), Some("armed"));
    assert_eq!(w.p.settings.pilot.place_class, PlaceClass::Confined);
}

#[test]
fn a_launch_into_an_infeasible_route_is_refused_naming_the_pinch() {
    let ((), logs) = super::supervisor::with_logs(|| {
        let mut w = World::new();
        confined(&mut w);
        on_pad(&mut w);
        map(&mut w, &area(&post(15.0)));
        mission(&mut w, "loaded", 0);
        w.run(200);
        let r = w.action_args("launch", json!({"after": "mission"}));
        assert_eq!(err(&r).as_deref(), Some("route_infeasible"));
        assert!(!w.armed);
        let rc = w.p.state(w.now).route_check.expect("route_check");
        assert_eq!(rc.state, RouteState::Infeasible);
        assert_eq!(rc.required_m, REQUIRED_M);
        assert_eq!(rc.margin_m, 0.477);
        assert_eq!(rc.sigma_m, Some(0.02));
        let pinch = rc.pinch.expect("pinch");
        assert_eq!((pinch.from_seq, pinch.to_seq), (2, 3));
        assert!((pinch.ned[0] - 15.0).abs() < 0.1, "{pinch:?}");
        assert_eq!(&pinch.ned[1..], &[0.0, -5.0]);
        // The cell's nearest possible point: 0.525 m to its centre less half
        // the 0.05 m cell's diagonal.
        assert!((pinch.clearance_m - 0.490).abs() < 0.002, "{pinch:?}");
        assert_eq!(pinch.required_m, REQUIRED_M);
        assert!(rc.unverified_legs.is_empty());
    });
    let line = logs
        .lines()
        .find(|l| l.contains("pilot_launch_refused"))
        .unwrap_or_else(|| panic!("{logs}"));
    assert!(
        line.contains("route_infeasible") && line.contains("2→3"),
        "{line}"
    );
    assert!(
        line.contains("[1") && line.contains("clearance_m=0.489"),
        "{line}"
    );
    assert!(line.contains("required_m=0.57"), "{line}");
    assert!(logs.contains("pilot_route_check") && logs.contains("infeasible"));
}

#[test]
fn a_feasible_route_launches_and_reports_feasible() {
    let mut w = World::new();
    confined(&mut w);
    map(&mut w, &area(&|_, _| false));
    mission(&mut w, "loaded", 0);
    w.run(200);
    // From the floor the scan plane is under every leg: none is judged yet,
    // and none holds the launch.
    let rc = w.p.state(w.now).route_check.expect("route_check");
    assert_eq!((rc.state, rc.pinch), (RouteState::Unverified, None));
    assert_eq!(rc.unverified_legs, vec![2, 3, 4]);
    assert!(ok(&w.action_args(
        "launch",
        json!({"after": "mission", "alt_m": 5.0})
    )));
    // At the legs' height they are judged, and clear.
    run_seen(&mut w, 3000);
    assert_eq!(w.pos[2], -5.0);
    let rc = w.p.state(w.now).route_check.expect("route_check");
    assert_eq!(rc.state, RouteState::Feasible, "{rc:?}");
    assert_eq!(rc.pinch, None, "no obstacle near the route");
    // The pose stops: its σ is held 5 s, then the route reads at best
    // unverified.
    w.run(5200);
    assert_eq!(
        w.p.state(w.now).route_check.unwrap().state,
        RouteState::Unverified
    );
}

#[test]
fn an_infeasible_leg_ahead_holds_a_mission_a_ground_station_started() {
    // AUTO comes from a ground station (no pilot launch): the FC flies item 2
    // with the infeasible leg 2→3 ahead.
    let mut w = World::new();
    confined(&mut w);
    w.airborne(copter::AUTO);
    assert_eq!(w.p.owner(), Owner::MissionFc);
    map(&mut w, &area(&post(15.0)));
    mission(&mut w, "active", 2);
    run_seen(&mut w, 100);
    // Far ahead (the pinch is 15 m off): not held on one check, the FC
    // slowed meanwhile; held once the next check confirms it.
    assert_eq!(w.set_modes(copter::BRAKE), 0, "not on one check");
    assert_eq!(speed_changes(&w), vec![0.5]);
    run_seen(&mut w, 1000);
    assert_eq!(w.set_modes(copter::BRAKE), 1, "held once confirmed");
    run_seen(&mut w, 1600);
    assert_eq!(w.mode, copter::GUIDED);
    assert_eq!(w.p.owner(), Owner::Pilot);
    let s = w.p.state(w.now);
    assert_eq!(s.hold_reason.as_deref(), Some("route-infeasible"));
    let stall = s.mission_stall.expect("the stall flow");
    assert_eq!((stall.kind.as_str(), stall.seq), ("mission", Some(2)));
    let in_s = stall
        .skip_in_s
        .expect("a leg from the current item counts down");
    assert!((43.0..=45.0).contains(&in_s), "{in_s}");
    let b = s.banner.expect("banner");
    assert_eq!(b.text_key, "pilot.banner.route_infeasible");
    assert_eq!(b.args["leg"], "2→3");
    assert_eq!(b.args["clearance_m"], 0.49);
    assert_eq!(b.args["required_m"], 0.58);
    // The operator skips item 2. The FC would fly from here (0, 0) straight
    // to item 3 past the same post: the pilot holds again instead, naming
    // the leg it would fly, and never puts the FC back in AUTO.
    assert!(ok(&w.action("mission_skip")));
    assert_ne!(w.mode, copter::AUTO);
    assert_eq!(w.set_modes(copter::AUTO), 0);
    mission(&mut w, "active", 3);
    run_seen(&mut w, 1500);
    assert_eq!(w.p.owner(), Owner::Pilot);
    let s = w.p.state(w.now);
    assert_eq!(s.hold_reason.as_deref(), Some("route-infeasible"));
    assert_eq!(s.banner.expect("banner").args["leg"], "1→3");
}

#[test]
fn a_leg_seen_tight_mid_flight_holds_the_mission() {
    let mut w = World::new();
    confined(&mut w);
    w.p.fc.params.insert("WP_SPD".into(), 5.0);
    w.airborne(copter::AUTO);
    // Only the first 12.8 m are mapped: the legs past it are unseen.
    map(&mut w, &map_payload(-6.4, 12.8, -9.6, 9.6, &|_, _| false));
    mission(&mut w, "active", 2);
    run_seen(&mut w, 1500);
    let rc = w.p.state(w.now).route_check.unwrap();
    assert_eq!(rc.state, RouteState::Unverified);
    assert_eq!(rc.unverified_legs, vec![3, 4]);
    assert_eq!(w.set_modes(copter::BRAKE), 0, "unseen is no reason to hold");
    // The rest of the tunnel is mapped, with the pinch on leg 2→3.
    map(&mut w, &area(&post(15.0)));
    run_seen(&mut w, 2200);
    assert_eq!(w.set_modes(copter::BRAKE), 1);
    assert_eq!(
        w.p.state(w.now).hold_reason.as_deref(),
        Some("route-infeasible")
    );
}

#[test]
fn the_cap_holds_pilot_flown_setpoints_near_a_wall_and_ends_clear_of_it() {
    let mut w = World::new();
    confined(&mut w);
    w.airborne(copter::GUIDED);
    // A wall 1.5 m east of the vehicle: inside required + 1.5 m.
    map(&mut w, &area(&|_, e| e >= 1.5));
    let id = w.lease();
    let ((), logs) = super::supervisor::with_logs(|| {
        for _ in 0..6 {
            seen(&mut w);
            w.goal(id, 20.0, 0.0);
            w.run(500);
        }
    });
    let cap = w.p.state(w.now).speed_cap.expect("the confined cap");
    assert_eq!((cap.reason.as_str(), cap.v_h_mps), ("confined-near", 0.5));
    let from = w.sent.len();
    for _ in 0..4 {
        seen(&mut w);
        w.goal(id, 20.0, 0.0);
        w.run(500);
    }
    let fast = w.sent[from..].iter().any(|m| {
        matches!(m, MavMessage::SET_POSITION_TARGET_LOCAL_NED(s) if s.vx.hypot(s.vy) > 0.5 + 1e-3)
    });
    assert!(!fast, "a setpoint above 0.5 m/s near the wall");
    assert!(
        logs.contains("pilot_confined_cap") && logs.contains("on=true") && logs.contains("near"),
        "{logs}"
    );
    // Clear of the wall (7.5 m west of it): the cap ends.
    w.pos = [0.0, -6.0, -5.0];
    for _ in 0..4 {
        seen(&mut w);
        w.goal(id, 20.0, -6.0);
        w.run(500);
    }
    assert_eq!(w.p.state(w.now).speed_cap, None);
    let from = w.sent.len();
    for _ in 0..4 {
        seen(&mut w);
        w.goal(id, 20.0, -6.0);
        w.run(500);
    }
    let fast = w.sent[from..].iter().any(
        |m| matches!(m, MavMessage::SET_POSITION_TARGET_LOCAL_NED(s) if s.vx.hypot(s.vy) > 1.0),
    );
    assert!(fast, "the goal flies at its own speed again");
}

#[test]
fn the_fc_mission_is_slowed_near_a_wall_and_restored_clear_of_it() {
    let mut w = World::new();
    confined(&mut w);
    w.p.fc.params.insert("WP_SPD".into(), 5.0);
    w.airborne(copter::AUTO);
    // A wall 1.6 m east from n 0 to 6; open beyond.
    map(&mut w, &area(&|n, e| e >= 1.6 && n < 6.0));
    mission(&mut w, "active", 2);
    run_seen(&mut w, 600);
    assert_eq!(speed_changes(&w), vec![0.5], "slowed once");
    let c = w.p.state(w.now).speed_cap.expect("cap in AUTO");
    assert_eq!(c.reason, "confined-near");
    run_seen(&mut w, 1000);
    assert_eq!(speed_changes(&w), vec![0.5], "not re-sent");
    // Past the wall: the FC's default speed comes back.
    w.pos = [12.0, 0.0, -5.0];
    run_seen(&mut w, 600);
    assert_eq!(speed_changes(&w), vec![0.5, 5.0]);
    assert_eq!(w.p.state(w.now).speed_cap, None);
    assert_eq!(w.p.owner(), Owner::MissionFc);
    assert_eq!(w.mode, copter::AUTO);
}

#[test]
fn an_open_place_flies_as_before() {
    // The same flights in an open place, with and without the confined
    // settings filled in, send the same frames and publish no confined
    // state.
    let fly = |radius: Option<f32>| {
        let mut w = World::new();
        w.p.settings.pilot.prop_tip_radius_m = radius;
        w.p.fc.params.insert("WP_SPD".into(), 5.0);
        w.airborne(copter::AUTO);
        map(
            &mut w,
            &area(&|n, e| post(15.0)(n, e) || (e >= 1.6 && n < 6.0)),
        );
        mission(&mut w, "active", 2);
        run_seen(&mut w, 2000);
        w.airborne(copter::GUIDED);
        let id = w.lease();
        for _ in 0..4 {
            seen(&mut w);
            w.goal(id, 20.0, 0.0);
            w.run(500);
        }
        let v = serde_json::to_value(w.p.state(w.now)).unwrap();
        (w, v)
    };
    let (w, v) = fly(None);
    let (w2, _) = fly(Some(TIP_M));
    assert_eq!(w.sent, w2.sent);
    assert!(speed_changes(&w).is_empty());
    assert_eq!(w.set_modes(copter::BRAKE), 0);
    assert!(
        v.get("place_class").is_none() && v.get("route_check").is_none(),
        "{v}"
    );
    assert!(v.get("speed_cap").is_none(), "{v}");
}

#[test]
fn the_golden_confined_frame_round_trips() {
    let text = include_str!("../../../../ados-control/tests/fixtures/pilot-frame-confined.json");
    let v: Value = serde_json::from_str(text).unwrap();
    let s: ados_protocol::pilot::PilotState = serde_json::from_value(v.clone()).unwrap();
    assert_eq!(serde_json::to_value(&s).unwrap(), v);
    assert_eq!(s.place_class, PlaceClass::Confined);
    let rc = s.route_check.unwrap();
    assert_eq!(rc.state, RouteState::Infeasible);
    assert_eq!(rc.pinch.unwrap().to_seq, 4);
    assert_eq!(rc.unverified_legs, vec![5, 6]);
    assert_eq!(s.speed_cap.unwrap().reason, "confined-near");
}

#[test]
fn the_router_route_reaches_the_pilot_in_the_local_frame() {
    let snapshot = json!({"mission": {"state": "active", "seq": 2, "total": 4,
        "item_reached": 1, "id": null, "source": "mission_state", "age_ms": 40,
        "landing_seq": false, "rev": 2, "uploads_seen": 0,
        "items": [{"seq": 1, "cmd": 22, "frame": 3, "lat_e7": 0, "lon_e7": 0, "alt_m": 5.0, "ned": null},
                  {"seq": 2, "cmd": 16, "frame": 6, "lat_e7": 1, "lon_e7": 1, "alt_m": 5.0,
                   "ned": [10.0, 2.0, -5.0]}]}});
    let m = crate::router_state::mission_of(snapshot);
    let feed = crate::core::control::route_of(&m);
    let items = feed.items;
    assert_eq!(items.len(), 2);
    assert_eq!((items[0].seq, items[0].cmd, items[0].ned), (1, 22, None));
    assert!(items[0].here, "latitude and longitude 0");
    assert_eq!(items[1].ned, Some([10.0, 2.0, -5.0]));
    assert!(!items[1].here);
    // The take-off without a position starts at home.
    let legs = crate::core::control::legs_of(&items, [1.0, 1.0], &|_| false, None, None);
    assert_eq!(legs.legs, vec![(1, [1.0, 1.0, -5.0], 2, [10.0, 2.0, -5.0])]);
    assert!(legs.unchecked.is_empty());
}
