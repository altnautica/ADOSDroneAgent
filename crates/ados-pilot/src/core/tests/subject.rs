//! The subject brake: the subject a skill's goal names (`subject_ne`) is a
//! person the vehicle keeps clear of whatever the skill commands, slowing
//! for them on the goal profile and cut at the profile's stick acceleration
//! when that is not enough, once the subject has been named for 3 s; and it
//! climbs clear over them while near. With avoidance off: the brake needs no
//! obstacle sensor.

use ados_protocol::pilot::{AvoidMode, Goal, GoalKind, Request, VelMode};

use super::harness::{wall, World};
use super::shaping::{fly_for, flying, peak_acc_jerk, A_MAX, JERK};
use crate::core::control::stop_cap;

/// The distance kept from the subject with no sigma, centre to CG: its
/// body's 0.4 m, the vehicle's default 0.4 m and the 0.5 m margin.
const KEEP: f32 = 1.3;
/// The climb floor above ground: the 2 m body, the vehicle's 0.4 m and the
/// 1 m clearance over it.
const FLOOR_AGL: f32 = 3.4;
/// How long after a subject is named the brake holds off, ms.
const ARM_MS: u64 = 3000;

/// A follow goal commanding `vn` north at height `d` (NED), naming
/// `subject`.
fn follow(vn: f32, subject: Option<[f32; 2]>, d: f32) -> Goal {
    let mut g = Goal::new(GoalKind::Follow, [100.0, 0.0, d], 6.0);
    g.vn = vn;
    g.vel_mode = VelMode::Command;
    g.subject_ne = subject;
    g
}

fn send(w: &mut World, id: u64, goal: Goal) {
    w.req(Request::Goal { lease_id: id, goal });
}

/// The last setpoint's horizontal velocity and down position.
fn setpoint(w: &World) -> ([f32; 2], f32) {
    let sp = w.last_setpoint().expect("a setpoint");
    ([sp.vx, sp.vy], sp.z)
}

/// The limits the published command names.
fn limits(w: &World) -> Vec<String> {
    let frame = serde_json::to_value(w.p.state(w.now)).unwrap();
    frame["command"]["limits"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|l| l.as_str().unwrap().to_owned())
        .collect()
}

fn subject_cut(w: &World) -> bool {
    limits(w).contains(&"subject".to_owned())
}

/// A world flying with a subject standing `dist` m north, named and held
/// still until the arming delay is all but over: the next tick is armed.
fn armed_at(dist: f32) -> (World, u64, [f32; 2]) {
    let (mut w, id) = flying();
    let subject = [w.pos[0] + dist, w.pos[1]];
    fly_for(&mut w, id, ARM_MS - 20, |_, _| {
        follow(0.0, Some(subject), -5.0)
    });
    assert!(w.vel[0].abs() < 1e-3, "moved before armed");
    (w, id, subject)
}

/// A commanded 2 m/s straight at a standing subject 14 m ahead: within a
/// metre of the kept distance the setpoint's speed toward them stays within
/// what still stops outside it at the stick acceleration, and the vehicle
/// stops outside it. Before, the pilot flew the 2 m/s into them.
#[test]
fn a_command_straight_at_the_subject_stops_outside_the_kept_distance() {
    let (mut w, id, subject) = armed_at(14.0);
    let a_h = w.p.limits_now(w.now).a_h;
    let mut nearest = f32::INFINITY;
    let mut braked = false;
    let t0 = w.now;
    while w.now < t0 + 12_000 {
        if (w.now - t0).is_multiple_of(100) {
            send(&mut w, id, follow(2.0, Some(subject), -5.0));
        }
        let (d, closing) = (subject[0] - w.pos[0], w.vel[0]);
        w.run(20);
        let (v, _) = setpoint(&w);
        let cap = stop_cap(d, closing, a_h, KEEP);
        assert!(
            d >= KEEP + 1.0 || v[0] <= cap + 1e-3,
            "{d:.2} m off: {:.2} m/s toward, the stick stop allows {cap:.2}",
            v[0]
        );
        braked |= subject_cut(&w);
        nearest = nearest.min(subject[0] - w.pos[0]);
    }
    assert!(braked, "never braked for the subject");
    assert!(nearest >= KEEP, "came within {nearest:.2} m of the subject");
    assert!(w.vel[0].abs() < 0.05, "still flying {:.2} m/s", w.vel[0]);
}

