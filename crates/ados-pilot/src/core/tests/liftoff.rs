//! The supervised lift-off on an ArduPilot multirotor: the FC set up for it,
//! the launch flown with attitude targets instead of the FC's take-off, the
//! phases on the pilot state, and every way it stops on the ground.

use ados_protocol::mavlink::ardupilotmega::*;
use serde_json::json;

use super::harness::World;
use crate::core::Reply;
use crate::liftoff::sim::Airframe;
use crate::modes::copter;

fn params_set(w: &World, name: &str) -> Vec<f32> {
    w.sent
        .iter()
        .filter_map(|m| match m {
            MavMessage::PARAM_SET(p)
                if p.param_id
                    .to_str()
                    .is_ok_and(|s| s.trim_end_matches('\0') == name) =>
            {
                Some(p.param_value)
            }
            _ => None,
        })
        .collect()
}

fn attitude_targets(w: &World) -> usize {
    w.sent
        .iter()
        .filter(|m| matches!(m, MavMessage::SET_ATTITUDE_TARGET(_)))
        .count()
}

/// Force disarms (21196) the pilot sent.
fn force_disarms(w: &World) -> usize {
    w.sent
        .iter()
        .filter(|m| {
            matches!(m, MavMessage::COMMAND_LONG(c)
                if c.command == MavCmd::MAV_CMD_COMPONENT_ARM_DISARM
                    && c.param1 < 0.5 && c.param2 == 21196.0)
        })
        .count()
}

/// A test world whose operator surface stays connected through a long
/// lift-off (a real cockpit streams to the pilot all along).
fn world() -> World {
    let mut w = World::new();
    w.p.settings.pilot.link_loss_s = 300;
    w.p.settings.pilot.supervised_liftoff = true;
    w
}

fn launch(w: &mut World) {
    let r = w.action_args("launch", json!({"after": "hold", "alt_m": 3.0}));
    assert!(matches!(&r, Reply::Action(a) if a.ok), "{r:?}");
}

fn liftoff_phase(w: &World) -> Option<String> {
    w.p.state(w.now)
        .sequence
        .and_then(|s| s.liftoff)
        .map(|l| l.phase)
}

/// Run until the lift-off reaches `phase` (at most `ms`).
fn run_to_phase(w: &mut World, phase: &str, ms: u64) {
    let end = w.now + ms;
    while w.now < end && liftoff_phase(w).as_deref() != Some(phase) {
        w.run(20);
    }
    assert_eq!(liftoff_phase(w).as_deref(), Some(phase));
}

#[test]
fn a_launch_lifts_off_under_the_pilot_not_the_fc_take_off() {
    let mut w = world();
    // The operator's own GUID_OPTIONS bit survives.
    w.p.fc.params.insert("GUID_OPTIONS".into(), 1.0);
    launch(&mut w);
    w.run(500);
    assert_eq!(params_set(&w, "GUID_OPTIONS"), vec![9.0]);
    assert_eq!(params_set(&w, "GCS_PID_MASK"), vec![3.0]);
    run_to_phase(&mut w, "spool", 3000);
    assert!(w.armed);
    run_to_phase(&mut w, "lift", 6000);
    w.run(15_000);
    assert_eq!(w.commands(MavCmd::MAV_CMD_NAV_TAKEOFF, None), 0);
    assert!(attitude_targets(&w) > 50);
    assert!(
        -w.pos[2] > 2.6,
        "climbed to the launch height: {}",
        -w.pos[2]
    );
    // Handed on to the pilot's hold; the launch finished without an error.
    assert_eq!(w.p.owner(), ados_protocol::pilot::Owner::Pilot);
    assert!(w.p.banner.is_none(), "{:?}", w.p.banner);
}

#[test]
fn the_switch_off_flies_the_fc_take_off_and_says_so() {
    let mut w = World::new();
    w.p.settings.pilot.supervised_liftoff = false;
    let item =
        w.p.readiness(w.now)
            .into_iter()
            .find(|r| r.id == "supervised_liftoff")
            .expect("shown while off");
    assert!(!item.ok);
    launch(&mut w);
    w.run(3000);
    assert!(w.commands(MavCmd::MAV_CMD_NAV_TAKEOFF, None) > 0);
    assert_eq!(attitude_targets(&w), 0);
    assert!(params_set(&w, "GUID_OPTIONS").is_empty());
    w.p.settings.pilot.supervised_liftoff = true;
    assert!(w
        .p
        .readiness(w.now)
        .iter()
        .all(|r| r.id != "supervised_liftoff"));
}

#[test]
fn an_fc_that_never_takes_the_setting_is_never_armed() {
    let mut w = world();
    w.answers_params = false;
    launch(&mut w);
    w.run(8000);
    assert!(!w.armed);
    let s = w.p.state(w.now).sequence;
    assert_eq!(
        s.and_then(|s| s.error).as_deref(),
        Some("liftoff_fc_params")
    );
}

#[test]
fn the_phases_read_in_operator_words() {
    let mut w = world().with_airframe(Airframe::new(0.5, [0.0, 0.2]));
    launch(&mut w);
    run_to_phase(&mut w, "spool", 3000);
    let l = w.p.state(w.now).sequence.unwrap().liftoff.unwrap();
    assert_eq!(l.text, "Spinning up");
    assert_eq!(l.trim, "probe");
    run_to_phase(&mut w, "lift", 6000);
    run_to_phase(&mut w, "probe", 10_000);
    assert_eq!(
        w.p.state(w.now).sequence.unwrap().liftoff.unwrap().text,
        "Checking balance"
    );
}

