//! Landing to the ground: the descent profile and the window after
//! touchdown, against a scripted copter that flies the commanded vertical
//! speed with a lag and sits on the ground once it gets there.

use ados_protocol::mavlink::ardupilotmega::*;
use ados_protocol::pilot::Owner;

use super::harness::{World, FC_COMP, FC_SYS};
use crate::core::Reply;
use crate::fc::MASK_VELOCITY;
use crate::modes::copter;

/// The FC's vertical velocity response (first order).
const TAU_S: f32 = 0.5;
/// What the downward rangefinder reads with the vehicle on the ground.
const RF_GROUND_M: f32 = 0.15;
/// ArduCopter's land detector: this long on the ground in LAND.
const DETECT_MS: u64 = 1000;

#[derive(Default)]
struct Sim {
    /// LAND descent speed (the FC's `LAND_SPD_MS`).
    land_spd: f32,
    /// Whether the FC's land detector ever reports ON_GROUND.
    detector: bool,
    on_ground: bool,
    touchdown_ms: Option<u64>,
    /// Vertical speed at ground contact.
    contact_vd: Option<f32>,
    /// Vertical speed when the FC entered LAND.
    handover_vd: Option<f32>,
    /// Fastest descent commanded before the hand-over.
    max_cmd_vd: f32,
    /// Ground time in LAND, for the land detector.
    in_land_ms: u64,
    /// Something commanded a climb off the ground.
    lifted: bool,
}

fn physics(w: &mut World, s: &mut Sim) {
    let dt = 0.02;
    let cmd_vd = if w.mode == copter::LAND {
        s.land_spd
    } else if w.mode == copter::GUIDED {
        match w.last_setpoint() {
            Some(sp) if sp.type_mask.bits() == MASK_VELOCITY => sp.vz,
            Some(sp) => (sp.z - w.pos[2]).clamp(-2.0, 2.0),
            None => 0.0,
        }
    } else {
        0.0
    };
    if w.mode == copter::LAND && s.handover_vd.is_none() {
        s.handover_vd = Some(w.vel[2]);
    }
    if s.handover_vd.is_none() {
        s.max_cmd_vd = s.max_cmd_vd.max(cmd_vd);
    }
    if s.on_ground {
        if cmd_vd < -0.05 {
            s.lifted = true;
            s.on_ground = false;
        } else {
            w.vel[2] = 0.0;
            s.in_land_ms = if w.mode == copter::LAND {
                s.in_land_ms + 20
            } else {
                0
            };
            if s.detector && s.in_land_ms >= DETECT_MS {
                w.landed = true;
            }
            return;
        }
    }
    w.vel[2] += (cmd_vd - w.vel[2]) * dt / TAU_S;
    w.pos[2] += w.vel[2] * dt;
    if w.pos[2] >= 0.0 {
        s.contact_vd.get_or_insert(w.vel[2]);
        s.touchdown_ms.get_or_insert(w.now);
        s.on_ground = true;
        w.pos[2] = 0.0;
        w.vel[2] = 0.0;
    }
    w.agl = Some(RF_GROUND_M - w.pos[2]);
}

fn fly(w: &mut World, s: &mut Sim, ms: u64) {
    let end = w.now + ms;
    while w.now < end {
        physics(w, s);
        w.run(20);
    }
}

fn fly_to_touchdown(w: &mut World, s: &mut Sim) {
    let end = w.now + 30_000;
    while s.touchdown_ms.is_none() && w.now < end {
        fly(w, s, 20);
    }
    assert!(s.touchdown_ms.is_some(), "never touched down");
}

