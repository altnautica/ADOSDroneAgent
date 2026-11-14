//! An FC failsafe RTL or land (an RC switch, a radio, battery or EKF
//! failsafe) is never overridden by the pilot: Take control, Resume and a
//! launch are refused with `failsafe`, nothing asks the FC for GUIDED or
//! AUTO, and the FC keeps flying its failsafe. The operator's own mode
//! change ends the failsafe state, and Take control works after it.

use ados_protocol::mavlink::ardupilotmega::MavType;
use ados_protocol::pilot::{Owner, CLIENT_MODE_TOPIC};
use serde_json::json;

use super::harness::World;
use crate::core::Reply;
use crate::modes::copter;

fn err(r: &Reply) -> Option<&str> {
    match r {
        Reply::Action(a) if !a.ok => a.err.as_deref(),
        _ => None,
    }
}

fn ok(r: &Reply) -> bool {
    matches!(r, Reply::Action(a) if a.ok)
}

/// Flying as the pilot in GUIDED, then the FC switches itself to `mode`.
fn fc_failsafe(mode: u32) -> World {
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    w.run(100);
    assert!(ok(&w.action("take_control")));
    w.run(100);
    assert_eq!(w.p.owner(), Owner::Pilot);
    w.mode = mode;
    w.run(100);
    assert!(w.p.state(w.now).fc_failsafe, "mode {mode}");
    assert_eq!(w.p.owner(), Owner::FcMode, "mode {mode}");
    w.sent.clear();
    w
}

/// The FC's EKF failsafe lands the vehicle; the operator's Take control is
/// refused with `failsafe` (not answered ok), no GUIDED request goes out,
/// and the owner never reads `pilot` while the FC lands.
#[test]
fn take_control_refused_during_an_fc_failsafe() {
    for mode in [copter::LAND, copter::RTL, copter::SMART_RTL] {
        let mut w = fc_failsafe(mode);
        let r = w.action("take_control");
        assert_eq!(err(&r), Some("failsafe"), "mode {mode}: {r:?}");
        for _ in 0..10 {
            w.run(100);
            assert_eq!(w.p.owner(), Owner::FcMode, "mode {mode}");
        }
        assert_eq!(w.mode, mode, "the FC keeps flying its failsafe");
        assert_eq!(w.set_modes(copter::GUIDED), 0, "mode {mode}");
        assert!(w.p.state(w.now).fc_failsafe, "mode {mode}");
    }
}

/// Take control is refused during the failsafe in every control mode.
#[test]
fn take_control_refused_during_an_fc_failsafe_in_every_control_mode() {
    for cm in ["fc_manual", "elrs"] {
        let mut w = World::new();
        w.airborne(copter::LOITER);
        assert!(ok(&w.action(&format!("control_mode:{cm}"))), "{cm}");
        w.run(100);
        w.mode = copter::RTL;
        w.run(100);
        assert!(w.p.state(w.now).fc_failsafe, "{cm}");
        w.sent.clear();
        assert_eq!(err(&w.action("take_control")), Some("failsafe"), "{cm}");
        w.run(300);
        assert_eq!(w.mode, copter::RTL, "{cm}");
        assert_eq!(w.set_modes(copter::GUIDED), 0, "{cm}");
    }
}

/// The operator's mode change ends the failsafe state: after Hand to FC
/// (LOITER), or a ground station's Mode sheet LOITER the router saw, Take
/// control flies again.
#[test]
fn take_control_works_after_the_operator_changes_mode() {
    let mut w = fc_failsafe(copter::RTL);
    assert!(ok(&w.action_args(
        "hand_to_fc",
        json!({"mode": "loiter", "control_mode": "ados"})
    )));
    w.run(200);
    assert_eq!(w.mode, copter::LOITER);
    assert!(!w.p.state(w.now).fc_failsafe);
    assert!(ok(&w.action("take_control")));
    w.run(200);
    assert_eq!(w.mode, copter::GUIDED);
    assert_eq!(w.p.owner(), Owner::Pilot);

    let mut w = fc_failsafe(copter::LAND);
    w.p.on_feed(CLIENT_MODE_TOPIC, &json!({ "mode": copter::LOITER }), w.now);
    w.mode = copter::LOITER;
    w.run(100);
    assert!(!w.p.state(w.now).fc_failsafe);
    assert!(ok(&w.action("take_control")));
    w.run(200);
    assert_eq!(w.mode, copter::GUIDED);
    assert_eq!(w.p.owner(), Owner::Pilot);
}