/// The same approach is slowed by the soft tier on the goal profile: the
/// flown velocity keeps the goal acceleration and jerk all the way to the
/// stop, so the hard tier never steps it (a step would be a tilt).
#[test]
fn a_subject_approached_on_the_goal_profile_is_slowed_without_a_cut() {
    let (mut w, id, subject) = armed_at(14.0);
    let mut gaps = Vec::new();
    let v = fly_for(&mut w, id, 12_000, |w, _| {
        gaps.push(subject[0] - w.pos[0]);
        follow(2.0, Some(subject), -5.0)
    });
    let (a, j) = peak_acc_jerk(&v);
    let worst = v
        .windows(2)
        .enumerate()
        .max_by(|x, y| {
            let dv = |p: &[[f32; 2]]| (p[1][0] - p[0][0]).abs();
            dv(x.1).total_cmp(&dv(y.1))
        })
        .map(|(i, p)| (i, p[0][0], p[1][0], gaps[(i + 1) / 5]));
    assert!(
        a <= A_MAX + 0.05,
        "peak acceleration {a:.2} m/s² at {worst:?}"
    );
    assert!(j <= JERK + 0.1, "peak jerk {j:.2} m/s³");
    let gap = subject[0] - w.pos[0];
    assert!(gap >= KEEP, "stopped {gap:.2} m off");
    assert!(w.vel[0].abs() < 0.05, "still flying {:.2} m/s", w.vel[0]);
}

/// The subject 1 m away, inside the kept 1.3 m: whether the skill
/// commands nothing or 1 m/s toward them, the setpoint backs away at
/// 1 m/s per metre inside.
#[test]
fn inside_the_kept_distance_the_vehicle_backs_away() {
    for vn in [0.0, 1.0] {
        let (mut w, id, subject) = armed_at(1.0);
        send(&mut w, id, follow(vn, Some(subject), -5.0));
        w.run(20);
        let (v, _) = setpoint(&w);
        assert!(
            v[0] <= -(KEEP - 1.0) + 1e-3,
            "asked {vn}: setpoint {:.2} m/s toward the subject",
            v[0]
        );
        assert!(subject_cut(&w), "{:?}", limits(&w));
    }
}

/// For 3 s after a subject is first named the brake holds off: a subject
/// named 1 m away is not pushed from until then.
#[test]
fn the_brake_waits_three_seconds_after_the_subject_is_named() {
    let (mut w, id) = flying();
    let subject = [w.pos[0] + 1.0, w.pos[1]];
    let t0 = w.now;
    while w.now < t0 + ARM_MS - 100 {
        if (w.now - t0).is_multiple_of(100) {
            send(&mut w, id, follow(0.0, Some(subject), -5.0));
        }
        w.run(20);
        assert!(!subject_cut(&w), "braked {} ms after naming", w.now - t0);
        assert!(setpoint(&w).0[0].abs() < 1e-3, "moved");
    }
    fly_for(&mut w, id, 300, |_, _| follow(0.0, Some(subject), -5.0));
    assert!(setpoint(&w).0[0] < -0.1, "not backing away once armed");
    assert!(subject_cut(&w));
}

