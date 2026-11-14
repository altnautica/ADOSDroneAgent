//! Take control is answered from the flight controller: the pilot owns the
//! vehicle only once the FC confirms GUIDED.

use ados_protocol::mavlink::ardupilotmega::{MavCmd, MavMessage, MavResult, COMMAND_ACK_DATA};
use ados_protocol::pilot::{ActionReq, Owner, Request, CLIENT_MODE_TOPIC};
use serde_json::json;

use super::harness::{World, FC_COMP, FC_SYS};
use crate::core::Reply;
use crate::modes::copter;

const REQUIRES_POSITION: &str = "Mode change to Guided failed: requires position";

fn answer(r: &Reply) -> (bool, Option<&str>) {
    match r {
        Reply::Action(a) => (a.ok, a.err.as_deref()),
        other => panic!("{other:?}"),
    }
}

fn setpoints(w: &World) -> usize {
    w.sent
        .iter()
        .filter(|m| matches!(m, MavMessage::SET_POSITION_TARGET_LOCAL_NED(_)))
        .count()
}

/// Take control as the pilot replies to it, before the FC answers.
fn take_now(w: &mut World) -> u64 {
    let r = w.req_now(Request::Action(ActionReq {
        name: "take_control".into(),
        src: "mc".into(),
        args: None,
        id: Some("t1".into()),
    }));
    match r {
        Reply::Deferred(t) => t,
        other => panic!("{other:?}"),
    }
}

fn ack(w: &mut World, result: MavResult) {
    let m = MavMessage::COMMAND_ACK(COMMAND_ACK_DATA {
        command: MavCmd::MAV_CMD_DO_SET_MODE,
        result,
    });
    w.p.on_fc(FC_SYS, FC_COMP, &m, w.now);
    w.collect();
}

fn answered(w: &World, token: u64) -> Option<(bool, Option<&str>)> {
    w.answers
        .iter()
        .find(|(t, _)| *t == token)
        .map(|(_, a)| (a.ok, a.err.as_deref()))
}

/// Airborne in a landing a ground station commanded (not an FC failsafe,
/// where Take control is refused before the FC is asked), past the window in
/// which its mode command makes the FC's acks ambiguous.
fn gcs_landing() -> World {
    let mut w = World::new();
    w.airborne(copter::LOITER);
    w.p.on_feed(CLIENT_MODE_TOPIC, &json!({"mode": "land"}), w.now);
    w.mode = copter::LAND;
    w.run(40);
    w.run(super::super::CLIENT_MODE_WINDOW_MS + 100);
    assert!(!w.p.state(w.now).fc_failsafe);
    assert_eq!(w.p.owner(), Owner::FcMode);
    w
}

/// The FC refuses GUIDED: the operator reads the FC's reason, the owner
/// stays the FC's and the pilot never streams a setpoint.
fn assert_refused(w: &mut World, r: &Reply, want: &str) {
    assert_eq!(answer(r), (false, Some(want)));
    assert_eq!(w.mode, copter::LAND);
    assert_eq!(w.p.owner(), Owner::FcMode);
    w.run(1000);
    assert_eq!(w.p.owner(), Owner::FcMode);
    let s = w.p.state(w.now);
    assert_eq!(s.owner, Owner::FcMode);
    assert_eq!(setpoints(w), 0);
}

#[test]
fn a_failed_ack_refuses_with_the_fc_reason_and_keeps_the_owner() {
    let mut w = gcs_landing();
    w.acks = true;
    w.refuse_mode = Some((
        copter::GUIDED,
        MavResult::MAV_RESULT_FAILED,
        Some(REQUIRES_POSITION),
    ));
    let r = w.action("take_control");
    assert_refused(&mut w, &r, &format!("fc_refused:{REQUIRES_POSITION}"));
}

#[test]
fn a_denied_ack_without_a_reason_refuses() {
    let mut w = gcs_landing();
    w.acks = true;
    w.refuse_mode = Some((copter::GUIDED, MavResult::MAV_RESULT_DENIED, None));
    let r = w.action("take_control");
    assert_refused(&mut w, &r, "fc_refused");
}

#[test]
fn a_lost_ack_with_the_fc_reason_refuses_with_it() {
    let mut w = gcs_landing();
    w.refuse_mode = Some((
        copter::GUIDED,
        MavResult::MAV_RESULT_FAILED,
        Some(REQUIRES_POSITION),
    ));
    let r = w.action("take_control");
    assert_refused(&mut w, &r, &format!("fc_refused:{REQUIRES_POSITION}"));
}

#[test]
fn no_answer_and_no_guided_refuses() {
    let mut w = gcs_landing();
    w.refuse_mode = Some((copter::GUIDED, MavResult::MAV_RESULT_FAILED, None));
    let r = w.action("take_control");
    assert_refused(&mut w, &r, "fc_no_answer");
}

