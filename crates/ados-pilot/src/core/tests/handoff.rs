//! The FC flying on its own: a ground station's reposition outside `ados`,
//! an operator's RTL or land against a failsafe, and Pause over either.

use ados_protocol::mavlink::ardupilotmega::{
    MavCmd, MavMessage, MavResult, COMMAND_ACK_DATA, HOME_POSITION_DATA, RC_CHANNELS_DATA,
};
use ados_protocol::pilot::{Owner, Request, CLIENT_MODE_TOPIC};
use serde_json::{json, Value};

use super::harness::{World, FC_COMP, FC_SYS};
use crate::core::Reply;
use crate::modes::copter;

fn ok(r: &Reply) -> bool {
    matches!(r, Reply::Action(a) if a.ok)
}

fn err(r: &Reply) -> Option<&str> {
    match r {
        Reply::Action(a) => a.err.as_deref(),
        _ => None,
    }
}

fn setpoints(w: &World) -> usize {
    w.sent
        .iter()
        .filter(|m| matches!(m, MavMessage::SET_POSITION_TARGET_LOCAL_NED(_)))
        .count()
}

/// The FC's `COMMAND_ACK` for a ground station's accepted `DO_REPOSITION`.
fn reposition_accepted(w: &mut World) {
    let ack = MavMessage::COMMAND_ACK(COMMAND_ACK_DATA {
        command: MavCmd::MAV_CMD_DO_REPOSITION,
        result: MavResult::MAV_RESULT_ACCEPTED,
    });
    w.p.on_fc(FC_SYS, FC_COMP, &ack, w.now);
    w.collect();
}

/// The router saw a ground station send the FC a mode command.
fn client_mode(w: &mut World, mode: Value) {
    w.p.on_feed(CLIENT_MODE_TOPIC, &json!({ "mode": mode }), w.now);
}

fn home(w: &mut World) {
    let m = MavMessage::HOME_POSITION(HOME_POSITION_DATA {
        latitude: 120_000_000,
        longitude: 770_000_000,
        ..Default::default()
    });
    w.p.on_fc(FC_SYS, FC_COMP, &m, w.now);
}

fn point() -> Value {
    json!({"lat": 12.0009, "lon": 77.0, "alt_m": 6.0})
}

fn redirected_goto() -> Request {
    Request::Goto {
        n: 20.0,
        e: 0.0,
        d: -5.0,
        yaw: None,
    }
}

/// Outside `ados` the pilot yields to the FC's accepted reposition; the
/// operator's Pause then brakes the vehicle as the pilot again, instead of
/// answering ok and leaving it flying.
#[test]
fn pause_brakes_a_ground_station_reposition_outside_ados() {
    for mode in ["fc_manual", "elrs"] {
        let mut w = World::new();
        w.airborne(copter::LOITER);
        assert!(ok(&w.action(&format!("control_mode:{mode}"))), "{mode}");
        assert!(ok(&w.action("take_control")), "{mode}");
        w.run(300);
        reposition_accepted(&mut w);
        w.vel = [4.0, 0.0, 0.0];
        w.run(200);
        assert_eq!(w.p.owner(), Owner::FcMode, "{mode}");
        w.sent.clear();
        let r = w.action("pause");
        assert!(ok(&r), "{mode}: {r:?}");
        assert_eq!(w.p.owner(), Owner::Pilot, "{mode}");
        w.run(200);
        assert_eq!(w.mode, copter::GUIDED, "{mode}");
        assert!(
            setpoints(&w) >= 1,
            "{mode}: pause sent nothing; the FC keeps flying its reposition"
        );
    }
}

/// Pause in a mode the FC flies by itself (here LOITER) takes the vehicle
/// into GUIDED and holds it, in every control mode.
#[test]
fn pause_in_an_fc_mode_holds_as_the_pilot() {
    for mode in ["ados", "fc_manual", "elrs"] {
        let mut w = World::new();
        w.airborne(copter::LOITER);
        assert!(ok(&w.action(&format!("control_mode:{mode}"))), "{mode}");
        w.run(100);
        assert_eq!(w.p.owner(), Owner::FcMode, "{mode}");
        assert!(ok(&w.action("pause")), "{mode}");
        w.run(200);
        assert_eq!(w.mode, copter::GUIDED, "{mode}");
        assert_eq!(w.p.owner(), Owner::Pilot, "{mode}");
        assert!(w.last_setpoint().is_some(), "{mode}");
    }
}

