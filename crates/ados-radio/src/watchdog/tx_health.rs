//! Data-plane transmit health watchdog.

use super::*;

/// Folds a transmit plane's cumulative stats totals into the tested
/// [`TxLivenessWindow`]: each poll's delta becomes one observation, taken only
/// when the plane printed at least one new stats line (a flat line count is the
/// silent-loop case the window detects on its own). Pure over `(now, totals)`.
#[derive(Debug)]
pub struct TxPlaneTracker {
    prev: TxPlaneTotals,
    window: TxLivenessWindow,
}

impl TxPlaneTracker {
    pub fn new(now: tokio::time::Instant, totals: TxPlaneTotals, silence: Duration) -> Self {
        Self {
            prev: totals,
            window: TxLivenessWindow::new(now, silence),
        }
    }

    /// Fold one poll and judge the window (`None` while it is still filling).
    pub fn observe(
        &mut self,
        now: tokio::time::Instant,
        totals: TxPlaneTotals,
    ) -> Option<TxVerdict> {
        if totals.lines > self.prev.lines {
            self.window.observe(
                TxPkt {
                    bytes_in: totals.bytes_in.saturating_sub(self.prev.bytes_in),
                    bytes_injected: totals
                        .bytes_injected
                        .saturating_sub(self.prev.bytes_injected),
                    packets_dropped: 0,
                },
                now,
            );
        }
        self.prev = totals;
        self.window.evaluate(now)
    }
}

/// Watch the data-plane `wfb_tx`'s own stats counters. Returns when it should
/// be killed ([`WatchdogFired::TxStalled`]), when its PHY is muted
/// ([`WatchdogFired::PhyMuted`], a recovery rather than a kill), or on
/// `cancel`. `stats` is the data plane's process-lifetime counter handle, so a
/// retune respawn keeps feeding the same window.
pub async fn tx_health_watchdog(
    iface: &str,
    stats: Arc<TxPlaneCounters>,
    counters: CounterHandle,
    cancel: Shutdown,
) -> WatchdogFired {
    let mut tracker = TxPlaneTracker::new(
        tokio::time::Instant::now(),
        stats.totals(),
        TX_SILENCE_THRESHOLD,
    );
    let mut last_upstream_silent_log = Instant::now() - UPSTREAM_SILENT_LOG_INTERVAL;

    loop {
        tokio::select! {
            _ = tokio::time::sleep(POLL_INTERVAL) => {}
            _ = cancel.wait() => return WatchdogFired::Cancelled,
        }
        match tracker.observe(tokio::time::Instant::now(), stats.totals()) {
            Some(TxVerdict::StalledNotInjecting) => {
                // Ingress arrives but nothing reaches the radio. If the PHY is
                // pinned at the muted not-permitted floor (the RTL8812EU `set
                // type monitor` mute), killing wfb_tx can never un-mute it: the
                // fault is in the driver/PHY, so route to a PHY recovery.
                let muted = crate::adapter::read_tx_power(iface)
                    .await
                    .map(|dbm| dbm <= crate::adapter::MUTED_TX_POWER_DBM)
                    .unwrap_or(false);
                if muted {
                    tracing::warn!(
                        iface,
                        "wfb_tx_stalled_phy_muted: routing to PHY-recovery, not a kill"
                    );
                    return WatchdogFired::PhyMuted;
                }
                tracing::warn!(iface, "wfb_tx_stalled_kill: ingress with no injection");
                counters.lock().await.tx_zombie_kills += 1;
                return WatchdogFired::TxStalled;
            }
            Some(TxVerdict::StalledSilent) => {
                tracing::warn!(
                    iface,
                    "wfb_tx_stalled_kill: no stats line for the whole window"
                );
                counters.lock().await.tx_zombie_kills += 1;
                return WatchdogFired::TxStalled;
            }
            Some(TxVerdict::Idle) => {
                // Upstream (the video encoder) offered nothing — not a fault.
                if last_upstream_silent_log.elapsed() >= UPSTREAM_SILENT_LOG_INTERVAL {
                    tracing::info!(iface, "wfb_tx_upstream_silent");
                    last_upstream_silent_log = Instant::now();
                }
            }
            Some(TxVerdict::Healthy) | None => {}
        }
    }
}
