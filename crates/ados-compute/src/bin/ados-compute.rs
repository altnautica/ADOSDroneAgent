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
//! - `ADOS_ATLAS_ENABLED`     overrides the `atlas.enabled` config gate (`1`/`true`
//!   to mount the world-model event receiver); absent, the `atlas.enabled` key of
//!   `/etc/ados/config.yaml` is read (default disabled, so a non-atlas node is
//!   byte-unchanged)
//! - `ADOS_CONFIG_YAML`       agent config path (default `/etc/ados/config.yaml`),
//!   read only for the atlas gate

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ados_atlas_transport::{atlas_event_router, AtlasEvent};
use ados_compute::{
    build_router, write_compute_heartbeat, AtlasIngest, Cluster, ComputeAuth, Engine, JobStore,
    MockDetector, MockReconstructor, Prepared, Scheduler, DEFAULT_PAIRING_PATH,
};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, Mutex};

/// Canonical agent config file the atlas gate reads (the air-side `ados-atlas`
/// service reads the same path + key).
const DEFAULT_CONFIG_YAML: &str = "/etc/ados/config.yaml";
/// Bounded capacity of the Atlas event receive channel. When it fills, the event
/// router returns `503` so the sender's failover ladder retries or drops — the
/// reconstructor running behind never grows an unbounded in-memory queue.
const ATLAS_EVENT_CHANNEL_CAP: usize = 256;

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

/// Resolve a stable per-node id: the `ADOS_COMPUTE_NODE_ID` override if set,
/// else derived from the host `machine-id` so two compute nodes never collide on
/// the mDNS instance / deviceId / cluster identity, else a generic fallback.
/// Pure (the inputs are injected) so the derivation is unit-tested.
fn derive_node_id(env: Option<String>, machine_id: Option<String>) -> String {
    if let Some(id) = env {
        let id = id.trim();
        if !id.is_empty() {
            return id.to_string();
        }
    }
    if let Some(mid) = machine_id {
        let mid = mid.trim();
        if !mid.is_empty() {
            return format!("compute-{}", &mid[..mid.len().min(12)]);
        }
    }
    "compute-node".to_string()
}

fn resolve_node_id() -> String {
    derive_node_id(
        std::env::var("ADOS_COMPUTE_NODE_ID").ok(),
        std::fs::read_to_string("/etc/machine-id").ok(),
    )
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// True when the world-model program is enabled for this node, mirroring the
/// air-side `ados-atlas` gate. `ADOS_ATLAS_ENABLED` is the install-layer override
/// (consistent with the daemon's other env-driven config); absent, the
/// `atlas.enabled` key of the agent config is read. A missing / unparseable file
/// reads disabled, so the receiver stays inert and a non-atlas node is
/// byte-unchanged.
fn atlas_enabled() -> bool {
    if let Ok(v) = std::env::var("ADOS_ATLAS_ENABLED") {
        return matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        );
    }
    atlas_enabled_in_yaml(&env_or("ADOS_CONFIG_YAML", DEFAULT_CONFIG_YAML))
}

