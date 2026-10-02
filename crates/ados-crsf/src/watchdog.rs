//! Transmit-path liveness watchdog for the RC lane.
//!
//! Process-liveness alone is never proof of work, and neither is a counter
//! that merely exists — the lane's health is proven by a DUAL check:
//!
//! 1. **TX-live**: the RC-frame counter (frames the serial driver actually
//!    accepted, bumped only after a successful write + flush) must advance at
//!    the transmit cadence WHILE A SOURCE IS LIVE. With no live source the
//!    transmitter deliberately sends nothing (so the receiver's failsafe
//!    runs), and a flat counter then is the correct idle lane, not a stall.
//!    A flat counter past the silence window with a source live means the
//!    module/driver has silently wedged — this watchdog fires and the run
//!    loop reinitialises the transport (reopen + fresh tasks) and re-verifies
//!    from scratch.
//! 2. **Received-side proof**: frames accepted by the driver only prove the
//!    host→module hop. Whether the RF energy reaches a receiver is decided by
//!    the decoded link-statistics telemetry and its uplink link quality — a
//!    TX that advances while the uplink LQ reads zero is `rf_unverified`, a
//!    state the lane surfaces on its sidecar and never reports flyable. That
//!    half of the check lives in the `link` state machine; this module owns
//!    the flat-TX half.
//!
//! Timing uses the async runtime's clock so the stall window is testable
//! under a paused clock.

use ados_protocol::shutdown::Shutdown;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use tokio::sync::Mutex;
use tokio::time::{Duration, Instant};

use crate::sources::SourceMerge;
use crate::transport::WireCounters;

/// How often the watchdog samples the TX frame counter.
pub const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// How long the TX counter may stay flat, with a source live the whole time,
/// before the transport is declared wedged. The transmitter runs at ≥1 Hz
/// while a source is live, so five silent seconds is a real stall.
pub const TX_SILENCE_THRESHOLD: Duration = Duration::from_secs(5);

/// Why the watchdog returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchdogFired {
    /// The TX frame counter was flat for the full silence window while the
    /// transmitter was supposed to be running: the caller must reinitialise
    /// the transport (reopen the port, respawn the tasks) and re-verify.
    TxStalled,
    /// The cancel notify fired.
    Cancelled,
}

