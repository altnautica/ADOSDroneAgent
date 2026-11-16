use serde_json::json;

use super::*;

/// Feed flow at 20 Hz with `quality` from `from` for `ms`.
fn flow(t: &mut FlowTrack, quality: u8, from: u64, ms: u64) -> u64 {
    let mut now = from;
    while now < from + ms {
        t.on_msg(quality, now);
        now += 50;
    }
    now
}

#[test]
fn flow_goes_absent_ok_low_lost() {
    let mut t = FlowTrack::default();
    assert_eq!(t.state(0), FlowState::Absent);
    let now = flow(&mut t, 200, 1000, 2000);
    // At the last sample, 20 Hz.
    let h = t.health(now - 50);
    assert_eq!((h.quality, h.rate_hz), (Some(200), Some(20.0)));
    // Low quality: still ok until it has held for low_hold_ms.
    let start = now;
    let now = flow(&mut t, 60, start, 900);
    assert_eq!(t.state(now), FlowState::Ok);
    let now = flow(&mut t, 60, now, 200);
    assert_eq!(t.state(now), FlowState::Low);
    assert_eq!(t.bad_since(now), Some(start + 1000));
    // One good sample clears the hold.
    t.on_msg(200, now);
    assert_eq!(t.state(now), FlowState::Ok);
    // Nothing for lost_ms after the last one: lost.
    assert_eq!(t.state(now + 999), FlowState::Ok);
    assert_eq!(t.state(now + 1000), FlowState::Lost);
    assert_eq!(t.bad_since(now + 1500), Some(now + 1000));
    let h = t.health(now + 1500);
    assert_eq!(h.age_ms, Some(1500));
}

#[test]
fn a_low_running_into_a_loss_counts_from_the_low() {
    let mut t = FlowTrack::default();
    let now = flow(&mut t, 50, 0, 1500);
    assert_eq!(t.state(now), FlowState::Low);
    assert_eq!(t.state(now + 2000), FlowState::Lost);
    assert_eq!(t.bad_since(now + 2000), Some(1000));
}

#[test]
fn range_out_of_range_and_lost() {
    let mut t = RangeTrack::default();
    assert_eq!(t.state(0), RangeState::Absent);
    t.on_msg(420, 2, 1200, 100);
    let h = t.health(150);
    assert_eq!(
        (h.state, h.height_m, h.age_ms),
        (RangeState::Ok, Some(4.2), Some(50))
    );
    t.on_msg(1300, 2, 1200, 200);
    let h = t.health(200);
    assert_eq!((h.state, h.height_m), (RangeState::OutOfRange, None));
    t.on_msg(1, 2, 1200, 300);
    assert_eq!(t.state(300), RangeState::OutOfRange);
    assert_eq!(t.state(1300), RangeState::Lost);
}

/// Scans every `period_ms` from `from` for `ms`, each with `returns`
/// returns; the time of the last one.
fn scans_with(t: &mut LidarTrack, returns: usize, period_ms: u64, from: u64, ms: u64) -> u64 {
    let mut now = from;
    while now < from + ms {
        t.on_scan(returns, now);
        now += period_ms;
    }
    now - period_ms
}

/// Scans that returned a surface.
fn scans(t: &mut LidarTrack, period_ms: u64, from: u64, ms: u64) -> u64 {
    scans_with(t, 120, period_ms, from, ms)
}

#[test]
fn lidar_scans_without_a_return_read_no_returns_after_the_hold() {
    let hold = spec().thresholds.lidar.no_returns_ms;
    let mut t = LidarTrack::default();
    let now = scans(&mut t, 100, 1000, 1000);
    assert_eq!(t.state(now), LidarState::Ok);
    // Empty scans keep arriving at the full rate: still ok until the hold.
    let first_empty = now + 100;
    let now = scans_with(&mut t, 0, 100, first_empty, hold);
    assert_eq!(t.state(now), LidarState::Ok, "within the hold");
    assert_eq!(t.state(first_empty + hold), LidarState::NoReturns);
    let h = t.health(first_empty + hold);
    // Scanning at the full rate the whole time (the last 100 ms of the
    // 2 s window not yet filled): no returns, not a rate fault.
    assert_eq!(h.state, LidarState::NoReturns);
    assert!(h.rate_hz.is_some_and(|r| r >= 9.5), "{:?}", h.rate_hz);
    // Still scanning, so silence is lost, not no returns.
    assert_eq!(t.state(now + 500), LidarState::Lost);
    // A single return ends it.
    let now = scans(&mut t, 100, now + 100, 100);
    assert_eq!(t.state(now), LidarState::Ok);
}

