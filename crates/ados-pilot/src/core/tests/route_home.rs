//! The routed return's reserve: while the pilot routes a blocked return
//! home, the low-battery reserve counts the planned way, not the flown path
//! or the straight line.

use super::harness::*;
use crate::core::control::Routed;
use crate::modes::copter;

#[test]
fn the_reserve_counts_the_planned_way_home_while_routed() {
    let mut w = World::new();
    w.airborne_at(copter::GUIDED, [30.0, 0.0, -10.0]);
    w.run(500);
    let now = w.now;
    let straight = w.p.battery_reserve(60.0, 30.0, now).return_s;
    w.p.sup.routed = Some(Routed {
        since_ms: now,
        remaining_m: 200.0,
        straight_m: 30.0,
    });
    let routed = w.p.battery_reserve(60.0, 30.0, now).return_s;
    // 170 m more at no more than the 5 m/s waypoint speed; the countdown
    // (counted before, as for any return not yet flying) no longer is.
    let countdown = w.p.settings.pilot.low_battery_countdown_s as f32;
    assert!(
        routed - straight >= 170.0 / 5.0 - countdown - 0.5,
        "straight {straight} s, routed {routed} s"
    );
    let wire = w.p.state(now).return_route.expect("published while routed");
    assert_eq!((wire.remaining_m, wire.straight_m), (200.0, 30.0));
    w.p.sup.routed = None;
    assert!(w.p.state(now).return_route.is_none());
}
