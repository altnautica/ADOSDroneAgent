//! Stick feel in command space: the operator's sticks fly one shaped
//! reference. A step of any stick reaches the profile's speed within its
//! acceleration and jerk, a release brakes to a hold at the brake limits and
//! holds where the reference stopped, the vertical speed and the yaw rate
//! never step, and stick frames that arrive late, out of order or from
//! another surface never fly. The scripted vehicle flies each setpoint as a
//! stiff FC would, turning and climbing with it.

use ados_protocol::mavlink::ardupilotmega::MavMessage;
use ados_protocol::pilot::{AvoidMode, Goal, GoalKind, Input, Profile, Request};

use super::harness::{wall, World};
use crate::modes::copter;

const DT: f32 = 0.02;
/// The Normal profile's stick limits.
const V_H: f32 = 7.0;
const V_UP: f32 = 2.5;
const V_DOWN: f32 = 1.5;
const YAW_DPS: f32 = 90.0;
const A_ACC: f32 = 2.0;
const A_BRK: f32 = 2.5;
const JERK: f32 = 5.0;
const A_V: f32 = 1.0;
const YAW_DPS2: f32 = 180.0;
/// `SET_POSITION_TARGET_LOCAL_NED` ignores the yaw (flies the yaw rate).
const YAW_IGNORE: u16 = 1024;
/// It ignores the position.
const POS_IGNORE: u16 = 0b111;

const FWD: [f32; 4] = [0.0, 1.0, 0.0, 0.0];
const BACK: [f32; 4] = [0.0, -1.0, 0.0, 0.0];
const CENTRED: [f32; 4] = [0.0; 4];

/// One tick's setpoint.
#[derive(Debug, Clone, Copy)]
struct Cmd {
    pos: [f32; 3],
    vel: [f32; 3],
    /// The heading the vehicle turns to (the setpoint's, or its own turned
    /// at the setpoint's rate), radians.
    yaw: f32,
    /// The yaw rate commanded this tick, rad/s.
    yaw_rate: f32,
}

impl Cmd {
    fn speed(&self) -> f32 {
        self.vel[0].hypot(self.vel[1])
    }
}

fn wrap(a: f32) -> f32 {
    crate::geo::wrap_pi(a)
}

/// An airborne copter in GUIDED, flying its setpoints, with the operator's
/// sticks past the handover gate and avoidance off.
fn flying() -> World {
    let mut w = World::new();
    w.p.settings.pilot.avoid = AvoidMode::Off;
    w.airborne(copter::GUIDED);
    w.action("take_control");
    w.fly = true;
    w.stick(CENTRED, 600);
    w
}

/// The kinematic stop distance from `v` at the brake limits, with 0.15 s
/// of latency.
fn stop_distance(v: f32, a: f32, j: f32) -> f32 {
    v * v / (2.0 * a) + v * a / (2.0 * j) + v * 0.15
}

/// Fly `ms` of 20 ms ticks, sending `axes` from the operator's surface
/// before each (`None`: no frame). The vehicle follows each setpoint in
/// height and heading too. The setpoint of each tick.
fn ticks(w: &mut World, axes: Option<[f32; 4]>, ms: u64) -> Vec<Cmd> {
    let mut out = Vec::new();
    let end = w.now + ms;
    while w.now < end {
        if let Some(a) = axes {
            w.input(a);
        }
        let from = w.sent.len();
        let yaw0 = w.yaw;
        w.run(20);
        let Some(sp) = w.sent[from..].iter().rev().find_map(|m| match m {
            MavMessage::SET_POSITION_TARGET_LOCAL_NED(s) => Some(s.clone()),
            _ => None,
        }) else {
            continue;
        };
        let mask = sp.type_mask.bits();
        let (yaw, yaw_rate) = if mask & YAW_IGNORE != 0 {
            (wrap(yaw0 + sp.yaw_rate * DT), sp.yaw_rate)
        } else {
            (sp.yaw, wrap(sp.yaw - yaw0) / DT)
        };
        w.yaw = yaw;
        if mask & POS_IGNORE == 0 {
            w.pos[2] = sp.z;
        } else {
            w.pos[2] += sp.vz * DT;
        }
        w.vel[2] = sp.vz;
        w.agl = Some(-w.pos[2]);
        out.push(Cmd {
            pos: [sp.x, sp.y, sp.z],
            vel: [sp.vx, sp.vy, sp.vz],
            yaw,
            yaw_rate,
        });
    }
    out
}

