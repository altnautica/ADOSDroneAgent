//! systemd readiness + watchdog notifications. No-op off Linux (and a no-op
//! when not run under a `Type=notify` unit, i.e. `NOTIFY_SOCKET` unset).

use std::time::Duration;

/// `WatchdogSec=` on the `ados-supervisor` Type=notify unit. The independent
/// keep-alive ticker pings at a fraction of this so a single ping miss still
/// leaves margin before systemd's SIGKILL deadline. Kept in lock-step with
/// `data/systemd/ados-supervisor.service`.
pub const WATCHDOG_SEC: u64 = 30;

/// The cadence the independent keep-alive ticker pings at: `WatchdogSec / 3`,
/// floored at 1 s. A third of the deadline tolerates two consecutive missed
/// pings before systemd would act, which is the conventional safe margin.
/// Pulled out as a pure function so the cadence is unit-testable without the
/// runtime.
pub fn watchdog_interval(watchdog_sec: u64) -> Duration {
    Duration::from_secs((watchdog_sec / 3).max(1))
}

/// Spawn an always-on watchdog keep-alive ticker, independent of the monitor
/// pass. Supervisor liveness is "the process is alive and the loop is
/// scheduling tasks", NOT "a monitor pass completed" — `monitor_pass` chains
/// `systemctl` calls (a stop ceiling, a usb-rehome stop+wait+rebind+start, and
/// timeout-less nmcli link repairs) that can legitimately exceed `WatchdogSec`
/// in a single recovery pass. Pinging the watchdog only after the pass would
/// let one slow-but-healthy pass starve the watchdog and trigger a SIGKILL
/// mid-recovery. This task pings on its own timer so that can never happen; it
/// keeps running until the process exits.
#[cfg(target_os = "linux")]
pub fn spawn_watchdog_pinger() {
    let interval = watchdog_interval(WATCHDOG_SEC);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            watchdog();
        }
    });
}

/// Off Linux there is no systemd watchdog to feed; the ticker is a no-op so the
/// startup path stays identical.
#[cfg(not(target_os = "linux"))]
pub fn spawn_watchdog_pinger() {}

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
}
