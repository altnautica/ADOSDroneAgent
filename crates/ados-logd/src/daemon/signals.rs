//! systemd notify and watchdog pings, and the process stop signal.

use super::*;

/// systemd readiness ping. No-op off Linux and when not run under a
/// `Type=notify` unit (`NOTIFY_SOCKET` unset).
///
/// Public because the disabled-store path in `main` must send it too: under
/// `Type=notify`, a process that exits without ever announcing readiness is
/// recorded as `result 'protocol'` — a failure — however clean its exit code.
#[cfg(target_os = "linux")]
pub fn sd_ready() {
    if let Err(e) = sd_notify::notify(&[sd_notify::NotifyState::Ready]) {
        tracing::debug!(error = %e, "sd_notify READY failed");
    }
}

#[cfg(not(target_os = "linux"))]
pub fn sd_ready() {}

/// systemd stopping ping. No-op off Linux / outside a notify unit.
#[cfg(target_os = "linux")]
pub(super) fn sd_stopping() {
    if let Err(e) = sd_notify::notify(&[sd_notify::NotifyState::Stopping]) {
        tracing::debug!(error = %e, "sd_notify STOPPING failed");
    }
}

#[cfg(not(target_os = "linux"))]
pub(super) fn sd_stopping() {}

/// systemd watchdog keep-alive ping. No-op off Linux and when not run under a
/// `WatchdogSec`-armed `Type=notify` unit (`WATCHDOG_USEC` unset).
#[cfg(target_os = "linux")]
pub(super) fn sd_watchdog() {
    let _ = sd_notify::notify(&[sd_notify::NotifyState::Watchdog]);
}

#[cfg(not(target_os = "linux"))]
pub(super) fn sd_watchdog() {}

/// The watchdog keep-alive loop, coupled to the writer's progress stamp.
///
/// Runs until aborted: on each tick it feeds the watchdog ONLY while the
/// writer is still stamping. A writer that wedged inside a commit, or one
/// whose loop returned, stops the pings, so `WatchdogSec` expires and systemd
/// restarts the unit instead of leaving an `active (running)` daemon in front
/// of a store that is recording nothing.
///
/// Generic over the ping so the coupling is testable without systemd; the
/// supervisor's watchdog is built the same way for the same reason.
pub(crate) async fn watchdog_loop<F>(
    interval: Duration,
    budget: Duration,
    health: WriterHealth,
    mut ping: F,
) where
    F: FnMut(),
{
    let mut tick = tokio::time::interval(interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // The first tick fires immediately; skip pinging on it so the cadence
    // stays a steady `interval`.
    tick.tick().await;
    let mut withheld = false;
    loop {
        tick.tick().await;
        if health.is_alive(budget) {
            if withheld {
                withheld = false;
                tracing::warn!("writer resumed; feeding the watchdog again");
            }
            ping();
        } else if !withheld {
            withheld = true;
            // One line per stall entry, not one per tick: systemd is about to
            // restart the unit and this is the only in-process record of why.
            tracing::error!(
                since_mark_s = health.since_mark().as_secs(),
                budget_s = budget.as_secs(),
                "writer stopped making progress; withholding the systemd watchdog"
            );
        }
    }
}

/// Resolve when the process receives `SIGTERM` or `SIGINT`. The production stop
/// trigger.
pub(super) async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "failed to install SIGTERM handler");
                return;
            }
        };
        let mut sigint = match signal(SignalKind::interrupt()) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "failed to install SIGINT handler");
                return;
            }
        };
        tokio::select! {
            _ = sigterm.recv() => tracing::info!("received SIGTERM"),
            _ = sigint.recv() => tracing::info!("received SIGINT"),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("received interrupt");
    }
}
