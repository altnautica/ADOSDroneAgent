//! systemd readiness + watchdog notifications. No-op off Linux (and a no-op
//! when not run under a `Type=notify` unit, i.e. `NOTIFY_SOCKET` unset).

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