/// The largest horizontal acceleration while speeding up, deceleration
/// while slowing, and jerk of a series of commanded velocities.
fn horizontal_peaks(c: &[Cmd]) -> (f32, f32, f32) {
    let acc: Vec<[f32; 2]> = c
        .windows(2)
        .map(|p| {
            [
                (p[1].vel[0] - p[0].vel[0]) / DT,
                (p[1].vel[1] - p[0].vel[1]) / DT,
            ]
        })
        .collect();
    let (mut up, mut down) = (0.0f32, 0.0f32);
    for (p, a) in c.windows(2).zip(&acc) {
        let a_mag = a[0].hypot(a[1]);
        if p[1].speed() >= p[0].speed() {
            up = up.max(a_mag);
        } else {
            down = down.max(a_mag);
        }
    }
    let jerk = acc
        .windows(2)
        .map(|p| ((p[1][0] - p[0][0]) / DT).hypot((p[1][1] - p[0][1]) / DT))
        .fold(0.0, f32::max);
    (up, down, jerk)
}

/// The largest change of `f` between two ticks.
fn max_step(c: &[Cmd], f: impl Fn(&Cmd) -> f32) -> f32 {
    c.windows(2)
        .map(|p| (f(&p[1]) - f(&p[0])).abs())
        .fold(0.0, f32::max)
}

fn frame(src: &str, session: u32, seq: u32, axes: [f32; 4], t_ms: u64) -> Request {
    Request::Input(Input {
        src: src.into(),
        session,
        seq,
        t_ms,
        axes,
        buttons: 0,
    })
}

/// Full stick on each axis flies the profile's horizontal, climb, descent
/// and yaw limits. Before, Normal flew 8 m/s, 3 m/s up and 2 m/s down.
#[test]
fn full_stick_flies_the_profile_limits() {
    let mut w = flying();
    let c = ticks(&mut w, Some(FWD), 6000);
    let v = c.last().unwrap().speed();
    assert!((v - V_H).abs() <= 0.05 * V_H, "horizontal {v}");
    ticks(&mut w, Some(CENTRED), 6000);
    let c = ticks(&mut w, Some([0.0, 0.0, 1.0, 0.0]), 5000);
    let up = -c.last().unwrap().vel[2];
    assert!((up - V_UP).abs() <= 0.05 * V_UP, "climb {up}");
    ticks(&mut w, Some(CENTRED), 5000);
    let c = ticks(&mut w, Some([0.0, 0.0, -1.0, 0.0]), 4000);
    let down = c.last().unwrap().vel[2];
    assert!((down - V_DOWN).abs() <= 0.05 * V_DOWN, "descent {down}");
    ticks(&mut w, Some(CENTRED), 4000);
    let c = ticks(&mut w, Some([0.0, 0.0, 0.0, 1.0]), 3000);
    let yaw = c.last().unwrap().yaw_rate.to_degrees();
    assert!((yaw - YAW_DPS).abs() <= 0.05 * YAW_DPS, "yaw {yaw}");
}

/// A full-stick step from a hover and its release: the commanded velocity
/// speeds up within the accelerate limit, slows within the brake limit,
/// and never changes its acceleration faster than 1.5 times the jerk limit,
/// at the end of a ramp included. It reaches 90 % of the speed within
/// `v/a + a/j + 0.3 s`. Before, the shaper snapped its acceleration to zero
/// on the tick it reached the speed (a jerk of a/dt) and accelerated at
/// 3 m/s².
#[test]
fn a_stick_step_and_release_hold_the_acceleration_and_jerk_limits() {
    let mut w = flying();
    let mut c = ticks(&mut w, Some(FWD), 6000);
    let rise = c
        .iter()
        .position(|c| c.speed() >= 0.9 * V_H)
        .expect("reaches 90 %") as f32
        * DT;
    assert!(
        rise <= V_H / A_ACC + A_ACC / JERK + 0.3,
        "rise to 90 % took {rise} s"
    );
    c.extend(ticks(&mut w, Some(CENTRED), 8000));
    let (up, down, jerk) = horizontal_peaks(&c);
    assert!(up <= A_ACC * 1.1, "accelerated at {up}");
    assert!(down <= A_BRK * 1.1, "braked at {down}");
    assert!(jerk <= 1.5 * JERK, "command jerk {jerk}");
}