/// Once armed the brake stays armed through a slow goal stream (one goal
/// every 0.9 s, which the lease allows) and through goals that arrive
/// bunched together: only the subject's velocity restarts after a gap, and
/// a small step between bunched goals is no new subject.
#[test]
fn slow_or_bunched_goals_keep_the_brake_armed() {
    let (mut w, id, _) = armed_at(1.0);
    for _ in 0..4 {
        let near = [w.pos[0] + 1.0, w.pos[1]];
        send(&mut w, id, follow(0.0, Some(near), -5.0));
        w.run(20);
        assert!(subject_cut(&w), "disarmed after a goal gap");
        assert!(setpoint(&w).0[0] < -0.1, "not backing away after a gap");
        w.run(880);
    }
    for _ in 0..4 {
        let near = [w.pos[0] + 1.0, w.pos[1]];
        send(&mut w, id, follow(0.0, Some(near), -5.0));
        w.run(20);
        let nearer = [near[0] - 0.3, near[1]];
        send(&mut w, id, follow(0.0, Some(nearer), -5.0));
        w.run(20);
        assert!(subject_cut(&w), "disarmed by bunched goals");
        assert!(setpoint(&w).0[0] < -0.1, "not backing away");
        w.run(60);
    }
}

/// Backing away from a subject never flies into an obstacle behind: the
/// obstacle brake has the last word over the subject brake's cut. A
/// subject closing at 1 m/s pins the vehicle against a wall 3 m behind,
/// and the vehicle holds outside the obstacle margin rather than reversing
/// into it.
#[test]
fn the_back_away_stops_for_an_obstacle_behind() {
    let (mut w, id, _) = armed_at(4.0);
    w.p.settings.pilot.avoid = AvoidMode::Brake;
    let wall_n = w.pos[0] - 3.0;
    let start = w.pos[0] + 4.0;
    let mut nearest = f32::INFINITY;
    let t0 = w.now;
    while w.now < t0 + 4_500 {
        let range = w.pos[0] - wall_n;
        nearest = nearest.min(range);
        w.scan_ranges = wall(180, range);
        if (w.now - t0).is_multiple_of(100) {
            let s = start - 1.0 * (w.now - t0) as f32 / 1000.0;
            send(&mut w, id, follow(0.0, Some([s, 0.0]), -5.0));
        }
        w.run(20);
    }
    assert!(nearest >= 1.4, "backed within {nearest:.2} m of the wall");
}

/// A subject that steps faster than 8 m/s between goals (a re-designation
/// or a re-acquisition) is a new subject: no velocity is read from the
/// step, and the brake waits 3 s again. Read as motion, the 5 m step onto
/// the vehicle would have backed it away at metres per second.
#[test]
fn a_subject_jump_is_named_afresh() {
    let (mut w, id, _) = armed_at(6.0);
    let near = [w.pos[0] + 1.0, w.pos[1]];
    let t0 = w.now;
    while w.now < t0 + ARM_MS - 100 {
        if (w.now - t0).is_multiple_of(100) {
            send(&mut w, id, follow(0.0, Some(near), -5.0));
        }
        w.run(20);
        assert!(!subject_cut(&w), "braked {} ms after the jump", w.now - t0);
        assert!(setpoint(&w).0[0].abs() < 1e-3, "moved after the jump");
    }
    fly_for(&mut w, id, 120, |_, _| follow(0.0, Some(near), -5.0));
    let v = setpoint(&w).0[0];
    // Armed again: the push from 0.3 m inside, and no approach read.
    assert!(
        (-0.35..=-0.28).contains(&v),
        "backing away at {:.2} m/s",
        -v
    );
}

/// A subject walking at 1.5 m/s onto a vehicle commanded to stay: the
/// pilot takes the subject's velocity from the goals and backs away ahead
/// of them, at least as fast as they come once inside the kept distance,
/// and keeps pace with them.
#[test]
fn a_subject_coming_on_is_kept_clear_of() {
    const WALK: f32 = 1.5;
    let (mut w, id) = flying();
    let start = w.pos[0] + 9.0;
    let mut subject = [start, w.pos[1]];
    let mut nearest = f32::INFINITY;
    let mut retreat_from = None;
    let t0 = w.now;
    while w.now < t0 + 10_000 {
        let t = (w.now - t0) as f32 / 1000.0;
        if (w.now - t0).is_multiple_of(100) {
            subject = [start - WALK * t, w.pos[1]];
            send(&mut w, id, follow(0.0, Some(subject), -5.0));
        }
        let d = subject[0] - w.pos[0];
        w.run(20);
        nearest = nearest.min(d);
        let (v, _) = setpoint(&w);
        if t > 4.5 && d < KEEP {
            assert!(
                v[0] <= -WALK + 1e-2,
                "inside at {d:.2} m backing away at only {:.2} m/s",
                -v[0]
            );
        }
        if t >= 8.0 && retreat_from.is_none() {
            retreat_from = Some(w.pos[0]);
        }
    }
    // One goal step (0.15 m) inside at most.
    assert!(nearest >= KEEP - 0.2, "came within {nearest:.2} m");
    let backed = retreat_from.unwrap() - w.pos[0];
    assert!(backed >= 0.95 * WALK * 2.0, "backed {backed:.2} m in 2 s");
}

