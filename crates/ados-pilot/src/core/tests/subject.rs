//! The subject brake: the subject a skill's goal names (`subject_ne`) is a
//! person the vehicle keeps clear of at the profile's stick acceleration,
//! whatever the skill commands, and climbs clear over while near. With
//! avoidance off: the brake needs no obstacle sensor.

use ados_protocol::pilot::{Goal, GoalKind, Request, VelMode};

use super::harness::World;
use super::shaping::{fly_for, flying};
use crate::core::control::stop_cap;

/// The distance kept from the subject with no sigma, centre to CG: its
/// body's 0.4 m, the vehicle's default 0.4 m and the 0.5 m margin.
const KEEP: f32 = 1.3;
/// The climb floor above ground: the 2 m body, the vehicle's 0.4 m and the
/// 1 m clearance over it.
const FLOOR_AGL: f32 = 3.4;

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

/// Flying a commanded 2 m/s north, the goal names its subject 3 m ahead.
/// The setpoint's speed toward it is cut at once to what still stops
/// outside the kept distance at the stick acceleration, and the vehicle
/// never comes inside it. Before, the pilot flew the 2 m/s into them.
#[test]
fn a_command_straight_at_the_subject_is_held_to_the_stop_cap() {
    let (mut w, id) = flying();
    fly_for(&mut w, id, 5_000, |_, _| follow(2.0, None, -5.0));
    assert!((w.vel[0] - 2.0).abs() < 0.02, "cruising {:.2}", w.vel[0]);
    let subject = [w.pos[0] + 3.0, w.pos[1]];
    let a_h = w.p.limits_now(w.now).a_h;
    let cap = stop_cap(3.0, w.vel[0], a_h, KEEP);
    send(&mut w, id, follow(2.0, Some(subject), -5.0));
    w.run(20);
    let (v, _) = setpoint(&w);
    assert!(
        v[0] <= cap + 1e-3,
        "toward the subject {:.2} > cap {cap:.2}",
        v[0]
    );
    assert!(v[0] < 1.9, "toward the subject {:.2}", v[0]);
    assert!(
        limits(&w).contains(&"subject".to_owned()),
        "{:?}",
        limits(&w)
    );
    let mut nearest = f32::INFINITY;
    let t0 = w.now;
    while w.now < t0 + 5_000 {
        if (w.now - t0).is_multiple_of(100) {
            send(&mut w, id, follow(2.0, Some(subject), -5.0));
        }
        w.run(20);
        nearest = nearest.min((subject[0] - w.pos[0]).hypot(subject[1] - w.pos[1]));
    }
    assert!(nearest >= KEEP, "came within {nearest:.2} m of the subject");
}

/// The subject 1 m away, inside the kept 1.3 m: whether the skill
/// commands nothing or 1 m/s toward them, the setpoint backs away at
/// 1 m/s per metre inside.
#[test]
fn inside_the_kept_distance_the_vehicle_backs_away() {
    for vn in [0.0, 1.0] {
        let (mut w, id) = flying();
        let subject = [w.pos[0] + 1.0, w.pos[1]];
        send(&mut w, id, follow(vn, Some(subject), -5.0));
        w.run(20);
        let (v, _) = setpoint(&w);
        assert!(
            v[0] <= -(KEEP - 1.0) + 1e-3,
            "asked {vn}: setpoint {:.2} m/s toward the subject",
            v[0]
        );
        assert!(
            limits(&w).contains(&"subject".to_owned()),
            "{:?}",
            limits(&w)
        );
    }
}

/// A subject walking at 1.5 m/s onto a vehicle commanded to stay: the
/// pilot takes the subject's velocity from the goals and backs away ahead
/// of them, at least as fast as they come once inside the kept distance,
/// and keeps pace with them.
#[test]
fn a_subject_coming_on_is_kept_clear_of() {
    const WALK: f32 = 1.5;
    let (mut w, id) = flying();
    let start = w.pos[0] + 6.0;
    let mut subject = [start, w.pos[1]];
    let mut nearest = f32::INFINITY;
    let mut retreat_from = None;
    let t0 = w.now;
    while w.now < t0 + 8_000 {
        let t = (w.now - t0) as f32 / 1000.0;
        if (w.now - t0).is_multiple_of(100) {
            subject = [start - WALK * t, w.pos[1]];
            send(&mut w, id, follow(0.0, Some(subject), -5.0));
        }
        let d = subject[0] - w.pos[0];
        w.run(20);
        nearest = nearest.min(d);
        let (v, _) = setpoint(&w);
        if t > 3.0 && d < KEEP {
            assert!(
                v[0] <= -WALK + 1e-2,
                "inside at {d:.2} m backing away at only {:.2} m/s",
                -v[0]
            );
        }
        if t >= 6.0 && retreat_from.is_none() {
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
/// (the vehicle backs away), at none (kept 1.3 m) it is not (the vehicle
/// holds).
#[test]
fn the_subject_sigma_widens_the_kept_distance() {
    for (sigma, inside) in [(1.0, true), (0.0, false)] {
        let (mut w, id) = flying();
        let subject = [w.pos[0] + 2.5, w.pos[1]];
        let mut g = follow(0.0, Some(subject), -5.0);
        g.subject_sigma_m = Some(sigma);
        send(&mut w, id, g);
        w.run(20);
        let (v, _) = setpoint(&w);
        let cut = limits(&w).contains(&"subject".to_owned());
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
/// leaves the height asked. Each move of the subject comes after a gap in
/// the goals, so it reads as no velocity.
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
    assert!(!limits(&w).contains(&"subject".to_owned()));
}

/// Following a subject who walks away at 1.4 m/s, settling 3 m behind
/// them from 6 m: the vehicle closes on the subject's own pace plus its
/// catch-up, and the brake judges only that relative closing, so it never
/// cuts. A brake on the vehicle's own speed alone cut each catch-up.
#[test]
fn following_a_subject_walking_away_is_not_cut() {
    const WALK: f32 = 1.4;
    let (mut w, id) = flying();
    let start = w.pos[0] + 6.0;
    let mut cut = Vec::new();
    let t0 = w.now;
    while w.now < t0 + 12_000 {
        if (w.now - t0).is_multiple_of(100) {
            let s = start + WALK * (w.now - t0) as f32 / 1000.0;
            let mut g = Goal::new(GoalKind::Follow, [s - 3.0, 0.0, -5.0], 6.0);
            g.vn = WALK;
            g.subject_ne = Some([s, 0.0]);
            send(&mut w, id, g);
        }
        w.run(20);
        if limits(&w).contains(&"subject".to_owned()) {
            cut.push((w.now - t0) as f32 / 1000.0);
        }
    }
    assert!(cut.is_empty(), "cut at {cut:?} s");
    let s = start + WALK * (w.now - t0) as f32 / 1000.0;
    let gap = s - w.pos[0];
    assert!((gap - 3.0).abs() < 0.3, "{gap:.2} m behind");
    assert!((w.vel[0] - WALK).abs() < 0.1, "flying {:.2}", w.vel[0]);
}
