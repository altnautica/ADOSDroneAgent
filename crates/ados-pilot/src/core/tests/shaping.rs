//! Goal setpoints are shaped: a skill goal or a goto flies with bounded
//! acceleration and jerk, slows into a hold on the profile, and does not
//! chase a noisy goal at its update rate. The vehicle flies each setpoint as
//! a stiff FC would ([`World::fly`]), so its velocity is what was commanded.

use ados_protocol::pilot::{AvoidMode, Goal, GoalKind, Request, VelMode};

use super::harness::World;
use crate::modes::copter;

pub(super) const DT: f32 = 0.02;
/// The Normal profile's goal limits (`crate::model::goal_limits`).
pub(super) const A_MAX: f32 = 1.0;
pub(super) const JERK: f32 = 2.0;

/// An airborne copter in GUIDED flying its setpoints, with avoidance off,
/// and a motion lease.
pub(super) fn flying() -> (World, u64) {
    flying_at([0.0, 0.0])
}

/// As [`flying`], the vehicle at `ne` before the lease starts its reference.
pub(super) fn flying_at(ne: [f32; 2]) -> (World, u64) {
    let mut w = World::new();
    w.p.settings.pilot.avoid = AvoidMode::Off;
    w.airborne(copter::GUIDED);
    w.pos = [ne[0], ne[1], -5.0];
    w.fly = true;
    let id = w.lease();
    (w, id)
}

/// Fly for `ms`, the skill sending `goal(world, seconds)` every 100 ms;
/// the vehicle's horizontal velocity after every 20 ms tick.
pub(super) fn fly_for(
    w: &mut World,
    id: u64,
    ms: u64,
    mut goal: impl FnMut(&World, f32) -> Goal,
) -> Vec<[f32; 2]> {
    let t0 = w.now;
    let mut out = Vec::new();
    while w.now < t0 + ms {
        if (w.now - t0).is_multiple_of(100) {
            let g = goal(w, (w.now - t0) as f32 / 1000.0);
            w.req(Request::Goal {
                lease_id: id,
                goal: g,
            });
        }
        w.run(20);
        out.push([w.vel[0], w.vel[1]]);
    }
    out
}

/// The largest acceleration and jerk of a velocity series at 20 ms.
pub(super) fn peak_acc_jerk(v: &[[f32; 2]]) -> (f32, f32) {
    let acc: Vec<[f32; 2]> = v
        .windows(2)
        .map(|p| [(p[1][0] - p[0][0]) / DT, (p[1][1] - p[0][1]) / DT])
        .collect();
    let a = acc.iter().map(|a| a[0].hypot(a[1])).fold(0.0, f32::max);
    let j = acc
        .windows(2)
        .map(|p| ((p[1][0] - p[0][0]) / DT).hypot((p[1][1] - p[0][1]) / DT))
        .fold(0.0, f32::max);
    (a, j)
}

fn follow(n: f32, vn: f32) -> Goal {
    let mut g = Goal::new(GoalKind::Follow, [n, 0.0, -5.0], 6.0);
    g.vn = vn;
    g
}

/// A goal 10 m away appears at once. The vehicle flies there within the
/// goal acceleration and jerk, without overshooting it. Before, the
/// setpoint jumped onto the goal and the FC's position loop asked 10 m/s at
/// once.
#[test]
fn a_step_goal_flies_within_the_goal_acceleration_and_jerk() {
    let (mut w, id) = flying();
    let v = fly_for(&mut w, id, 15_000, |_, _| {
        Goal::new(GoalKind::Position, [10.0, 0.0, -5.0], 5.0)
    });
    let (a, j) = peak_acc_jerk(&v);
    assert!(a <= A_MAX + 0.05, "peak acceleration {a:.2} m/s²");
    assert!(j <= JERK + 0.1, "peak jerk {j:.2} m/s³");
    assert!((w.pos[0] - 10.0).abs() < 0.1, "ended at {:.2} m", w.pos[0]);
    assert!(w.vel[0].abs() < 0.05, "still flying {:.2} m/s", w.vel[0]);
}