/// The goal's `subject_sigma_m` widens the kept distance by two sigmas, at
/// most 1.5 m: at 1 m sigma (kept 2.8 m) a subject 2.5 m off is inside it
/// (the vehicle backs away), as it is at a sigma that is not a number
/// (widened the most), and at none (kept 1.3 m) it is not (the vehicle
/// holds).
#[test]
fn the_subject_sigma_widens_the_kept_distance() {
    for (sigma, inside) in [(1.0, true), (f32::NAN, true), (0.0, false)] {
        let (mut w, id) = flying();
        let subject = [w.pos[0] + 2.5, w.pos[1]];
        let goal = move || {
            let mut g = follow(0.0, Some(subject), -5.0);
            g.subject_sigma_m = Some(sigma);
            g
        };
        fly_for(&mut w, id, ARM_MS - 20, |_, _| goal());
        assert!(setpoint(&w).0[0].abs() < 1e-3, "moved before armed");
        send(&mut w, id, goal());
        w.run(20);
        let (v, _) = setpoint(&w);
        let cut = subject_cut(&w);
        if inside {
            // Kept 1.3 + 1.5 m: 0.3 m inside.
            assert!(v[0] <= -0.3 + 1e-3, "sigma {sigma}: {:.2} m/s", v[0]);
            assert!(cut, "sigma {sigma}: {:?}", limits(&w));
        } else {
            assert!(v[0].abs() < 1e-3, "sigma {sigma}: {:.2} m/s", v[0]);
            assert!(!cut, "sigma {sigma}: {:?}", limits(&w));
        }
    }
}

/// Following at 1.5 m above ground: with the subject within the climb zone
/// (3 m beyond the kept distance) the setpoint climbs to 3.4 m, the
/// vehicle's extent 1 m over a 2 m person, and holds it until the subject
/// is a further 1 m off. A subject far off, or a goal that names none,
/// leaves the height asked. The climb does not wait for the brake to arm.
/// Each move of the subject comes after a gap in the goals, so it reads as
/// no velocity.
#[test]
fn near_the_subject_the_vehicle_climbs_clear() {
    let (mut w, id) = flying();
    w.pos[2] = -1.5;
    w.agl = Some(1.5);
    let place = |w: &mut World, dist: f32| {
        w.run(600);
        let s = [w.pos[0] + dist, w.pos[1]];
        fly_for(w, id, 300, |_, _| follow(0.0, Some(s), -1.5));
        setpoint(w).1
    };
    let d = place(&mut w, 10.0);
    assert!((d + 1.5).abs() < 0.01, "far: d {d:.2}");
    for (dist, climbing) in [
        (3.5, true),
        (4.8, true),
        (6.0, false),
        (4.8, false),
        (3.5, true),
    ] {
        let want = if climbing { -FLOOR_AGL } else { -1.5 };
        let d = place(&mut w, dist);
        assert!((d - want).abs() < 0.01, "subject {dist} m off: d {d:.2}");
    }
    fly_for(&mut w, id, 300, |_, _| follow(0.0, None, -1.5));
    let d = setpoint(&w).1;
    assert!((d + 1.5).abs() < 0.01, "no subject: d {d:.2}");
}

