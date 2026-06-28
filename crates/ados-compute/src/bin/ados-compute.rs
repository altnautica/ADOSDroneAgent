//! The `ados-compute` daemon: the compute node's service. It opens the job
//! store, builds the node engine, runs a worker loop that drains the queue and
//! periodically reclaims terminal jobs, and serves the REST job API on a single
//! TCP listener. The supervisor starts it for the `compute` profile.
//!
//! Local-first reach (Rule 39): the job API is gated by the pairing posture
//! (unpaired ⇒ open, paired + on-box ⇒ open, paired + off-box ⇒ `X-ADOS-Key`),
//! so binding a non-loopback address is safe. It still defaults to `127.0.0.1`;
//! the installer opts a node into serving the LAN with `ADOS_COMPUTE_BIND`. mDNS
//! discovery wraps this surface later.
//!
//! Worker note: the worker claims the next job under the engine lock, then
//! releases the lock and runs the (real, possibly minutes-long) backend
//! WITHOUT it, so a long reconstruction never blocks the API. It re-acquires
//! the lock only briefly to record the terminal state; a cancel that lands
//! during the run wins (`Scheduler::finalize` refuses to overwrite a job that
//! is no longer `Running`).
//!
//! Configuration is read from the environment so the install layer can set it
//! without a config-file dependency:
//! - `ADOS_COMPUTE_DB`        job store path (default `/var/ados/compute/jobs.db`)
//! - `ADOS_COMPUTE_BIND`      bind address (default `127.0.0.1:8092`, loopback)
//! - `ADOS_COMPUTE_NODE_ID`   this node's id (default `compute-node`)
//! - `ADOS_COMPUTE_WORKERS`   worker slots (default `1`)
//! - `ADOS_COMPUTE_RETENTION_S` terminal-job retention seconds (default `86400`)
//! - `ADOS_PAIRING_JSON`      pairing.json path (default `/etc/ados/pairing.json`,
//!   the same override the rest of the agent honours)

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ados_compute::{
    build_router, write_compute_heartbeat, Cluster, ComputeAuth, Engine, JobStore, MockDetector,
    MockReconstructor, Prepared, Scheduler, DEFAULT_PAIRING_PATH,
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

    if db != ":memory:" {
        if let Some(parent) = std::path::Path::new(&db).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
    }

    let store = JobStore::open(&db)?;
    let scheduler = Scheduler::new(store, Arc::new(MockReconstructor), Arc::new(MockDetector));
    let engine = Engine::new(scheduler, Cluster::new_master(node_id), workers);
    let state = Arc::new(Mutex::new(engine));

    // Startup recovery: a job left in Running (the daemon crashed mid-backend)
    // is neither claimable nor purgeable, so requeue it before the workers start.
    {
        let engine = state.lock().await;
        match engine.scheduler().store().requeue_stale_running(now_ms()) {
            Ok(n) if n > 0 => {
                tracing::info!(requeued = n, "requeued stale running jobs at startup")
            }
            Ok(_) => {}
            Err(e) => tracing::error!(error = %e, "startup requeue failed"),
        }
    }

    // One worker task per configured slot. Each claims a distinct job atomically
    // (claim_next_queued), runs its backend WITHOUT the engine lock, and
    // finalizes under the lock, so N backends run in parallel while the API stays
    // responsive. A separate task runs retention on a fixed cadence.
    for _ in 0..workers.max(1) {
        let ws = state.clone();
        tokio::spawn(async move { worker_loop(ws).await });
    }
    let rs = state.clone();
    tokio::spawn(async move { retention_loop(rs, retention_ms).await });
    // Publish the cluster + queue state to the heartbeat sidecar so the native
    // cloud relay can fold the compute fields into the agent heartbeat (RUST-
    // first; the relay reads the file, no cross-crate coupling).
    let hs = state.clone();
    tokio::spawn(async move { heartbeat_loop(hs).await });

    let auth = Arc::new(ComputeAuth::new(PathBuf::from(env_or(
        "ADOS_PAIRING_JSON",
        DEFAULT_PAIRING_PATH,
    ))));
    let router = build_router(state, auth);
    let listener = TcpListener::bind(&bind).await?;
    tracing::info!(bind = %bind, workers, "compute job API listening (pairing-gated)");
    // ConnectInfo carries the peer address the auth gate reads to resolve on-box
    // loopback trust.
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}

/// One worker: claim a job under the lock, run the backend WITHOUT it, finalize
/// under the lock. Idles 500 ms when the queue is empty. A cancel that lands
/// during the backend run wins inside `finalize`.
async fn worker_loop(state: Arc<Mutex<Engine>>) {
    loop {
        let (prepared, reconstructor, detector) = {
            let engine = state.lock().await;
            let prepared = engine.scheduler().claim_and_prepare(now_ms());
            let (reconstructor, detector) = engine.scheduler().backends();
            (prepared, reconstructor, detector)
        };
        match prepared {
            Ok(Prepared::Ready { job, input }) => {
                let result =
                    Scheduler::run_backend(&*reconstructor, &*detector, &job, &input, now_ms());
                let outcome = {
                    let engine = state.lock().await;
                    engine.scheduler().finalize(&job, result, now_ms())
                };
                match outcome {
                    Ok(o) => tracing::info!(job = %o.job_id, state = ?o.state, "ran job"),
                    Err(e) => tracing::error!(job = %job.id, error = %e, "finalize failed"),
                }
            }
            Ok(Prepared::Failed(o)) => {
                tracing::info!(job = %o.job_id, state = ?o.state, "job failed at prepare");
            }
            Ok(Prepared::Empty) => tokio::time::sleep(Duration::from_millis(500)).await,
            Err(e) => {
                tracing::error!(error = %e, "worker claim failed");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

/// Periodically reclaim terminal jobs older than the retention window.
async fn retention_loop(state: Arc<Mutex<Engine>>, retention_ms: i64) {
    loop {
        tokio::time::sleep(Duration::from_secs(60)).await;
        let engine = state.lock().await;
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
}

/// Every 5 s, snapshot the engine heartbeat and write it to the sidecar the
/// cloud relay folds into the agent heartbeat. Best-effort: a store or write
/// error is logged, never fatal (the relay treats an absent/stale sidecar as
/// "no compute state", which is the honest reading).
async fn heartbeat_loop(state: Arc<Mutex<Engine>>) {
    loop {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let hb = {
            let engine = state.lock().await;
            engine.heartbeat()
        };
        match hb {
            Ok(hb) => {
                if let Err(e) = write_compute_heartbeat(&hb, now_ms()) {
                    tracing::warn!(error = %e, "compute heartbeat sidecar write failed");
                }
            }
            Err(e) => tracing::warn!(error = %e, "compute heartbeat snapshot failed"),
        }
    }
}