/// A follow at 2 m/s turns into the skill's "stop here" (a position goal on
/// the vehicle with no velocity, every 100 ms). The vehicle slows on the
/// profile and stops: the commanded speed never falls by more than the goal
/// acceleration allows in a tick. Before, the first hold tick commanded a
/// stop from 2 m/s at once.
#[test]
fn a_hold_after_motion_slows_on_the_profile_never_to_zero_at_once() {
    let (mut w, id) = flying();
    let run = fly_for(&mut w, id, 12_000, |_, t| follow(5.0 + 2.0 * t, 2.0));
    let cruise = w.vel[0];
    let every_half_s: Vec<String> = run
        .iter()
        .step_by(25)
        .map(|v| format!("{:.2}", v[0]))
        .collect();
    assert!(
        (cruise - 2.0).abs() < 0.1,
        "cruising at {cruise:.2} m/s, every 0.5 s: {every_half_s:?}, {:.2} m behind the goal",
        5.0 + 2.0 * 11.9 - w.pos[0]
    );
    let mut v = vec![[w.vel[0], w.vel[1]]];
    v.extend(fly_for(&mut w, id, 8_000, |w, _| {
        Goal::new(GoalKind::Position, [w.pos[0], w.pos[1], -5.0], 6.0)
    }));
    let worst = v
        .windows(2)
        .map(|p| (p[0][0] - p[1][0]).hypot(p[0][1] - p[1][1]) / DT)
        .fold(0.0, f32::max);
    assert!(worst <= A_MAX + 0.05, "slowed at up to {worst:.2} m/s²");
    let (_, j) = peak_acc_jerk(&v);
    assert!(j <= JERK + 0.1, "peak jerk {j:.2} m/s³");
    assert!(w.vel[0].abs() < 0.05, "still flying {:.2} m/s", w.vel[0]);
}

/// A standing subject's goal flickers ±0.7 m with a velocity estimate of
/// ±1 m/s, reversing every 100 ms (the tracker noise of the forest takes).
/// The command does not reverse with it: the vehicle stays nearly still.
/// Before, the commanded velocity reversed at the goal's rate.
#[test]
fn a_reversing_noisy_goal_does_not_reverse_the_command_at_its_rate() {
    noisy_goal_stays_still(VelMode::FeedForward);
}

/// The same noise on a commanded velocity (`vel_mode` command, how a follow
/// sends its goals): the shaper tracks the noise's mean, not an offset its
/// acceleration cycles about. Before the ask was smoothed, the shaper's
/// acceleration cycled about whichever offset the first reversal left, and
/// the vehicle drifted at half the noise's speed.
#[test]
fn a_reversing_commanded_velocity_does_not_drift() {
    noisy_goal_stays_still(VelMode::Command);
}

fn noisy_goal_stays_still(mode: VelMode) {
    let (mut w, id) = flying_at([10.0, 0.0]);
    let mut k = 0u32;
    let v = fly_for(&mut w, id, 10_000, |_, _| {
        k += 1;
        let s = if k.is_multiple_of(2) { 1.0 } else { -1.0 };
        let mut g = follow(10.0 + 0.7 * s, s);
        g.vel_mode = mode;
        g
    });
    let mut sign = 0.0f32;
    let mut flips = 0;
    for x in v.iter().map(|v| v[0]) {
        if x.abs() > 0.1 {
            if sign != 0.0 && x.signum() != sign {
                flips += 1;
            }
            sign = x.signum();
        }
    }
    assert!(flips <= 2, "{mode:?}: {flips} command reversals in 10 s");
    let fastest = v.iter().map(|v| v[0].hypot(v[1])).fold(0.0, f32::max);
    assert!(fastest < 0.1, "{mode:?}: moved at up to {fastest:.2} m/s");
    let (_, j) = peak_acc_jerk(&v);
    assert!(j <= JERK + 0.1, "{mode:?}: peak jerk {j:.2} m/s³");
    assert!(
        (w.pos[0] - 10.0).abs() < 0.5,
        "{mode:?}: drifted to {:.2} m",
        w.pos[0]
    );
}

