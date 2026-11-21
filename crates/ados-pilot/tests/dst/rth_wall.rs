//! Returns home with a wall across the way, and the FC's proximity backstop.
//!
//! - Routed return (avoid `bypass`): the copter flies out north with
//!   nothing in the way, a wall then stands between it and home, and the
//!   operator asks for the return. The FC's return heads straight home (the
//!   simulated SmartRTL), the supervisor stops it short of the wall, and the
//!   pilot routes home round the wall's end and lands there, never inside
//!   the obstacle margin; the low-battery reserve counts the planned way,
//!   longer than the straight line.
//! - Backstop: the FC's proximity stop (`AVOID_ENABLE` 2, `AVOID_MARGIN`
//!   inside the pilot's margin) never acts while the pilot flies (here and
//!   in every matrix scenario: [`crate::matrix`]); with the pilot gone and
//!   the FC flying the last velocity target at the wall, it stops the copter
//!   short of it, where the same flight with the backstop off hits it.

use ados_pilot::modes::copter;
use ados_protocol::mavlink::ardupilotmega::*;
use ados_protocol::pilot::Owner;

use crate::metrics::{COLLISION_M, MARGIN_M};
use crate::scenarios::launch_to_hold;
use crate::sim::Sim;

/// The flight height, m.
const ALT_M: f32 = 4.0;
/// How far out the copter flies before the return, m.
const OUT_N_M: f32 = 20.0;
/// The wall: posts this far north, out to this either side, m.
const WALL_N_M: f32 = 10.0;
const WALL_HALF_M: f32 = 4.0;
const POST_R_M: f32 = 0.25;
const POST_STEP_M: f32 = 0.4;
/// The return lands home within this, m.
const HOME_M: f32 = 1.5;

fn wall(n: f32) -> Vec<([f32; 2], f32)> {
    let k = (2.0 * WALL_HALF_M / POST_STEP_M).round() as i32;
    (0..=k)
        .map(|i| ([n, -WALL_HALF_M + POST_STEP_M * i as f32], POST_R_M))
        .collect()
}

/// What a routed return run showed.
#[derive(Debug, Default)]
struct RthRun {
    routed: bool,
    /// The planned way and the straight line when the pilot took over, m.
    planned_m: f32,
    straight_m: f32,
    /// The reserve's time home then, s.
    return_s: u32,
    min_clearance_m: f32,
    landed_home_m: Option<f32>,
}

fn rth_round_the_wall(seed: u64) -> (Sim, RthRun) {
    // Bypass: a blocked goto routes round in both planner modes.
    let mut s = Sim::new("rth_wall", seed, |st| {
        st.pilot.avoid = ados_protocol::pilot::AvoidMode::Bypass;
    });
    s.boot();
    launch_to_hold(&mut s, ALT_M);
    s.request(ados_protocol::pilot::Request::Goto {
        n: OUT_N_M,
        e: 0.0,
        d: -ALT_M,
        yaw: None,
    });
    assert_eq!(s.p.owner(), Owner::Goto);
    let out = s.run_until(40_000, |s| (s.fc.truth()[0] - OUT_N_M).abs() < 1.0);
    assert!(out, "s{seed}: never flew out");
    s.run(2_000);
    // Something now stands across the way home.
    s.stand = wall(WALL_N_M);
    s.run(1_000);
    let a = s.action("rth", None);
    assert!(a.ok, "s{seed}: rth refused: {a:?}");
    let mut r = RthRun {
        min_clearance_m: f32::INFINITY,
        ..Default::default()
    };
    let t0 = s.now;
    while s.now - t0 < 120_000 && s.fc.armed {
        s.step();
        r.min_clearance_m = r.min_clearance_m.min(s.stand_clearance_m());
        if !r.routed {
            if let Some(rr) = s.p.state(s.now).return_route {
                r.routed = true;
                // The planner holds its route once it has replanned.
                s.run(600);
                let st = s.p.state(s.now);
                let rr = st.return_route.unwrap_or(rr);
                r.planned_m = rr.remaining_m;
                r.straight_m = rr.straight_m;
                r.return_s = st.battery_reserve.map_or(0, |b| b.return_s);
            }
        }
    }
    if !s.fc.armed {
        let t = s.fc.truth();
        r.landed_home_m = Some(t[0].hypot(t[1]));
    }
    (s, r)
}

