//! systemd notify pings and the process stop trigger.

/// systemd readiness ping. No-op off Linux and when not run under a
/// `Type=notify` unit (`NOTIFY_SOCKET` unset).
#[cfg(target_os = "linux")]
pub(super) fn sd_ready() {
    if let Err(e) = sd_notify::notify(&[sd_notify::NotifyState::Ready]) {
        tracing::debug!(error = %e, "sd_notify READY failed");
    }
}

#[cfg(not(target_os = "linux"))]
pub(super) fn sd_ready() {}

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