/// A profile change at full stick slows to the new profile's speed on its
/// own brake and jerk limits, with no step. Before, the shaper ended the
/// slowdown with a snap.
#[test]
fn a_profile_change_at_full_stick_slows_without_a_jerk_spike() {
    let mut w = flying();
    let mut c = ticks(&mut w, Some(FWD), 6000);
    w.p.settings.pilot.profile = Profile::Cine;
    c.extend(ticks(&mut w, Some(FWD), 6000));
    let v = c.last().unwrap().speed();
    assert!((v - 3.0).abs() <= 0.05 * 3.0, "Cine full stick {v}");
    let (_, down, jerk) = horizontal_peaks(&c);
    assert!(down <= A_BRK * 1.1, "braked at {down}");
    assert!(jerk <= 1.5 * JERK, "command jerk {jerk}");
}

/// The throttle and the yaw stick ramp the vertical speed and the yaw rate
/// within their accelerations, and on release brake them the same way: the
/// height and heading hold where the reference stopped, with no snap back
/// to where the stick was let go. Before, both stepped to the stick's rate
/// in one tick, and a released yaw stick was pulled back to its heading.
#[test]
fn throttle_and_yaw_ramp_and_brake_without_steps() {
    let mut w = flying();
    let mut c = ticks(&mut w, Some([0.0, 0.0, 1.0, 0.0]), 4000);
    let released = c.len();
    c.extend(ticks(&mut w, Some(CENTRED), 4000));
    let vz_step = max_step(&c, |c| c.vel[2]);
    assert!(vz_step <= A_V * DT * 1.1 + 1e-3, "vertical speed step {vz_step}");
    assert!(c.last().unwrap().vel[2].abs() < 0.05, "climb braked");
    // NED: up is negative. The height reached is held, never given back.
    let top = c[released..].iter().map(|c| c.pos[2]).fold(f32::MAX, f32::min);
    let back = c.last().unwrap().pos[2] - top;
    assert!(back <= 0.15, "height overshoot back {back} m");

    let mut c = ticks(&mut w, Some([0.0, 0.0, 0.0, 1.0]), 3000);
    let released = c.len();
    let rate0 = c.last().unwrap().yaw_rate;
    c.extend(ticks(&mut w, Some(CENTRED), 3000));
    let rate_step = max_step(&c, |c| c.yaw_rate);
    assert!(
        rate_step <= YAW_DPS2.to_radians() * DT * 1.1 + 1e-3,
        "yaw rate step {} °/s",
        rate_step.to_degrees()
    );
    let stop = c[released..]
        .iter()
        .position(|c| c.yaw_rate.abs() < 1f32.to_radians())
        .expect("yaw stops") as f32
        * DT;
    assert!(stop <= rate0 / YAW_DPS2.to_radians() + 0.3, "yaw stop {stop} s");
    let held = c.last().unwrap().yaw;
    let at_release = c[released - 1].yaw;
    let furthest = c[released..]
        .iter()
        .map(|t| wrap(t.yaw - at_release))
        .fold(f32::MIN, f32::max);
    let back = furthest - wrap(held - at_release);
    assert!(back.to_degrees() <= 2.0, "heading back {} °", back.to_degrees());
    let drift = max_step(&c[c.len() - 50..], |c| c.yaw);
    assert!(drift < 1e-4, "heading holds");
}

