//! systemd readiness + watchdog notifications. No-op off Linux (and a no-op
//! when not run under a `Type=notify` unit, i.e. `NOTIFY_SOCKET` unset).
//!
//! The watchdog ping is COUPLED to monitor-pass progress. Liveness for this
//! unit is not "the process exists" and not "the tokio runtime still schedules
//! tasks" — a wedged monitor pass satisfies both while service death-detection,
//! auto-restart and hot-plug handling have all stopped, which is the
//! operator-undiagnosable failure the supervisor exists to prevent one level
//! down. So the ticker pings only while [`MonitorProgress`] keeps advancing;
//! when it stalls past the budget the pings stop, `WatchdogSec` expires and
//! systemd restarts the unit.
//!
//! The pass stamps progress at every stage boundary, not just at the end, so a
//! slow-but-advancing recovery pass (a stop ceiling, a usb-rehome
//! stop+wait+rebind+start, a repair ladder rung) keeps the watchdog fed while a
//! pass stuck inside one stage does not. Each individual subprocess is
//! separately bounded by [`crate::oscmd`], so a stall is a real wedge (a
//! deadlock, a blocked-forever syscall) rather than a slow external command.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;

/// `WatchdogSec=` on the `ados-supervisor` Type=notify unit. The keep-alive
/// ticker pings at a fraction of this so a single ping miss still leaves margin
/// before systemd's SIGKILL deadline. Kept in lock-step with
/// `data/systemd/ados-supervisor.service`.
pub const WATCHDOG_SEC: u64 = 30;

/// How far behind the monitor pass may fall before the watchdog stops being
/// fed, expressed as a multiple of the monitor interval plus a fixed grace.
/// Three intervals tolerates two skipped ticks; the 60 s grace covers the
/// longest legitimate single stage (a service stop ceiling plus a USB rebind),
/// each of whose subprocesses is itself bounded.
const STALL_INTERVAL_MULTIPLE: u32 = 3;
const STALL_GRACE: Duration = Duration::from_secs(60);

/// The cadence the keep-alive ticker pings at: `WatchdogSec / 3`, floored at
/// 1 s. A third of the deadline tolerates two consecutive missed pings before
/// systemd would act, which is the conventional safe margin. Pulled out as a
/// pure function so the cadence is unit-testable without the runtime.
pub fn watchdog_interval(watchdog_sec: u64) -> Duration {
    Duration::from_secs((watchdog_sec / 3).max(1))
}

/// The stall budget for a monitor interval: how long the pass may go without
/// stamping progress before the watchdog stops being fed. Pure so the budget is
/// testable and so the one arithmetic lives in one place.
pub fn stall_budget(monitor_interval: Duration) -> Duration {
    monitor_interval * STALL_INTERVAL_MULTIPLE + STALL_GRACE
}

/// Shared monitor-pass progress marker.
///
/// [`Supervisor::monitor_pass`](crate::lifecycle::Supervisor::monitor_pass)
/// stamps this as it advances through the pass; the watchdog ticker reads it
/// and refuses to ping when it stops moving. Cheap enough to stamp at every
/// stage boundary (one relaxed atomic store) and lock-free, so it can never
/// itself be the thing that blocks the pass.
///
/// Uses `tokio::time::Instant` so a paused-clock test drives it deterministically.
#[derive(Clone, Debug)]
pub struct MonitorProgress {
    epoch: Instant,
    /// Milliseconds since `epoch` at the last stamp.
    last_ms: Arc<AtomicU64>,
}