/// Read the `atlas.enabled` boolean from the agent config at `path` (the one
/// canonical key the air-side reader uses — no `system.atlas` alias). Pure (the
/// path is injected) so the gate is unit-tested. Disabled on any read/parse
/// error or an absent block.
fn atlas_enabled_in_yaml(path: &str) -> bool {
    #[derive(serde::Deserialize, Default)]
    struct Raw {
        #[serde(default)]
        atlas: Option<AtlasSection>,
    }
    #[derive(serde::Deserialize, Default)]
    struct AtlasSection {
        #[serde(default)]
        enabled: bool,
    }
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    serde_norway::from_str::<Raw>(&text)
        .ok()
        .and_then(|r| r.atlas)
        .map(|a| a.enabled)
        .unwrap_or(false)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_logging();

    let db = env_or("ADOS_COMPUTE_DB", "/var/ados/compute/jobs.db");
    let bind = env_or("ADOS_COMPUTE_BIND", "127.0.0.1:8092");
    let node_id = resolve_node_id();
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
    let engine = Engine::new(scheduler, Cluster::new_master(node_id.clone()), workers);
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
    let mut router = build_router(state.clone(), auth);

    // Atlas world-model receiver. INERT unless atlas is enabled (Rule 46 single
    // canonical gate): when on, mount POST /api/atlas/event alongside the compute
    // job API on the same listener, and drain decoded events into the job queue
    // (a bagged capture-state submits the reconstruct job the workers pick up).
    // When off, neither the route nor the drain task exists, so a non-atlas
    // workstation node is byte-unchanged.
    if atlas_enabled() {
        let (atlas_tx, atlas_rx) = mpsc::channel::<AtlasEvent>(ATLAS_EVENT_CHANNEL_CAP);
        router = router.merge(atlas_event_router(atlas_tx));
        let rs = state.clone();
        tokio::spawn(async move { atlas_receiver_loop(atlas_rx, rs).await });
        tracing::info!(
            channel_cap = ATLAS_EVENT_CHANNEL_CAP,
            "atlas enabled: world-model event receiver mounted at POST /api/atlas/event"
        );
    }

    let listener = TcpListener::bind(&bind).await?;
    tracing::info!(bind = %bind, workers, "compute job API listening (pairing-gated)");
    // Advertise on mDNS so the GCS Add-a-Node card auto-discovers this node for
    // LAN pairing (Rule 39). Best-effort: a None means no auto-discovery, manual
    // add-by-IP still works. Held for the process lifetime (unregisters on exit).
    let job_port = bind
        .rsplit(':')
        .next()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(8092);
    let _mdns_advert = ados_compute::advertise_compute(&node_id, job_port);
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

/// Drain the Atlas event receiver into the job queue. One [`AtlasIngest`] lives
/// for the task: it counts a capture session's keyframes (the received-side
/// delivery proof) and, on the terminal `Bagged` state, submits the reconstruct
/// job the worker loop picks up. The store write is brief, so the engine lock is
/// held only for the `ingest` call (the same lock-briefly discipline the worker
/// uses). A malformed frame is swallowed inside `ingest`; a real store fault is
/// logged and the loop continues. The loop ends when the event channel closes
/// (the receiver router dropped its senders), which only happens on shutdown.
async fn atlas_receiver_loop(mut rx: mpsc::Receiver<AtlasEvent>, state: Arc<Mutex<Engine>>) {
    let mut ingest = AtlasIngest::new();
    while let Some(event) = rx.recv().await {
        let outcome = {
            let engine = state.lock().await;
            ingest.ingest(&event, engine.scheduler().store(), now_ms())
        };
        match outcome {
            Ok(Some(job_id)) => tracing::info!(
                job = %job_id,
                keyframes_received = ingest.keyframes_seen(),
                "atlas capture bagged: reconstruct job enqueued"
            ),
            Ok(None) => {}
            Err(e) => tracing::error!(error = %e, "atlas ingest failed"),
        }
    }
    tracing::info!("atlas receiver loop ended (event channel closed)");
}

#[cfg(test)]
mod tests {
    use super::{atlas_enabled_in_yaml, derive_node_id};

    #[test]
    fn node_id_prefers_the_env_override() {
        assert_eq!(
            derive_node_id(Some("rtx-box".to_string()), Some("mid".to_string())),
            "rtx-box"
        );
    }

    #[test]
    fn node_id_derives_from_machine_id_when_env_unset_and_is_unique_per_host() {
        let a = derive_node_id(None, Some("aaaaaaaaaaaaaaaa1111".to_string()));
        let b = derive_node_id(None, Some("bbbbbbbbbbbbbbbb2222".to_string()));
        assert_eq!(a, "compute-aaaaaaaaaaaa");
        assert_ne!(a, b, "distinct machine-ids must yield distinct node ids");
    }

    #[test]
    fn node_id_falls_back_when_nothing_is_available() {
        assert_eq!(derive_node_id(None, None), "compute-node");
        // A blank env / blank machine-id both fall through to the next source.
        assert_eq!(derive_node_id(Some("  ".to_string()), None), "compute-node");
    }

    fn write_cfg(yaml: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(&path, yaml).unwrap();
        (dir, path)
    }

    #[test]
    fn atlas_gate_is_disabled_when_the_config_is_missing() {
        // A missing file reads disabled so the receiver stays inert (byte-unchanged).
        assert!(!atlas_enabled_in_yaml("/nonexistent/ados/config.yaml"));
    }

    #[test]
    fn atlas_gate_reads_the_canonical_enabled_key() {
        let (_d, p) = write_cfg("agent:\n  profile: workstation\natlas:\n  enabled: true\n");
        assert!(atlas_enabled_in_yaml(p.to_str().unwrap()));
    }

    #[test]
    fn atlas_gate_is_disabled_when_the_block_is_absent_or_false() {
        let (_d1, absent) = write_cfg("agent:\n  profile: workstation\n");
        assert!(!atlas_enabled_in_yaml(absent.to_str().unwrap()));
        let (_d2, off) = write_cfg("atlas:\n  enabled: false\n");
        assert!(!atlas_enabled_in_yaml(off.to_str().unwrap()));
    }

    #[test]
    fn atlas_gate_is_disabled_on_a_malformed_config() {
        // An unparseable file never enables the receiver (fail-closed).
        let (_d, bad) = write_cfg("atlas: [this is not a map\n");
        assert!(!atlas_enabled_in_yaml(bad.to_str().unwrap()));
    }
}
