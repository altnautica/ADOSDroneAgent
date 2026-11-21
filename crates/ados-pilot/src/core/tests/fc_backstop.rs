//! An upgraded vehicle whose FC was set up before the proximity backstop
//! joined the profile takes off with no operator step: the backstop is a
//! warning until the ADOS profile is applied in setup, never a block.

use ados_protocol::pilot::{Owner, Readiness, Severity};

use super::harness::World;
use crate::core::Reply;
use crate::modes::copter;

fn item(w: &World, id: &str) -> Readiness {
    w.p.state(w.now)
        .readiness
        .into_iter()
        .find(|r| r.id == id)
        .unwrap_or_else(|| panic!("a {id} item"))
}

/// The backstop rows as the profile before the backstop left them:
/// avoidance off, ArduCopter's defaults for the rest.
fn set_up_before_the_backstop(w: &mut World) {
    for (n, v) in [
        ("AVOID_ENABLE", 0.0),
        ("AVOID_MARGIN", 2.0),
        ("AVOID_BEHAVE", 0.0),
        ("PRX1_TYPE", 0.0),
    ] {
        w.p.fc.params.insert(n.into(), v);
    }
}

#[test]
fn an_fc_set_up_before_the_backstop_takes_off_with_a_warning() {
    let mut w = World::new();
    set_up_before_the_backstop(&mut w);
    w.run(200);
    assert!(item(&w, "fc_profile").ok, "{:?}", item(&w, "fc_profile"));
    let b = item(&w, "fc_backstop");
    assert_eq!((b.ok, b.severity), (false, Severity::Warn), "{b:?}");
    assert!(
        b.msg
            .contains("PRX1_TYPE 0 (want 2, used after an FC reboot)"),
        "{}",
        b.msg
    );
    w.action("takeoff");
    w.run(500);
    assert!(w.armed);
    assert_eq!(w.mode, copter::GUIDED);
    w.run(1500);
    assert_eq!(w.p.owner(), Owner::Pilot);
    assert_eq!(w.p.state(w.now).sequence.unwrap().step, "done");
}

#[test]
fn a_fresh_profile_carries_the_backstop() {
    let mut w = World::new();
    w.run(200);
    assert!(item(&w, "fc_profile").ok);
    let b = item(&w, "fc_backstop");
    assert_eq!((b.ok, b.msg.as_str()), (true, "FC proximity backstop set"));
}

#[test]
fn fence_avoidance_on_the_fc_still_refuses_the_arm() {
    let mut w = World::new();
    w.p.fc.params.insert("AVOID_ENABLE".into(), 3.0);
    assert!(
        matches!(w.action("arm"), Reply::Action(a) if a.err.as_deref() == Some("not_ready:fc_profile"))
    );
}