/// Released at full speed, the vehicle stops within the kinematic stop
/// distance and time, holds the point where the reference stopped, and
/// never flies back to it.
#[test]
fn release_at_full_speed_stops_within_the_stop_distance_and_holds() {
    let mut w = flying();
    let c = ticks(&mut w, Some(FWD), 8000);
    let v0 = c.last().unwrap().speed();
    let n0 = w.pos[0];
    let mut path = Vec::new();
    let mut stopped_at = None;
    for i in 0..500 {
        let c = ticks(&mut w, Some(CENTRED), 20);
        path.push(w.pos[0]);
        if stopped_at.is_none() && c.last().is_some_and(|c| c.speed() < 0.1) {
            stopped_at = Some(i as f32 * DT);
        }
    }
    let far = path.iter().copied().fold(f32::MIN, f32::max);
    let end = *path.last().unwrap();
    assert!(
        far - n0 <= stop_distance(v0, A_BRK, JERK),
        "stopped after {} m from {v0} m/s",
        far - n0
    );
    let stop = stopped_at.expect("stops");
    assert!(stop <= 3.5, "stop took {stop} s");
    assert!(far - end <= 0.2, "flew {} m back", far - end);
    let settled = path
        .iter()
        .rposition(|n| (n - end).abs() > 0.1)
        .map_or(0.0, |i| (i + 1) as f32 * DT);
    assert!(settled <= stop + 1.5, "settled after {settled} s");
}

/// A stick frame whose `seq` is not newer than the last one flown from that
/// session (a reordered or repeated datagram) is dropped, across the wrap
/// of `seq` too. Before, the pilot never read `seq` and flew it.
#[test]
fn a_reordered_or_repeated_stick_frame_is_dropped() {
    // The session's count starts near the wrap of `seq` and wraps during
    // the step: every frame after it is newer and flies.
    let mut w = World::new();
    w.seq = u32::MAX - 60;
    w.p.settings.pilot.avoid = AvoidMode::Off;
    w.airborne(copter::GUIDED);
    w.action("take_control");
    w.fly = true;
    w.stick(CENTRED, 600);
    let c = ticks(&mut w, Some(FWD), 4000);
    assert!(w.seq < 1000, "the count wrapped: {}", w.seq);
    for p in c.windows(2) {
        assert!(p[1].vel[0] >= p[0].vel[0] - 1e-3, "a frame across the wrap was dropped");
    }
    assert!(c.last().unwrap().vel[0] > 0.9 * V_H);
    let last = w.seq - 1;
    for seq in [last.wrapping_sub(3), last, u32::MAX] {
        w.req(frame("mc", 1, seq, BACK, w.now));
        let c = ticks(&mut w, None, 100);
        for p in c.windows(2) {
            assert!(
                p[1].vel[0] >= p[0].vel[0] - 1e-3,
                "seq {seq} flew: {} then {}",
                p[0].vel[0],
                p[1].vel[0]
            );
        }
    }
}

/// Gaps of up to 250 ms between frames keep the last stick flying.
#[test]
fn a_frame_gap_under_the_stale_limit_keeps_flying() {
    let mut w = flying();
    ticks(&mut w, Some(FWD), 6000);
    let mut c = ticks(&mut w, None, 240);
    c.extend(ticks(&mut w, Some(FWD), 1000));
    let low = c.iter().map(Cmd::speed).fold(f32::MAX, f32::min);
    assert!(low >= 0.9 * V_H, "dipped to {low}");
}

/// No frame for over 300 ms: the controller is lost. The reference brakes
/// to a hold at the profile's brake limits, never a step, and the handover
/// gate re-arms: resumed frames fly only after the sticks are centred.
/// Before, the brake started again from the measured velocity at the shared
/// acceleration, and a resumed deflected stick flew at once.
#[test]
fn a_lost_controller_brakes_at_the_profile_and_rearms_the_gate() {
    let mut w = flying();
    let mut c = ticks(&mut w, Some(FWD), 6000);
    c.extend(ticks(&mut w, None, 4000));
    let s = w.p.state(w.now);
    assert_eq!(s.hold_reason.as_deref(), Some("controller-lost"));
    let (_, down, jerk) = horizontal_peaks(&c);
    assert!(down <= A_BRK * 1.1, "braked at {down}");
    assert!(jerk <= 1.5 * JERK, "command jerk {jerk}");
    assert!(c.last().unwrap().speed() < 0.1, "held");
    let c = ticks(&mut w, Some(FWD), 1000);
    assert!(
        c.iter().all(|c| c.speed() < 0.1),
        "a resumed deflected stick flew before the gate"
    );
    assert!(w.p.state(w.now).gate.is_some_and(|g| g.pending));
    ticks(&mut w, Some(CENTRED), 600);
    let c = ticks(&mut w, Some(FWD), 1000);
    assert!(c.last().unwrap().speed() > 0.5, "flies after the gate");
}

