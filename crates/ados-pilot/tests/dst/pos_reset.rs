//! FC position resets in flight: the EKF's horizontal estimate steps 21 m
//! (a source-set change, the origin set in flight, a GPS glitch) while the
//! copter hovers. The simulated FC flies ArduCopter's GUIDED as it handles a
//! reset ([`Fc::ap_guided`]): a position submode keeps its absolute target
//! and flies the step to it at the position controller's speed, past a
//! trunk on the way. The pilot sees the step and sends a zero-velocity
//! setpoint, so the copter stays where it is: under its own hold, and under
//! a GUIDED target a ground station left with nobody streaming. Fast flight
//! and a bench clock stepped on are no reset.

use ados_pilot::config::Settings;
use ados_pilot::fc::{Tx, MASK_POS_VEL_YAW, MASK_VELOCITY};
use ados_pilot::modes::copter;
use ados_protocol::mavlink::ardupilotmega::{
    MavCmd, MavMessage, MavResult, MavSeverity, COMMAND_ACK_DATA, STATUSTEXT_DATA,
};
use ados_protocol::pilot::{ControlMode, Owner};

use crate::fc::Fc;
use crate::rng::Rng;
use crate::scenarios::launch_to_hold;
use crate::sim::Sim;

/// The step of the incident, m.
const JUMP_M: f32 = 21.0;
/// The trunk the dash would pass: this far along it, radius 0.3 m.
const TRUNK_ALONG_M: f32 = 8.0;
const TRUNK_R_M: f32 = 0.3;
const BANNER: &str = "pilot.banner.position_reset";
/// How far the copter may wander, and its targets lie, around the reset.
const LEASH_M: f32 = 1.0;

/// A seeded horizontal step of `len` m in any direction, and `down` m.
fn step_of(rng: &mut Rng, len: f32, down: f32) -> [f32; 3] {
    let a = rng.range(0.0, std::f32::consts::TAU);
    [len * a.cos(), len * a.sin(), down]
}

/// Where the dash goes: back along the step from where the copter is.
fn trunk_on_the_dash(at: [f32; 3], d: [f32; 3]) -> [f32; 2] {
    let k = TRUNK_ALONG_M / JUMP_M;
    [at[0] - d[0] * k, at[1] - d[1] * k]
}

fn h(v: [f32; 3]) -> f32 {
    v[0].hypot(v[1])
}

fn ack_reposition() -> MavMessage {
    MavMessage::COMMAND_ACK(COMMAND_ACK_DATA {
        command: MavCmd::MAV_CMD_DO_REPOSITION,
        result: MavResult::MAV_RESULT_ACCEPTED,
    })
}

fn text(t: &str) -> MavMessage {
    MavMessage::STATUSTEXT(STATUSTEXT_DATA {
        severity: MavSeverity::MAV_SEVERITY_CRITICAL,
        text: t.into(),
    })
}

/// Fly `ms` after a reset of `d` taken now, tick by tick: the copter stays
/// within [`LEASH_M`] of where it truly was (height too), every position
/// target the pilot sends lies within it of the copter's estimate, and no
/// setpoint asks for more than `v_max`. Whether the pilot named the reset in
/// its banner, with that cause.
fn ride_out(s: &mut Sim, d: [f32; 3], ms: u64, v_max: f32) -> Option<String> {
    let t0 = s.fc.truth();
    s.fc.ekf_reset(d);
    let mut cause = None;
    let end = s.now + ms;
    while s.now < end {
        let from = s.sent.len();
        s.step();
        let t = s.fc.truth();
        let wander = (t[0] - t0[0]).hypot(t[1] - t0[1]);
        let climb = (t[2] - t0[2]).abs();
        assert!(
            wander <= LEASH_M && climb <= LEASH_M,
            "{}: flew {wander} m, {climb} m up or down at {}",
            s.name,
            s.now
        );
        for (at, m) in &s.sent[from..] {
            let MavMessage::SET_POSITION_TARGET_LOCAL_NED(sp) = m else {
                continue;
            };
            if sp.type_mask.bits() & 0b111 == 0 {
                let off = (sp.x - s.fc.pos[0]).hypot(sp.y - s.fc.pos[1]);
                let off_d = (sp.z - s.fc.pos[2]).abs();
                assert!(
                    off <= LEASH_M && off_d <= LEASH_M,
                    "{}: target {off} m, {off_d} m off at {at}",
                    s.name
                );
            }
            let v = sp.vx.hypot(sp.vy);
            assert!(
                v <= v_max + 1e-3,
                "{}: {v} m/s over {v_max} at {at}",
                s.name
            );
        }
        if let Some(b) = s.state.banner.as_ref().filter(|b| b.text_key == BANNER) {
            cause = b
                .args
                .get("cause")
                .and_then(|c| c.as_str())
                .map(String::from);
        }
    }
    cause
}

