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

/// The reopen pace of a frame source: attempts start at most once per
/// `SOURCE_RETRY`, forever, never given up on.
pub const SOURCE_RETRY: Duration = Duration::from_secs(2);

/// The pause before reopening a source whose last attempt ran for `ran`.
///
/// Attempts are paced start to start, so an attempt that fails at once waits
/// out the rest of [`SOURCE_RETRY`] (a source that keeps failing is redialled
/// every 2 s, never faster), while one that ran longer than that (it streamed,
/// then dropped or went silent) is reopened at once: the first reopen after an
/// outage costs nothing, and only a source failing again and again is paced.
pub fn reopen_pause(ran: Duration) -> Duration {
    SOURCE_RETRY.saturating_sub(ran)
}

/// Wait out `pause` before a reopen, waking early on shutdown. Returns `true`
/// when shutdown fired during the wait (the caller should stop), `false` when
/// the pause elapsed (the caller should reopen its source).
pub async fn backoff(cancel: &ados_protocol::shutdown::Shutdown, pause: Duration) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(pause) => false,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_source_that_fails_at_once_is_reopened_after_two_seconds() {
        let cancel = ados_protocol::shutdown::Shutdown::new();
        let start = tokio::time::Instant::now();
        assert!(!backoff(&cancel, reopen_pause(Duration::ZERO)).await);
        assert_eq!(start.elapsed(), SOURCE_RETRY);
    }

    /// The schedule: a first reopen after a source that streamed is immediate,
    /// and attempts that keep failing start one `SOURCE_RETRY` apart, never
    /// faster, however many there are.
    #[tokio::test(start_paused = true)]
    async fn reopens_are_immediate_after_an_outage_and_paced_while_failing() {
        assert_eq!(reopen_pause(Duration::from_secs(10)), Duration::ZERO);
        assert_eq!(reopen_pause(SOURCE_RETRY), Duration::ZERO);
        assert_eq!(
            reopen_pause(Duration::from_millis(500)),
            Duration::from_millis(1500)
        );

        let cancel = ados_protocol::shutdown::Shutdown::new();
        // One attempt that streamed for 10 s and went silent, then fifty that
        // each fail 100 ms in.
        let mut ran = Duration::from_secs(10);
        let silent = tokio::time::Instant::now();
        let mut starts = Vec::new();
        for _ in 0..51 {
            assert!(!backoff(&cancel, reopen_pause(ran)).await);
            starts.push(tokio::time::Instant::now());
            tokio::time::advance(Duration::from_millis(100)).await;
            ran = Duration::from_millis(100);
        }
        assert_eq!(starts[0], silent, "the first reopen is at once");
        for pair in starts.windows(2) {
            assert_eq!(pair[1] - pair[0], SOURCE_RETRY, "a failing source is paced");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_ends_the_pause() {
        let cancel = ados_protocol::shutdown::Shutdown::new();
        cancel.trigger();
        assert!(backoff(&cancel, SOURCE_RETRY).await);
    }
}