/// A goal that names no subject flies as before: its commanded velocity,
/// its height, and no subject limit.
#[test]
fn a_goal_without_a_subject_is_unchanged() {
    let (mut w, id) = flying();
    w.pos[2] = -1.5;
    w.agl = Some(1.5);
    fly_for(&mut w, id, 5_000, |_, _| follow(2.0, None, -1.5));
    let (v, d) = setpoint(&w);
    assert!((v[0] - 2.0).abs() < 0.02, "flying {:.2}", v[0]);
    assert!((d + 1.5).abs() < 0.01, "d {d:.2}");
    assert!(!subject_cut(&w));
}

/// Following a subject who walks away at 1.4 m/s, settling 3 m behind
/// them from 6 m: the brake judges only the closing between them, so the
/// catch-up flies on the goal profile with no cut, and the settled follow
/// is not braked at all.
#[test]
fn following_a_subject_walking_away_is_not_cut() {
    const WALK: f32 = 1.4;
    let (mut w, id) = flying();
    let start = w.pos[0] + 6.0;
    let mut braked_late = Vec::new();
    let mut v = Vec::new();
    let t0 = w.now;
    while w.now < t0 + 14_000 {
        if (w.now - t0).is_multiple_of(100) {
            let s = start + WALK * (w.now - t0) as f32 / 1000.0;
            let mut g = Goal::new(GoalKind::Follow, [s - 3.0, 0.0, -5.0], 6.0);
            g.vn = WALK;
            g.subject_ne = Some([s, 0.0]);
            send(&mut w, id, g);
        }
        w.run(20);
        v.push([w.vel[0], w.vel[1]]);
        if w.now - t0 > 8_000 && subject_cut(&w) {
            braked_late.push((w.now - t0) as f32 / 1000.0);
        }
    }
    let (a, j) = peak_acc_jerk(&v);
    assert!(a <= A_MAX + 0.05, "peak acceleration {a:.2} m/s²");
    assert!(j <= JERK + 0.1, "peak jerk {j:.2} m/s³");
    assert!(braked_late.is_empty(), "braked at {braked_late:?} s");
    let s = start + WALK * (w.now - t0) as f32 / 1000.0;
    let gap = s - w.pos[0];
    assert!((gap - 3.0).abs() < 0.3, "{gap:.2} m behind");
    assert!((w.vel[0] - WALK).abs() < 0.1, "flying {:.2}", w.vel[0]);
}

/// A rangefinder stuck low under a vehicle climbing clear of the subject:
/// the ground is taken when the climb begins, so the setpoint rises once to
/// the floor over it and stays there as the vehicle climbs, never a climb
/// that follows the vehicle up.
#[test]
fn a_stuck_low_rangefinder_raises_the_climb_floor_once() {
    let (mut w, id) = flying();
    w.pos[2] = -1.5;
    w.agl = Some(1.5);
    let s = [w.pos[0] + 3.5, w.pos[1]];
    for _ in 0..20 {
        fly_for(&mut w, id, 200, |_, _| follow(0.0, Some(s), -1.5));
        // The vehicle reaches the setpoint's height; the reading does not
        // move.
        w.pos[2] = setpoint(&w).1;
    }
    let d = setpoint(&w).1;
    assert!((d + FLOOR_AGL).abs() < 0.01, "setpoint d {d:.2}");
}

/// A stuck rangefinder that dithers between two readings under a vehicle
/// climbing clear with no horizontal motion: every reading change re-takes
/// the ground, but a re-take stays within a reading step of the first, so
/// the floor never ratchets up with the climb.
#[test]
fn a_dithering_stuck_rangefinder_holds_the_climb_floor() {
    let (mut w, id) = flying();
    w.pos[2] = -1.5;
    w.agl = Some(1.5);
    let s = [w.pos[0] + 3.5, w.pos[1]];
    for k in 0..20 {
        fly_for(&mut w, id, 200, |_, _| follow(0.0, Some(s), -1.5));
        w.pos[2] = setpoint(&w).1;
        w.agl = Some(if k % 2 == 0 { 1.6 } else { 1.5 });
    }
    let d = setpoint(&w).1;
    assert!((d + FLOOR_AGL).abs() < 0.1, "setpoint d {d:.2}");
}