/// Frames from a surface that does not hold the sticks never keep the
/// owner's session alive. Before, any surface's frame reset the stale
/// timer, so the owner's lost controller never braked.
#[test]
fn another_surface_does_not_keep_the_owners_sticks_alive() {
    let mut w = flying();
    ticks(&mut w, Some(FWD), 1000);
    let end = w.now + 600;
    let mut seq = 1;
    while w.now < end {
        w.req(frame("android", 9, seq, CENTRED, w.now));
        seq += 1;
        w.run(20);
    }
    let s = w.p.state(w.now);
    assert_eq!(s.input_session.unwrap().src, "mc");
    assert_eq!(s.hold_reason.as_deref(), Some("controller-lost"));
}

/// A stick takeover of a skill flying at speed keeps the reference's
/// velocity and acceleration: the gate holds the deflected sticks, so the
/// reference brakes to a hold from the skill's velocity at the profile's
/// brake limits, with no step. Before, the stick model started again from
/// the measured velocity and braked at the shared acceleration.
#[test]
fn a_stick_takeover_of_a_skill_keeps_the_reference_velocity() {
    let mut w = World::new();
    w.p.settings.pilot.avoid = AvoidMode::Off;
    w.airborne(copter::GUIDED);
    w.fly = true;
    let id = w.lease();
    let goal = |w: &World| Goal::new(GoalKind::Position, [w.pos[0] + 50.0, 0.0, -5.0], 5.0);
    let mut c = Vec::new();
    for _ in 0..40 {
        let g = goal(&w);
        w.req(Request::Goal { lease_id: id, goal: g });
        c.extend(ticks(&mut w, None, 200));
    }
    assert!(c.last().unwrap().speed() > 4.5, "the skill cruises");
    for _ in 0..10 {
        let g = goal(&w);
        w.req(Request::Goal { lease_id: id, goal: g });
        c.extend(ticks(&mut w, Some(FWD), 200));
    }
    assert_eq!(w.p.owner(), ados_protocol::pilot::Owner::Pilot, "taken over");
    c.extend(ticks(&mut w, Some(FWD), 4000));
    let step = c
        .windows(2)
        .map(|p| (p[1].vel[0] - p[0].vel[0]).hypot(p[1].vel[1] - p[0].vel[1]))
        .fold(0.0, f32::max);
    assert!(step <= A_BRK * DT * 1.1 + 1e-3, "velocity step {step}");
    let (_, down, jerk) = horizontal_peaks(&c);
    assert!(down <= A_BRK * 1.1, "braked at {down}");
    assert!(jerk <= 1.5 * JERK, "command jerk {jerk}");
    assert!(c.last().unwrap().speed() < 0.1, "held behind the gate");
}

/// The stick brake plans its stop toward an obstacle on the profile's
/// brake deceleration: the speed it allows toward a wall is what stops at
/// `a_brk` after the FC's lag. Before, it planned on the shared 3 m/s².
#[test]
fn the_stick_brake_plans_on_the_brake_deceleration() {
    let mut w = World::new();
    w.scan_ranges = wall(0, 6.0);
    w.airborne(copter::GUIDED);
    w.action("take_control");
    w.stick(CENTRED, 600);
    let c = ticks(&mut w, Some(FWD), 3000);
    let margin = w.p.settings.pilot.effective_margin_m();
    let cap = crate::core::control::stop_cap(6.0, 0.0, A_BRK, margin);
    let v = c.last().unwrap().speed();
    assert!(v > 0.5, "flies toward the wall: {v}");
    assert!(v <= cap + 0.02, "{v} m/s toward a wall 6 m away; stops in time at {cap}");
}