/// An operator stop in each ground phase spins down and force-disarms
/// (ground contact confirmed); the same stop in the climb holds instead.
#[test]
fn pause_on_the_ground_stops_and_disarms_in_every_phase() {
    for (phase, airframe) in [
        ("spool", Airframe::balanced()),
        ("lift", Airframe::balanced()),
        ("probe", Airframe::new(0.5, [0.0, 0.2])),
    ] {
        for action in ["pause", "land", "rth"] {
            let mut w = world().with_airframe(airframe.clone());
            launch(&mut w);
            run_to_phase(&mut w, phase, 15_000);
            let r = w.action(action);
            assert!(
                matches!(&r, Reply::Action(a) if a.ok),
                "{phase} {action}: {r:?}"
            );
            w.run(1000);
            assert_eq!(force_disarms(&w), 1, "{phase} {action}");
            assert!(!w.armed, "{phase} {action}");
            assert_eq!(
                liftoff_phase(&w).as_deref(),
                Some("stopped"),
                "{phase} {action}"
            );
            let s = w.p.state(w.now).sequence.unwrap();
            assert_eq!(
                s.error.as_deref(),
                Some("liftoff_operator"),
                "{phase} {action}"
            );
        }
    }
}

#[test]
fn pause_in_the_climb_holds_in_the_air() {
    let mut w = world();
    launch(&mut w);
    run_to_phase(&mut w, "climb", 12_000);
    w.action("pause");
    w.run(1000);
    assert_eq!(force_disarms(&w), 0);
    assert!(w.armed);
    assert_eq!(w.mode, copter::GUIDED);
}

#[test]
fn a_lost_operator_link_mid_spool_stops_and_disarms() {
    let mut w = World::new();
    w.p.settings.pilot.link_loss_s = 2;
    w.p.settings.pilot.supervised_liftoff = true;
    launch(&mut w);
    run_to_phase(&mut w, "spool", 3000);
    // Nothing more from any surface.
    w.run(3000);
    assert_eq!(force_disarms(&w), 1);
    let s = w.p.state(w.now).sequence.unwrap();
    assert_eq!(s.error.as_deref(), Some("liftoff_link"));
}

#[test]
fn a_motor_out_of_headroom_stops_on_the_ground_and_says_so() {
    // nose50: no motor headroom left at the thrust it needs to lift.
    let mut w = world().with_airframe(Airframe::new(0.62, [0.0, 0.42]));
    launch(&mut w);
    run_to_phase(&mut w, "stopped", 40_000);
    w.run(1000);
    assert!(w.pos[2] > -0.05, "never lifted");
    assert_eq!(force_disarms(&w), 1);
    let s = w.p.state(w.now).sequence.unwrap();
    assert_eq!(s.error.as_deref(), Some("liftoff_saturated"));
    let l = s.liftoff.unwrap();
    assert_eq!(l.text, "Take-off stopped");
    assert!(l.detail.contains("No climb margin"), "{}", l.detail);
}

#[test]
fn a_tip_with_no_rate_data_stops_on_the_ground() {
    let mut w = world().with_airframe(Airframe::new(0.5, [0.0, 0.3]));
    w.rates_stream = false;
    launch(&mut w);
    run_to_phase(&mut w, "stopped", 20_000);
    w.run(1000);
    assert!(w.pos[2] > -0.05);
    assert_eq!(force_disarms(&w), 1);
    let s = w.p.state(w.now).sequence.unwrap();
    assert_eq!(s.error.as_deref(), Some("liftoff_no_rate_data"));
}

#[test]
fn without_confirmed_ground_contact_it_lands_instead_of_cutting_the_motors() {
    let mut w = world().with_airframe(Airframe::new(0.5, [0.0, 0.2]));
    launch(&mut w);
    run_to_phase(&mut w, "lift", 8000);
    // The rangefinder reads the vehicle off the ground (a step, a lip).
    w.agl = Some(0.6);
    w.action("pause");
    // The world's airframe keeps the rangefinder honest each tick: hold it.
    for _ in 0..50 {
        w.run(20);
        w.agl = Some(0.6);
    }
    assert_eq!(force_disarms(&w), 0);
    assert_eq!(w.mode, copter::LAND);
}

/// The peak tilt of one launch to the hold, up to 0.5 m.
fn fly_launch(w: &mut World) -> f32 {
    launch(w);
    let mut peak: f32 = 0.0;
    for _ in 0..2000 {
        w.run(20);
        if w.af.h < 0.5 {
            peak = peak.max(w.pitch.abs().max(w.roll.abs()).to_degrees());
        }
    }
    peak
}

#[test]
fn first_and_learned_flights_lift_off_level_on_an_unbalanced_airframe() {
    // The hover estimate (MOT_THST_HOVER 0.5) is short of the airframe's.
    let mut w = world().with_airframe(Airframe::new(0.55, [0.0, 0.25]));
    let first = fly_launch(&mut w);
    assert!(first < 5.0, "first flight peak {first}");
    assert!(-w.pos[2] > 2.6);
    let trim = w.p.airframe.trim().expect("the lift-off's trim is kept");
    assert!((trim.i[1] - 0.25).abs() < 0.05, "{trim:?}");
    // Landed and disarmed: the FC's integrators relax to zero.
    w.armed = false;
    w.landed = true;
    w.pos = [0.0; 3];
    w.af = Airframe::new(0.55, [0.0, 0.25]);
    w.run(2000);
    let probes_before = w.p.state(w.now).sequence.is_none();
    assert!(probes_before);
    let second = fly_launch(&mut w);
    assert!(second < 5.0, "learned flight peak {second}");
    assert!(-w.pos[2] > 2.6);
}
