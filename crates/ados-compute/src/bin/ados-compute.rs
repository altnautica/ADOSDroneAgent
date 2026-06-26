//! The `ados-compute` daemon: the compute node's service. It opens the job
//! store, builds the node engine, runs a worker loop that drains the queue, and
//! serves the REST job API on the LAN. The supervisor starts it for the
//! `compute` profile. mDNS discovery and the pairing auth wrap this surface; on
//! its own it is the lean local-first job API.
//!
//! Configuration is read from the environment so the install layer can set it
//! without a config-file dependency:
//! - `ADOS_COMPUTE_DB`       job store path (default `/var/ados/compute/jobs.db`)
//! - `ADOS_COMPUTE_BIND`     LAN bind address (default `127.0.0.1:8092`)
//! - `ADOS_COMPUTE_NODE_ID`  this node's id (default `compute-node`)
//! - `ADOS_COMPUTE_WORKERS`  worker slots (default `1`)

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ados_compute::{
    build_router, Cluster, Engine, JobStore, MockDetector, MockReconstructor, Scheduler,
};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

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
    tracing_subscriber::fmt::init();

    let db = env_or("ADOS_COMPUTE_DB", "/var/ados/compute/jobs.db");
    let bind = env_or("ADOS_COMPUTE_BIND", "127.0.0.1:8092");
    let node_id = env_or("ADOS_COMPUTE_NODE_ID", "compute-node");
    let workers: u32 = env_or("ADOS_COMPUTE_WORKERS", "1").parse().unwrap_or(1);

    if db != ":memory:" {
        if let Some(parent) = std::path::Path::new(&db).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
    }

    let store = JobStore::open(&db)?;
    let scheduler = Scheduler::new(store, Box::new(MockReconstructor), Box::new(MockDetector));
    let engine = Engine::new(scheduler, Cluster::new_master(node_id), workers);
    let state = Arc::new(Mutex::new(engine));

    // The worker loop drains the queue, then idles. Each iteration releases the
    // lock so the API handlers interleave.
    let worker_state = state.clone();
    tokio::spawn(async move {
        loop {
            let ran = {
                let engine = worker_state.lock().await;
                engine.tick(now_ms())
            };
            match ran {
                Ok(Some(outcome)) => {
                    tracing::info!(job = %outcome.job_id, state = ?outcome.state, "ran job");
                }
                Ok(None) => tokio::time::sleep(Duration::from_millis(500)).await,
                Err(e) => {
                    tracing::error!(error = %e, "worker tick failed");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    });

    let router = build_router(state);
    let listener = TcpListener::bind(&bind).await?;
    tracing::info!(bind = %bind, "compute job API listening");
    axum::serve(listener, router).await?;
    Ok(())
}
