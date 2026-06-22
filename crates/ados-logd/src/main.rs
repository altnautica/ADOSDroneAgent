//! `ados-logd` daemon — the durable local logging and telemetry store.
//!
//! The runnable daemon. Opens the WAL-mode SQLite store (the sole read-write
//! handle), spawns the single-writer thread, binds the ingest socket, serves the
//! accept loop, and shuts down cleanly on `SIGTERM`/`SIGINT`, draining and
//! committing the final batch before exit. The synchronous SQLite work runs on a
//! dedicated OS thread; the async accept loop bridges to it over a bounded
//! channel.
//!
//! Modeled on the sibling daemons: journald logging on Linux with an fmt
//! fallback off Linux or outside a journald unit, and systemd readiness notify.
//! The binary is functional but ships dark — no systemd unit enables it yet, so
//! it has no effect at the install layer until that unit lands.

use anyhow::Result;

// Use mimalloc as the global allocator. The daemon is long-running and its
// workload — a constant stream of short-lived read-only SQLite connections
// served off the blocking pool plus the steady ingest and hardware-sample
// churn — fragments the system allocator, which grows per-thread heap arenas it
// then keeps resident. mimalloc bounds the fragmentation and returns freed pages
// to the OS, which holds the daemon's resident set down over a long uptime.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

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

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();
    tracing::info!(
        db = ados_logd::paths::DB_PATH,
        ingest = ados_logd::paths::INGEST_SOCKET,
        "logging store starting"
    );
    match ados_logd::daemon::run_daemon().await {
        Ok(()) => {
            tracing::info!("logging store exited cleanly");
            Ok(())
        }
        Err(e) => {
            tracing::error!(error = %e, "logging store fatal error");
            Err(e)
        }
    }
}
