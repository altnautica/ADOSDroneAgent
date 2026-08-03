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

    // The store is opt-in. It is ~96% of everything this box writes to its card
    // and the largest single lump of space it occupies, so it does not run
    // unless somebody asked for it. Exiting BEFORE `run_daemon` is what makes
    // "off" mean no store file is created, rather than an empty one that grows
    // the moment anything connects.
    //
    // The installer also declines to enable the unit when the key is off; this
    // second read covers the unit being started by hand or left enabled by an
    // older install. Exit 0, because declining to run is not a failure and
    // `Restart=on-failure` must not turn it into a loop.
    if !ados_logd::gate::store_enabled() {
        tracing::info!(
            key = "logging.store.enabled",
            "logging store is disabled; not starting (journalctl is the log of record)"
        );
        // Announce readiness before exiting. The unit is `Type=notify`, so
        // systemd waits for this and treats a process that exits without ever
        // sending it as `result 'protocol'` — a FAILURE — no matter that the
        // exit code is 0. Both rigs therefore carried a permanently failed
        // ados-logd after the store was switched off by default, which is worse
        // than useless: a node that always shows a failed unit teaches its
        // operator to stop reading failed units, and the next one that matters
        // is read the same way.
        //
        // READY then exit is the correct handshake for "started successfully,
        // and there is nothing to do" — systemd records a clean start and a
        // clean stop, and `Restart=on-failure` has no failure to act on.
        ados_logd::daemon::sd_ready();
        return Ok(());
    }

    tracing::info!(
        db = %ados_logd::paths::db_path(),
        ingest = %ados_logd::paths::ingest_socket(),
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