/// The fastest any setpoint asked for over the last `ms`, or the copter
/// flew now.
fn pre_speed(s: &Sim, ms: u64) -> f32 {
    let since = s.now.saturating_sub(ms);
    s.sent
        .iter()
        .filter(|(t, _)| *t >= since)
        .filter_map(|(_, m)| match m {
            MavMessage::SET_POSITION_TARGET_LOCAL_NED(sp) => Some(sp.vx.hypot(sp.vy)),
            _ => None,
        })
        .fold(h(s.fc.vel), f32::max)
}

fn hover(name: &'static str, seed: u64, settings: impl FnOnce(&mut Settings)) -> Sim {
    let mut s = Sim::new(name, seed, settings);
    s.fc.ap_guided = true;
    s.boot();
    launch_to_hold(&mut s, 5.0);
    s
}

/// A 21 m reset, with up to 1.5 m of height, in the pilot's own hold: the
/// copter holds where it is, the hold given again at the copter,
/// `hold_reason` `position-reset`.
pub fn reset_hold(seed: u64) -> Sim {
    let mut s = hover("reset_hold", seed, |_| {});
    let down = s.rng.range(-1.5, 1.5);
    let d = step_of(&mut s.rng, JUMP_M, down);
    s.stand = vec![(trunk_on_the_dash(s.fc.truth(), d), TRUNK_R_M)];
    s.run(4_000);
    let v_pre = pre_speed(&s, 2_000);
    let cause = ride_out(&mut s, d, 10_000, v_pre);
    assert_eq!(cause.as_deref(), Some("position_step"));
    assert_eq!(s.p.owner(), Owner::Pilot);
    assert_eq!(s.state.hold_reason.as_deref(), Some("position-reset"));
    assert!(s.stand_clearance_m() > TRUNK_ALONG_M - 2.0 * LEASH_M);
    s
}