impl MonitorProgress {
    /// Start marked as fresh, so the ticker feeds the watchdog from boot
    /// through the first pass.
    pub fn new() -> Self {
        MonitorProgress {
            epoch: Instant::now(),
            last_ms: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Stamp progress. Called at every monitor-pass stage boundary.
    pub fn mark(&self) {
        let ms = Instant::now().duration_since(self.epoch).as_millis() as u64;
        self.last_ms.store(ms, Ordering::Relaxed);
    }

    /// How long since the last stamp.
    pub fn since_mark(&self) -> Duration {
        let now_ms = Instant::now().duration_since(self.epoch).as_millis() as u64;
        Duration::from_millis(now_ms.saturating_sub(self.last_ms.load(Ordering::Relaxed)))
    }
}

impl Default for MonitorProgress {
    fn default() -> Self {
        Self::new()
    }
}

/// The watchdog keep-alive loop, with the ping action injected.
///
/// Runs forever: on each tick it feeds the watchdog only while the monitor pass
/// is still advancing. Compiled on every host and generic over the ping so the
/// coupling is testable without systemd.
pub async fn watchdog_loop<F>(
    interval: Duration,
    budget: Duration,
    progress: MonitorProgress,
    mut ping: F,
) where
    F: FnMut(),
{
    let mut tick = tokio::time::interval(interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut stalled = false;
    loop {
        tick.tick().await;
        let since = progress.since_mark();
        if since > budget {
            if !stalled {
                stalled = true;
                // One line per stall entry, not one per tick: the unit is about
                // to be restarted by systemd and this is the only in-process
                // record of why.
                tracing::error!(
                    stalled_for_s = since.as_secs(),
                    budget_s = budget.as_secs(),
                    "monitor_pass_stalled_withholding_watchdog"
                );
            }
            continue;
        }
        if stalled {
            stalled = false;
            tracing::warn!(
                since_mark_s = since.as_secs(),
                "monitor_pass_resumed_feeding_watchdog"
            );
        }
        ping();
    }
}

/// Spawn the watchdog keep-alive ticker, coupled to `progress`.
#[cfg(target_os = "linux")]
pub fn spawn_watchdog_pinger(progress: MonitorProgress, monitor_interval: Duration) {
    let interval = watchdog_interval(WATCHDOG_SEC);
    let budget = stall_budget(monitor_interval);
    tokio::spawn(async move { watchdog_loop(interval, budget, progress, watchdog).await });
}

/// Off Linux there is no systemd watchdog to feed; the ticker is a no-op so the
/// startup path stays identical.
#[cfg(not(target_os = "linux"))]
pub fn spawn_watchdog_pinger(_progress: MonitorProgress, _monitor_interval: Duration) {}

#[cfg(target_os = "linux")]
pub fn ready() {
    if let Err(e) = sd_notify::notify(false, &[sd_notify::NotifyState::Ready]) {
        tracing::debug!(error = %e, "sd_notify READY failed");
    }
}

#[cfg(target_os = "linux")]
pub fn watchdog() {
    let _ = sd_notify::notify(false, &[sd_notify::NotifyState::Watchdog]);
}

#[cfg(not(target_os = "linux"))]
pub fn ready() {}

#[cfg(not(target_os = "linux"))]
pub fn watchdog() {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn watchdog_interval_is_a_third_of_the_deadline() {
        // The shipped unit deadline → a 10 s keep-alive cadence (a third of 30 s),
        // which tolerates two missed pings before systemd's SIGKILL deadline.
        assert_eq!(watchdog_interval(30), Duration::from_secs(10));
        assert_eq!(watchdog_interval(WATCHDOG_SEC), Duration::from_secs(10));
        assert_eq!(watchdog_interval(90), Duration::from_secs(30));
    }

    #[test]
    fn watchdog_interval_never_collapses_to_zero() {
        // A tiny or zero deadline must still yield a positive, schedulable cadence
        // so `tokio::time::interval` never panics on a zero period.
        assert_eq!(watchdog_interval(2), Duration::from_secs(1));
        assert_eq!(watchdog_interval(1), Duration::from_secs(1));
        assert_eq!(watchdog_interval(0), Duration::from_secs(1));
        assert!(watchdog_interval(0) > Duration::ZERO);
    }

    #[test]
    fn the_stall_budget_leaves_room_for_a_slow_but_advancing_pass() {
        // Three 5 s intervals + the 60 s grace: a pass that stamps progress at
        // every stage boundary can spend over a minute in one stage without the
        // watchdog being withheld, while a pass that stops stamping trips it.
        assert_eq!(
            stall_budget(Duration::from_secs(5)),
            Duration::from_secs(75)
        );
        assert!(stall_budget(Duration::from_secs(5)) > Duration::from_secs(WATCHDOG_SEC));
    }

    #[tokio::test(start_paused = true)]
    async fn the_watchdog_is_withheld_while_the_monitor_pass_is_stalled() {
        // The SUP-WATCHDOG-DECOUPLED regression. A pass that stops making
        // progress must stop feeding the watchdog so systemd restarts the unit,
        // instead of the unit reporting healthy forever with service
        // reconciliation dead.
        let pings = Arc::new(AtomicUsize::new(0));
        let progress = MonitorProgress::new();
        let interval = Duration::from_secs(10);
        let budget = stall_budget(Duration::from_secs(5)); // 75 s

        let counter = pings.clone();
        let loop_progress = progress.clone();
        tokio::spawn(async move {
            watchdog_loop(interval, budget, loop_progress, move || {
                counter.fetch_add(1, Ordering::Relaxed);
            })
            .await
        });

        // A healthy pass stamping at every tick keeps the watchdog fed. Each
        // step yields so the ticker task actually gets polled for the tick the
        // advance made ready.
        for _ in 0..6 {
            progress.mark();
            tokio::time::advance(interval).await;
            tokio::task::yield_now().await;
        }
        let fed_while_healthy = pings.load(Ordering::Relaxed);
        assert!(
            fed_while_healthy >= 3,
            "a progressing pass must feed the watchdog, got {fed_while_healthy}"
        );

        // Now the pass wedges: no more stamps. Inside the budget the ticker
        // still feeds (a slow stage is not a wedge).
        for _ in 0..5 {
            tokio::time::advance(interval).await;
            tokio::task::yield_now().await;
        }
        let fed_inside_budget = pings.load(Ordering::Relaxed);
        assert!(
            fed_inside_budget > fed_while_healthy,
            "a pass inside the stall budget must still be fed"
        );

        // Past the budget the pings must stop dead.
        for _ in 0..4 {
            tokio::time::advance(interval).await;
            tokio::task::yield_now().await;
        }
        let at_stall = pings.load(Ordering::Relaxed);
        for _ in 0..30 {
            tokio::time::advance(interval).await;
            tokio::task::yield_now().await;
        }
        assert_eq!(
            pings.load(Ordering::Relaxed),
            at_stall,
            "a stalled monitor pass must NOT be reported to systemd as healthy"
        );

        // And a pass that recovers resumes feeding, so a transient wedge that
        // clears inside WatchdogSec does not need an operator.
        progress.mark();
        for _ in 0..2 {
            tokio::time::advance(interval).await;
            tokio::task::yield_now().await;
        }
        assert!(
            pings.load(Ordering::Relaxed) > at_stall,
            "a recovered pass must resume feeding the watchdog"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn progress_since_mark_tracks_the_last_stamp() {
        let progress = MonitorProgress::new();
        tokio::time::advance(Duration::from_secs(7)).await;
        assert!(progress.since_mark() >= Duration::from_secs(7));
        progress.mark();
        assert!(progress.since_mark() < Duration::from_secs(1));
    }
}
