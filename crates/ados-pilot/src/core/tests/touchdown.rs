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

/// What the downward rangefinder reports.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
enum Rf {
    /// The distance to the ground.
    #[default]
    Valid,
    /// Readings past its maximum (no return), which the pilot drops.
    OutOfRange,
    /// The distance until the height falls under this, then nothing.
    DropBelow(f32),
    /// No rangefinder.
    Absent,
}

#[derive(Default)]
struct Sim {
    /// LAND descent speed (the FC's `LAND_SPD_MS`).
    land_spd: f32,
    /// Whether the FC's land detector ever reports ON_GROUND.
    detector: bool,
    rf: Rf,
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
    if s.on_ground && cmd_vd < -0.05 {
        s.lifted = true;
        s.on_ground = false;
    }
    if s.on_ground {
        w.vel[2] = 0.0;
        s.in_land_ms = if w.mode == copter::LAND {
            s.in_land_ms + 20
        } else {
            0
        };
        if s.detector && s.in_land_ms >= DETECT_MS {
            w.landed = true;
        }
    } else {
        w.vel[2] += (cmd_vd - w.vel[2]) * dt / TAU_S;
        w.pos[2] += w.vel[2] * dt;
        if w.pos[2] >= 0.0 {
            s.contact_vd.get_or_insert(w.vel[2]);
            s.touchdown_ms.get_or_insert(w.now);
            s.on_ground = true;
            w.pos[2] = 0.0;
            w.vel[2] = 0.0;
        }
    }
    let height = -w.pos[2];
    w.agl = match s.rf {
        Rf::Valid => Some(RF_GROUND_M + height),
        Rf::OutOfRange => Some(13.0),
        Rf::DropBelow(h) if height > h => Some(RF_GROUND_M + height),
        Rf::DropBelow(_) | Rf::Absent => None,
    };
}

fn fly(w: &mut World, s: &mut Sim, ms: u64) {
    let end = w.now + ms;
    while w.now < end {
        physics(w, s);
        w.run(20);
    }
}

/// Fly until `done`, at most 60 s; `each` runs after every tick.
fn fly_until(
    w: &mut World,
    s: &mut Sim,
    mut done: impl FnMut(&World, &Sim) -> bool,
    mut each: impl FnMut(&World, &Sim),
) {
    let end = w.now + 60_000;
    while !done(w, s) {
        assert!(w.now < end, "never got there");
        fly(w, s, 20);
        each(w, s);
    }
}

fn fly_to_touchdown(w: &mut World, s: &mut Sim) {
    fly_until(w, s, |_, s| s.touchdown_ms.is_some(), |_, _| {});
}

/// Hovering `h` m above the ground, owner `pilot` in GUIDED, home `home_d`
/// (NED down; the ground is at 0), the FC's landing speed `LAND_SPD_MS` =
/// 0.3 as on the bench.
fn hovering_over(h: f32, detector: bool, rf: Rf, home_d: f32) -> (World, Sim) {
    let mut w = World::new();
    w.p.fc.params.insert("LAND_SPD_MS".into(), 0.3);
    let home = MavMessage::HOME_POSITION(HOME_POSITION_DATA {
        latitude: 120_000_000,
        longitude: 770_000_000,
        z: home_d,
        ..Default::default()
    });
    w.p.on_fc(FC_SYS, FC_COMP, &home, w.now);
    w.airborne(copter::GUIDED);
    w.pos = [0.0, 0.0, -h];
    let mut s = Sim {
        land_spd: 0.3,
        detector,
        rf,
        ..Default::default()
    };
    physics(&mut w, &mut s);
    w.run(200);
    assert_eq!(w.p.owner(), Owner::Pilot);
    (w, s)
}

fn hovering(h: f32, detector: bool) -> (World, Sim) {
    hovering_over(h, detector, Rf::Valid, 0.0)
}

fn err(r: Reply) -> Option<String> {
    match r {
        Reply::Action(a) => a.err,
        other => panic!("{other:?}"),
    }
}

/// Take-offs and setpoints asking for a climb, sent from `from` on.
fn climbs(w: &World, from: usize) -> Vec<String> {
    w.sent[from..]
        .iter()
        .filter_map(|m| match m {
            MavMessage::COMMAND_LONG(c) if c.command == MavCmd::MAV_CMD_NAV_TAKEOFF => {
                Some("takeoff".into())
            }
            MavMessage::SET_POSITION_TARGET_LOCAL_NED(sp)
                if (sp.type_mask.bits() == MASK_VELOCITY && sp.vz < -0.05)
                    || (sp.type_mask.bits() != MASK_VELOCITY && sp.z < -0.3) =>
            {
                Some(format!("setpoint {sp:?}"))
            }
            _ => None,
        })
        .collect()
}

