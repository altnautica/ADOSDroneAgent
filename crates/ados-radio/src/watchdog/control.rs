//! Control-plane stall watchdog.

use super::*;

/// Flat-counter window before a control plane counts as silently stalled. Same
/// 30 s window as the data-plane TX watchdog.
pub(super) const CONTROL_SILENCE_THRESHOLD: Duration = Duration::from_secs(30);

/// Which control plane stopped doing work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlPlane {
    /// The tx-control `wfb_tx`: the ingress bytes its own stats report stopped
    /// advancing even though the presence-beacon emitter feeds it every 10 s.
    TxControl,
    /// The rx-control `wfb_rx`: it stopped printing its per-second stats line.
    RxControl,
}

impl ControlPlane {
    pub fn as_str(self) -> &'static str {
        match self {
            ControlPlane::TxControl => "tx_control",
            ControlPlane::RxControl => "rx_control",
        }
    }
}

/// Delta-counter judgement over the control planes' cumulative stats counters
/// `(tx-control ingress bytes, rx-control stats lines)`. Each counter must advance inside a rolling
/// [`CONTROL_SILENCE_THRESHOLD`] window; a live process whose counter stays flat
/// is doing no work. Pure over `(now, counters)` so the rule is testable without
/// processes or a clock.
#[derive(Debug)]
pub struct ControlStallTracker {
    last: (u64, u64),
    tx_progress_at: Instant,
    rx_progress_at: Instant,
}

impl ControlStallTracker {
    /// Start a window at `now` from the counters' current values.
    pub fn new(now: Instant, counters: (u64, u64)) -> Self {
        Self {
            last: counters,
            tx_progress_at: now,
            rx_progress_at: now,
        }
    }

    /// Fold one poll. Returns the plane whose counter has been flat for the
    /// whole window, or `None` while both are advancing.
    pub fn observe(&mut self, now: Instant, counters: (u64, u64)) -> Option<ControlPlane> {
        if counters.0 > self.last.0 {
            self.tx_progress_at = now;
        }
        if counters.1 > self.last.1 {
            self.rx_progress_at = now;
        }
        self.last = counters;
        if now.saturating_duration_since(self.rx_progress_at) >= CONTROL_SILENCE_THRESHOLD {
            Some(ControlPlane::RxControl)
        } else if now.saturating_duration_since(self.tx_progress_at) >= CONTROL_SILENCE_THRESHOLD {
            Some(ControlPlane::TxControl)
        } else {
            None
        }
    }
}

/// Watch the two control planes' own stats counters and return
/// [`WatchdogFired::ControlStalled`] when either stays flat for the silence
/// window, so the caller respawns the group. Process liveness is never proof of
/// work: a `wfb_rx` blocked on a full pipe or a `wfb_tx` wedged in the driver is
/// still a live PID. Returns [`WatchdogFired::Cancelled`] on `cancel`.
pub async fn control_plane_watchdog(
    proc: Arc<Mutex<RadioProcesses>>,
    cancel: Shutdown,
) -> WatchdogFired {
    let control_counters = |(tx, rx): (crate::process::TxPlaneTotals, u64)| (tx.bytes_in, rx);
    let start = control_counters(proc.lock().await.control_progress());
    let mut tracker = ControlStallTracker::new(Instant::now(), start);
    loop {
        tokio::select! {
            _ = tokio::time::sleep(POLL_INTERVAL) => {}
            _ = cancel.wait() => return WatchdogFired::Cancelled,
        }
        let counters = control_counters(proc.lock().await.control_progress());
        if let Some(plane) = tracker.observe(Instant::now(), counters) {
            tracing::warn!(
                plane = plane.as_str(),
                window_s = CONTROL_SILENCE_THRESHOLD.as_secs(),
                "wfb_control_plane_stalled_respawning"
            );
            return WatchdogFired::ControlStalled;
        }
    }
}
