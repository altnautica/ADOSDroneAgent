//! Stick-flight safety: the obstacle brake, its reported state, hold capture
//! and the launch's arming mode.

use std::f32::consts::TAU;

use ados_protocol::mavlink::ardupilotmega::MavType;
use ados_protocol::pilot::AvoidState;
use serde_json::{json, Value};

use super::harness::World;
use crate::core::control::{brake_limit, stop_cap, BRAKE_LAG_S, HOLD_OBSTACLE_CLOSE};
use crate::core::Reply;
use crate::fc::{MASK_POS_VEL_YAW, MASK_VELOCITY};
use crate::geo;
use crate::modes::{copter, plane};
use crate::obstacles::Obstacles;

/// Mark a square block of cells centred on `c` with half-width `half`.
fn block(o: &mut Obstacles, c: [f32; 2], half: [f32; 2]) {
    o.grid.reset([0.0, 0.0]);
    let mut n = c[0] - half[0];
    while n <= c[0] + half[0] {
        let mut e = c[1] - half[1];
        while e <= c[1] + half[1] {
            o.grid.mark([n, e]);
            e += 0.1;
        }
        n += 0.1;
    }
    o.grid.transform();
}

/// The brake's cap toward a point `d` away for a vehicle at rest (normal
/// profile, 1.5 m margin).
fn cap(d: f32) -> f32 {
    stop_cap(d, 0.0, 3.0, 1.5)
}

#[test]
fn stop_cap_stops_before_the_margin_after_the_lag() {
    for d in [2.0f32, 4.0, 8.0, 15.0] {
        let v = cap(d);
        let stop = v * BRAKE_LAG_S + v * v / (2.0 * 3.0);
        assert!((stop - (d - 1.5)).abs() < 1e-3, "d {d}: stops in {stop}");
    }
    assert_eq!(cap(1.0), 0.0);
    // Still closing at 4 m/s, 5 m out: it covers 3.2 m before braking.
    assert!((stop_cap(5.0, 4.0, 3.0, 1.5) - (6.0f32 * 0.3).sqrt()).abs() < 1e-4);
}

#[test]
fn brake_caps_speed_before_a_wall_and_keeps_sliding() {
    let mut o = Obstacles::new();
    block(&mut o, [3.1, 0.0], [0.05, 15.0]);
    let b = brake_limit(&o, [0.0, 0.0], [0.0, 0.0], [8.0, 0.0], 3.0, 1.5, 0);
    assert!(b.cut && !b.close);
    assert!((b.v[0] - cap(3.0)).abs() < 0.35, "capped to {:?}", b.v);
    assert!(b.v[1].abs() < 0.05, "no sideways motion added: {:?}", b.v);
    let s = brake_limit(&o, [0.0, 0.0], [0.0, 0.0], [0.0, 3.0], 3.0, 1.5, 0);
    assert!((s.v[1] - 3.0).abs() < 0.05, "slide kept: {:?}", s.v);
}

#[test]
fn brake_sees_a_column_between_sample_rays() {
    // A 0.6 m column 7 m ahead and 1 m right of the track: no ray of a 15°
    // fan from the nose touches it, yet flying at it must be capped.
    let mut o = Obstacles::new();
    block(&mut o, [7.0, 1.0], [0.3, 0.3]);
    let b = brake_limit(&o, [0.0, 0.0], [0.0, 0.0], [8.0, 0.0], 3.0, 1.5, 0);
    assert!(b.cut, "column ignored: {:?}", b.v);
    let rel = [7.0f32, 1.0];
    let dir = geo::scale(rel, 1.0 / geo::norm(rel));
    let near_face = geo::norm(rel) - 0.4;
    assert!(
        geo::dot(b.v, dir) <= cap(near_face) + 0.2,
        "speed toward the column {} over {}",
        geo::dot(b.v, dir),
        cap(near_face)
    );
}

#[test]
fn brake_measures_from_where_the_vehicle_coasts_to() {
    // 3.75 m from a wall, commanding 2 m/s, but the vehicle still closes at
    // 5 m/s: in the FC's response time it is inside the margin, so nothing
    // more toward the wall may be asked.
    let mut o = Obstacles::new();
    block(&mut o, [3.95, 0.0], [0.05, 4.0]);
    let b = brake_limit(&o, [0.0, 0.0], [5.0, 0.0], [2.0, 0.0], 3.0, 1.5, 0);
    assert!(
        b.v[0] < 0.05 && b.v[1].abs() < 0.05,
        "still asking {:?}",
        b.v
    );
    // A slower command with the vehicle at rest stays.
    let r = brake_limit(&o, [0.0, 0.0], [0.0, 0.0], [1.5, 0.0], 3.0, 1.5, 0);
    assert!((r.v[0] - 1.5).abs() < 1e-3, "{:?}", r.v);
}

