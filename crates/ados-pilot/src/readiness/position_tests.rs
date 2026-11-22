//! The `position` item as the launch gate: the GPS source set needs an
//! absolute position, and the position must have held good for the settle
//! time.

use ados_protocol::mavlink::ardupilotmega::{
    EkfStatusFlags as F, MavMessage, EKF_STATUS_REPORT_DATA,
};
use ados_protocol::pilot::{Readiness, Severity};

use super::{position_item, POSITION_SETTLE_MS};
use crate::fc::{FcState, LocalPos, Stamped};

/// What the EKF reports while flow alone positions it: a relative position
/// and a predicted one, no absolute (the bench's 0x16f).
const REL: F = F::EKF_ATTITUDE
    .union(F::EKF_VELOCITY_HORIZ)
    .union(F::EKF_POS_HORIZ_REL)
    .union(F::EKF_PRED_POS_HORIZ_REL);
const ABS: F = REL
    .union(F::EKF_POS_HORIZ_ABS)
    .union(F::EKF_PRED_POS_HORIZ_ABS);

/// A linked FC on the ground with flow enabled and `posxy` as source set 1.
fn fc(posxy: f32) -> FcState {
    let mut fc = FcState {
        sysid: 1,
        compid: 1,
        heartbeat_ms: 1,
        ..Default::default()
    };
    for (n, v) in [("EK3_SRC1_POSXY", posxy), ("FLOW_TYPE", 5.0)] {
        fc.params.insert(n.into(), v);
    }
    fc
}

/// Feed one EKF report with variance `var` every 200 ms from `from` to `to`
/// (exclusive).
fn feed(fc: &mut FcState, flags: F, var: f32, from: u64, to: u64) {
    let msg = MavMessage::EKF_STATUS_REPORT(EKF_STATUS_REPORT_DATA {
        flags,
        pos_horiz_variance: var,
        ..Default::default()
    });
    let mut t = from;
    while t < to {
        fc.update(1, 1, &msg, t);
        t += 200;
    }
}

/// The item at `now` with a fresh local position and `nav_set` reported.
fn at(fc: &mut FcState, nav_set: Option<u8>, now: u64) -> Readiness {
    fc.local = Some(Stamped {
        v: LocalPos::default(),
        at_ms: now,
    });
    position_item(fc, nav_set, now)
}

/// Feed good-variance reports, then read the item at `to` with no set
/// reported.
fn run(fc: &mut FcState, flags: F, from: u64, to: u64) -> Readiness {
    feed(fc, flags, 0.1, from, to);
    at(fc, None, to)
}

/// Past the settle time from 1000.
const LATE: u64 = 1000 + POSITION_SETTLE_MS + 400;

/// No Navigation, set 1 GPS, set 2 flow (chosen on the FC by an RC switch):
/// the active set is unknown, so the GPS rule never blocks a flow position;
/// once Navigation names the set, it decides.
#[test]
fn an_unknown_set_with_a_flow_set_beside_gps_is_not_held_to_the_gps_rule() {
    let mut f = fc(3.0);
    for (n, v) in [("EK3_SRC2_POSXY", 0.0), ("EK3_SRC2_VELXY", 5.0)] {
        f.params.insert(n.into(), v);
    }
    assert!(run(&mut f, REL, 1000, LATE).ok);
    assert!(at(&mut f, Some(2), LATE).ok);
    let r = at(&mut f, Some(1), LATE);
    assert!(!r.ok && r.msg.contains("GPS source"), "{r:?}");
}

/// Before the FC's parameters are read, set 1 is ArduPilot's default, GPS.
#[test]
fn an_unread_set_one_counts_as_gps() {
    let mut f = fc(3.0);
    f.params.remove("EK3_SRC1_POSXY");
    let r = run(&mut f, REL, 1000, LATE);
    assert!(!r.ok && r.msg.contains("GPS source"), "{r:?}");
    assert!(at(&mut f, Some(1), LATE).msg.contains("GPS source"));
}

