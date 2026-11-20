//! FC position resets: the EKF's horizontal estimate steps (a source-set
//! change, the origin set in flight, a GPS glitch) while the vehicle stays
//! put. ArduCopter's GUIDED position submodes keep their absolute target
//! through the step and fly the vehicle the step's length to it. The pilot
//! sees the step, sends a zero-velocity setpoint (GUIDED starts again at the
//! vehicle), moves its own setpoints with the estimate and holds, so no
//! position target it sends is ever far from the vehicle and no velocity
//! exceeds what it flew before. Fast flight and a stepped clock are no
//! reset.

use ados_protocol::mavlink::ardupilotmega::{
    MavCmd, MavMessage, MavResult, MavSeverity, COMMAND_ACK_DATA, GPS_GLOBAL_ORIGIN_DATA,
    SET_POSITION_TARGET_LOCAL_NED_DATA, STATUSTEXT_DATA,
};
use ados_protocol::pilot::{AvoidMode, Owner, Request};
use serde_json::json;

use super::harness::{World, FC_COMP, FC_SYS};
use crate::core::pos_reset::HOLD_POSITION_RESET;
use crate::core::Reply;
use crate::fc::MASK_VELOCITY;
use crate::modes::copter;

/// The step of the incident: 21.2 m.
const JUMP: [f32; 2] = [15.0, -15.0];
const BANNER: &str = "pilot.banner.position_reset";
const CENTRED: [f32; 4] = [0.0; 4];

fn ok(r: &Reply) -> bool {
    matches!(r, Reply::Action(a) if a.ok)
}

fn setpoints(w: &World, from: usize) -> Vec<SET_POSITION_TARGET_LOCAL_NED_DATA> {
    w.sent[from..]
        .iter()
        .filter_map(|m| match m {
            MavMessage::SET_POSITION_TARGET_LOCAL_NED(s) => Some(s.clone()),
            _ => None,
        })
        .collect()
}

fn uses_pos(s: &SET_POSITION_TARGET_LOCAL_NED_DATA) -> bool {
    s.type_mask.bits() & 0b111 == 0
}

fn speed(s: &SET_POSITION_TARGET_LOCAL_NED_DATA) -> f32 {
    s.vx.hypot(s.vy)
}

/// The fastest horizontal velocity any setpoint since `from` commanded.
fn max_speed(w: &World, from: usize) -> f32 {
    setpoints(w, from).iter().map(speed).fold(0.0, f32::max)
}

/// The pre-reset bound: the fastest the setpoints commanded or the vehicle
/// flew (a brake starts from the vehicle's own velocity).
fn pre_speed(w: &World) -> f32 {
    max_speed(w, 0).max(w.vel[0].hypot(w.vel[1]))
}

/// The estimate steps by `d`; the vehicle stays where it is.
fn reset(w: &mut World, d: [f32; 2]) {
    w.pos[0] += d[0];
    w.pos[1] += d[1];
}

fn banner(w: &World) -> Option<(String, serde_json::Value)> {
    w.p.state(w.now)
        .banner
        .map(|b| (b.text_key, serde_json::Value::Object(b.args)))
}

fn statustext(w: &mut World, text: &str) {
    let m = MavMessage::STATUSTEXT(STATUSTEXT_DATA {
        severity: MavSeverity::MAV_SEVERITY_CRITICAL,
        text: text.into(),
    });
    w.p.on_fc(FC_SYS, FC_COMP, &m, w.now);
    w.collect();
}

/// Fly `ms` in 20 ms ticks (streaming `axes` first, when given) and check
/// every setpoint as it is sent: a position target within `leash` of the
/// vehicle, a horizontal velocity of at most `v_max`.
fn fly_checked(w: &mut World, axes: Option<[f32; 4]>, ms: u64, leash: f32, v_max: f32) {
    let end = w.now + ms;
    while w.now < end {
        if let Some(a) = axes {
            w.input(a);
        }
        let from = w.sent.len();
        let at = w.pos;
        w.run(20);
        for s in setpoints(w, from) {
            if uses_pos(&s) {
                let off = (s.x - at[0]).hypot(s.y - at[1]);
                assert!(
                    off <= leash,
                    "at {} the target is {off} m from the vehicle: {s:?}",
                    w.now
                );
            }
            assert!(
                speed(&s) <= v_max + 1e-3,
                "at {} a velocity of {} over {v_max}",
                w.now,
                speed(&s)
            );
        }
    }
}

/// Hovering in GUIDED as the pilot (Take control) in control mode `mode`,
/// the FC stamping its position samples.
fn hovering(mode: &str) -> World {
    let mut w = World::new();
    w.fc_clock = true;
    w.airborne(copter::LOITER);
    assert!(ok(&w.action(&format!("control_mode:{mode}"))), "{mode}");
    assert!(ok(&w.action("take_control")), "{mode}");
    w.run(1_000);
    assert_eq!(w.p.owner(), Owner::Pilot, "{mode}");
    assert_eq!(w.mode, copter::GUIDED, "{mode}");
    w
}

