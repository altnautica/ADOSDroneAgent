//! The `ados-compute` daemon: the compute node's service. It opens the job
//! store, builds the node engine, runs a worker loop that drains the queue and
//! periodically reclaims terminal jobs, and serves the REST job API on a single
//! TCP listener. The supervisor starts it for the `compute` profile.
//!
//! Local-first reach (Rule 39) is the goal, but the job API has no
//! authentication yet, so the daemon binds **loopback by default** and refuses
//! to bind a non-loopback address until the pairing-auth layer ships. mDNS
//! discovery and that auth wrap this surface later; today it is the lean local
//! job API on `127.0.0.1`.
//!
//! Worker note: the worker runs one job per tick while holding the engine lock.
//! That is fine for the instant mock backends; before a real backend (which
//! runs for minutes) lands, the worker must claim-run-finalize so the long
//! backend run does not hold the lock and block the API (tracked for M15).
//!
//! Configuration is read from the environment so the install layer can set it
//! without a config-file dependency:
//! - `ADOS_COMPUTE_DB`        job store path (default `/var/ados/compute/jobs.db`)
//! - `ADOS_COMPUTE_BIND`      bind address (default `127.0.0.1:8092`, loopback)
//! - `ADOS_COMPUTE_NODE_ID`   this node's id (default `compute-node`)
//! - `ADOS_COMPUTE_WORKERS`   worker slots (default `1`)
//! - `ADOS_COMPUTE_RETENTION_S` terminal-job retention seconds (default `86400`)

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ados_compute::{
    build_router, Cluster, Engine, JobStore, MockDetector, MockReconstructor, Scheduler,
};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

fn init_logging() {
    use ados_protocol::logd::layer::LogdLayer;
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::EnvFilter;

    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string());

    // The logd layer ships records to the logging daemon alongside the primary
    // sink; it is best-effort and never blocks the service (Rule 41).
    #[cfg(target_os = "linux")]
    {
        if let Ok(journald) = tracing_journald::layer() {
            let _ = tracing_subscriber::registry()
                .with(EnvFilter::new(&filter))
                .with(journald)
                .with(LogdLayer::new("ados-compute"))
                .try_init();
            return;
        }
    }

    let _ = tracing_subscriber::registry()
        .with(EnvFilter::new(&filter))
        .with(tracing_subscriber::fmt::layer())
        .with(LogdLayer::new("ados-compute"))
        .try_init();
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_logging();

    let db = env_or("ADOS_COMPUTE_DB", "/var/ados/compute/jobs.db");
    let bind = env_or("ADOS_COMPUTE_BIND", "127.0.0.1:8092");
    let node_id = env_or("ADOS_COMPUTE_NODE_ID", "compute-node");
    let workers: u32 = env_or("ADOS_COMPUTE_WORKERS", "1").parse().unwrap_or(1);
    let retention_ms: i64 = env_or("ADOS_COMPUTE_RETENTION_S", "86400")
        .parse::<i64>()
        .unwrap_or(86_400)
        .saturating_mul(1000);

    // Safety gate: the job API is unauthenticated, so a non-loopback bind would
    // expose it LAN-wide. Refuse it until the pairing-auth layer is in place. A
    // hostname (unparseable as a SocketAddr) is left to the listener to resolve.
    if let Ok(addr) = bind.parse::<std::net::SocketAddr>() {
        if !addr.ip().is_loopback() {
            return Err(format!(
                "refusing to bind {bind}: the compute job API is not authenticated yet, \
                 so it must stay on loopback. Use a 127.0.0.0/8 or ::1 address until the \
                 pairing-auth layer ships."
            )
            .into());
        }
    }

    if db != ":memory:" {
        if let Some(parent) = std::path::Path::new(&db).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
    }

    let store = JobStore::open(&db)?;
    let scheduler = Scheduler::new(store, Box::new(MockReconstructor), Box::new(MockDetector));
    let engine = Engine::new(scheduler, Cluster::new_master(node_id), workers);
    let state = Arc::new(Mutex::new(engine));

    // The worker loop drains the queue, then idles. When idle, it periodically
    // reclaims terminal jobs older than the retention window so the store does
    // not grow without bound. Each iteration releases the lock so the API
    // handlers interleave.
    let worker_state = state.clone();
    tokio::spawn(async move {
        let mut idle_ticks: u32 = 0;
        loop {
            let ran = {
                let engine = worker_state.lock().await;
                engine.tick(now_ms())
            };
            match ran {
                Ok(Some(outcome)) => {
                    idle_ticks = 0;
                    tracing::info!(job = %outcome.job_id, state = ?outcome.state, "ran job");
                }
                Ok(None) => {
                    // Run retention roughly once a minute of idle (every 120
                    // idle ticks at 500 ms).
                    idle_ticks = idle_ticks.saturating_add(1);
                    if idle_ticks.is_multiple_of(120) {
                        let engine = worker_state.lock().await;
                        match engine
                            .scheduler()
                            .store()
                            .purge_terminal_before(now_ms() - retention_ms)
                        {
                            Ok(n) if n > 0 => tracing::info!(removed = n, "retention purge"),
                            Ok(_) => {}
                            Err(e) => tracing::warn!(error = %e, "retention purge failed"),
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                Err(e) => {
                    tracing::error!(error = %e, "worker tick failed");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    });

    let router = build_router(state);
    let listener = TcpListener::bind(&bind).await?;
    tracing::info!(bind = %bind, "compute job API listening (loopback, unauthenticated)");
    axum::serve(listener, router).await?;
    Ok(())
}
