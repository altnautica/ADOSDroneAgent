//! The air wizard's take-off on an ArduPilot multirotor is the supervised
//! lift-off: the FC set up as the pilot's launch sets it up, no NAV_TAKEOFF,
//! a level lift-off on an unbalanced airframe, and a stop on the ground that
//! fails the step with the stop's words and reaches airframe health.

use ados_protocol::mavlink::ardupilotmega::{MavCmd, MavMessage, MavType};
use serde_json::json;

use super::{ap_copter_params, Sim, A, SETUP_STRINGS};
use crate::liftoff::sim::Airframe;
use crate::setup::out::Effect;
use crate::setup::schema::{StepState, WizardState};

/// A copter with the ground wizard done and `af` on its skids, its take-off
/// supervised.
fn lift_ready(af: Airframe) -> Sim {
    let mut p = ap_copter_params();
    p.insert("GUID_OPTIONS".into(), 0.0);
    p.insert("GCS_PID_MASK".into(), 0.0);
    let mut s = Sim::new(MavType::MAV_TYPE_QUADROTOR, p);
    s.e.state.ground.state = WizardState::Done;
    s.e.state.data.prop_in = Some(10.0);
    s.e.state.data.motor_kv = Some(900.0);
    s.e.state.data.cells = Some(4.0);
    s.fc.inputs.supervised_liftoff = true;
    s.fc.inputs.liftoff_hover = af.hover;
    s.fc.airframe = Some(af);
    s
}

fn start_hover(s: &mut Sim) {
    assert_eq!(s.e.run_step(A, "first_hover", s.now)["ok"], json!(true));
    s.run(100);
    s.act(A, "first_hover", "start", json!({}));
}

fn attitude_targets(s: &Sim) -> usize {
    s.fc.sent
        .iter()
        .filter(|m| matches!(m, MavMessage::SET_ATTITUDE_TARGET(_)))
        .count()
}

fn outcomes(s: &Sim) -> Vec<crate::airframe::LiftoffOutcome> {
    s.fc.effects
        .iter()
        .filter_map(|e| match e {
            Effect::LiftoffOutcome(o) => Some(*o),
            _ => None,
        })
        .collect()
}

#[test]
fn first_hover_lifts_a_nose_heavy_copter_off_level() {
    let mut s = lift_ready(Airframe::new(0.5, [0.0, 0.2]));
    start_hover(&mut s);
    let mut tilt_on_skids = 0.0f32;
    let end = s.now + 60_000;
    // The hop counts its 3 m reached at 90 % and holds where it is then.
    while s.fc.alt < 2.7 {
        assert!(s.now < end, "never reached the hover height");
        s.run(20);
        let af = s.fc.airframe.as_ref().unwrap();
        if af.h == 0.0 {
            tilt_on_skids = tilt_on_skids.max(af.att[0].abs().max(af.att[1].abs()).to_degrees());
        }
    }
    assert!(
        s.sent_cmds(MavCmd::MAV_CMD_NAV_TAKEOFF).is_empty(),
        "the FC's own take-off was never asked for"
    );
    assert!(attitude_targets(&s) > 0);
    assert!(tilt_on_skids < 5.0, "tilted {tilt_on_skids}° on the skids");
    // The FC was set up as the pilot's launch sets it up.
    assert_eq!(s.param("GUID_OPTIONS") as u32 & 8, 8);
    assert_eq!(s.param("GCS_PID_MASK") as u32 & 3, 3);
    let intervals = s.sent_cmds(MavCmd::MAV_CMD_SET_MESSAGE_INTERVAL);
    for id in [194.0, 36.0] {
        assert!(
            intervals.iter().any(|p| p[0] == id && p[1] == 40_000.0),
            "{id} at 25 Hz: {intervals:?}"
        );
    }
    // What it found reaches airframe health.
    let found = outcomes(&s);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].i.is_some() && !found[0].saturated);

    s.until(|s| s.e.current_view().is_none(), 80_000);
    assert_eq!(s.step_state(A, "first_hover"), StepState::Done);
    assert!(!s.fc.armed);
}

#[test]
fn a_lift_off_out_of_motor_headroom_fails_the_step_on_the_ground() {
    // nose50: no motor headroom left at the thrust it needs to lift.
    let mut s = lift_ready(Airframe::new(0.62, [0.0, 0.42]));
    start_hover(&mut s);
    s.until(
        |s| s.step_state(A, "first_hover") == StepState::Failed,
        60_000,
    );
    s.run(1000);
    assert!(!s.fc.armed);
    assert!(s.fc.alt < 0.05, "never lifted");
    assert!(s.sent_cmds(MavCmd::MAV_CMD_NAV_TAKEOFF).is_empty());
    // Spun down and cut on its skids: one forced disarm.
    let forced = s
        .sent_cmds(MavCmd::MAV_CMD_COMPONENT_ARM_DISARM)
        .iter()
        .filter(|p| p[0] == 0.0 && p[1] == 21196.0)
        .count();
    assert_eq!(forced, 1);
    let rec = &s.e.state.air.steps["first_hover"];
    assert_eq!(rec["blockers"], json!(["setup.blocker.liftoff_saturated"]));
    assert!(SETUP_STRINGS["strings"]["setup.blocker.liftoff_saturated"]
        .as_str()
        .is_some_and(|t| t.starts_with("Take-off stopped. ")));
    // The saturation stop reaches airframe health.
    assert!(outcomes(&s).iter().any(|o| o.saturated));
}

#[test]
fn with_the_switch_off_the_fc_takes_off_on_its_own() {
    let mut s = lift_ready(Airframe::balanced());
    s.fc.inputs.supervised_liftoff = false;
    start_hover(&mut s);
    s.run(8000);
    assert!(!s.sent_cmds(MavCmd::MAV_CMD_NAV_TAKEOFF).is_empty());
    assert_eq!(attitude_targets(&s), 0);
}
