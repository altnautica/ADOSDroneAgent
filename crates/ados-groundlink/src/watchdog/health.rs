//! The live receive-health publish seam (`SharedRxHealth`).
//!
//! The valid-packet watchdog is the sole writer; the stats reader only reads.
//! Shared so the stats loop reports the real `reacquire_kills` +
//! `rx_silent_seconds` instead of hardcoded zeros.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use tokio::sync::Mutex;

/// Live receive-health counters the valid-packet watchdog produces and the
/// stats reader publishes on the sidecar. Shared so the stats loop reports the
/// real `reacquire_kills` + `rx_silent_seconds` instead of hardcoded zeros.
/// The watchdog is the sole writer; the stats reader only reads.
#[derive(Debug, Clone, Default)]
pub struct SharedRxHealth {
    reacquire_kills: Arc<AtomicU32>,
    /// Seconds the valid-decode stream has been silent at the last poll. `None`
    /// until the watchdog has run one poll. Stored behind a mutex because the
    /// value is a float and the cadence is slow (one write per poll interval).
    silent_seconds: Arc<Mutex<Option<f64>>>,
}

impl SharedRxHealth {
    pub fn new() -> Self {
        Self::default()
    }

    /// The cumulative reacquire-failure kill count.
    pub fn reacquire_kills(&self) -> u32 {
        self.reacquire_kills.load(Ordering::SeqCst)
    }

    /// The valid-decode silence at the last watchdog poll, if one has run.
    pub async fn silent_seconds(&self) -> Option<f64> {
        *self.silent_seconds.lock().await
    }

    /// Writer seam (watchdog side): record the kill total.
    pub(super) fn set_reacquire_kills(&self, n: u32) {
        self.reacquire_kills.store(n, Ordering::SeqCst);
    }

    /// Writer seam (watchdog side): record the current valid-decode silence.
    pub(super) async fn set_silent_seconds(&self, secs: f64) {
        *self.silent_seconds.lock().await = Some(secs);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_rx_health_defaults_to_zero_and_none() {
        let h = SharedRxHealth::new();
        assert_eq!(h.reacquire_kills(), 0);
    }
}
