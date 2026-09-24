//! TX-byte liveness + smoothed transmit-rate tracking for the heartbeat.
//!
//! Fed the data-plane `wfb_tx`'s own cumulative injected-bytes total (its
//! per-second stats line, summed), from which it derives both the "the radio is
//! actually injecting" liveness signal and the smoothed transmit rate the
//! sidecar surfaces. The RTL monitor netdev's `tx_bytes` is not used: it is not
//! a reliable primary signal on that interface. The [`TxRates`] snapshot is the
//! per-tick rate pair the sidecar writer consumes.

use std::time::{Duration, Instant};

/// Liveness window: the radio counts as actively injecting when its injected
/// byte total has moved within this many seconds.
const TX_LIVE_WINDOW: Duration = Duration::from_secs(5);

/// Transmit/uplink rate snapshot surfaced on the heartbeat. `tx_bytes_per_s` is
/// the smoothed radio transmit rate; `valid_rx_packets_per_s` is the uplink
/// valid-decode rate from the rx-control stats stream (0 on a drone hearing no
/// uplink).
#[derive(Clone, Copy, Default)]
pub(crate) struct TxRates {
    pub(crate) tx_bytes_per_s: f64,
    pub(crate) valid_rx_packets_per_s: f64,
}

impl TxRates {
    /// This heartbeat's rate pair: the smoothed transmit rate and the latest
    /// per-interval uplink decode rate.
    pub(crate) fn sample(tx: &TxLiveness, link: &ados_radio::link_quality::LinkStats) -> Self {
        Self {
            tx_bytes_per_s: tx.tx_bytes_per_s(),
            valid_rx_packets_per_s: link.valid_packets_per_s(),
        }
    }
}

/// Tracks the data plane's injected-bytes total so the heartbeat can report
/// whether frames are actually being injected AND the smoothed transmit rate.
/// Polled in the 2 s heartbeat loop; `tx_live()` is the "active" signal the
/// link-state derivation uses, `tx_bytes_per_s()` is the rate the sidecar
/// surfaces.
pub(crate) struct TxLiveness {
    last_value: u64,
    last_change: Instant,
    seen: bool,
    /// Value + instant of the previous poll, for the rate delta.
    prev_value: u64,
    prev_at: Instant,
    rate_bytes_per_s: f64,
}

impl TxLiveness {
    pub(crate) fn new() -> Self {
        let now = Instant::now();
        Self {
            last_value: 0,
            last_change: now,
            seen: false,
            prev_value: 0,
            prev_at: now,
            rate_bytes_per_s: 0.0,
        }
    }

    /// Feed the current injected-bytes total; records a change instant when it
    /// advances and updates the smoothed transmit rate from the inter-poll
    /// delta. The first reading seeds the baseline without counting as a change.
    pub(crate) fn observe(&mut self, value: u64) {
        let now = Instant::now();
        if !self.seen {
            self.last_value = value;
            self.prev_value = value;
            self.prev_at = now;
            self.seen = true;
            return;
        }
        if value != self.last_value {
            self.last_value = value;
            self.last_change = now;
        }
        // Rate over the elapsed poll interval (counters never decrease, but a
        // wrap/reset is clamped to 0 rather than producing a negative rate).
        let elapsed = now.duration_since(self.prev_at).as_secs_f64();
        if elapsed > 0.0 {
            let delta = value.saturating_sub(self.prev_value) as f64;
            self.rate_bytes_per_s = delta / elapsed;
        }
        self.prev_value = value;
        self.prev_at = now;
    }

    /// True when the counter is non-zero and advanced within the live window.
    pub(crate) fn tx_live(&self) -> bool {
        self.last_value > 0 && self.last_change.elapsed() < TX_LIVE_WINDOW
    }

    /// The smoothed radio transmit rate in bytes/second.
    pub(crate) fn tx_bytes_per_s(&self) -> f64 {
        self.rate_bytes_per_s
    }
}

#[cfg(test)]
mod tests {
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
        let link = ados_radio::link_quality::LinkStats {
            packets_received: 120,
            ..Default::default()
        };
        assert_eq!(TxRates::sample(&tx, &link).valid_rx_packets_per_s, 120.0);
        assert_eq!(TxRates::sample(&tx, &link).valid_rx_packets_per_s, 120.0);
        // Nothing decoded this interval is an honest zero.
        let quiet = ados_radio::link_quality::LinkStats::default();
        assert_eq!(TxRates::sample(&tx, &quiet).valid_rx_packets_per_s, 0.0);
    }
}
