//! An FC failsafe RTL or land (an RC switch, a radio, battery or EKF
//! failsafe) is never overridden by the pilot: Take control, Resume and a
//! launch are refused with `failsafe`, nothing asks the FC for GUIDED or
//! AUTO, and the FC keeps flying its failsafe. The operator's own mode
//! change ends the failsafe state, and Take control works after it.

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