/// Pause answers ok only when it did something: on the ground and during an
/// FC failsafe it is refused, and nothing is sent.
#[test]
fn pause_refused_when_it_does_nothing() {
    let mut w = World::new();
    w.armed = true;
    w.run(200);
    w.sent.clear();
    assert_eq!(err(&w.action("pause")), Some("landed"));
    assert!(w
        .sent
        .iter()
        .all(|m| !matches!(m, MavMessage::COMMAND_LONG(_))));

    let mut w = World::new();
    w.airborne(copter::GUIDED);
    w.run(100);
    w.mode = copter::RTL;
    w.run(100);
    w.sent.clear();
    assert_eq!(err(&w.action("pause")), Some("failsafe"));
    w.run(200);
    assert_eq!(w.mode, copter::RTL, "the FC keeps flying its failsafe");
    assert_eq!(w.set_modes(copter::GUIDED), 0);
}

/// Under `ados` the router hands the pilot a ground station's reposition as
/// a goto. During an FC failsafe RTL or land the pilot ignores it, so the FC
/// stays in its failsafe, and its published state lets the router reject
/// the reposition.
#[test]
fn redirected_reposition_never_leaves_an_fc_failsafe() {
    for mode in [copter::RTL, copter::SMART_RTL, copter::LAND] {
        let mut w = World::new();
        w.airborne(copter::GUIDED);
        w.run(100);
        w.mode = mode;
        w.run(100);
        w.req(redirected_goto());
        w.run(200);
        assert_eq!(w.mode, mode, "the redirected reposition left the failsafe");
        assert_eq!(w.p.owner(), Owner::FcMode, "mode {mode}");
    }
}

/// `start_goto` is the one entry for every goto (a surface point, a
/// redirected reposition, a launch's first leg). It flies nothing during an
/// FC failsafe, disarmed, on the ground, or while the setup engine tunes.
#[test]
fn start_goto_refuses_a_failsafe_the_ground_and_tuning() {
    let target = [20.0, 0.0, -5.0];

    let mut w = World::new();
    w.airborne(copter::GUIDED);
    w.run(100);
    w.mode = copter::RTL;
    w.run(100);
    assert!(w.p.state(w.now).fc_failsafe);
    w.p.start_goto(target, w.now);
    w.run(200);
    assert_eq!(w.p.owner(), Owner::FcMode, "failsafe");
    assert_eq!(w.mode, copter::RTL, "the goto left the failsafe");
    assert_eq!(w.set_modes(copter::GUIDED), 0, "failsafe");

    let mut w = World::new();
    w.run(100);
    w.p.start_goto(target, w.now);
    assert_eq!(w.p.owner(), Owner::Idle, "disarmed");

    let mut w = World::new();
    w.armed = true;
    w.run(200);
    w.p.start_goto(target, w.now);
    assert_ne!(w.p.owner(), Owner::Goto, "landed");

    let mut w = World::new();
    w.airborne(copter::GUIDED);
    w.p.auth.enter_tuning().unwrap();
    w.run(100);
    w.p.start_goto(target, w.now);
    assert_eq!(w.p.owner(), Owner::Tuning, "tuning");

    let mut w = World::new();
    w.airborne(copter::LOITER);
    w.run(100);
    w.p.start_goto(target, w.now);
    w.run(100);
    assert_eq!(w.p.owner(), Owner::Goto, "flying, no failsafe");
    assert_eq!(w.mode, copter::GUIDED);
}

/// The pilot kind says when the FC flies a failsafe, for the router's
/// reposition answer, and leaves the field out otherwise.
#[test]
fn the_pilot_kind_names_an_fc_failsafe() {
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    w.run(100);
    let s = w.p.state(w.now);
    assert!(!s.fc_failsafe);
    let v = serde_json::to_value(&s).unwrap();
    assert!(v.get("fc_failsafe").is_none());
    assert_eq!(v["failsafe_cause"], Value::Null);
    w.mode = copter::RTL;
    w.run(100);
    let s = w.p.state(w.now);
    assert!(s.fc_failsafe);
    let v = serde_json::to_value(&s).unwrap();
    assert_eq!(v["fc_failsafe"], true);
    // No RC input reported: the FC changed mode by itself.
    assert_eq!(v["failsafe_cause"], "fc");
    assert!(ok(&w.action("take_control")));
    w.run(100);
    assert!(
        !w.p.state(w.now).fc_failsafe,
        "an operator's mode change ends it"
    );
    assert_eq!(w.p.state(w.now).failsafe_cause, None);

    // With RC input the same change may be an RC switch: the banner's cause.
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    let rc = MavMessage::RC_CHANNELS(RC_CHANNELS_DATA {
        chancount: 8,
        chan1_raw: 1500,
        ..Default::default()
    });
    w.p.on_fc(FC_SYS, FC_COMP, &rc, w.now);
    w.run(100);
    w.mode = copter::RTL;
    w.run(100);
    let s = w.p.state(w.now);
    assert!(s.fc_failsafe);
    assert_eq!(s.failsafe_cause.as_deref(), Some("rc_or_failsafe"));
    assert_eq!(
        s.banner.as_ref().and_then(|b| b.args.get("cause")),
        Some(&json!("rc_or_failsafe"))
    );

    let mut w = World::new();
    w.airborne(copter::GUIDED);
    w.run(100);
    client_mode(&mut w, json!("rtl"));
    w.mode = copter::RTL;
    w.run(100);
    assert!(!w.p.state(w.now).fc_failsafe, "a ground station's RTL");
    assert_eq!(w.p.state(w.now).failsafe_cause, None);
}

