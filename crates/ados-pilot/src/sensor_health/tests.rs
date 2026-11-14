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

fn scans(t: &mut LidarTrack, period_ms: u64, from: u64, ms: u64) -> u64 {
    let mut now = from;
    while now < from + ms {
        t.on_scan(now);
        now += period_ms;
    }
    now - period_ms
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