const LEVEL: [f32; 2] = [0.0, 0.0];

/// A wide, still surface `range_m` away (straight north) returned by
/// enough scans in a row to count.
fn confirm(t: &mut LidarTrack, at: [f32; 3], tilt: [f32; 2], range_m: f32) {
    for _ in 0..SURFACE_CONFIRM_SCANS {
        t.on_surface(at, tilt, range_m, [at[0] + range_m, at[1]], 3.0);
    }
}

/// Whether an empty scan from `ned` tilted `tilt`, the first of a new empty
/// run, still has the surface in reach.
fn first_empty_in_reach(t: &mut LidarTrack, ned: [f32; 3], tilt: [f32; 2]) -> bool {
    t.on_scan(1, 0);
    t.on_scan(0, 100);
    t.surface_in_reach(ned, tilt)
}

#[test]
fn a_surface_stays_in_reach_until_the_vehicle_leaves_its_scan() {
    let mut t = LidarTrack::default();
    assert!(!first_empty_in_reach(&mut t, [0.0, 0.0, -5.0], LEVEL));
    // A wall 3 m away seen from 5 m up: in reach while the 12 m planar scan
    // still covers it (moved + 3 < 12 - 1) and the vehicle stays in its plane.
    confirm(&mut t, [0.0, 0.0, -5.0], LEVEL, 3.0);
    assert!(first_empty_in_reach(&mut t, [0.0, 0.0, -5.0], LEVEL));
    assert!(first_empty_in_reach(&mut t, [7.9, 0.0, -5.3], LEVEL));
    assert!(
        !first_empty_in_reach(&mut t, [8.1, 0.0, -5.0], LEVEL),
        "flown away from it"
    );
    assert!(
        !first_empty_in_reach(&mut t, [0.0, 0.0, -5.6], LEVEL),
        "climbed above it"
    );
    // Tilted: the plane moves 3 m · tan Δ at the wall. 5° of pitch moves it
    // 0.26 m, still on the wall; 15° of roll 0.80 m, maybe over it.
    assert!(first_empty_in_reach(
        &mut t,
        [0.0, 0.0, -5.0],
        [0.0, 5f32.to_radians()]
    ));
    assert!(
        !first_empty_in_reach(&mut t, [0.0, 0.0, -5.0], [15f32.to_radians(), 0.0]),
        "tilted off it"
    );
    // Farther away the same tilt reaches farther: 5° at 8 m is 0.70 m.
    assert!(!first_empty_in_reach(
        &mut t,
        [5.0, 0.0, -5.0],
        [0.0, 5f32.to_radians()]
    ));
    // A surface first seen near the horizon is never in reach.
    confirm(&mut t, [0.0, 0.0, -5.0], LEVEL, 11.5);
    assert!(!first_empty_in_reach(&mut t, [0.0, 0.0, -5.0], LEVEL));
    // A new flight forgets the surface until the next one.
    confirm(&mut t, [0.0, 0.0, -5.0], LEVEL, 3.0);
    t.new_flight();
    assert_eq!(t.surface(), None);
    assert!(!first_empty_in_reach(&mut t, [0.0, 0.0, -5.0], LEVEL));
}