/// Resume during an FC failsafe RTL would put the FC back in AUTO, out of
/// its failsafe: refused, and nothing is sent.
#[test]
fn resume_refused_during_an_fc_failsafe() {
    let mut w = fc_failsafe(copter::RTL);
    w.mission("paused", 3, 10);
    w.sent.clear();
    assert_eq!(err(&w.action("resume")), Some("failsafe"));
    w.run(300);
    assert_eq!(w.mode, copter::RTL, "the FC keeps flying its failsafe");
    assert_eq!(w.set_modes(copter::AUTO), 0);
    assert_eq!(w.p.owner(), Owner::FcMode);
}

/// Armed on the ground at the end of an FC failsafe land, before its
/// disarm: a launch would put the FC in GUIDED and climb out of it.
#[test]
fn launch_refused_on_the_ground_during_an_fc_failsafe() {
    let mut w = fc_failsafe(copter::LAND);
    w.landed = true;
    w.pos = [0.0, 0.0, 0.0];
    w.agl = Some(0.1);
    w.run(200);
    assert!(w.p.state(w.now).fc_failsafe);
    for (name, args) in [
        ("launch", json!({"after": "hold", "alt_m": 4.0})),
        ("takeoff", json!({})),
    ] {
        w.sent.clear();
        assert_eq!(err(&w.action_args(name, args)), Some("failsafe"), "{name}");
        w.run(300);
        assert_eq!(w.mode, copter::LAND, "{name}");
        assert_eq!(w.set_modes(copter::GUIDED), 0, "{name}");
    }
}

/// Copter's failsafe actions "Auto DO_LAND_START or RTL" (AUTO_RTL) and
/// "Brake or Land" (BRAKE) are FC failsafes too when nobody commanded them:
/// the FC owns the vehicle, and Take control is refused.
#[test]
fn uncommanded_auto_rtl_and_brake_are_fc_failsafes() {
    for mode in [copter::AUTO_RTL, copter::BRAKE] {
        let mut w = fc_failsafe(mode);
        assert_eq!(
            err(&w.action("take_control")),
            Some("failsafe"),
            "mode {mode}"
        );
        w.run(300);
        assert_eq!(w.p.owner(), Owner::FcMode, "mode {mode}");
        assert_eq!(w.mode, mode, "the FC keeps flying its failsafe");
        assert_eq!(w.set_modes(copter::GUIDED), 0, "mode {mode}");
    }
}

/// A mission in AUTO whose FC switches itself to AUTO_RTL is not the
/// operator's mission any more: the owner is the FC's, and a stick
/// deflection does not take the vehicle into GUIDED.
#[test]
fn uncommanded_auto_rtl_from_a_mission_is_not_a_mission() {
    let mut w = World::new();
    w.airborne(copter::AUTO);
    w.run(100);
    assert_eq!(w.p.owner(), Owner::MissionFc);
    w.mode = copter::AUTO_RTL;
    w.run(100);
    assert!(w.p.state(w.now).fc_failsafe);
    assert_eq!(w.p.owner(), Owner::FcMode);
    w.sent.clear();
    w.stick([0.0, 0.5, 0.0, 0.0], 600);
    assert_eq!(w.mode, copter::AUTO_RTL);
    assert_eq!(w.set_modes(copter::GUIDED), 0);
    assert_eq!(err(&w.action("take_control")), Some("failsafe"));
}

/// The operator's own AUTO_RTL or BRAKE (a ground station's mode command
/// the router saw) is no failsafe: Take control flies again.
#[test]
fn operator_commanded_auto_rtl_and_brake_are_not_failsafes() {
    for mode in [copter::AUTO_RTL, copter::BRAKE] {
        let mut w = World::new();
        w.airborne(copter::LOITER);
        w.run(100);
        w.p.on_feed(CLIENT_MODE_TOPIC, &json!({ "mode": mode }), w.now);
        w.mode = mode;
        w.run(100);
        assert!(!w.p.state(w.now).fc_failsafe, "mode {mode}");
        assert!(ok(&w.action("take_control")), "mode {mode}");
        w.run(200);
        assert_eq!(w.mode, copter::GUIDED, "mode {mode}");
        assert_eq!(w.p.owner(), Owner::Pilot, "mode {mode}");
    }
}

