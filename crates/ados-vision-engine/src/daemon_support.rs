//! Process plumbing for the `ados-vision` daemon binary: logging setup, the
//! systemd readiness ping, board SoC resolution and the shutdown waits.

use std::time::Duration;

/// Board override file the HAL writes the resolved SoC into. Best-effort read
/// to pick the backend; absence falls back to the env / a generic SoC.
const BOARD_OVERRIDE: &str = "/etc/ados/board_override";

/// Install the tracing subscriber: journald on Linux (fmt elsewhere) plus the
/// logd layer.
pub fn init_tracing() {
    use ados_protocol::logd::layer::LogdLayer;
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::EnvFilter;

    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string());

    // The logd layer ships records to the logging daemon's ingest socket
    // alongside the primary sink; it is best-effort and never blocks the service.
    #[cfg(target_os = "linux")]
    {
        if let Ok(journald) = tracing_journald::layer() {
            let _ = tracing_subscriber::registry()
                .with(EnvFilter::new(&filter))
                .with(journald)
                .with(LogdLayer::new("ados-vision"))
                .try_init();
            return;
        }
    }

    let _ = tracing_subscriber::registry()
        .with(EnvFilter::new(&filter))
        .with(tracing_subscriber::fmt::layer())
        .with(LogdLayer::new("ados-vision"))
        .try_init();
}

/// systemd readiness ping. No-op off Linux and when not run under a
/// `Type=notify` unit (`NOTIFY_SOCKET` unset).
#[cfg(target_os = "linux")]
pub fn sd_ready() {
    if let Err(e) = sd_notify::notify(&[sd_notify::NotifyState::Ready]) {
        tracing::debug!(error = %e, "sd_notify READY failed");
    }
}

#[cfg(not(target_os = "linux"))]
pub fn sd_ready() {}

/// Read the resolved SoC for backend selection. Prefers `ADOS_BOARD_SOC`, then
/// the board-override file's first token, else a generic placeholder.
pub fn board_soc() -> String {
    if let Ok(soc) = std::env::var("ADOS_BOARD_SOC") {
        if !soc.is_empty() {
            return soc;
        }
    }
    if let Ok(text) = std::fs::read_to_string(BOARD_OVERRIDE) {
        if let Some(first) = text.split_whitespace().next() {
            return first.to_string();
        }
    }
    "generic".to_string()
}

/// A short backoff that also wakes on shutdown. Returns `true` when shutdown
/// fired during the wait (the caller should stop), `false` when the full
/// backoff elapsed (the caller should reopen its source).
pub async fn backoff(cancel: &ados_protocol::shutdown::Shutdown) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(Duration::from_millis(500)) => false,
        _ = cancel.wait() => true,
    }
}

/// Resolve when the service receives SIGTERM or SIGINT.
pub async fn wait_for_shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        let mut int = signal(SignalKind::interrupt()).expect("install SIGINT handler");
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
