//! A confined mission never hovers until the battery runs out: once the
//! no-progress rule has used its automatic skips and holds for the operator,
//! nobody answering flies SmartRTL home after `stall_return_after_s`, with
//! the countdown on every surface; an operator's answer ends the countdown.
//! While the hold waits, it says so once a passing condition has cleared.

use ados_protocol::pilot::Owner;

use super::confined::{area, confined, map, mission, post, run_seen};
use super::harness::World;
use crate::modes::copter;

/// A ground station's AUTO in a confined place toward item 2 of the route
/// north along e = 0 at 5 m, a post 0.525 m off every leg past n 15: every
/// leg from here on is too tight, so each hold skips, and the third holds
/// for the operator (two automatic skips in a row).
fn pinched() -> World {
    let mut w = World::new();
    confined(&mut w);
    w.airborne(copter::AUTO);
    w.run(100);
    map(&mut w, &area(&post(15.0)));
    mission(&mut w, "active", 2);
    w
}

fn banner_key(w: &World) -> Option<String> {
    w.p.state(w.now).banner.map(|b| b.text_key)
}

/// Run past the two automatic skips to the operator wait: held, no skip
/// countdown, the return countdown running. Returns its seconds left.
fn to_operator_wait(w: &mut World) -> f64 {
    run_seen(w, 1100);
    run_seen(w, 46_000);
    run_seen(w, 46_000);
    let s = w.p.state(w.now);
    let stall = s.mission_stall.expect("held for the operator");
    assert_eq!(stall.seq, Some(4), "{stall:?}");
    assert_eq!(stall.skip_in_s, None, "{stall:?}");
    let in_s = stall.return_in_s.expect("return countdown");
    // `stall_return_after_s` (45 s) from the wait's start, a few seconds in.
    assert!((40.0..45.0).contains(&in_s), "{in_s}");
    assert_eq!(s.hold_reason.as_deref(), Some("route-infeasible"));
    assert_eq!(w.p.owner(), Owner::Pilot);
    assert_eq!(w.set_modes(copter::SMART_RTL), 0);
    in_s
}

#[test]
fn a_confined_mission_held_for_the_operator_returns_home_unanswered() {
    let mut w = pinched();
    let left = to_operator_wait(&mut w);
    // The countdown runs down on the surfaces, the vehicle still held.
    run_seen(&mut w, 30_000);
    let stall = w.p.state(w.now).mission_stall.expect("still held");
    let in_s = stall.return_in_s.expect("return countdown");
    assert!((in_s - (left - 30.0)).abs() < 0.2, "{in_s} of {left}");
    assert_eq!(w.set_modes(copter::SMART_RTL), 0);
    // Nobody answered: SmartRTL home along the flown path, announced as
    // such (the FC's own mode banner follows once it flies it).
    run_seen(&mut w, (in_s * 1000.0) as u64 - 200);
    assert_eq!(w.set_modes(copter::SMART_RTL), 0);
    let mut announced = None;
    for _ in 0..30 {
        w.run(20);
        if w.set_modes(copter::SMART_RTL) > 0 {
            announced = banner_key(&w);
            break;
        }
    }
    assert_eq!(
        announced.as_deref(),
        Some("pilot.banner.mission_stalled_returning")
    );
    w.run(500);
    assert_eq!(w.set_modes(copter::SMART_RTL), 1);
    assert_eq!(w.mode, copter::SMART_RTL);
    assert_eq!(w.p.owner(), Owner::FcMode);
    let s = w.p.state(w.now);
    assert!(s.mission_stall.is_none(), "{:?}", s.mission_stall);
    assert_eq!(s.hold_reason, None);
    // Never AUTO into the pinch on the way.
    assert_eq!(w.set_modes(copter::AUTO), 0);
}

#[test]
fn an_operator_answer_during_the_return_countdown_cancels_it() {
    // Sticks: the operator flies, the mission waits for `resume`.
    let mut w = pinched();
    to_operator_wait(&mut w);
    w.input([0.0; 4]);
    w.stick([0.6, 0.0, 0.0, 0.0], 200);
    let s = w.p.state(w.now);
    assert!(s.mission_stall.is_none(), "{:?}", s.mission_stall);
    assert_eq!(w.p.owner(), Owner::Pilot);
    w.stick([0.0; 4], 60_000);
    assert_eq!(w.set_modes(copter::SMART_RTL), 0, "no return of its own");
    // Land here: the vehicle lands, it does not return. The operator's
    // surface stays connected (its zero sticks), so no link-loss return.
    let mut w = pinched();
    to_operator_wait(&mut w);
    assert!(super::confined::ok(&w.action("land")));
    assert!(w.p.state(w.now).mission_stall.is_none());
    w.stick([0.0; 4], 60_000);
    assert_eq!(w.set_modes(copter::SMART_RTL), 0, "no return of its own");
}

#[test]
fn a_skip_held_on_an_unverified_position_waits_as_a_stall_once_it_reads_again() {
    // At the first automatic skip the FC's local position has gone: the
    // mission is held for the operator. Once the position is back the hold
    // still waits, and says so, with the return countdown running.
    let mut w = pinched();
    run_seen(&mut w, 1100);
    run_seen(&mut w, 43_000);
    w.local = false;
    run_seen(&mut w, 3000);
    let s = w.p.state(w.now);
    assert!(
        s.mission.expect("mission").gaps.iter().any(|g| g.seq == 2),
        "skipped"
    );
    w.local = true;
    run_seen(&mut w, 2000);
    let s = w.p.state(w.now);
    assert_eq!(s.hold_reason.as_deref(), Some("mission-stalled"));
    let stall = s.mission_stall.expect("held for the operator");
    assert_eq!(stall.skip_in_s, None);
    assert!(stall.return_in_s.is_some_and(|s| s > 38.0), "{stall:?}");
    assert_eq!(w.p.owner(), Owner::Pilot);
    assert_ne!(w.mode, copter::AUTO);
    run_seen(&mut w, 45_000);
    assert_eq!(w.mode, copter::SMART_RTL);
}

#[test]
fn an_unanswered_wait_on_an_unverified_position_lands_here() {
    // The position never comes back: the return is never flown on it, the
    // vehicle lands where it is.
    let mut w = pinched();
    run_seen(&mut w, 1100);
    run_seen(&mut w, 43_000);
    w.local = false;
    run_seen(&mut w, 3000);
    let stall =
        w.p.state(w.now)
            .mission_stall
            .expect("held for the operator");
    assert!(stall.return_in_s.is_some(), "{stall:?}");
    run_seen(&mut w, 45_000);
    assert_eq!(w.set_modes(copter::SMART_RTL), 0);
    assert_eq!(w.mode, copter::LAND);
    let s = w.p.state(w.now);
    assert!(s.mission_stall.is_none(), "{:?}", s.mission_stall);
    assert_eq!(s.hold_reason, None);
}