#[test]
fn pause_and_goto_after_a_land_touchdown_are_refused() {
    // The bench sequence: Land from 4 m under `ados`, Pause 0.3 s after
    // touchdown, a goto to 2.5 m and Pause again 2.5 s after it, while the FC
    // has not yet reported the vehicle on the ground.
    let (mut w, mut s) = hovering(4.0, false);
    assert_eq!(err(w.action("land")), None);
    fly_to_touchdown(&mut w, &mut s);
    assert_eq!(w.mode, copter::LAND);
    let from = w.sent.len();
    fly(&mut w, &mut s, 300);
    assert_eq!(err(w.action("pause")).as_deref(), Some("landed"));
    assert_eq!(w.mode, copter::LAND, "a refused pause leaves the FC's land");
    fly(&mut w, &mut s, 2200);
    let goto = serde_json::json!({"lat": 12.0, "lon": 77.0, "alt_m": 2.5});
    assert_eq!(err(w.action_args("goto", goto)).as_deref(), Some("landed"));
    assert_eq!(err(w.action("pause")).as_deref(), Some("landed"));
    fly(&mut w, &mut s, 3000);
    assert!(!s.lifted, "nothing climbed off the ground");
    assert_eq!(climbs(&w, from), Vec::<String>::new());
    assert!(w.p.landed());
    // The published state stays the FC's own.
    assert_eq!(w.p.state(w.now).landed, Some(false));
    assert_eq!(err(w.action("rth")), None, "RTH is never refused");
    assert_eq!(err(w.action("land")), None, "nor is Land");
    assert_eq!(err(w.action("kill")), None, "nor is kill");
}

/// Land from 4 m to a touchdown, then Take control and climb 0.5 m on the
/// sticks.
fn take_control_and_lift() -> (World, Sim) {
    let (mut w, mut s) = hovering(4.0, false);
    w.action("land");
    fly_to_touchdown(&mut w, &mut s);
    fly(&mut w, &mut s, 500);
    assert!(w.p.landed());
    assert_eq!(err(w.action("control_mode:ados")), None);
    // Take control is never refused, and the operator flies from here.
    assert_eq!(err(w.action("take_control")), None);
    assert!(!w.p.landed(), "Take control ends the touchdown");
    // Centred sticks first (the take-over gate), then full up.
    for _ in 0..40 {
        w.input([0.0; 4]);
        fly(&mut w, &mut s, 20);
    }
    let end = w.now + 10_000;
    while -w.pos[2] < 0.5 {
        assert!(w.now < end, "never lifted");
        w.input([0.0, 0.0, 1.0, 0.0]);
        fly(&mut w, &mut s, 20);
    }
    w.input([0.0; 4]);
    fly(&mut w, &mut s, 200);
    assert!(s.lifted && !w.landed);
    assert!(!w.p.landed());
    (w, s)
}

#[test]
fn a_short_lift_after_take_control_pauses_and_fails_safe() {
    // Pause holds in the air.
    let (mut w, _) = take_control_and_lift();
    assert_eq!(err(w.action("pause")), None);
    assert_eq!(w.mode, copter::GUIDED);

    // The surface link goes quiet: the link-loss RTH fires.
    let (mut w, mut s) = take_control_and_lift();
    let silent = w.now;
    fly_until(&mut w, &mut s, |w, _| w.mode == copter::RTL, |_, _| {});
    assert!(w.now - silent <= 10_500, "link loss after its window");

    // The battery runs low: the low-battery RTH fires.
    let (mut w, mut s) = take_control_and_lift();
    let low = MavMessage::SYS_STATUS(SYS_STATUS_DATA {
        battery_remaining: 10,
        ..Default::default()
    });
    w.p.on_fc(FC_SYS, FC_COMP, &low, w.now);
    // The surface stays linked throughout.
    let end = w.now + 15_000;
    while w.mode != copter::RTL {
        assert!(w.now < end, "no low-battery RTH");
        w.req(crate::core::Request::Hb { src: "mc".into() });
        fly(&mut w, &mut s, 20);
    }
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
    assert!(w.p.touchdown.floor_d.is_some());
    // A deliberate take-off from the ground ends the touchdown.
    assert_eq!(err(w.action("takeoff")), None);
    assert!(w.p.touchdown.floor_d.is_none());
    w.run(1500);
    assert_eq!(w.mode, copter::GUIDED);
    assert!(w.commands(MavCmd::MAV_CMD_NAV_TAKEOFF, None) > 0);
}

