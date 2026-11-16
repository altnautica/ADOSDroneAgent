//! Airframe health's record in the per-aircraft setup state: filed under the
//! FC identity, read back after a restart, kept apart per FC.

use ados_protocol::mavlink::ardupilotmega::MavType;

use crate::airframe::{Record, Trim};
use crate::setup::fake_fc::{ap_copter_params, FakeFc, TICK};
use crate::setup::Engine;

/// An engine on `dir` booted against an FC with `uid` until it files its
/// state under the identity; returns it and the time.
fn boot(dir: &std::path::Path, uid: u64) -> (Engine, u64) {
    let mut e = Engine::new(dir, 0);
    let mut fc = FakeFc::new(MavType::MAV_TYPE_QUADROTOR, ap_copter_params());
    fc.uid = uid;
    let mut t = 0;
    while e.uid.is_none() {
        assert!(t < 10_000, "no identity adopted");
        t += TICK;
        fc.step(&mut e, t);
    }
    (e, t)
}

#[test]
fn an_airframe_record_is_filed_per_fc_and_read_back_after_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let (mut a, t) = boot(dir.path(), 0xA);
    let uid_a = a.airframe().unwrap().0.to_owned();
    assert_eq!(a.airframe().unwrap().1, None, "nothing stored yet");
    let rec = Record {
        trim: Some(Trim {
            i: [-0.05, 0.25],
            hover_thr: 0.61,
            samples: 300,
        }),
        busiest: Some(0.84),
        busiest_samples: 300,
        saturated: true,
        stop_motor: 0.96,
        rev: 3,
    };
    a.set_airframe(&uid_a, rec.clone(), t);
    // Another FC's record is never filed under this one.
    a.set_airframe("0000000000000bbb", Record::default(), t);
    assert_eq!(a.airframe().unwrap().1, Some(&rec));
    let state = std::fs::read_to_string(dir.path().join(&uid_a).join("state.json")).unwrap();
    assert!(state.contains("\"airframe\""), "{state}");
    // After a restart the same FC reads it back…
    let (a2, _) = boot(dir.path(), 0xA);
    assert_eq!(
        a2.airframe().map(|(u, r, _)| (u.to_owned(), r.cloned())),
        Some((uid_a, Some(rec)))
    );
    // …and another FC has none.
    let (b, _) = boot(dir.path(), 0xB);
    assert_eq!(b.airframe().unwrap().1, None);
}

#[test]
fn the_wizard_ors_the_lift_off_bits_into_the_operators_values() {
    let s = super::apply_profile_differing(&[("GUID_OPTIONS", 1.0), ("GCS_PID_MASK", 4.0)]);
    assert_eq!(s.param("GUID_OPTIONS"), 9.0);
    assert_eq!(s.param("GCS_PID_MASK"), 7.0);
    assert_eq!(s.fc.reboots, 0, "both are read live");
}