#[test]
fn inside_the_hard_stop_only_motion_away_is_kept() {
    // A wall 0.75 m to the east, inside 0.6 × 1.5 m.
    let mut o = Obstacles::new();
    block(&mut o, [0.0, 0.85], [3.0, 0.05]);
    let slide = brake_limit(&o, [0.0, 0.0], [0.0, 0.0], [2.0, 0.0], 3.0, 1.5, 0);
    assert!(slide.close);
    assert!(geo::norm(slide.v) < 1e-3, "slid along it: {:?}", slide.v);
    let away = brake_limit(&o, [0.0, 0.0], [0.0, 0.0], [0.0, -1.0], 3.0, 1.5, 0);
    assert!(
        (away.v[1] + 1.0).abs() < 0.05,
        "backing off kept: {:?}",
        away.v
    );
}

/// The recorded approach of a stick-flight crash: full forward at a column
/// 14.8 m ahead in SITL. Pitched nose-down while accelerating, the planar
/// lidar met the floor 7-9 m ahead, so the column (beyond that) was in no
/// return until 6 m out, when the vehicle already did 7 m/s. Replay each
/// recorded scan with the attitude, rangefinder and EKF state of its moment:
/// the speed the brake allows toward the column must let the vehicle stop
/// short of it (FC lag, 3 m/s², 1 m clearance plus a 0.35 m body), using the
/// simulator's true distance.
#[test]
fn replayed_column_approach_never_allows_more_than_it_can_stop_from() {
    let fx: Value = serde_json::from_str(include_str!("fixtures/wall_approach.json")).unwrap();
    let f32s = |v: &Value| -> Vec<f32> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_f64().unwrap() as f32)
            .collect()
    };
    for f in fx["frames"].as_array().unwrap() {
        let num = |k: &str| f[k].as_f64().unwrap() as f32;
        let (pos, vel) = (f32s(&f["pos_ne"]), f32s(&f["vel_ne"]));
        let (pos, vel) = ([pos[0], pos[1]], [vel[0], vel[1]]);
        let level = crate::obstacles::Level {
            roll: num("roll"),
            pitch: num("pitch"),
            yaw: num("yaw"),
            agl: Some(num("rangefinder_m")),
        };
        let scan = json!({"frame": "body_frd", "angle_start_rad": 0.0,
                          "angle_increment_rad": TAU / 360.0, "ranges_m": f["ranges_m"]});
        let mut o = Obstacles::new();
        o.ingest_scan(&scan, level, pos, 1000);
        o.rebuild(pos, 1000);
        let b = brake_limit(&o, pos, vel, [8.0, 0.0], 3.0, 1.5, 1000);
        let truth = num("truth_to_column_m");
        let allowed = b.v[0];
        let safe = stop_cap(truth, 0.0, 3.0, 1.0 + 0.35);
        assert!(
            allowed <= safe + 0.05,
            "t {}: allows {allowed:.2} m/s toward a column {truth} m away (can stop from {safe:.2})",
            num("t_s")
        );
    }
    // Hovering 14.8 m out, a gentle push is not refused.
    let f = &fx["frames"][0];
    let level = crate::obstacles::Level {
        roll: 0.0,
        pitch: 0.0,
        yaw: 0.0,
        agl: Some(f["rangefinder_m"].as_f64().unwrap() as f32),
    };
    let scan = json!({"frame": "body_frd", "angle_start_rad": 0.0,
                      "angle_increment_rad": TAU / 360.0, "ranges_m": f["ranges_m"]});
    let mut o = Obstacles::new();
    o.ingest_scan(&scan, level, [0.0, 0.0], 1000);
    let b = brake_limit(&o, [0.0, 0.0], [0.0, 0.0], [2.0, 0.0], 3.0, 1.5, 1000);
    assert!((b.v[0] - 2.0).abs() < 1e-3, "{:?}", b.v);
}

/// A planar scan of a wall `d` ahead (vehicle yaw 0), as world-engine sends it.
fn wall_scan(d: f32) -> Value {
    let ranges: Vec<f32> = (0..360)
        .map(|i| {
            let th = if i > 180 { i as f32 - 360.0 } else { i as f32 }.to_radians();
            if th.abs() < 1.2 {
                d / th.cos()
            } else {
                0.0
            }
        })
        .collect();
    json!({"frame": "body_frd", "angle_start_rad": 0.0,
           "angle_increment_rad": TAU / 360.0, "ranges_m": ranges})
}

