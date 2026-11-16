//! A confined launch on external navigation waits for the FC's EKF to
//! settle after it starts using it: an estimate still ringing in velocity
//! at lift-off carries that error into the climb.

use ados_protocol::mavlink::ardupilotmega::{MavMessage, MavSeverity, STATUSTEXT_DATA};
use ados_protocol::pilot::{Readiness, Severity};

use super::confined::{confined, err};
use super::harness::World;

fn settle(w: &World) -> Option<Readiness> {
    w.p.state(w.now)
        .readiness
        .into_iter()
        .find(|r| r.id == "extnav_settle")
}

/// The FC says its EKF started using external navigation.
fn extnav_started(w: &mut World) {
    w.inbox.push(MavMessage::STATUSTEXT(STATUSTEXT_DATA {
        severity: MavSeverity::MAV_SEVERITY_INFO,
        text: "EKF3 IMU0 is using external nav data".into(),
    }));
}

/// A confined place flown on external navigation, on the ground.
fn on_extnav() -> World {
    let mut w = World::new();
    confined(&mut w);
    w.p.fc.params.insert("EK3_SRC1_POSXY".into(), 6.0);
    w.run(200);
    w
}

#[test]
fn a_confined_launch_on_external_nav_waits_for_the_ekf_to_settle() {
    let mut w = on_extnav();
    extnav_started(&mut w);
    // The EKF rings: 0.03 m/s horizontally on a still vehicle.
    w.vel = [-0.027, 0.012, 0.0];
    w.run(1000);
    let it = settle(&w).expect("extnav_settle item");
    assert!(!it.ok);
    assert_eq!(it.severity, Severity::Block);
    assert_eq!(
        it.msg,
        "External navigation settling: 0.03 m/s, needs 0.01 m/s for 2 s"
    );
    assert_eq!(
        err(&w.action("arm")).as_deref(),
        Some("not_ready:extnav_settle")
    );
    // Passing through zero is not settling: 1.5 s within 0.01 m/s, then the
    // ring swings back.
    w.vel = [0.004, 0.0, 0.0];
    w.run(1500);
    assert!(!settle(&w).unwrap().ok);
    w.vel = [0.02, 0.0, 0.0];
    w.run(200);
    w.vel = [0.006, -0.003, 0.0];
    w.run(1900);
    assert!(!settle(&w).unwrap().ok, "1.9 s still");
    assert_eq!(
        err(&w.action("arm")).as_deref(),
        Some("not_ready:extnav_settle")
    );
    w.run(200);
    let it = settle(&w).unwrap();
    assert!(it.ok, "{it:?}");
    assert_eq!(it.msg, "External navigation settled");
    assert_ne!(
        err(&w.action("arm")).as_deref(),
        Some("not_ready:extnav_settle")
    );
    // The EKF starting over (a reset) starts the wait over.
    extnav_started(&mut w);
    w.run(100);
    assert!(!settle(&w).unwrap().ok);
}

#[test]
fn the_settle_counts_from_the_first_local_position_when_the_start_went_unheard() {
    // The pilot started after the EKF took up external navigation: the
    // estimate must still be seen still for 2 s.
    let mut w = on_extnav();
    w.vel = [0.0, 0.0, 0.0];
    w.run(1500);
    assert!(settle(&w).is_some_and(|r| !r.ok));
    w.run(600);
    assert!(settle(&w).is_some_and(|r| r.ok));
}

#[test]
fn only_a_confined_place_on_external_nav_on_the_ground_has_the_item() {
    let mut w = on_extnav();
    w.p.fc.params.insert("EK3_SRC1_POSXY".into(), 3.0);
    w.run(100);
    assert_eq!(settle(&w), None, "GNSS");
    let mut w = World::new();
    w.p.fc.params.insert("EK3_SRC1_POSXY".into(), 6.0);
    w.run(100);
    assert_eq!(settle(&w), None, "an open place");
    let mut w = on_extnav();
    w.airborne(crate::modes::copter::GUIDED);
    w.vel = [1.0, 0.0, 0.0];
    w.run(100);
    assert_eq!(settle(&w), None, "airborne");
}