#[test]
fn the_fc_leaving_the_ground_ends_the_touchdown() {
    let (mut w, mut s) = hovering(4.0, true);
    w.action("land");
    fly_to_touchdown(&mut w, &mut s);
    fly(&mut w, &mut s, 1500);
    assert!(w.landed && w.p.touchdown.floor_d.is_some());
    // The FC reports the vehicle in the air again, with no climb seen yet.
    w.landed = false;
    w.run(40);
    assert!(!w.p.landed());
    assert_eq!(err(w.action("pause")), None);
}

#[test]
fn an_fc_land_reaching_the_floor_refuses_pause() {
    // The operator's Land while the FC flies (LOITER): the pilot puts the FC
    // in LAND, and the rangefinder at the floor starts the touchdown.
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
    // Half a metre up, seen by the local position alone (no rangefinder) or
    // by the rangefinder alone.
    for (d, agl) in [(-0.5, None), (0.0, Some(RF_GROUND_M + 0.5))] {
        let (mut w, mut s) = hovering(4.0, false);
        w.action("land");
        fly_to_touchdown(&mut w, &mut s);
        fly(&mut w, &mut s, 300);
        assert!(w.p.landed());
        // An RC pilot switches to LOITER and climbs.
        w.mode = copter::LOITER;
        w.pos[2] = d;
        w.agl = agl;
        w.run(100);
        assert!(!w.p.landed(), "{d} {agl:?}");
        assert_eq!(err(w.action("pause")), None);
        assert_eq!(w.mode, copter::GUIDED);
    }
}

/// Land from `h` m with no usable rangefinder reading near the ground: the
/// pilot never reads the vehicle as landed before the FC does.
fn land_without_floor_evidence(h: f32, rf: Rf, home_d: f32) -> (World, Sim) {
    let (mut w, mut s) = hovering_over(h, true, rf, home_d);
    assert_eq!(err(w.action("land")), None);
    fly_until(
        &mut w,
        &mut s,
        |w, _| w.landed,
        |w, _| {
            let early = w.p.landed() && !w.landed;
            assert!(!early, "{rf:?}: landed before the FC at {:?}", w.pos);
            assert_eq!(w.p.state(w.now).landed, Some(w.landed));
        },
    );
    assert!(w.p.landed(), "the FC's ON_GROUND is evidence");
    (w, s)
}

#[test]
fn no_touchdown_without_a_rangefinder_reading_at_the_floor() {
    // 12 m with a rangefinder past its range all the way down, and 4 m with
    // one that drops out at 2 m: the floor is never read from home height.
    land_without_floor_evidence(12.0, Rf::OutOfRange, 0.0);
    land_without_floor_evidence(4.0, Rf::DropBelow(2.0), 0.0);
    // After touchdown, before the FC's ON_GROUND, nothing is refused.
    for action in ["take_control", "rth", "pause"] {
        let (mut w, mut s) = hovering_over(12.0, false, Rf::OutOfRange, 0.0);
        w.action("land");
        fly_to_touchdown(&mut w, &mut s);
        fly(&mut w, &mut s, 500);
        assert!(!w.p.landed());
        assert_eq!(err(w.action(action)), None, "{action}");
    }
}

#[test]
fn ground_below_home_never_reads_as_the_floor() {
    // Home 3 m above the ground (a roof), no rangefinder: 5 m up reads 2 m
    // above home, so the pilot hands over to LAND 3.6 m above the ground.
    let (_, s) = land_without_floor_evidence(5.0, Rf::Absent, -3.0);
    assert!(s.handover_vd.is_some());
    // Mid-way down the FC's LAND every action stays available.
    for action in ["rth", "take_control", "pause"] {
        let (mut w, mut s) = hovering_over(5.0, false, Rf::Absent, -3.0);
        w.action("land");
        fly_until(
            &mut w,
            &mut s,
            |w, _| w.mode == copter::LAND && w.pos[2] > -2.0,
            |_, _| {},
        );
        assert!(!w.p.landed());
        assert_eq!(err(w.action(action)), None, "{action}");
    }
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