fn stick_with_scan(w: &mut World, axes: [f32; 4], scan: &Value, ms: u64) {
    let end = w.now + ms;
    while w.now < end {
        w.input(axes);
        w.p.on_feed("plugin.world-engine.scan", scan, w.now);
        w.run(20);
    }
}

#[test]
fn stick_brake_reports_avoid_state_and_hard_stop() {
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    w.action("take_control");
    w.stick([0.0; 4], 600);
    let fwd = [0.0, 1.0, 0.0, 0.0];
    stick_with_scan(&mut w, fwd, &wall_scan(3.0), 1500);
    assert_eq!(w.p.state(w.now).avoid_state, AvoidState::Detour);
    assert!(w.last_setpoint().unwrap().vx < cap(2.9) + 0.05);
    stick_with_scan(&mut w, fwd, &wall_scan(1.2), 400);
    assert_eq!(w.p.state(w.now).avoid_state, AvoidState::Blocked);
    assert!(w.last_setpoint().unwrap().vx.abs() < 0.05);
    stick_with_scan(&mut w, fwd, &wall_scan(0.8), 200);
    let s = w.p.state(w.now);
    assert_eq!(s.avoid_state, AvoidState::Blocked);
    assert_eq!(s.hold_reason.as_deref(), Some(HOLD_OBSTACLE_CLOSE));
    stick_with_scan(&mut w, [0.0, -1.0, 0.0, 0.0], &wall_scan(0.8), 600);
    assert!(
        w.last_setpoint().unwrap().vx < -0.1,
        "backing away is allowed"
    );
}

#[test]
fn centred_sticks_hold_only_once_the_vehicle_slows() {
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    w.action("take_control");
    w.stick([0.0; 4], 600);
    w.stick([0.0, 1.0, 0.0, 0.0], 1000);
    // Sticks centred while the vehicle still coasts at 3 m/s.
    w.vel = [3.0, 0.0, 0.0];
    w.stick([0.0; 4], 2500);
    let sp = w.last_setpoint().unwrap();
    assert_eq!(
        sp.type_mask.bits(),
        MASK_VELOCITY,
        "hold point taken while coasting"
    );
    assert!(sp.vx.abs() < 1e-3 && sp.vy.abs() < 1e-3, "commands a stop");
    w.vel = [0.1, 0.0, 0.0];
    w.pos[0] = 6.0;
    w.stick([0.0; 4], 100);
    let sp = w.last_setpoint().unwrap();
    assert_eq!(sp.type_mask.bits(), MASK_POS_VEL_YAW);
    assert_eq!(sp.x, 6.0, "the hold is where the vehicle stopped");
}

#[test]
fn centred_sticks_hold_after_settling_even_in_a_drift() {
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    w.action("take_control");
    w.stick([0.0; 4], 600);
    w.stick([0.0, 1.0, 0.0, 0.0], 500);
    w.vel = [0.5, 0.0, 0.0];
    w.stick([0.0; 4], 3500);
    assert_eq!(
        w.last_setpoint().unwrap().type_mask.bits(),
        MASK_POS_VEL_YAW
    );
}

#[test]
fn plane_launch_selects_takeoff_mode_before_arming() {
    let mut w = World::new();
    w.mavtype = MavType::MAV_TYPE_FIXED_WING;
    for (n, v) in [
        ("FS_GCS_ENABL", 1.0),
        ("FS_LONG_TIMEOUT", 3.0),
        ("FS_LONG_ACTN", 1.0),
        ("BATT_FS_LOW_ACT", 1.0),
    ] {
        w.p.fc.params.insert(n.into(), v);
    }
    // ArduPlane boots in RTL and refuses to arm there.
    w.mode = plane::RTL;
    w.unarmable = vec![plane::RTL];
    w.run(100);
    let r = w.action_args("launch", json!({"after": "hold"}));
    assert!(matches!(&r, Reply::Action(a) if a.ok), "{r:?}");
    w.run(2000);
    assert!(w.armed, "armed in a mode the FC accepts");
    assert_eq!(w.mode, plane::TAKEOFF);
    let seq = w.p.state(w.now).sequence.unwrap();
    assert_eq!(seq.step, "takeoff");
    assert!(seq.error.is_none());
}

#[test]
fn copter_launch_from_an_unarmable_mode_arms_in_guided() {
    let mut w = World::new();
    w.mode = copter::RTL;
    w.unarmable = vec![copter::RTL];
    w.run(100);
    w.action("takeoff");
    w.run(1000);
    assert!(w.armed);
    assert_eq!(w.mode, copter::GUIDED);
}
