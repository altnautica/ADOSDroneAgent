//! Honest received-side counters for the config-tunnel channel.
//!
//! Per Rule 37/44, a config lane that reports "healthy" without proof trains
//! the operator to distrust telemetry. These counters are the delivery proof:
//! a received frame count that never advances means nothing is arriving over
//! the bearer — a truthful "not active", never a fabricated green.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Unix-millis now, `0` if the clock is before the epoch (never in practice).
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Live counters shared between the service loop and the sidecar writer.
#[derive(Debug, Default)]
pub struct Counters {
    rx_frames: AtomicU64,
    tx_frames: AtomicU64,
    requests: AtomicU64,
    responses: AtomicU64,
    rejected: AtomicU64,
    timeouts: AtomicU64,
    last_rx_ms: AtomicU64,
}

/// A point-in-time snapshot for the sidecar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CountersSnapshot {
    pub rx_frames: u64,
    pub tx_frames: u64,
    pub requests: u64,
    pub responses: u64,
    pub rejected: u64,
    pub timeouts: u64,
    /// Unix-millis of the last received frame, `0` when none has arrived.
    pub last_rx_ms: u64,
}

impl Counters {
    /// A received TUNNEL frame was accepted off the bearer; stamps last-rx.
    pub fn mark_rx(&self) {
        self.rx_frames.fetch_add(1, Ordering::Relaxed);
        self.last_rx_ms.store(now_ms(), Ordering::Relaxed);
    }
    /// A TUNNEL frame was sent onto the bearer.
    pub fn mark_tx(&self) {
        self.tx_frames.fetch_add(1, Ordering::Relaxed);
    }
    /// A complete config REQUEST was reassembled + handled (drone side).
    pub fn mark_request(&self) {
        self.requests.fetch_add(1, Ordering::Relaxed);
    }
    /// A complete config RESPONSE was reassembled + correlated (GS side).
    pub fn mark_response(&self) {
        self.responses.fetch_add(1, Ordering::Relaxed);
    }
    /// A well-formed chunk breached a bound and its message was dropped.
    pub fn mark_rejected(&self) {
        self.rejected.fetch_add(1, Ordering::Relaxed);
    }
    /// `n` in-flight messages were swept out for exceeding the reassembly
    /// deadline.
    pub fn add_timeouts(&self, n: u64) {
        self.timeouts.fetch_add(n, Ordering::Relaxed);
    }

    /// Read a consistent-enough snapshot for the sidecar (relaxed loads; the
    /// counters are advisory telemetry, not a lock-step ledger).
    #[must_use]
    pub fn snapshot(&self) -> CountersSnapshot {
        CountersSnapshot {
            rx_frames: self.rx_frames.load(Ordering::Relaxed),
            tx_frames: self.tx_frames.load(Ordering::Relaxed),
            requests: self.requests.load(Ordering::Relaxed),
            responses: self.responses.load(Ordering::Relaxed),
            rejected: self.rejected.load(Ordering::Relaxed),
            timeouts: self.timeouts.load(Ordering::Relaxed),
            last_rx_ms: self.last_rx_ms.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_accumulate_and_snapshot() {
        let c = Counters::default();
        assert_eq!(c.snapshot(), CountersSnapshot::default());
        c.mark_rx();
        c.mark_rx();
        c.mark_tx();
        c.mark_request();
        c.mark_rejected();
        c.add_timeouts(3);
        let s = c.snapshot();
        assert_eq!(s.rx_frames, 2);
        assert_eq!(s.tx_frames, 1);
        assert_eq!(s.requests, 1);
        assert_eq!(s.rejected, 1);
        assert_eq!(s.timeouts, 3);
        assert!(s.last_rx_ms > 0);
    }
}