#[test]
fn an_accepting_fc_hands_the_vehicle_to_the_pilot() {
    let mut w = gcs_landing();
    w.acks = true;
    let r = w.action("take_control");
    assert_eq!(answer(&r), (true, None));
    assert_eq!(w.mode, copter::GUIDED);
    assert_eq!(w.p.owner(), Owner::Pilot);
    w.run(100);
    assert!(setpoints(&w) > 0);
}

#[test]
fn an_accepting_ack_answers_ok_before_the_heartbeat_shows_guided() {
    let mut w = gcs_landing();
    let t = take_now(&mut w);
    assert_eq!(
        w.p.owner(),
        Owner::FcMode,
        "nothing changes hands on the request"
    );
    assert_eq!(answered(&w, t), None);
    ack(&mut w, MavResult::MAV_RESULT_ACCEPTED);
    assert_eq!(answered(&w, t), Some((true, None)));
    assert_eq!(w.p.owner(), Owner::Pilot);
    let a = &w.answers[0].1;
    assert_eq!(
        (a.name.as_str(), a.id.as_deref()),
        ("take_control", Some("t1"))
    );
}

#[test]
fn a_lost_ack_is_answered_only_by_the_heartbeat_in_guided() {
    let mut w = gcs_landing();
    let t = take_now(&mut w);
    w.p.tick(w.now + 20);
    w.collect();
    assert_eq!(answered(&w, t), None);
    assert_eq!(w.p.owner(), Owner::FcMode);
    w.run(20);
    assert_eq!(answered(&w, t), Some((true, None)));
    assert_eq!(w.p.owner(), Owner::Pilot);
}

#[test]
fn an_ack_while_a_ground_station_changes_mode_waits_for_the_heartbeat() {
    let mut w = gcs_landing();
    w.p.on_feed(CLIENT_MODE_TOPIC, &json!({"mode": copter::LOITER}), w.now);
    w.refuse_mode = Some((copter::GUIDED, MavResult::MAV_RESULT_FAILED, None));
    let t = take_now(&mut w);
    // Either ack could be the ground station's: neither decides.
    ack(&mut w, MavResult::MAV_RESULT_ACCEPTED);
    assert_eq!(answered(&w, t), None);
    assert_eq!(w.p.owner(), Owner::FcMode);
    w.run(3000);
    assert_eq!(answered(&w, t), Some((false, Some("fc_no_answer"))));
    assert_eq!(w.p.owner(), Owner::FcMode);
}

#[test]
fn a_second_take_control_while_the_fc_answers_is_pending() {
    let mut w = gcs_landing();
    w.refuse_mode = Some((copter::GUIDED, MavResult::MAV_RESULT_FAILED, None));
    take_now(&mut w);
    assert_eq!(answer(&w.req_now(take_req())), (false, Some("pending")));
}

#[test]
fn rth_while_the_fc_answers_ends_the_take_control_busy() {
    let mut w = gcs_landing();
    w.refuse_mode = Some((copter::GUIDED, MavResult::MAV_RESULT_FAILED, None));
    let t = take_now(&mut w);
    assert_eq!(answer(&w.action("rth")), (true, None));
    assert_eq!(answered(&w, t), Some((false, Some("busy"))));
    assert_eq!(w.p.owner(), Owner::FcMode);
}

/// The FC enters GUIDED just after Take control was refused: the pilot
/// handles it like any GUIDED it did not ask for, so the vehicle is never
/// left in GUIDED with nobody flying it.
#[test]
fn a_guided_after_the_refusal_is_not_left_unflown() {
    let mut w = gcs_landing();
    w.refuse_mode = Some((copter::GUIDED, MavResult::MAV_RESULT_FAILED, None));
    let t = take_now(&mut w);
    w.run(2600);
    assert_eq!(answered(&w, t), Some((false, Some("fc_no_answer"))));
    assert_eq!(w.p.owner(), Owner::FcMode);
    w.refuse_mode = None;
    w.mode = copter::GUIDED;
    w.run(200);
    assert_eq!(w.p.owner(), Owner::Pilot, "the late GUIDED is adopted");
    assert_eq!(w.p.state(w.now).owner, Owner::Pilot);
    assert!(setpoints(&w) > 0);
}

#[test]
fn in_guided_already_take_control_answers_at_once() {
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    assert!(matches!(w.req_now(take_req()), Reply::Action(a) if a.ok));
    assert_eq!(w.p.owner(), Owner::Pilot);
}

fn take_req() -> Request {
    Request::Action(ActionReq {
        name: "take_control".into(),
        src: "mc".into(),
        args: None,
        id: None,
    })
}