/// A mission the pilot paused, then an FC failsafe return: the frame shows
/// the router's mission state, not `paused`, since Resume is refused until
/// the operator changes mode.
#[test]
fn a_paused_mission_does_not_read_paused_during_an_fc_failsafe() {
    let mut w = World::new();
    w.airborne(copter::AUTO);
    w.run(40);
    w.mission("active", 2, 4);
    w.stick([0.0, 0.5, 0.0, 0.0], 400);
    assert_eq!(w.p.owner(), Owner::Pilot);
    w.mission("loaded", 2, 4);
    assert_eq!(w.p.state(w.now).mission.unwrap().state, "paused");
    w.stick([0.0; 4], 600);
    w.mode = copter::RTL;
    w.run(100);
    w.mission("returning", 2, 4);
    let s = w.p.state(w.now);
    assert!(s.fc_failsafe);
    assert_eq!(s.mission.unwrap().state, "returning");
    assert_eq!(err(&w.action("resume")), Some("failsafe"));
}

/// An FC failsafe during the setup wizard is recorded under `tuning`, and
/// leaving the wizard does not fly over it: the FC keeps its mode, and Take
/// control and Pause are refused with `failsafe`.
#[test]
fn an_fc_failsafe_during_the_setup_wizard_is_kept_after_it() {
    for mode in [copter::RTL, copter::LAND, copter::BRAKE, copter::AUTO_RTL] {
        let mut w = World::new();
        w.airborne(copter::GUIDED);
        w.p.auth.enter_tuning().unwrap();
        w.run(100);
        w.mode = mode;
        w.run(100);
        assert!(w.p.state(w.now).fc_failsafe, "mode {mode}");
        w.sent.clear();
        w.p.exit_tuning(w.now);
        w.run(100);
        assert_eq!(w.p.owner(), Owner::FcMode, "mode {mode}");
        assert_eq!(
            err(&w.action("take_control")),
            Some("failsafe"),
            "mode {mode}"
        );
        assert_eq!(err(&w.action("pause")), Some("failsafe"), "mode {mode}");
        w.run(300);
        assert_eq!(w.mode, mode, "the FC keeps flying its failsafe");
        assert_eq!(w.set_modes(copter::GUIDED), 0, "mode {mode}");
    }
}

/// The wizard's own landing leg: the setup engine asked the FC for LAND, so
/// it is no failsafe, and Pause is not refused with `failsafe`.
#[test]
fn a_wizard_landing_leg_is_not_an_fc_failsafe() {
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    w.p.auth.enter_tuning().unwrap();
    w.run(100);
    w.p.note_setup_mode(copter::LAND, w.now);
    w.mode = copter::LAND;
    w.run(100);
    assert!(!w.p.state(w.now).fc_failsafe);
    w.p.exit_tuning(w.now);
    w.run(100);
    assert!(!w.p.state(w.now).fc_failsafe);
    assert_ne!(err(&w.action("pause")), Some("failsafe"));
}

/// The Plane wizard's abort RTL is the wizard's own: after it, Take control
/// is available. (`fc_manual`: the harness Plane has no flight profile, which
/// `ados` readiness would refuse first.)
#[test]
fn the_plane_wizard_abort_rtl_leaves_take_control_available() {
    use crate::modes::plane;
    let mut w = World::new();
    w.mavtype = MavType::MAV_TYPE_FIXED_WING;
    w.airborne(plane::GUIDED);
    assert!(ok(&w.action("control_mode:fc_manual")));
    w.run(100);
    w.p.auth.enter_tuning().unwrap();
    w.run(100);
    w.p.note_setup_mode(plane::RTL, w.now);
    w.mode = plane::RTL;
    w.run(100);
    assert!(!w.p.state(w.now).fc_failsafe);
    w.p.exit_tuning(w.now);
    w.run(100);
    assert_eq!(w.p.owner(), Owner::FcMode);
    assert!(!w.p.state(w.now).fc_failsafe);
    let r = w.action("take_control");
    assert!(ok(&r), "{r:?}");
}

/// The Plane wizard's abort from FBWA: its RTL is sent and the wizard lets
/// go before the FC's heartbeat shows RTL. Leaving the wizard does not ask
/// for GUIDED over that RTL; the FC flies it.
#[test]
fn leaving_the_wizard_right_after_its_abort_rtl_sends_no_guided() {
    use crate::modes::plane;
    let mut w = World::new();
    w.mavtype = MavType::MAV_TYPE_FIXED_WING;
    w.airborne(plane::FBWA);
    w.p.auth.enter_tuning().unwrap();
    w.run(100);
    w.sent.clear();
    w.p.note_setup_mode(plane::RTL, w.now);
    w.p.exit_tuning(w.now);
    w.run(100);
    assert_eq!(w.set_modes(plane::GUIDED), 0);
    assert_eq!(w.p.owner(), Owner::FcMode);
    w.mode = plane::RTL;
    w.run(200);
    assert!(!w.p.state(w.now).fc_failsafe);
    assert_eq!(w.p.owner(), Owner::FcMode);
}
