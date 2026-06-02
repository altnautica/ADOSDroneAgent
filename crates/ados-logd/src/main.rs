//! `ados-logd` daemon — the durable local logging and telemetry store.
//!
//! This binary is a skeleton: it initializes logging the same way the sibling
//! daemons do and exits. It binds no socket, opens no store, and installs no
//! systemd unit yet, so it ships dark and has no runtime effect until the
//! ingestion, hardware-collector, and query-API chunks land.
//!
//! Modeled on the `ados-net` binary shape: journald logging on Linux with an
//! fmt fallback off Linux or outside a journald unit.

use anyhow::Result;

fn init_logging() {
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

fn main() -> Result<()> {
    init_logging();
    tracing::info!(
        db = ados_logd::paths::DB_PATH,
        "logging store skeleton: no socket bound, no behavior yet"
    );
    Ok(())
}