/// The incident: outside `ados` a ground station's reposition left the FC
/// on its GUIDED target with nobody streaming (owner `fc_mode`), past the
/// setpoint timeout. The pilot's zero-velocity setpoint is all it sends,
/// and the copter holds where it is.
pub fn reset_idle(seed: u64) -> Sim {
    let mut s = hover("reset_idle", seed, |st| {
        st.control.mode = ControlMode::FcManual
    });
    s.fc_frame(ack_reposition());
    assert_eq!(s.p.owner(), Owner::FcMode);
    let down = s.rng.range(-1.5, 1.5);
    let d = step_of(&mut s.rng, JUMP_M, down);
    s.stand = vec![(trunk_on_the_dash(s.fc.truth(), d), TRUNK_R_M)];
    s.run(5_000);
    let from = s.sent.len();
    let v_pre = pre_speed(&s, 2_000);
    let cause = ride_out(&mut s, d, 10_000, v_pre);
    assert_eq!(cause.as_deref(), Some("position_step"));
    let sent: Vec<_> = s.sent[from..]
        .iter()
        .filter_map(|(_, m)| match m {
            MavMessage::SET_POSITION_TARGET_LOCAL_NED(sp) => Some(sp.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(sent.len(), 1, "the zero velocity only: {sent:?}");
    assert_eq!(sent[0].type_mask.bits(), MASK_VELOCITY);
    assert_eq!(
        s.p.owner(),
        Owner::FcMode,
        "the ground station keeps the FC"
    );
    assert!(s.stand_clearance_m() > TRUNK_ALONG_M - 2.0 * LEASH_M);
    s
}

/// A GPS glitch, then the EKF's reset back onto GPS 1-1.8 m away: the
/// glitch names the step and lowers what counts, so even a step under the
/// bare threshold is held.
pub fn reset_glitch(seed: u64) -> Sim {
    let mut s = hover("reset_glitch", seed, |_| {});
    s.run(2_000);
    let len = s.rng.range(1.0, 1.8);
    let d = step_of(&mut s.rng, len, 0.0);
    s.fc_frame(text("GPS Glitch or Compass error"));
    let wait = s.rng.range(200.0, 3_000.0) as u64;
    s.run(wait);
    s.fc_frame(text("Glitch cleared"));
    let v_pre = pre_speed(&s, 2_000);
    let cause = ride_out(&mut s, d, 5_000, v_pre);
    assert_eq!(cause.as_deref(), Some("gps_glitch"));
    assert_eq!(s.state.hold_reason.as_deref(), Some("position-reset"));
    s
}

/// Run `ms`; whether the pilot named a position reset meanwhile (its
/// banner or `hold_reason`).
fn saw_reset(s: &mut Sim, ms: u64) -> bool {
    let end = s.now + ms;
    let mut seen = false;
    while s.now < end {
        s.step();
        seen |= s
            .state
            .banner
            .as_ref()
            .is_some_and(|b| b.text_key == BANNER)
            || s.state.hold_reason.as_deref() == Some("position-reset");
    }
    seen
}

/// Full sticks out, a hard reversal, a turn across, a climb and a descent,
/// then a bench clock stepped 30 s on with the copter 200 m on: no reset.
pub fn no_false_reset(seed: u64) -> Sim {
    let mut s = Sim::new("no_false_reset", seed, |st| {
        st.pilot.avoid = ados_protocol::pilot::AvoidMode::Off;
    });
    s.boot();
    launch_to_hold(&mut s, 5.0);
    let ms = s.rng.range(5_000.0, 8_000.0) as u64;
    let mut seen = false;
    for axes in [
        [0.0, 0.0, 1.0, 0.0],
        [0.0, 0.0, -1.0, 0.0],
        [0.0, 1.0, 1.0, 0.0],
    ] {
        s.sticks = Some([0.0; 4]);
        seen |= saw_reset(&mut s, 800);
        s.sticks = Some(axes);
        seen |= saw_reset(&mut s, 2_500);
        assert!(s.fc.vel[2].abs() > 0.5, "climbing: {:?}", s.fc.vel);
    }
    for axes in [
        [0.0, 1.0, 0.0, 0.0],
        [0.0, -1.0, 0.0, 0.0],
        [1.0, 0.0, 0.0, 0.0],
    ] {
        s.sticks = Some([0.0; 4]);
        seen |= saw_reset(&mut s, 800);
        s.sticks = Some(axes);
        seen |= saw_reset(&mut s, ms);
        assert!(h(s.fc.vel) > 5.0, "fast: {:?}", s.fc.vel);
    }
    s.now += 30_000;
    s.fc.pos[0] += 200.0;
    seen |= saw_reset(&mut s, 3_000);
    s.sticks = Some([0.0; 4]);
    seen |= saw_reset(&mut s, 2_000);
    assert!(!seen, "a reset seen in plain flight");
    s
}

/// The model has teeth: with no pilot answering, the simulated FC flies a
/// 21 m reset under a GUIDED position target at its position controller's
/// speed, through the trunk.
pub fn fc_flies_the_reset_alone(seed: u64) {
    let mut rng = Rng::new(seed);
    let mut fc = Fc::new(&mut rng);
    fc.ap_guided = true;
    fc.armed = true;
    fc.landed = false;
    fc.mode = copter::GUIDED;
    fc.pos = [0.0, 0.0, -5.0];
    let tx = Tx {
        sysid: 1,
        compid: 1,
    };
    let mut now = 1_000;
    fc.apply(
        &tx.setpoint(MASK_POS_VEL_YAW, fc.pos, [0.0; 3], 0.0, 0.0),
        now,
    );
    for _ in 0..100 {
        now += 20;
        fc.step(now, 0.02, &mut rng, false);
    }
    let t0 = fc.truth();
    let d = step_of(&mut rng, JUMP_M, 0.0);
    let trunk = trunk_on_the_dash(t0, d);
    fc.ekf_reset(d);
    let (mut peak, mut nearest) = (0.0f32, f32::INFINITY);
    for _ in 0..500 {
        now += 20;
        fc.step(now, 0.02, &mut rng, false);
        let t = fc.truth();
        peak = peak.max(h(fc.vel));
        nearest = nearest.min((t[0] - trunk[0]).hypot(t[1] - trunk[1]) - TRUNK_R_M);
    }
    let t = fc.truth();
    let flown = (t[0] - t0[0]).hypot(t[1] - t0[1]);
    assert!(flown > JUMP_M - 1.0, "flew the step: {flown} m");
    assert!(peak > 7.0, "at speed: {peak} m/s");
    assert!(nearest < 0.5, "past the trunk: {nearest} m");
}
