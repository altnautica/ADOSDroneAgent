//! The router's mission state in the pilot: the launch "when done" on mission
//! completion, and `resume` of a paused mission (owner `mission_fc`).

use ados_protocol::mavlink::ardupilotmega::MavCmd;
use ados_protocol::pilot::Owner;
use serde_json::json;

use super::harness::World;
use crate::core::Reply;
use crate::modes::copter;

fn ok(r: &Reply) -> bool {
    matches!(r, Reply::Action(a) if a.ok)
}

/// Launch into the mission (`after: mission`) with `when_done`, up to the
/// point the FC flies it in AUTO.
fn launched_into_mission(when_done: Option<&str>) -> World {
    let mut w = World::new();
    let mut args = json!({"after": "mission", "alt_m": 3.0});
    if let Some(wd) = when_done {
        args["when_done"] = json!(wd);
    }
    assert!(ok(&w.action_args("launch", args)));
    w.mission("loaded", 0, 4);
    w.run(3000);
    assert!(w.armed);
    assert_eq!(w.mode, copter::AUTO);
    assert_eq!(w.p.owner(), Owner::MissionFc);
    assert_eq!(w.p.state(w.now).sequence.unwrap().step, "done");
    w
}

#[test]
fn launched_mission_runs_when_done_rth_once_on_complete() {
    let mut w = launched_into_mission(Some("rth"));
    for seq in 1..4 {
        w.mission("active", seq, 4);
        w.run(200);
    }
    assert_eq!(w.mode, copter::AUTO, "nothing before the mission completes");
    assert_eq!(w.set_modes(copter::RTL), 0);
    w.mission("complete", 3, 4);
    w.run(100);
    assert_eq!(w.mode, copter::RTL);
    assert_eq!(w.p.owner(), Owner::FcMode);
    // The router keeps reporting `complete` (latched), and a reconnect starts
    // from an unknown state: the action still ran once.
    for _ in 0..10 {
        w.mission("complete", 3, 4);
        w.run(100);
    }
    w.p.on_feed("mission", &serde_json::Value::Null, w.now);
    w.mission("complete", 3, 4);
    w.run(100);
    assert_eq!(w.set_modes(copter::RTL), 1);
    assert_eq!(w.p.state(w.now).mission.unwrap().state, "complete");
}

#[test]
fn launched_mission_with_when_done_land_lands_on_complete() {
    let mut w = launched_into_mission(Some("land"));
    w.mission("active", 2, 4);
    w.run(200);
    w.mission("complete", 3, 4);
    w.run(100);
    // The FC was flying the mission, so it lands in its LAND mode.
    assert_eq!(w.mode, copter::LAND);
    assert_eq!(w.p.owner(), Owner::FcMode);
    assert_eq!(w.set_modes(copter::LAND), 1);
    assert_eq!(w.set_modes(copter::RTL), 0);
}

#[test]
fn no_when_done_without_a_launch_that_armed_one() {
    // A mission started outside a launch sequence.
    let mut w = World::new();
    w.airborne(copter::AUTO);
    w.run(40);
    assert_eq!(w.p.owner(), Owner::MissionFc);
    w.mission("active", 3, 4);
    w.run(200);
    w.mission("complete", 3, 4);
    w.run(500);
    assert_eq!(w.mode, copter::AUTO);
    assert_eq!(w.p.owner(), Owner::MissionFc);
    assert_eq!(w.set_modes(copter::RTL) + w.set_modes(copter::LAND), 0);

    // A launch into the mission without "when done".
    let mut w = launched_into_mission(None);
    w.mission("active", 3, 4);
    w.run(200);
    w.mission("complete", 3, 4);
    w.run(500);
    assert_eq!(w.mode, copter::AUTO);
    assert_eq!(w.set_modes(copter::RTL) + w.set_modes(copter::LAND), 0);

    // A launch into a hold, with "when done", and a mission started later:
    // "when done" belongs to the launch's own after-takeoff action.
    let mut w = World::new();
    let args = json!({"after": "hold", "alt_m": 3.0, "when_done": "rth"});
    assert!(ok(&w.action_args("launch", args)));
    w.run(3000);
    assert_eq!(w.p.owner(), Owner::Pilot);
    w.mode = copter::AUTO;
    w.run(100);
    assert_eq!(w.p.owner(), Owner::MissionFc);
    w.mission("active", 3, 4);
    w.run(200);
    w.mission("complete", 3, 4);
    w.run(500);
    assert_eq!(w.mode, copter::AUTO);
    assert_eq!(w.set_modes(copter::RTL), 0);
}