/// The first setpoint after a reset is a zero velocity, position ignored:
/// GUIDED leaves its position submode and starts again at the vehicle.
fn kicked(w: &World, from: usize) {
    let first = setpoints(w, from).into_iter().next().expect("a setpoint");
    assert_eq!(first.type_mask.bits(), MASK_VELOCITY, "{first:?}");
    assert_eq!([first.vx, first.vy, first.vz], [0.0; 3]);
}

#[test]
fn a_21_m_reset_in_the_pilots_hold_holds_at_the_vehicle() {
    for mode in ["ados", "fc_manual"] {
        let mut w = hovering(mode);
        let v_pre = max_speed(&w, 0);
        let from = w.sent.len();
        reset(&mut w, JUMP);
        fly_checked(&mut w, None, 3_000, 1.0, v_pre);
        kicked(&w, from);
        let held = setpoints(&w, from);
        let hold = held
            .iter()
            .rfind(|s| uses_pos(s))
            .unwrap_or_else(|| panic!("{mode}: the hold given again after the kick"));
        assert!(
            (hold.x - w.pos[0]).hypot(hold.y - w.pos[1]) < 0.1,
            "{mode}: held at the vehicle: {hold:?} vs {:?}",
            w.pos
        );
        let s = w.p.state(w.now);
        assert_eq!(
            s.hold_reason.as_deref(),
            Some(HOLD_POSITION_RESET),
            "{mode}"
        );
        assert_eq!(w.p.owner(), Owner::Pilot, "{mode}");
        let (key, args) = banner(&w).expect("a banner");
        assert_eq!(key, BANNER, "{mode}");
        assert_eq!(args["cause"], "position_step", "{mode}");
        assert!(
            (args["shift_m"].as_f64().unwrap() - 21.2).abs() < 0.05,
            "{args}"
        );
    }
}

/// The incident: outside `ados` a ground station's reposition left the FC
/// on a GUIDED position target with nobody streaming (owner `fc_mode`). The
/// reset gets the zero-velocity setpoint and nothing else: a velocity
/// submode holds where the vehicle is and never moves on a later reset.
#[test]
fn a_reset_under_a_held_guided_target_with_no_owner_stops_the_fc() {
    let mut w = hovering("fc_manual");
    let ack = MavMessage::COMMAND_ACK(COMMAND_ACK_DATA {
        command: MavCmd::MAV_CMD_DO_REPOSITION,
        result: MavResult::MAV_RESULT_ACCEPTED,
    });
    w.p.on_fc(FC_SYS, FC_COMP, &ack, w.now);
    w.collect();
    w.run(1_000);
    assert_eq!(w.p.owner(), Owner::FcMode);
    let from = w.sent.len();
    reset(&mut w, JUMP);
    w.run(2_000);
    let sent = setpoints(&w, from);
    assert_eq!(sent.len(), 1, "the zero velocity only: {sent:?}");
    kicked(&w, from);
    assert_eq!(
        w.p.owner(),
        Owner::FcMode,
        "the ground station keeps the FC"
    );
    assert_eq!(banner(&w).map(|b| b.0).as_deref(), Some(BANNER));
}

/// The operator's sticks keep flying across a reset: the reference moves
/// with the estimate, so the target never jumps and the speed never rises.
#[test]
fn the_sticks_fly_on_from_the_moved_reference() {
    let mut w = World::new();
    w.fc_clock = true;
    w.p.settings.pilot.avoid = AvoidMode::Off;
    w.airborne(copter::GUIDED);
    assert!(ok(&w.action("take_control")));
    w.fly = true;
    w.stick(CENTRED, 600);
    let fwd = [0.0, 0.6, 0.0, 0.0];
    w.stick(fwd, 3_000);
    let from = w.sent.len();
    w.stick(fwd, 500);
    let v_pre = max_speed(&w, from);
    assert!(v_pre > 2.0, "flying: {v_pre}");
    let from = w.sent.len();
    reset(&mut w, JUMP);
    fly_checked(&mut w, Some(fwd), 2_000, 1.0, v_pre);
    kicked(&w, from);
    assert_eq!(w.p.owner(), Owner::Pilot);
    assert_eq!(w.p.state(w.now).hold_reason, None, "the operator flies on");
    assert!(w.vel[0] > 2.0, "still flying: {:?}", w.vel);
}

/// A skill's goal was given in the old frame: the skill gives way to the
/// pilot's hold at the vehicle.
#[test]
fn a_skill_gives_way_to_the_hold() {
    let mut w = World::new();
    w.fc_clock = true;
    w.p.settings.pilot.avoid = AvoidMode::Off;
    w.airborne(copter::GUIDED);
    w.fly = true;
    let id = w.lease();
    for _ in 0..10 {
        w.goal(id, 30.0, 0.0);
        w.run(200);
    }
    let v_pre = pre_speed(&w);
    let from = w.sent.len();
    reset(&mut w, JUMP);
    w.goal(id, 30.0, 0.0);
    fly_checked(&mut w, None, 3_000, 1.0, v_pre);
    kicked(&w, from);
    assert_eq!(w.p.owner(), Owner::Pilot);
    assert_eq!(
        w.revoked.last(),
        Some(&(id, HOLD_POSITION_RESET.to_owned()))
    );
    assert_eq!(
        w.p.state(w.now).hold_reason.as_deref(),
        Some(HOLD_POSITION_RESET)
    );
    let v = w.vel[0].hypot(w.vel[1]);
    assert!(v < 0.2, "held: {v}");
}

