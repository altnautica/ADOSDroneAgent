//! UDP receive-queue backlog watchdog.

use super::*;

/// Everything the receive-queue watchdog observes about the world outside its
/// own state machine: the kernel queue depth and the data plane's ingress total
/// it cross-checks, the monotonic clock its sustained-window arithmetic runs on,
/// and the wait between polls. Behind this seam a scenario is a list of values;
/// [`ProcSignals`] is the production implementation.
pub trait RecvqSignals: Send + Sync {
    /// Cumulative bytes the data plane has read off its UDP ingress — the
    /// evidence that it is actually emptying the socket rather than merely
    /// still being alive.
    fn ingress_bytes(&self) -> u64;

    /// Kernel receive-queue depth in bytes for `port`.
    fn udp_recvq(&self, port: u16) -> impl Future<Output = Option<u64>> + Send;

    /// Wait one poll interval. Raced against the cancel notification by the
    /// caller, so an implementation that never completes simply parks the
    /// watchdog until it is cancelled.
    fn wait(&self, interval: Duration) -> impl Future<Output = ()> + Send;

    /// Read the monotonic clock. Called once per poll; every window in the loop
    /// is measured against that single reading.
    fn now(&self) -> Instant;
}

/// The production [`RecvqSignals`]: the data plane's own stats totals, the real
/// `/proc/net/udp` depth, the real tokio timer, the real monotonic clock.
#[derive(Debug, Clone)]
pub struct ProcSignals {
    pub(super) stats: Arc<TxPlaneCounters>,
}

impl RecvqSignals for ProcSignals {
    fn ingress_bytes(&self) -> u64 {
        self.stats.totals().bytes_in
    }

    async fn udp_recvq(&self, port: u16) -> Option<u64> {
        read_udp_recvq(port).await
    }