#[test]
fn a_mission_completing_on_the_ground_needs_no_when_done() {
    let mut w = launched_into_mission(Some("rth"));
    w.mission("active", 3, 4);
    w.run(200);
    // The last item landed the aircraft; it is still armed.
    w.landed = true;
    w.agl = Some(0.1);
    w.pos[2] = 0.0;
    w.run(100);
    w.mission("complete", 4, 4);
    w.run(200);
    assert_eq!(w.set_modes(copter::RTL), 0);
    assert_eq!(w.mode, copter::AUTO);
}

#[test]
fn pause_in_mission_then_resume_continues_in_auto() {
    let mut w = World::new();
    w.airborne(copter::AUTO);
    w.run(40);
    w.mission("active", 2, 5);
    assert!(ok(&w.action("pause")));
    assert_eq!(
        w.commands(MavCmd::MAV_CMD_DO_PAUSE_CONTINUE, Some((0, 0.0))),
        1
    );
    w.mission("paused", 2, 5);
    assert!(ok(&w.action("resume")));
    assert_eq!(
        w.commands(MavCmd::MAV_CMD_DO_PAUSE_CONTINUE, Some((0, 1.0))),
        1
    );
    assert_eq!(w.mode, copter::AUTO);
    assert_eq!(w.p.owner(), Owner::MissionFc);
}

#[test]
fn mission_paused_in_a_hold_mode_resumes_with_auto() {
    let mut w = World::new();
    w.airborne(copter::AUTO);
    w.run(40);
    w.mission("active", 3, 5);
    // The RC switch (or the FC) puts it in LOITER mid-mission: the router
    // reports the mission paused; the pilot itself paused nothing.
    w.mode = copter::LOITER;
    w.run(100);
    assert_eq!(w.p.owner(), Owner::FcMode);
    w.mission("paused", 3, 5);
    let before = w.set_modes(copter::AUTO);
    assert!(ok(&w.action("resume")));
    assert_eq!(w.set_modes(copter::AUTO), before + 1, "DO_SET_MODE AUTO");
    assert_eq!(
        w.commands(MavCmd::MAV_CMD_DO_PAUSE_CONTINUE, None),
        0,
        "the FC carries on from its current item"
    );
    assert_eq!(w.p.owner(), Owner::MissionFc);
    w.run(100);
    assert_eq!(w.mode, copter::AUTO);
    assert_eq!(w.p.owner(), Owner::MissionFc);
}

#[test]
fn stick_takeover_in_mission_then_resume_sets_auto() {
    let mut w = World::new();
    w.airborne(copter::AUTO);
    w.run(40);
    w.mission("active", 2, 4);
    w.stick([0.0, 0.5, 0.0, 0.0], 400);
    assert_eq!(w.mode, copter::GUIDED);
    assert_eq!(w.p.owner(), Owner::Pilot);
    // GUIDED is not a hold mode: the router reports the mission loaded.
    w.mission("loaded", 2, 4);
    w.stick([0.0; 4], 600);
    let before = w.set_modes(copter::AUTO);
    assert!(ok(&w.action("resume")));
    assert_eq!(w.set_modes(copter::AUTO), before + 1);
    assert_eq!(w.mode, copter::AUTO);
    assert_eq!(w.p.owner(), Owner::MissionFc);
}

#[test]
fn nothing_to_resume_once_the_mission_is_done_or_unknown() {
    let mut w = World::new();
    w.airborne(copter::AUTO);
    w.run(40);
    w.mission("active", 2, 4);
    w.stick([0.0, 0.5, 0.0, 0.0], 400);
    assert_eq!(w.p.owner(), Owner::Pilot);
    // The plan was replaced and the router reports none: the pilot's own
    // pause is stale.
    w.mission("none", 0, 0);
    let r = w.action("resume");
    assert!(matches!(r, Reply::Action(a) if a.err.as_deref() == Some("nothing_paused")));
    assert_eq!(w.mode, copter::GUIDED);

    // A hold mode with no mission state from the router resumes nothing.
    let mut w = World::new();
    w.airborne(copter::LOITER);
    w.run(40);
    let r = w.action("resume");
    assert!(matches!(r, Reply::Action(a) if a.err.as_deref() == Some("nothing_paused")));
    assert_eq!(w.mode, copter::LOITER);
}