/// Hovering at `h` m, owner `pilot` in GUIDED, home set, the FC's landing
/// speed `LAND_SPD_MS` = 0.3 as on the bench.
fn hovering(h: f32, detector: bool) -> (World, Sim) {
    let mut w = World::new();
    w.p.fc.params.insert("LAND_SPD_MS".into(), 0.3);
    let home = MavMessage::HOME_POSITION(HOME_POSITION_DATA {
        latitude: 120_000_000,
        longitude: 770_000_000,
        ..Default::default()
    });
    w.p.on_fc(FC_SYS, FC_COMP, &home, w.now);
    w.airborne(copter::GUIDED);
    w.pos = [0.0, 0.0, -h];
    w.agl = Some(RF_GROUND_M + h);
    w.run(200);
    assert_eq!(w.p.owner(), Owner::Pilot);
    let s = Sim {
        land_spd: 0.3,
        detector,
        ..Default::default()
    };
    (w, s)
}

fn err(r: Reply) -> Option<String> {
    match r {
        Reply::Action(a) => a.err,
        other => panic!("{other:?}"),
    }
}

/// Mode commands, take-offs and climbing setpoints sent from `from` on.
fn leave_or_climb(w: &World, from: usize) -> Vec<String> {
    w.sent[from..]
        .iter()
        .filter_map(|m| match m {
            MavMessage::COMMAND_LONG(c)
                if c.command == MavCmd::MAV_CMD_DO_SET_MODE && c.param2 as u32 != copter::LAND =>
            {
                Some(format!("mode {}", c.param2))
            }
            MavMessage::COMMAND_LONG(c) if c.command == MavCmd::MAV_CMD_NAV_TAKEOFF => {
                Some("takeoff".into())
            }
            MavMessage::SET_POSITION_TARGET_LOCAL_NED(sp) => Some(format!("setpoint {sp:?}")),
            _ => None,
        })
        .collect()
}

#[test]
fn pause_and_goto_after_a_land_touchdown_are_refused() {
    // The bench sequence: Land from 4 m under `ados`, Pause 0.3 s after
    // touchdown, Take control and a goto to 2.5 m 2.5 s after it, while the
    // FC has not yet reported the vehicle on the ground.
    let (mut w, mut s) = hovering(4.0, false);
    assert_eq!(err(w.action("land")), None);
    fly_to_touchdown(&mut w, &mut s);
    assert_eq!(w.mode, copter::LAND);
    let from = w.sent.len();
    fly(&mut w, &mut s, 300);
    assert_eq!(err(w.action("pause")).as_deref(), Some("landed"));
    fly(&mut w, &mut s, 1100);
    assert_eq!(err(w.action("control_mode:ados")), None);
    assert_eq!(err(w.action("take_control")).as_deref(), Some("landed"));
    fly(&mut w, &mut s, 1100);
    let goto = serde_json::json!({"lat": 12.0, "lon": 77.0, "alt_m": 2.5});
    assert_eq!(err(w.action_args("goto", goto)).as_deref(), Some("landed"));
    assert_eq!(err(w.action("rth")).as_deref(), Some("landed"));
    fly(&mut w, &mut s, 3000);
    assert_eq!(w.mode, copter::LAND, "the FC stays in its land");
    assert!(!s.lifted, "nothing climbed off the ground");
    assert_eq!(leave_or_climb(&w, from), Vec::<String>::new());
    let st = w.p.state(w.now);
    assert_eq!(st.landed, Some(true));
    assert_eq!(st.armed, Some(true));
}

#[test]
fn a_take_off_waits_for_the_fc_to_finish_its_land() {
    let (mut w, mut s) = hovering(4.0, true);
    w.action("land");
    fly_to_touchdown(&mut w, &mut s);
    fly(&mut w, &mut s, 300);
    assert_eq!(err(w.action("takeoff")).as_deref(), Some("bad_state"));
    assert_eq!(w.mode, copter::LAND);
    fly(&mut w, &mut s, 1200);
    assert!(w.landed, "the FC's land detector finished");
    // A deliberate take-off from the ground is the one way back up.
    assert_eq!(err(w.action("takeoff")), None);
    w.run(1500);
    assert_eq!(w.mode, copter::GUIDED);
    assert!(w.commands(MavCmd::MAV_CMD_NAV_TAKEOFF, None) > 0);
}

