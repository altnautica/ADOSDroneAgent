//! `ados_intro` with no RC transmitter: `use_ados_no_rc` is offered (first
//! when no receiver feeds the FC), writes the RC failsafe off and the FC
//! fence, and declares the posture after ADOS control; the plain choices put
//! the RC failsafe back and declare an RC transmitter before the mode.

use ados_protocol::mavlink::ardupilotmega::MavType;
use ados_protocol::pilot::{PlaceClass, RcReceiver};
use serde_json::json;

use super::{action_ids, Sim, G};
use crate::setup::fake_fc::ap_copter_params;
use crate::setup::out::Effect;
use crate::setup::schema::StepState;

fn copter_with_fence() -> Sim {
    copter_with(5.0)
}

/// A copter whose FC boots with `FS_THR_ENABLE` `fs_thr` (the engine reads
/// the FC's values at boot; a value changed later is not seen).
fn copter_with(fs_thr: f32) -> Sim {
    let mut p = ap_copter_params();
    for (k, v) in [
        ("FS_THR_ENABLE", fs_thr),
        ("FENCE_ENABLE", 0.0),
        ("FENCE_TYPE", 7.0),
        ("FENCE_RADIUS", 300.0),
        ("FENCE_ALT_MAX", 100.0),
        ("FENCE_ACTION", 1.0),
    ] {
        p.insert(k.into(), v);
    }
    Sim::new(MavType::MAV_TYPE_QUADROTOR, p)
}

fn intro(s: &mut Sim) {
    assert_eq!(s.e.run_step(G, "ados_intro", s.now)["ok"], json!(true));
    s.wait_step("ados_intro", StepState::WaitingUser);
}

#[test]
fn no_rc_is_offered_first_without_a_receiver_and_writes_the_posture() {
    let mut s = copter_with_fence();
    intro(&mut s);
    assert_eq!(
        action_ids(&s),
        ["use_ados_no_rc", "use_ados", "keep_fc"],
        "no receiver feeds the FC"
    );
    s.act(G, "ados_intro", "use_ados_no_rc", json!({}));
    s.until(|s| s.step_state(G, "ados_intro") == StepState::Done, 10_000);
    for (k, v) in [
        ("FS_THR_ENABLE", 0.0),
        ("FENCE_ENABLE", 1.0),
        ("FENCE_TYPE", 3.0),
        ("FENCE_RADIUS", 70.0),
        ("FENCE_ALT_MAX", 50.0),
        ("FENCE_ACTION", 5.0),
    ] {
        assert_eq!(s.param(k), v, "{k}");
    }
    let mode =
        s.fc.effects
            .iter()
            .position(|e| *e == Effect::SetControlMode("ados"));
    let rc =
        s.fc.effects
            .iter()
            .position(|e| *e == Effect::SetRcReceiver(RcReceiver::Absent));
    assert!(mode.unwrap() < rc.unwrap(), "{:?}", s.fc.effects);
    assert_eq!(s.result(G, "ados_intro")["rc_receiver"], json!("absent"));
}

#[test]
fn use_ados_declares_an_rc_transmitter_and_puts_the_rc_failsafe_back() {
    let mut s = copter_with(0.0);
    s.fc.inputs.rc_receiver = RcReceiver::Absent;
    s.run(super::TICK);
    intro(&mut s);
    s.act(G, "ados_intro", "keep_fc", json!({}));
    s.until(|s| s.step_state(G, "ados_intro") == StepState::Done, 10_000);
    assert_eq!(s.param("FS_THR_ENABLE"), 5.0);
    let rc =
        s.fc.effects
            .iter()
            .position(|e| *e == Effect::SetRcReceiver(RcReceiver::Present));
    let mode =
        s.fc.effects
            .iter()
            .position(|e| *e == Effect::SetControlMode("fc_manual"));
    assert!(
        rc.unwrap() < mode.unwrap(),
        "present before leaving ados: {:?}",
        s.fc.effects
    );
}

/// Leaving the no-RC posture lifts the posture's FC fence; a fence the
/// operator set some other way is left alone.
#[test]
fn use_ados_lifts_the_no_rc_fence_and_only_that_fence() {
    for (radius, lifted) in [(70.0, true), (300.0, false)] {
        let mut p = ap_copter_params();
        for (k, v) in [
            ("FS_THR_ENABLE", 0.0),
            ("FENCE_ENABLE", 1.0),
            ("FENCE_TYPE", 3.0),
            ("FENCE_RADIUS", radius),
            ("FENCE_ALT_MAX", 50.0),
            ("FENCE_ACTION", 5.0),
        ] {
            p.insert(k.into(), v);
        }
        let mut s = Sim::new(MavType::MAV_TYPE_QUADROTOR, p);
        intro(&mut s);
        s.act(G, "ados_intro", "use_ados", json!({}));
        s.until(|s| s.step_state(G, "ados_intro") == StepState::Done, 10_000);
        assert_eq!(s.param("FS_THR_ENABLE"), 5.0, "radius {radius}");
        let fence = if lifted { 0.0 } else { 1.0 };
        assert_eq!(s.param("FENCE_ENABLE"), fence, "radius {radius}");
    }
}

#[test]
fn no_rc_is_not_offered_in_a_confined_place_nor_on_a_plane() {
    let mut s = copter_with_fence();
    s.fc.inputs.place_class = PlaceClass::Confined;
    // One tick hands the engine the pilot's inputs.
    s.run(super::TICK);
    intro(&mut s);
    assert_eq!(action_ids(&s), ["use_ados", "keep_fc"]);
    let r = s.e.action(
        G,
        "ados_intro",
        "use_ados_no_rc",
        &Default::default(),
        s.now,
    );
    assert_eq!(r["ok"], json!(false), "{r}");

    let mut s = Sim::new(MavType::MAV_TYPE_FIXED_WING, super::ap_plane_params());
    intro(&mut s);
    assert_eq!(action_ids(&s), ["use_ados", "keep_fc"]);
}
