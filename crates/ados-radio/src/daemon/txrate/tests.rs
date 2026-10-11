use super::*;

#[test]
fn tx_liveness_tracks_counter_progress() {
    let mut live = TxLiveness::new();
    // No reading yet so not live.
    assert!(!live.tx_live());
    // First reading seeds the baseline (does not count as a change).
    live.observe(1000);
    assert!(live.tx_live()); // value > 0 and last_change is "now"
                             // A zero counter is never live even if it just changed.
    let mut zero = TxLiveness::new();
    zero.observe(0);
    assert!(!zero.tx_live());
}

#[test]
fn tx_liveness_rate_zero_before_second_reading() {
    let mut live = TxLiveness::new();
    // The seeding read produces no rate yet.
    live.observe(1000);
    assert_eq!(live.tx_bytes_per_s(), 0.0);
}

#[test]
fn tx_liveness_rate_clamps_counter_reset() {
    let mut live = TxLiveness::new();
    live.observe(5000);
    // A counter that goes BACKWARDS (iface reset / wrap) must not produce a
    // negative rate — saturating_sub clamps the delta to 0.
    live.observe(100);
    assert_eq!(live.tx_bytes_per_s(), 0.0);
}

/// A steady uplink reads its real rate on every heartbeat. `packets_received`
/// is the count for one 1 s stats interval, so two consecutive ticks carrying
/// the same 120-packet interval are 120 pkt/s each — never the 0 a
/// difference between the two readings would produce.
#[test]
fn steady_uplink_reports_its_rate_every_tick() {
    let tx = TxLiveness::new();
    let link = crate::link_quality::LinkStats {
        packets_received: 120,
        ..Default::default()
    };
    assert_eq!(TxRates::sample(&tx, &link).valid_rx_packets_per_s, 120.0);
    assert_eq!(TxRates::sample(&tx, &link).valid_rx_packets_per_s, 120.0);
    // Nothing decoded this interval is an honest zero.
    let quiet = crate::link_quality::LinkStats::default();
    assert_eq!(TxRates::sample(&tx, &quiet).valid_rx_packets_per_s, 0.0);
}