/// A goto's point was given in the old frame: the goto ends in the hold.
#[test]
fn a_goto_gives_way_to_the_hold() {
    let mut w = World::new();
    w.fc_clock = true;
    w.p.settings.pilot.avoid = AvoidMode::Off;
    w.airborne(copter::GUIDED);
    w.fly = true;
    w.req(Request::Goto {
        n: 30.0,
        e: 0.0,
        d: -5.0,
        yaw: None,
    });
    w.run(2_000);
    assert_eq!(w.p.owner(), Owner::Goto);
    let v_pre = pre_speed(&w);
    let from = w.sent.len();
    reset(&mut w, JUMP);
    fly_checked(&mut w, None, 3_000, 1.0, v_pre);
    kicked(&w, from);
    assert_eq!(w.p.owner(), Owner::Pilot);
    assert_eq!(
        w.p.state(w.now).hold_reason.as_deref(),
        Some(HOLD_POSITION_RESET)
    );
}

/// A step under the bare threshold (2 m) is a reset when the FC has just
/// said why: a GPS glitch, the origin set in flight, a source-set change.
#[test]
fn a_named_cause_catches_a_smaller_step() {
    type Cue = fn(&mut World);
    let cues: [(&str, Cue); 3] = [
        ("gps_glitch", |w| {
            statustext(w, "GPS Glitch or Compass error")
        }),
        ("origin_set", |w| {
            let m = MavMessage::GPS_GLOBAL_ORIGIN(GPS_GLOBAL_ORIGIN_DATA {
                latitude: 473_977_420,
                longitude: 85_455_940,
                ..Default::default()
            });
            w.p.on_fc(FC_SYS, FC_COMP, &m, w.now);
        }),
        ("source_set", |w| {
            let nav = json!({"active_set": 2});
            w.p.on_feed(crate::events::TOPIC_NAV_STATE, &nav, w.now);
        }),
    ];
    for (cause, cue) in cues {
        let mut w = hovering("ados");
        cue(&mut w);
        let from = w.sent.len();
        reset(&mut w, [1.2, 0.0]);
        fly_checked(&mut w, None, 1_000, 1.0, 0.0);
        kicked(&w, from);
        let (key, args) = banner(&w).expect("a banner");
        assert_eq!(key, BANNER, "{cause}");
        assert_eq!(args["cause"], cause);
    }
    // The same step with nothing said: an estimate correcting, flown on.
    let mut w = hovering("ados");
    let from = w.sent.len();
    reset(&mut w, [1.2, 0.0]);
    w.run(1_000);
    assert!(setpoints(&w, from)
        .iter()
        .all(|s| s.type_mask.bits() != MASK_VELOCITY));
    assert_ne!(banner(&w).map(|b| b.0).as_deref(), Some(BANNER));
}

/// Full-stick flight, a hard reversal and a bench clock stepped on are no
/// reset: the velocities explain every step, and samples further apart than
/// a second on the FC's clock are not compared.
#[test]
fn fast_flight_and_a_stepped_clock_are_no_reset() {
    let mut w = World::new();
    w.fc_clock = true;
    w.p.settings.pilot.avoid = AvoidMode::Off;
    w.p.settings.pilot.profile = ados_protocol::pilot::Profile::Sport;
    w.airborne(copter::GUIDED);
    assert!(ok(&w.action("take_control")));
    w.fly = true;
    w.stick(CENTRED, 600);
    let from = w.sent.len();
    w.stick([0.0, 1.0, 0.0, 0.0], 4_000);
    w.stick([0.0, -1.0, 0.0, 0.0], 3_000);
    w.stick([1.0, 0.0, 0.0, 0.0], 3_000);
    assert!(max_speed(&w, from) > 8.0, "fast: {}", max_speed(&w, from));
    assert!(setpoints(&w, from)
        .iter()
        .all(|s| s.type_mask.bits() != MASK_VELOCITY));
    assert_ne!(banner(&w).map(|b| b.0).as_deref(), Some(BANNER));
    assert_eq!(w.p.state(w.now).hold_reason, None);
    // The bench clock steps 30 s on and the vehicle with it.
    let from = w.sent.len();
    w.now += 30_000;
    w.pos[0] += 120.0;
    w.feed();
    w.collect();
    assert!(setpoints(&w, from).is_empty());
    assert_ne!(banner(&w).map(|b| b.0).as_deref(), Some(BANNER));
}

/// On the ground, or disarmed, a step only moves the pilot's frame.
#[test]
fn on_the_ground_a_step_sends_nothing() {
    let mut w = World::new();
    w.fc_clock = true;
    w.run(500);
    let from = w.sent.len();
    reset(&mut w, JUMP);
    w.run(500);
    assert!(setpoints(&w, from).is_empty());
    assert_ne!(banner(&w).map(|b| b.0).as_deref(), Some(BANNER));
}