/// Every control tick sends one setpoint, and its position moves no more
/// than the velocity it carries: the FC gets a continuous path at 50 Hz.
#[test]
fn a_moving_goal_gets_a_continuous_setpoint_every_tick() {
    let (mut w, id) = flying();
    let start = w.sent.len();
    fly_for(&mut w, id, 6_000, |_, t| follow(5.0 + 2.0 * t, 2.0));
    let sps: Vec<_> = w.sent[start..]
        .iter()
        .filter_map(|m| match m {
            ados_protocol::mavlink::ardupilotmega::MavMessage::SET_POSITION_TARGET_LOCAL_NED(s) => {
                Some(s.clone())
            }
            _ => None,
        })
        .collect();
    assert_eq!(sps.len(), 300, "setpoints in 6 s");
    for p in sps.windows(2) {
        let step = (p[1].x - p[0].x).hypot(p[1].y - p[0].y);
        let flown = p[1].vx.hypot(p[1].vy) * DT;
        assert!(
            step <= flown + 1e-3,
            "setpoint moved {step:.3} m flying {flown:.3} m"
        );
    }
}

/// A goal heading 90° from the vehicle's turns at the goal limits: at most
/// 30°/s, its rate changing at most 60°/s².
#[test]
fn a_heading_step_turns_at_the_goal_rate() {
    let (mut w, id) = flying();
    let mut yaws = Vec::new();
    let t0 = w.now;
    while w.now < t0 + 6_000 {
        if (w.now - t0).is_multiple_of(100) {
            let mut g = Goal::new(GoalKind::Position, [0.0, 0.0, -5.0], 5.0);
            g.yaw = Some(std::f32::consts::FRAC_PI_2);
            w.req(Request::Goal {
                lease_id: id,
                goal: g,
            });
        }
        w.run(20);
        yaws.push(w.last_setpoint().unwrap().yaw);
    }
    let rates: Vec<f32> = yaws.windows(2).map(|p| (p[1] - p[0]) / DT).collect();
    let rate = rates.iter().map(|r| r.abs()).fold(0.0, f32::max);
    let acc = rates
        .windows(2)
        .map(|p| ((p[1] - p[0]) / DT).abs())
        .fold(0.0, f32::max);
    assert!(
        rate <= 30f32.to_radians() + 1e-3,
        "{:.1}°/s",
        rate.to_degrees()
    );
    assert!(
        acc <= 60f32.to_radians() + 1e-2,
        "{:.1}°/s²",
        acc.to_degrees()
    );
    let last = *yaws.last().unwrap();
    assert!(
        (last - std::f32::consts::FRAC_PI_2).abs() < 0.01,
        "ended at {last}"
    );
}

/// A goal 30 m out over a scan with no return, flown with avoidance on:
/// the soft brake holds what the goal asks to what the vehicle can stop
/// from inside the space it has seen, so the shaped velocity never breaks
/// the safety brake's cap and only the soft brake slows the goal. Every
/// frame whose command names the seen-space limit reports the goal slowed
/// (`detour`), never `clear`. Before, the soft brake left the avoid state
/// clear while it held the vehicle back.
#[test]
fn a_goal_slowed_by_the_soft_brake_reports_it() {
    let (mut w, id) = flying();
    w.p.settings.pilot.avoid = AvoidMode::Brake;
    let empty = serde_json::json!({"frame": "body_frd", "angle_start_rad": 0.0,
        "angle_increment_rad": std::f32::consts::TAU / 360.0, "ranges_m": vec![0.0f32; 360]});
    let mut braked = 0;
    let t0 = w.now;
    while w.now < t0 + 8_000 {
        if (w.now - t0).is_multiple_of(100) {
            w.p.on_feed(crate::events::TOPIC_SCAN, &empty, w.now);
            w.req(Request::Goal {
                lease_id: id,
                goal: follow(30.0, 0.0),
            });
        }
        w.run(20);
        let s = serde_json::to_value(w.p.state(w.now)).unwrap();
        let seen = s["command"]["limits"]
            .as_array()
            .is_some_and(|l| l.iter().any(|l| l == "seen_space"));
        if seen {
            braked += 1;
            assert_eq!(
                s["avoid_state"],
                "detour",
                "t {} ms, v {:.2}: {}",
                w.now - t0,
                w.vel[0],
                s["command"]
            );
        }
    }
    assert!(braked > 50, "the brake slowed {braked} frames");
    assert!(w.vel[0] > 1.0, "the goal is flown: {:.2} m/s", w.vel[0]);
}
