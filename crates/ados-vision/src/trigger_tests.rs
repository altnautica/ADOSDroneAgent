use super::*;

const PERIOD: u64 = 50_000_000; // 20 Hz

fn rec(seq: u64, t_mid_ns: u64) -> TriggerRecord {
    TriggerRecord {
        seq,
        t_mid_ns,
        exposure_ns: 4_000_000,
        gain: 0.0,
    }
}

/// Hub triggers 1000.. every 50 ms from t = 10 s.
fn hub_records(n: u64) -> VecDeque<TriggerRecord> {
    (0..n)
        .map(|i| rec(1000 + i, 10_000_000_000 + i * PERIOD))
        .collect()
}

#[test]
fn the_offset_is_learned_from_time_then_followed_by_sequence() {
    let records = hub_records(20);
    let mut j = TriggerJoiner::learning(PERIOD / 2);
    // Frame 0 of the V4L2 stream, stamped 3 ms after the mid of trigger 1005.
    let first = j.join(&records, 0, 10_000_000_000 + 5 * PERIOD + 3_000_000).unwrap();
    assert_eq!(first.seq, 1005);
    // Later frames follow the sequence even with a stamp skewed by 20 ms
    // (still inside the tolerance), never snapping to a nearer record.
    let next = j.join(&records, 3, 10_000_000_000 + 8 * PERIOD + 20_000_000).unwrap();
    assert_eq!(next.seq, 1008);
    assert_eq!(next.t_mid_ns, 10_000_000_000 + 8 * PERIOD);
}

#[test]
fn a_dropped_frame_does_not_shift_the_match() {
    let records = hub_records(20);
    let mut j = TriggerJoiner::learning(PERIOD / 2);
    j.join(&records, 10, 10_000_000_000 + 2 * PERIOD).unwrap();
    // Frames 11 and 12 were dropped by the driver; 13 still maps to 1005.
    assert_eq!(
        j.join(&records, 13, 10_000_000_000 + 5 * PERIOD).map(|r| r.seq),
        Some(1005)
    );
}

#[test]
fn a_disagreeing_match_relearns_the_offset() {
    let records = hub_records(20);
    let mut j = TriggerJoiner::learning(PERIOD / 2);
    j.join(&records, 0, 10_000_000_000).unwrap();
    // The camera restarted streaming (its sequence reset to 0) half a second
    // later; the stale offset points at a record 10 periods away.
    let got = j.join(&records, 0, 10_000_000_000 + 10 * PERIOD).unwrap();
    assert_eq!(got.seq, 1010);
    assert_eq!(
        j.join(&records, 1, 10_000_000_000 + 11 * PERIOD).map(|r| r.seq),
        Some(1011)
    );
}

#[test]
fn no_record_near_the_frame_is_no_match() {
    let records = hub_records(5);
    let mut j = TriggerJoiner::learning(PERIOD / 2);
    assert!(j.join(&records, 0, 5_000_000_000).is_none());
    assert!(j.join(&VecDeque::new(), 0, 10_000_000_000).is_none());
}

#[test]
fn a_simulated_camera_joins_by_sequence_alone() {
    // Simulator triggers carry the render sequence; the stamps lag the frame
    // arrival by the encode and stream latency, far past any tolerance.
    let records: VecDeque<_> = (40..45).map(|s| rec(s, s * PERIOD)).collect();
    let mut j = TriggerJoiner::same_sequence();
    assert_eq!(j.join(&records, 42, 999 * PERIOD).map(|r| r.t_mid_ns), Some(42 * PERIOD));
    assert!(j.join(&records, 50, 50 * PERIOD).is_none());
    // A miss never re-learns a fixed offset.
    assert_eq!(j.join(&records, 43, 0).map(|r| r.seq), Some(43));
}

#[test]
fn the_feed_keeps_streams_apart_and_bounded() {
    let feed = TriggerFeed::default();
    for i in 0..(TRIGGER_HISTORY as u64 + 10) {
        feed.ingest(Origin::Hub, "cam1", rec(i, i));
    }
    feed.ingest(Origin::Fc, "cam0", rec(7, 7));
    let hub = (Origin::Hub, "cam1".to_string());
    feed.with_stream(&hub, |r| {
        assert_eq!(r.len(), TRIGGER_HISTORY);
        assert_eq!(r.front().unwrap().seq, 10);
    });
    let fc = (Origin::Fc, "cam0".to_string());
    feed.with_stream(&fc, |r| assert_eq!(r.len(), 1));
    let mut cam = CameraTriggers::new(feed, fc, TriggerJoiner::learning(PERIOD));
    assert_eq!(cam.join(0, 7).map(|r| r.seq), Some(7));
}

#[test]
fn trigger_sources_name_their_bus_streams() {
    assert_eq!(trigger_stream(CameraTrigger::None, 0), None);
    assert_eq!(
        trigger_stream(CameraTrigger::Hub, 2),
        Some((Origin::Hub, "cam2".to_string()))
    );
    assert_eq!(
        trigger_stream(CameraTrigger::Fc, 2),
        Some((Origin::Fc, "cam0".to_string()))
    );
}