/// An RTL or land a ground station asked for (Mode sheet, RTH, land) is
/// the operator's: no failsafe, the FC-control banner, and a map point or a
/// redirected reposition after it flies.
#[test]
fn a_ground_station_rtl_or_land_is_not_a_failsafe() {
    let cases = [
        (json!(copter::RTL), copter::RTL),
        (json!("rtl"), copter::RTL),
        (json!(copter::LAND), copter::LAND),
        (json!("land"), copter::LAND),
    ];
    for (cmd, mode) in cases {
        let mut w = World::new();
        home(&mut w);
        w.airborne(copter::GUIDED);
        w.run(100);
        client_mode(&mut w, cmd.clone());
        w.run(400);
        w.mode = mode;
        w.run(100);
        let s = w.p.state(w.now);
        assert_eq!(
            s.banner.unwrap().text_key,
            "pilot.banner.fc_control",
            "{cmd}"
        );
        let r = w.action_args("goto", point());
        assert!(ok(&r), "{cmd}: {r:?}");
        w.run(100);
        assert_eq!(w.p.owner(), Owner::Goto, "{cmd}");

        let mut w = World::new();
        w.airborne(copter::GUIDED);
        w.run(100);
        client_mode(&mut w, cmd.clone());
        w.mode = mode;
        w.run(100);
        w.req(redirected_goto());
        w.run(100);
        assert_eq!(w.mode, copter::GUIDED, "{cmd}");
        assert_eq!(w.p.owner(), Owner::Goto, "{cmd}");
    }
}

/// A ground station's command for another mode, or one older than the
/// window, does not make the FC's own RTL the operator's.
#[test]
fn an_unrelated_or_old_client_mode_leaves_it_a_failsafe() {
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    w.run(100);
    client_mode(&mut w, json!(copter::LOITER));
    w.mode = copter::RTL;
    w.run(100);
    assert_eq!(
        err(&w.action_args("goto", point())),
        Some("failsafe"),
        "a LOITER command"
    );

    let mut w = World::new();
    w.airborne(copter::GUIDED);
    w.run(100);
    client_mode(&mut w, json!("rtl"));
    w.run(2500);
    w.mode = copter::RTL;
    w.run(100);
    assert_eq!(
        err(&w.action_args("goto", point())),
        Some("failsafe"),
        "an RTL command 2.5 s earlier"
    );
    let s = w.p.state(w.now);
    assert_eq!(s.banner.unwrap().text_key, "pilot.banner.fc_mode_change");
}

/// Outside `ados` a skill's goals and the swarm's velocities stop at the
/// FC's accepted reposition: the lease is revoked and nothing is streamed
/// over the FC's target.
#[test]
fn skill_and_swarm_yield_to_a_ground_station_reposition_outside_ados() {
    let mut w = World::new();
    w.airborne(copter::LOITER);
    assert!(ok(&w.action("control_mode:fc_manual")));
    w.mode = copter::GUIDED;
    w.run(100);
    let id = w.lease();
    assert_eq!(w.p.owner(), Owner::Skill);
    w.goal(id, 10.0, 0.0);
    w.run(200);
    assert!(setpoints(&w) >= 1);
    reposition_accepted(&mut w);
    assert_eq!(w.p.owner(), Owner::FcMode);
    assert_eq!(w.revoked.last().unwrap(), &(id, "fc-reposition".to_owned()));
    w.sent.clear();
    w.goal(id, 10.0, 0.0);
    w.run(300);
    assert_eq!(setpoints(&w), 0, "the skill overwrote the FC's reposition");

    let mut w = World::new();
    w.airborne(copter::LOITER);
    assert!(ok(&w.action("control_mode:fc_manual")));
    assert!(ok(&w.action("take_control")));
    w.run(100);
    let swarm = |w: &mut World| {
        w.req(Request::SwarmGoal {
            vn: 1.0,
            ve: 0.0,
            vd: 0.0,
            yaw: None,
        });
    };
    swarm(&mut w);
    assert_eq!(w.p.owner(), Owner::Swarm);
    reposition_accepted(&mut w);
    assert_eq!(w.p.owner(), Owner::FcMode);
    w.sent.clear();
    for _ in 0..10 {
        swarm(&mut w);
        w.run(20);
    }
    assert_eq!(setpoints(&w), 0, "the swarm overwrote the FC's reposition");
}