#[test]
fn a_surface_counts_only_still_wide_and_scans_in_a_row() {
    let at = [0.0, 0.0, -5.0];
    let mut t = LidarTrack::default();
    // One stray return (rain, an insect), then nothing: no surface.
    t.on_surface(at, LEVEL, 1.5, [1.5, 0.0], 3.0);
    t.on_no_surface();
    assert_eq!(t.surface(), None);
    // Two in a row, then somewhere else: still none.
    t.on_surface(at, LEVEL, 1.5, [1.5, 0.0], 3.0);
    t.on_surface(at, LEVEL, 1.6, [1.6, 0.0], 3.0);
    t.on_surface(at, LEVEL, 4.0, [0.0, 4.0], 3.0);
    assert_eq!(t.surface(), None);
    // Narrow (a person, a drone), however still: never.
    for _ in 0..10 {
        t.on_surface(at, LEVEL, 3.0, [3.0, 0.0], 0.4);
    }
    assert_eq!(t.surface(), None);
    // Wide and still across three scans, the vehicle closing at 3 m/s at
    // 10 Hz: a surface, the newest.
    t.on_surface(at, LEVEL, 4.0, [4.0, 0.0], 3.0);
    t.on_surface([0.3, 0.0, -5.0], LEVEL, 3.7, [4.0, 0.0], 3.0);
    t.on_surface([0.6, 0.0, -5.0], LEVEL, 3.4, [4.0, 0.0], 3.0);
    assert_eq!(t.surface().map(|s| s.range_m), Some(3.4));
}

#[test]
fn the_tilt_is_judged_when_the_returns_stop_not_while_braking() {
    // Flying at a wall 5 m ahead pitched 7° forward when the lidar goes
    // blind: in reach. Braking pitches it 12° back, then level: still held.
    let mut t = LidarTrack::default();
    let fwd = [0.0, -7f32.to_radians()];
    confirm(&mut t, [0.0, 0.0, -5.0], fwd, 5.0);
    assert!(first_empty_in_reach(&mut t, [0.0, 0.0, -5.0], fwd));
    t.on_scan(0, 200);
    assert!(t.surface_in_reach([0.2, 0.0, -5.0], [0.0, 12f32.to_radians()]));
    t.on_scan(0, 300);
    assert!(t.surface_in_reach([0.3, 0.0, -5.0], LEVEL));
    // A climb out of the plane still releases it.
    t.on_scan(0, 400);
    assert!(!t.surface_in_reach([0.3, 0.0, -5.6], LEVEL));
}

#[test]
fn lidar_rate_degraded_and_lost_from_scan_arrivals() {
    let mut t = LidarTrack::default();
    assert!(!t.expected());
    assert_eq!(t.state(0), LidarState::Absent);
    let now = scans(&mut t, 100, 1000, 3000);
    assert!(t.expected());
    let h = t.health(now);
    assert_eq!((h.state, h.rate_hz), (LidarState::Ok, Some(10.0)));
    // 5 Hz for a full window: degraded.
    let now = scans(&mut t, 200, now + 200, 2400);
    let h = t.health(now);
    assert_eq!((h.state, h.rate_hz), (LidarState::Degraded, Some(5.0)));
    // No scan for lost_ms: lost.
    assert_eq!(t.state(now + 499), LidarState::Degraded);
    assert_eq!(t.state(now + 500), LidarState::Lost);
}

#[test]
fn lidar_topic_health_and_disconnect() {
    let mut t = LidarTrack::default();
    t.on_status(&json!({"v": 1, "connected": true, "health": "good"}), 0);
    assert!(t.expected());
    assert_eq!(t.state(0), LidarState::Absent);
    let now = scans(&mut t, 100, 100, 2500);
    assert_eq!(t.state(now), LidarState::Ok);
    t.on_status(
        &json!({"v": 1, "connected": true, "health": "warning"}),
        now,
    );
    let h = t.health(now);
    assert_eq!(
        (h.state, h.health),
        (LidarState::Degraded, Some(LidarDriverHealth::Warning))
    );
    t.on_status(&json!({"v": 1, "connected": false, "health": null}), now);
    assert_eq!(t.state(now), LidarState::Lost);
}