/// The pilot asks the FC for its guidance limits, under either name, and
/// holds the stick profile to them. Before, it never asked, so the profile
/// was never held to them.
#[test]
fn the_fc_guidance_limits_are_asked_for_and_cap_the_profile() {
    let mut w = World::new();
    w.run(200);
    for name in [
        "WP_ACC",
        "WPNAV_ACCEL",
        "PSC_NE_JERK",
        "PSC_JERK_XY",
        "WP_ACC_Z",
        "WPNAV_ACCEL_Z",
        "PSC_D_JERK",
        "PSC_JERK_Z",
        "ATC_ACC_Y_MAX",
        "ATC_ACCEL_Y_MAX",
    ] {
        let asked = w.sent.iter().any(|m| {
            matches!(m, MavMessage::PARAM_REQUEST_READ(r)
                if r.param_id.to_str().unwrap_or("").trim_end_matches('\0') == name)
        });
        assert!(asked, "{name} is never requested");
    }
    w.p.fc.params.insert("WP_ACC".into(), 1.5);
    w.p.fc.params.insert("ATC_ACC_Y_MAX".into(), 120.0);
    let l = w.p.state(w.now).profile_limits.expect("profile limits");
    assert_eq!((l.a_acc_mps2, l.a_brk_mps2, l.yaw_dps2), (1.5, 1.5, 120.0));
}

/// A profile change while climbing faster than the new profile allows, and
/// a skill's descent handed to the sticks, brake the vertical speed on the
/// vertical acceleration. Before, the vertical speed was cut to the new
/// limit in one tick.
#[test]
fn a_slower_profile_or_a_handed_over_descent_brakes_the_vertical_speed() {
    let mut w = flying();
    w.p.settings.pilot.profile = Profile::Sport;
    let mut c = ticks(&mut w, Some([0.0, 0.0, 1.0, 0.0]), 5000);
    assert!(-c.last().unwrap().vel[2] > 2.9, "Sport climbs at 3 m/s");
    w.p.settings.pilot.profile = Profile::Cine;
    c.extend(ticks(&mut w, Some([0.0, 0.0, 1.0, 0.0]), 4000));
    let step = max_step(&c, |c| c.vel[2]);
    assert!(step <= A_V * DT * 1.1 + 1e-3, "vertical speed step {step}");

    let mut w = World::new();
    w.p.settings.pilot.avoid = AvoidMode::Off;
    w.airborne(copter::GUIDED);
    w.pos = [0.0, 0.0, -20.0];
    w.agl = Some(20.0);
    w.fly = true;
    let id = w.lease();
    let goal = || {
        let mut g = Goal::new(GoalKind::Position, [0.0, 0.0, -10.0], 5.0);
        g.vd = 2.0;
        Request::Goal { lease_id: id, goal: g }
    };
    w.req(goal());
    ticks(&mut w, None, 200);
    // The sticks take over (a full yaw stick held 1 s) while the skill
    // descends at 2 m/s, past Normal's 1.5 m/s.
    let mut c = Vec::new();
    let mut from = None;
    for _ in 0..15 {
        w.req(goal());
        for t in ticks(&mut w, Some([0.0, 0.0, 0.0, 1.0]), 100) {
            c.push(t);
        }
        if from.is_none() && w.p.owner() == ados_protocol::pilot::Owner::Pilot {
            from = Some(c.len().saturating_sub(6));
        }
    }
    let from = from.expect("taken over");
    c.extend(ticks(&mut w, Some(CENTRED), 3000));
    let step = max_step(&c[from..], |c| c.vel[2]);
    assert!(step <= A_V * DT * 1.1 + 1e-3, "vertical speed step {step}");
}

/// Sticks at speed handing to a skill whose goal flies on at that speed:
/// the goal goes on from the velocity flown, with no dip.
#[test]
fn a_stick_to_skill_handover_at_speed_does_not_dip() {
    let mut w = flying();
    ticks(&mut w, Some(FWD), 6000);
    let v0 = w.vel[0];
    assert!(v0 > 6.5, "{v0}");
    let id = w.lease();
    let mut low = f32::MAX;
    for _ in 0..10 {
        let mut g = Goal::new(GoalKind::Follow, [w.pos[0] + 10.0, 0.0, -5.0], 8.0);
        g.vn = v0;
        w.req(Request::Goal { lease_id: id, goal: g });
        for c in ticks(&mut w, None, 100) {
            low = low.min(c.speed());
        }
    }
    assert_eq!(w.p.owner(), ados_protocol::pilot::Owner::Skill);
    assert!(low >= 0.9 * v0, "dipped to {low} from {v0}");
}