#[test]
fn dst_rth_with_a_wall_across_routes_home_round_it() {
    for seed in crate::SEEDS {
        let (s, r) = rth_round_the_wall(seed);
        eprintln!(
            "dst rth_wall s{seed}: {r:?}, fc avoid trips {}",
            s.fc.prox.trips
        );
        assert!(r.routed, "s{seed}: the pilot never took the return over");
        assert!(
            r.landed_home_m.is_some_and(|d| d < HOME_M),
            "s{seed}: did not land home: {r:?}"
        );
        assert!(
            r.min_clearance_m >= MARGIN_M,
            "s{seed}: into the margin: {r:?}"
        );
        // The way home is round the wall: longer than the straight line,
        // and the reserve's time home counts it.
        assert!(
            r.planned_m > r.straight_m + 1.0,
            "s{seed}: planned way not longer than the straight line: {r:?}"
        );
        assert!(
            r.return_s as f32 >= r.planned_m / 5.0,
            "s{seed}: the reserve does not count the planned way: {r:?}"
        );
        assert_eq!(s.fc.prox.trips, 0, "s{seed}: the FC's backstop acted");
        assert_eq!(s.fc.mode, copter::LAND, "s{seed}: landed by LAND");
    }
}

/// The pilot gone with the copter 4 m short of the wall and the FC still
/// flying a 3 m/s velocity target at it for the setpoint timeout; with
/// `backstop` off, `AVOID_ENABLE` 0 once airborne. Returns the least
/// clearance to the wall's surface, m.
fn killed_pilot_toward_the_wall(seed: u64, backstop: bool) -> f32 {
    let mut s = Sim::new("fc_backstop", seed, |_| {});
    s.boot();
    launch_to_hold(&mut s, ALT_M);
    if !backstop {
        s.fc.params.insert("AVOID_ENABLE".into(), 0.0);
    }
    // The wall 4 m ahead: the target would carry the copter 9 m.
    let n0 = s.fc.truth()[0];
    s.stand = wall(n0 + 4.0 + POST_R_M);
    s.run(1_000);
    // The pilot's last word: a velocity target at the wall. Then nothing.
    let sp = SET_POSITION_TARGET_LOCAL_NED_DATA {
        time_boot_ms: 0,
        x: 0.0,
        y: 0.0,
        z: -ALT_M,
        vx: 3.0,
        vy: 0.0,
        vz: 0.0,
        afx: 0.0,
        afy: 0.0,
        afz: 0.0,
        yaw: 0.0,
        yaw_rate: 0.0,
        // Position, acceleration, yaw and yaw rate ignored: velocity only.
        type_mask: PositionTargetTypemask::from_bits_truncate(0b1101_1100_0111),
        target_system: 1,
        target_component: 1,
        coordinate_frame: MavFrame::MAV_FRAME_LOCAL_NED,
    };
    assert_eq!(s.fc.mode, copter::GUIDED);
    s.fc.apply(&MavMessage::SET_POSITION_TARGET_LOCAL_NED(sp), s.now);
    let mut min = f32::INFINITY;
    for _ in 0..(8_000 / 20) {
        s.now += 20;
        s.fc.step(s.now, 0.02, &mut s.rng, false);
        min = min.min(s.stand_clearance_m());
    }
    min
}

#[test]
fn dst_fc_backstop_stops_short_of_the_wall_without_the_pilot() {
    for seed in crate::SEEDS {
        let on = killed_pilot_toward_the_wall(seed, true);
        let off = killed_pilot_toward_the_wall(seed, false);
        eprintln!("dst fc_backstop s{seed}: clearance {on:.2} m with it, {off:.2} m without");
        assert!(
            on > COLLISION_M,
            "s{seed}: hit the wall with the backstop: {on}"
        );
        assert!(
            off <= COLLISION_M,
            "s{seed}: the flight never reached the wall: {off}"
        );
    }
}