    async fn wait(&self, interval: Duration) {
        tokio::time::sleep(interval).await;
    }

    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// Watch the UDP 5600 kernel receive queue. Returns when the queue has been
/// sustained over 256 KiB for 15s **while `wfb_tx` is not draining it**.
/// Updates the shared counters with the live `tx_video_stalled` flag, the last
/// observed queue depth, and the stall-kill count on fire.
///
/// A deep queue on its own is not evidence of a wedge. Two very different
/// conditions produce the same queue depth:
///
/// - **Wedged**: `wfb_tx` has stopped reading the socket; its ingress total is
///   flat. Killing it is the correct and only recovery.
/// - **Backpressured**: `wfb_tx` is reading as fast as the air allows, but the
///   encoder is offering more than the current MCS and FEC can carry; its
///   ingress total advances. Killing it drops the link for a full radio-group
///   respawn, resets the adaptive bitrate controller, and leaves the encoder
///   still over-feeding, so the queue refills and the kill repeats on a fixed
///   period (observed on a bench rig as 26 consecutive kills at 23 s
///   intervals). Backpressure is logged periodically instead.
pub async fn video_recvq_watchdog(
    stats: Arc<TxPlaneCounters>,
    counters: CounterHandle,
    cancel: Shutdown,
) -> WatchdogFired {
    video_recvq_watchdog_with(ProcSignals { stats }, counters, cancel).await
}

/// [`video_recvq_watchdog`] with its view of the outside world supplied. The
/// public entry point above is this function with [`ProcSignals`]; tests drive
/// it with a scripted one.
pub async fn video_recvq_watchdog_with<S: RecvqSignals>(
    signals: S,
    counters: CounterHandle,
    cancel: Shutdown,
) -> WatchdogFired {
    let mut high_since: Option<Instant> = None;
    let mut prev_ingress: u64 = 0;
    let mut last_backpressure_log = signals.now() - BACKPRESSURE_LOG_INTERVAL;

    loop {
        tokio::select! {
            _ = signals.wait(POLL_INTERVAL) => {}
            _ = cancel.wait() => return WatchdogFired::Cancelled,
        }
        // One clock reading per poll, so every window below is measured against
        // the same instant and a slow tick cannot make two of them disagree.
        let now = signals.now();
        let q = signals.udp_recvq(5600).await.unwrap_or(0);
        let ingress = signals.ingress_bytes();
        let draining = ingress > prev_ingress;
        prev_ingress = ingress;

        let tick = recvq_tick_decision(q, draining);
        {
            let mut c = counters.lock().await;
            c.tx_video_recvq_bytes = q;
            // Report a stall only for a genuine wedge. A backpressured link is
            // busy, not stalled, and a surface that calls it stalled trains the
            // operator to ignore the flag that also means a real wedge.
            c.tx_video_stalled = tick == RecvqTick::Wedged;
            c.tx_video_backpressured = tick == RecvqTick::Backpressured;
        }

        match tick {
            RecvqTick::Wedged => {
                let since = *high_since.get_or_insert(now);
                if now.saturating_duration_since(since) >= RECVQ_SUSTAINED_THRESHOLD {
                    tracing::warn!(
                        queue_bytes = q,
                        "wfb_tx_video_recvq_kill: queue sustained with no drain progress"
                    );
                    counters.lock().await.tx_video_stall_kills += 1;
                    return WatchdogFired::RecvqBacklog;
                }
            }
            RecvqTick::Backpressured => {
                // The process is working through the backlog, so a kill would
                // interrupt real progress. Reset the wedge timer: a kill must
                // require a fresh uninterrupted window of genuinely-stuck ticks.
                high_since = None;
                if now.saturating_duration_since(last_backpressure_log) >= BACKPRESSURE_LOG_INTERVAL
                {
                    tracing::warn!(
                        queue_bytes = q,
                        "wfb_tx_video_backpressured: draining, offered rate exceeds link capacity"
                    );
                    last_backpressure_log = now;
                }
            }
            RecvqTick::Clear => high_since = None,
        }
    }
}

/// What one receive-queue poll concludes. Split out as a pure decision so the
/// wedge-versus-backpressure rule is testable without a live `/proc`, matching
/// [`aux_tick_decision`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RecvqTick {
    /// Below the backlog threshold. Nothing to do.
    Clear,
    /// Deep, but `wfb_tx` is reading it. Healthy and saturated: report, never
    /// restart.
    Backpressured,
    /// Deep with no read progress. Sustained, this is a wedge.
    Wedged,
}

/// Classify one poll. `draining` is whether the data plane's ingress total advanced
/// since the previous poll, i.e. whether it read anything at all.
pub(super) fn recvq_tick_decision(queue_bytes: u64, draining: bool) -> RecvqTick {
    if queue_bytes <= RECVQ_BACKLOG_THRESHOLD_BYTES {
        RecvqTick::Clear
    } else if draining {
        RecvqTick::Backpressured
    } else {
        RecvqTick::Wedged
    }
}

/// Read the UDP receive-queue depth for a given port from `/proc/net/udp`.
/// Returns the queue depth in bytes (hex `rx_queue` field from the kernel).
pub(super) async fn read_udp_recvq(port: u16) -> Option<u64> {
    // The port is in hex in /proc/net/udp, big-endian.
    let port_hex = format!("{:04X}", port);
    let raw = tokio::fs::read_to_string("/proc/net/udp").await.ok()?;
    for line in raw.lines().skip(1) {
        // Format: sl  local_address rem_address   st tx_queue:rx_queue ...
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 5 {
            continue;
        }
        // local_address is "addr:port" in hex; we match the port suffix.
        if cols[1].ends_with(&format!(":{}", port_hex)) {
            // tx_queue:rx_queue — we want rx_queue (right side of colon).
            if let Some(q) = cols[4].split(':').nth(1) {
                return u64::from_str_radix(q, 16).ok();
            }
        }
    }
    Some(0)
}