/// Watch the TX frame counter for silent stalls. Returns [`WatchdogFired::TxStalled`]
/// when the counter has not advanced for [`TX_SILENCE_THRESHOLD`] while a
/// source was live at every sample, or [`WatchdogFired::Cancelled`] on the
/// cancel notify. A sample with no live source restarts the window: the lane
/// is idle by design then. The caller respawns the whole transport on a
/// stall — a fresh bring-up re-arms a fresh watchdog, so recovery is
/// re-verified rather than assumed.
pub async fn tx_liveness_watchdog(
    counters: Arc<WireCounters>,
    merge: Arc<Mutex<SourceMerge>>,
    cancel: Shutdown,
) -> WatchdogFired {
    let mut prev = counters.tx_frames.load(Ordering::Relaxed);
    let mut last_progress = Instant::now();
    loop {
        tokio::select! {
            biased;
            _ = cancel.wait() => return WatchdogFired::Cancelled,
            _ = tokio::time::sleep(POLL_INTERVAL) => {}
        }
        let current = counters.tx_frames.load(Ordering::Relaxed);
        let source_live = merge
            .lock()
            .await
            .current(std::time::Instant::now())
            .is_some();
        if current > prev || !source_live {
            last_progress = Instant::now();
        } else if last_progress.elapsed() >= TX_SILENCE_THRESHOLD {
            tracing::warn!(
                tx_frames = current,
                elapsed_s = last_progress.elapsed().as_secs(),
                "crsf_tx_stalled"
            );
            return WatchdogFired::TxStalled;
        }
        prev = current;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::{CHANNEL_COUNT, CHANNEL_MID};
    use crate::sources::{ChannelSourceMode, MAX_INJECT_TTL};

    fn counters() -> Arc<WireCounters> {
        Arc::new(WireCounters::default())
    }

    /// A merge with no source: the transmitter is idle by design.
    fn idle_merge() -> Arc<Mutex<SourceMerge>> {
        Arc::new(Mutex::new(SourceMerge::new(ChannelSourceMode::Inject)))
    }

    /// A merge with a live injection. The merge's deadline runs on the wall
    /// clock, which the paused runtime clock does not advance, so the
    /// injection stays live for the whole (virtual-time) test.
    fn live_merge() -> Arc<Mutex<SourceMerge>> {
        let mut merge = SourceMerge::new(ChannelSourceMode::Inject);
        merge
            .inject_all(
                [CHANNEL_MID; CHANNEL_COUNT],
                MAX_INJECT_TTL,
                std::time::Instant::now(),
                None,
            )
            .unwrap();
        Arc::new(Mutex::new(merge))
    }

    /// A flat TX counter with a live source fires the stall after the silence
    /// window (paused clock: sleeps auto-advance, so the window elapses
    /// deterministically).
    #[tokio::test(start_paused = true)]
    async fn flat_tx_with_a_live_source_fires_the_stall() {
        let c = counters();
        let cancel = Shutdown::new();
        let fired = tokio::time::timeout(
            TX_SILENCE_THRESHOLD + POLL_INTERVAL * 3,
            tx_liveness_watchdog(c, live_merge(), cancel),
        )
        .await
        .expect("fires within the window");
        assert_eq!(fired, WatchdogFired::TxStalled);
    }

    /// With no live source the transmitter sends nothing on purpose, so a
    /// flat counter is the idle lane and must never be reported as a stall
    /// (a respawn loop there would churn the port for nothing).
    #[tokio::test(start_paused = true)]
    async fn flat_tx_without_a_source_never_fires() {
        let c = counters();
        let cancel = Shutdown::new();
        let watchdog = tokio::spawn(tx_liveness_watchdog(c, idle_merge(), cancel.clone()));
        for _ in 0..20 {
            tokio::time::sleep(POLL_INTERVAL).await;
            assert!(!watchdog.is_finished(), "an idle lane is not a stall");
        }
        cancel.trigger();
        assert_eq!(watchdog.await.unwrap(), WatchdogFired::Cancelled);
    }

    /// An advancing counter keeps the watchdog silent well past the window;
    /// it returns only when cancelled.
    #[tokio::test(start_paused = true)]
    async fn advancing_tx_never_fires() {
        let c = counters();
        let cancel = Shutdown::new();
        let watchdog = tokio::spawn(tx_liveness_watchdog(
            c.clone(),
            live_merge(),
            cancel.clone(),
        ));
        // Advance the counter every poll for several full silence windows.
        for _ in 0..20 {
            c.tx_frames.fetch_add(1, Ordering::Relaxed);
            tokio::time::sleep(POLL_INTERVAL).await;
            assert!(!watchdog.is_finished(), "no fire while TX advances");
        }
        cancel.trigger();
        assert_eq!(watchdog.await.unwrap(), WatchdogFired::Cancelled);
    }

    /// The stall → reinit → recovery cycle: after a fire, a fresh watchdog
    /// over a transport whose counter advances again stays silent — recovery
    /// is re-verified by the new window, not assumed.
    #[tokio::test(start_paused = true)]
    async fn a_fresh_watchdog_after_a_stall_re_verifies() {
        let c = counters();
        let merge = live_merge();
        let cancel = Shutdown::new();
        // First transport wedges: flat counter → stall.
        let fired = tx_liveness_watchdog(c.clone(), merge.clone(), cancel.clone()).await;
        assert_eq!(fired, WatchdogFired::TxStalled);

        // The reinit brings a fresh transport (fresh counters) that flows.
        let fresh = counters();
        let watchdog = tokio::spawn(tx_liveness_watchdog(fresh.clone(), merge, cancel.clone()));
        for _ in 0..10 {
            fresh.tx_frames.fetch_add(1, Ordering::Relaxed);
            tokio::time::sleep(POLL_INTERVAL).await;
            assert!(!watchdog.is_finished(), "recovered transport is healthy");
        }
        cancel.trigger();
        assert_eq!(watchdog.await.unwrap(), WatchdogFired::Cancelled);
    }

    /// A stall mid-run (advancing then flat) fires once the window elapses
    /// after the LAST progress, not after start.
    #[tokio::test(start_paused = true)]
    async fn stall_window_counts_from_last_progress() {
        let c = counters();
        let cancel = Shutdown::new();
        let watchdog = tokio::spawn(tx_liveness_watchdog(
            c.clone(),
            live_merge(),
            cancel.clone(),
        ));
        // Healthy for three windows' worth of ticks.
        for _ in 0..15 {
            c.tx_frames.fetch_add(1, Ordering::Relaxed);
            tokio::time::sleep(POLL_INTERVAL).await;
        }
        assert!(!watchdog.is_finished());
        // Then the driver wedges: no more progress → the stall fires.
        let fired = tokio::time::timeout(TX_SILENCE_THRESHOLD + POLL_INTERVAL * 3, watchdog)
            .await
            .expect("fires within the window")
            .unwrap();
        assert_eq!(fired, WatchdogFired::TxStalled);
    }

    /// The cancel arm wins immediately, even with the clock paused.
    #[tokio::test(start_paused = true)]
    async fn cancel_wins_promptly() {
        let c = counters();
        let cancel = Shutdown::new();
        cancel.trigger();
        assert_eq!(
            tx_liveness_watchdog(c, live_merge(), cancel).await,
            WatchdogFired::Cancelled
        );
    }
}