/// Goals that arrive bunched by transport jitter (5 ms, then 95 ms apart),
/// the tracker's 20 Hz step of a subject walking at 1.5 m/s between them,
/// keep the brake armed: the step is judged over at least a goal period.
#[test]
fn jittered_goal_arrivals_keep_the_brake_armed() {
    let (mut w, id, subject) = armed_at(1.0);
    // Past the arming delay before the jitter starts.
    w.run(40);
    let tick = |w: &mut World, ms: u64| {
        // Each tick's own limits, not the frame's latch.
        w.p.state_published();
        w.now += ms;
        w.feed();
        w.p.tick(w.now);
        w.collect();
    };
    let mut s = subject;
    for i in 0..20 {
        s[0] -= 0.075;
        send(&mut w, id, follow(0.0, Some(s), -5.0));
        tick(&mut w, if i % 2 == 0 { 1 } else { 19 });
        assert!(subject_cut(&w), "disarmed at goal {i}");
        w.now += if i % 2 == 0 { 4 } else { 76 };
    }
}

/// Under the GNSS-denied speed cap a subject walking onto the vehicle at
/// 4 m/s is backed away from at no more than the cap: the back-away is
/// held to the goal's speed limit like any other velocity.
#[test]
fn the_back_away_keeps_the_nav_cap() {
    const CAP: f32 = 2.0;
    let mut w = World::new();
    w.p.fc.params.insert("EK3_SRC1_POSXY".into(), 3.0);
    w.p.fc.params.insert("EK3_SRC2_POSXY".into(), 6.0);
    w.p.settings.pilot.avoid = AvoidMode::Off;
    w.airborne(crate::modes::copter::GUIDED);
    w.fly = true;
    let ext_nav = |w: &mut World| {
        w.p.on_feed(
            crate::events::TOPIC_NAV_STATE,
            &serde_json::json!({"active_set": 2, "gps_fix": 0}),
            w.now,
        );
    };
    ext_nav(&mut w);
    let id = w.lease();
    let start = w.pos[0] + 12.0;
    let mut fastest: f32 = 0.0;
    let mut braked = false;
    let t0 = w.now;
    while w.now < t0 + 6_000 {
        if (w.now - t0).is_multiple_of(500) {
            ext_nav(&mut w);
        }
        if (w.now - t0).is_multiple_of(100) {
            let s = start - 4.0 * (w.now - t0) as f32 / 1000.0;
            send(&mut w, id, follow(0.0, Some([s, 0.0]), -5.0));
        }
        w.run(20);
        fastest = fastest.max(setpoint(&w).0[0].hypot(setpoint(&w).0[1]));
        braked |= subject_cut(&w);
    }
    assert!(braked, "never braked");
    assert!(fastest <= CAP + 1e-3, "flew {fastest:.2} m/s under the cap");
}

/// A goal whose `subject_ne` is not a number keeps the last subject: the
/// brake stays armed and its velocity unpoisoned, so the 2 m/s approach
/// that follows is still held to the stick stop and stops outside the kept
/// distance. A sigma that is not a number widens it the most (above).
#[test]
fn a_subject_that_is_not_a_number_keeps_the_brake() {
    let (mut w, id, subject) = armed_at(8.0);
    w.run(40);
    send(&mut w, id, follow(0.0, Some([f32::NAN, f32::NAN]), -5.0));
    w.run(100);
    let a_h = w.p.limits_now(w.now).a_h;
    let mut nearest = f32::INFINITY;
    let mut braked = false;
    let t0 = w.now;
    while w.now < t0 + 10_000 {
        if (w.now - t0).is_multiple_of(100) {
            send(&mut w, id, follow(2.0, Some(subject), -5.0));
        }
        let (d, closing) = (subject[0] - w.pos[0], w.vel[0]);
        w.run(20);
        let (v, z) = setpoint(&w);
        assert!(v[0].is_finite() && z.is_finite(), "setpoint {v:?} {z}");
        let cap = stop_cap(d, closing, a_h, KEEP);
        assert!(
            d >= KEEP + 1.0 || v[0] <= cap + 1e-3,
            "{d:.2} m off: {:.2} m/s toward, the stick stop allows {cap:.2}",
            v[0]
        );
        braked |= subject_cut(&w);
        nearest = nearest.min(subject[0] - w.pos[0]);
    }
    assert!(braked, "never braked for the subject");
    assert!(nearest >= KEEP, "came within {nearest:.2} m of the subject");
}