#[test]
fn an_fc_land_reaching_the_floor_refuses_pause() {
    // The operator's Land while the FC flies (LOITER): the FC lands, and
    // the floor itself starts the touchdown.
    let (mut w, mut s) = hovering(5.0, false);
    w.mode = copter::LOITER;
    w.run(100);
    assert_eq!(w.p.owner(), Owner::FcMode);
    w.action("land");
    assert_eq!(w.mode, copter::LAND);
    fly_to_touchdown(&mut w, &mut s);
    fly(&mut w, &mut s, 300);
    assert_eq!(err(w.action("pause")).as_deref(), Some("landed"));
    assert_eq!(w.mode, copter::LAND);
    assert!(!s.lifted);
}

#[test]
fn a_climb_off_the_ground_ends_the_touchdown() {
    let (mut w, mut s) = hovering(4.0, false);
    w.action("land");
    fly_to_touchdown(&mut w, &mut s);
    fly(&mut w, &mut s, 300);
    assert_eq!(w.p.state(w.now).landed, Some(true));
    // An RC pilot switches to LOITER and climbs.
    w.mode = copter::LOITER;
    w.pos[2] = -1.5;
    w.agl = Some(RF_GROUND_M + 1.5);
    w.run(100);
    assert_eq!(w.p.state(w.now).landed, Some(false));
    assert_eq!(err(w.action("pause")), None);
    assert_eq!(w.mode, copter::GUIDED);
}

/// Land from `h` m with the FC's landing parameter `param`: the vertical
/// speeds at the hand-over and at ground contact, and the fastest descent
/// commanded.
fn land_profile(h: f32, param: Option<(&str, f32)>) -> (f32, f32, f32) {
    let (mut w, mut s) = hovering(h, false);
    w.p.fc.params.remove("LAND_SPD_MS");
    if let Some((n, v)) = param {
        w.p.fc.params.insert(n.into(), v);
    }
    s.land_spd = match param {
        Some(("LAND_SPEED", cm)) => cm / 100.0,
        Some((_, v)) => v,
        None => 0.5,
    };
    w.action("land");
    fly_to_touchdown(&mut w, &mut s);
    let out = (
        s.handover_vd.expect("handed over to LAND"),
        s.contact_vd.unwrap(),
        s.max_cmd_vd,
    );
    println!(
        "land from {h} m, {param:?}: hand-over {:.2} m/s, touchdown {:.2} m/s, fastest {:.2} m/s",
        out.0, out.1, out.2
    );
    out
}

#[test]
fn land_slows_to_the_fc_landing_speed_before_the_hand_over() {
    // The bench: 4 m, LAND_SPD_MS 0.3 m/s.
    let (handover, contact, max_cmd) = land_profile(4.0, Some(("LAND_SPD_MS", 0.3)));
    assert!(handover <= 0.45, "hand-over at {handover:.2} m/s");
    assert!(contact <= 0.4, "touchdown at {contact:.2} m/s");
    assert!(
        max_cmd > 1.5,
        "the descent still starts fast ({max_cmd:.2} m/s)"
    );
    // Higher up the descent reaches the profile's full speed first.
    let (handover, contact, max_cmd) = land_profile(15.0, Some(("LAND_SPD_MS", 0.3)));
    assert!(
        handover <= 0.45 && contact <= 0.4,
        "{handover:.2} {contact:.2}"
    );
    assert!((max_cmd - 2.0).abs() < 0.01, "{max_cmd:.2}");
    // Older firmware's LAND_SPEED is in cm/s; without either, 0.5 m/s.
    for p in [Some(("LAND_SPEED", 50.0)), None] {
        let (handover, contact, _) = land_profile(4.0, p);
        assert!(
            handover <= 0.65 && contact <= 0.6,
            "{p:?}: {handover:.2} {contact:.2}"
        );
    }
}
