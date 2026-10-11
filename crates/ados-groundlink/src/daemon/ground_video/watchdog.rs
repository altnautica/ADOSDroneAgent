//! Inbound-stall detection for the ground mediamtx.
//!
//! mediamtx's `bytesReceived` on path `main` is the proof that radio video is
//! reaching the media server. It is only expected to grow while the radio is
//! delivering: the wfb-stats sidecar's `packets_received` is the per-interval
//! count of valid decoded packets. So a flat counter with no RX is the healthy
//! idle state of a ground station with no drone in range, and only a flat
//! counter for [`STALL_WINDOW`] of continuous RX means the UDP listener wedged.

use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

/// Poll cadence for the mediamtx path counter and the RX snapshot.
pub const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// How long `bytesReceived` may stay flat while RX is active before mediamtx
/// is restarted.
pub const STALL_WINDOW: Duration = Duration::from_secs(12);

/// A wfb-stats snapshot older than this no longer describes the radio.
pub const RX_STATS_FRESH: Duration = Duration::from_secs(10);

/// What the caller should do after one poll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Healthy,
    Restart,
}

/// Stall tracker. Feed it one observation per poll.
#[derive(Debug, Default)]
pub struct StallWatchdog {
    last_bytes: Option<u64>,
    /// When the counter last grew, or RX last went idle (the window starts
    /// only once RX is active).
    last_progress: Option<Instant>,
}

impl StallWatchdog {
    pub fn new() -> Self {
        Self::default()
    }

    /// `bytes` is the path counter (`None` when the API did not answer);
    /// `rx_active` is whether the radio delivered packets in the last interval.
    pub fn observe(&mut self, bytes: Option<u64>, rx_active: bool, now: Instant) -> Verdict {
        let grew = match (self.last_bytes, bytes) {
            (Some(prev), Some(cur)) => cur != prev,
            (None, Some(_)) => true,
            (_, None) => false,
        };
        if bytes.is_some() {
            self.last_bytes = bytes;
        }
        if grew || !rx_active {
            self.last_progress = Some(now);
            return Verdict::Healthy;
        }
        let since = *self.last_progress.get_or_insert(now);
        if now.duration_since(since) >= STALL_WINDOW {
            Verdict::Restart
        } else {
            Verdict::Healthy
        }
    }
}

/// Whether the wfb-stats snapshot at `path` is fresh and reports packets in its
/// last interval. Missing, stale or malformed counts as no RX.
pub fn rx_active(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    let fresh = meta
        .modified()
        .ok()
        .and_then(|m| SystemTime::now().duration_since(m).ok())
        .is_some_and(|age| age <= RX_STATS_FRESH);
    if !fresh {
        return false;
    }
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| {
            v.get("packets_received")
                .and_then(serde_json::Value::as_i64)
        })
        .is_some_and(|n| n > 0)
}

#[cfg(test)]
mod tests;