/// Following a subject 3 m ahead, settled on flat ground, then up a 10 %
/// slope for 50 m, inside the climb zone the whole way: the ground is
/// taken again as the reading changes, so the climb floor stays 3.4 m over
/// the local ground rather than over the ground where the climb began (5 m
/// lower by the end).
#[test]
fn the_climb_floor_follows_rising_ground() {
    const WALK: f32 = 1.4;
    const GRADE: f32 = 0.1;
    const SETTLE_MS: u64 = 12_000;
    let (mut w, id) = flying();
    w.pos[2] = -1.5;
    w.agl = Some(1.5);
    let start = w.pos[0] + 3.0;
    // Where the slope starts, once the follow has settled.
    let mut foot: Option<f32> = None;
    // The ground's down position under north `n`.
    let ground = |foot: Option<f32>, n: f32| foot.map_or(0.0, |f| -GRADE * (n - f).max(0.0));
    let t0 = w.now;
    while w.now < t0 + SETTLE_MS + 37_000 {
        if (w.now - t0).is_multiple_of(100) {
            let s = start + WALK * (w.now - t0) as f32 / 1000.0;
            let mut g = Goal::new(GoalKind::Follow, [s - 3.0, 0.0, -1.5], 6.0);
            g.vn = WALK;
            g.subject_ne = Some([s, 0.0]);
            send(&mut w, id, g);
        }
        w.run(20);
        // The vehicle holds the setpoint's height; the rangefinder reads
        // the ground under it.
        w.pos[2] = setpoint(&w).1;
        if w.now - t0 >= SETTLE_MS && foot.is_none() {
            foot = Some(w.pos[0]);
        }
        w.agl = Some(ground(foot, w.pos[0]) - w.pos[2]);
        if let Some(f) = foot {
            let floor = ground(foot, w.pos[0]) - FLOOR_AGL;
            let d = w.pos[2];
            assert!(
                (d - floor).abs() < 0.08,
                "{:.1} m up the slope: d {d:.2}, the floor {floor:.2}",
                w.pos[0] - f
            );
        }
    }
    let up = w.pos[0] - foot.unwrap();
    assert!(up > 50.0, "followed {up:.1} m up the slope");
}

/// The estimate steps 1.2 m up (a height-source switch) while the vehicle
/// holds the climb floor over the subject: the ground taken under them moves
/// with it, so every setpoint keeps its true 3.4 m over the ground, and the
/// skill flies on.
#[test]
fn a_height_reset_keeps_the_floor_over_the_subject() {
    let (mut w, id) = flying();
    w.pos[2] = -1.5;
    w.agl = Some(1.5);
    let s = [w.pos[0] + 3.5, w.pos[1]];
    for _ in 0..10 {
        fly_for(&mut w, id, 200, |_, _| follow(0.0, Some(s), -1.5));
        w.pos[2] = setpoint(&w).1;
        w.agl = Some(-w.pos[2]);
    }
    assert!((setpoint(&w).1 + FLOOR_AGL).abs() < 0.05, "at the floor");
    w.fc_clock = true;
    fly_for(&mut w, id, 200, |_, _| follow(0.0, Some(s), -1.5));
    // The vehicle and its rangefinder stay; the estimate reads 1.2 m higher.
    w.pos[2] -= 1.2;
    for _ in 0..10 {
        fly_for(&mut w, id, 200, |_, _| follow(0.0, Some(s), -1.5));
        let over_ground = -(setpoint(&w).1 + 1.2);
        assert!(
            over_ground >= FLOOR_AGL - 0.05,
            "the setpoint is {over_ground:.2} m over the ground"
        );
    }
    assert_eq!(w.p.owner(), ados_protocol::pilot::Owner::Skill);
}
