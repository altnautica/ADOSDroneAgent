//! Logging init: journald on Linux, fmt fallback elsewhere, `RUST_LOG`-driven.
//!
//! Mirrors the supervisor's `init_logging`: when running under systemd on
//! Linux the logs go to the journal; on a dev host (or when journald is
//! unavailable) they fall back to a formatted stderr layer. `RUST_LOG` selects
//! the filter (default `info`).

/// Initialize the global tracing subscriber. Idempotent via `try_init`.
pub fn init_logging() {
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string());

    #[cfg(target_os = "linux")]
    {
        use tracing_subscriber::prelude::*;
        use tracing_subscriber::EnvFilter;
        if let Ok(journald) = tracing_journald::layer() {
            let _ = tracing_subscriber::registry()
                .with(EnvFilter::new(&filter))
                .with(journald)
                .try_init();
            return;
        }
    }

    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(&filter))
        .try_init();
}