/// On the ground a status gone quiet blocks and says so; in the air it only
/// warns.
#[test]
fn a_quiet_ekf_status_blocks_on_the_ground() {
    let mut f = fc(3.0);
    assert!(run(&mut f, ABS, 1000, LATE).ok);
    let r = at(&mut f, None, LATE + 4000);
    assert!(!r.ok, "{r:?}");
    assert_eq!(r.severity, Severity::Block);
    assert!(r.msg.contains("not reporting"), "{}", r.msg);
    f.armed = true;
    assert_eq!(at(&mut f, None, LATE + 4000).severity, Severity::Warn);
}

/// A variance that is not a number never passes, and says what is wrong.
#[test]
fn a_nan_variance_blocks_with_its_own_reason() {
    let mut f = fc(3.0);
    feed(&mut f, ABS, f32::NAN, 1000, LATE);
    let r = at(&mut f, None, LATE);
    assert!(!r.ok, "{r:?}");
    assert_eq!(r.severity, Severity::Block);
    assert!(r.msg.contains("variance not reported"), "{}", r.msg);
    assert_eq!(f.ekf_settle.abs_since_ms, None);
}

#[test]
fn a_gps_source_with_only_a_relative_position_from_flow_is_not_ready() {
    let mut f = fc(3.0);
    let r = run(&mut f, REL, 1000, 1000 + 10_000);
    assert!(!r.ok, "{r:?}");
    assert_eq!(r.severity, Severity::Block);
    assert!(
        r.msg.contains("no absolute horizontal position"),
        "{}",
        r.msg
    );
}

#[test]
fn an_absolute_position_younger_than_the_settle_time_is_not_ready() {
    let mut f = fc(3.0);
    let r = run(&mut f, ABS, 1000, 1000 + POSITION_SETTLE_MS - 200);
    assert!(!r.ok, "{r:?}");
    assert_eq!(r.severity, Severity::Block);
    assert!(r.msg.contains("settling"), "{}", r.msg);
}

#[test]
fn an_absolute_position_held_for_the_settle_time_is_ready() {
    let mut f = fc(3.0);
    // Flow positions it first; the clock starts only with the absolute fix.
    run(&mut f, REL, 1000, 6000);
    let r = run(&mut f, ABS, 6000, 6000 + POSITION_SETTLE_MS + 200);
    assert!(r.ok, "{r:?}");
}

#[test]
fn a_flow_only_source_with_a_relative_position_is_ready_after_the_settle_time() {
    let mut f = fc(0.0);
    let r = run(&mut f, REL, 1000, 1000 + POSITION_SETTLE_MS - 200);
    assert!(!r.ok, "{r:?}");
    assert!(r.msg.contains("settling"), "{}", r.msg);
    let r = run(
        &mut f,
        REL,
        1000 + POSITION_SETTLE_MS - 200,
        1000 + POSITION_SETTLE_MS + 200,
    );
    assert!(r.ok, "{r:?}");
}

#[test]
fn a_drop_restarts_the_settle_time() {
    let mut f = fc(3.0);
    assert!(run(&mut f, ABS, 1000, 5000).ok);
    // One report without the absolute position: the item blocks, and the
    // clock restarts when it comes back.
    run(&mut f, REL, 5000, 5200);
    let r = run(&mut f, ABS, 5200, 5200 + POSITION_SETTLE_MS - 200);
    assert!(!r.ok, "{r:?}");
    assert!(r.msg.contains("settling"), "{}", r.msg);
    assert!(run(&mut f, ABS, 5200 + POSITION_SETTLE_MS - 200, 9000).ok);
    // A gap in the reports as long as the staleness window restarts it too.
    let r = run(&mut f, ABS, 12_000, 12_200);
    assert!(!r.ok, "{r:?}");
    assert!(r.msg.contains("settling"), "{}", r.msg);
}

/// Armed, the item keeps ArduPilot's own test: no settle time and a
/// relative position with flow enabled still counts on a GPS source set.
#[test]
fn armed_the_item_keeps_ardupilots_own_test() {
    let mut f = fc(3.0);
    f.armed = true;
    let r = run(&mut f, REL, 1000, 1400);
    assert!(r.ok, "{r:?}");
}
