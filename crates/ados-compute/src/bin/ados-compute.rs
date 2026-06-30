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
//! - `ADOS_COMPUTE_WORK`      dataset + artifact work root (default
//!   `/var/ados/compute/work`); the persister writes keyframe datasets here, the
//!   reconstructor writes artifacts here, and the artifact route serves from here
//! - `ADOS_COMPUTE_BIND`      bind address (default `127.0.0.1:8092`, loopback)
//! - `ADOS_COMPUTE_PUBLIC_URL` base URL the GCS fetches artifacts from (default
//!   derived from the bind address, substituting the node hostname for a wildcard
//!   bind); the artifact URL is `<public_url>/artifacts/<relpath>`
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
    artifact_router, build_rerun_output, build_router, derive_public_base,
    rewrite_output_to_artifact_url, submit_reconstruct_job, write_compute_heartbeat, AtlasIngest,
    Cluster, ComputeAuth, ComputeJobState, Engine, JobStore, LiveReconstructConfig, MockDetector,
    Prepared, PreparedInput, Scheduler, SelectingReconstructor, DEFAULT_PAIRING_PATH,
};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, Mutex};
use tower_http::cors::CorsLayer;

/// Canonical agent config file the atlas gate reads (the air-side `ados-atlas`
/// service reads the same path + key).
const DEFAULT_CONFIG_YAML: &str = "/etc/ados/config.yaml";
/// Bounded capacity of the Atlas event receive channel. When it fills, the event
/// router returns `503` so the sender's failover ladder retries or drops — the
/// reconstructor running behind never grows an unbounded in-memory queue.
const ATLAS_EVENT_CHANNEL_CAP: usize = 256;
/// How often the live-reconstruction cadence is evaluated (the interval trigger's
/// granularity + the skip-while-running reconcile). The cadence's own thresholds
/// are much coarser (tens of seconds / keyframes); this is just the poll period.
const LIVE_CADENCE_TICK_SECS: u64 = 2;

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

/// True when a config flag string is an affirmative (`1`/`true`/`yes`/`on`).
fn is_truthy(v: &str) -> bool {
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// True when the world-model program is enabled for this node, mirroring the
/// air-side `ados-atlas` gate. `ADOS_ATLAS_ENABLED` is the install-layer override
/// (consistent with the daemon's other env-driven config); absent, the
/// `atlas.enabled` key of the agent config is read. A missing / unparseable file
/// reads disabled, so the receiver stays inert and a non-atlas node is
/// byte-unchanged.
fn atlas_enabled() -> bool {
    if let Ok(v) = std::env::var("ADOS_ATLAS_ENABLED") {
        return is_truthy(&v);
    }
    atlas_enabled_in_yaml(&env_or("ADOS_CONFIG_YAML", DEFAULT_CONFIG_YAML))
}

/// True when live (in-flight) reconstruction is enabled for this node:
/// `ADOS_ATLAS_LIVE` is the install-layer override; absent, the
/// `atlas.live_reconstruct` key of the agent config is read. Opt-in (default
/// off), so a node that only wants the post-flight bag reconstruct is unaffected.
fn atlas_live_enabled() -> bool {
    if let Ok(v) = std::env::var("ADOS_ATLAS_LIVE") {
        return is_truthy(&v);
    }
    atlas_live_in_yaml(&env_or("ADOS_CONFIG_YAML", DEFAULT_CONFIG_YAML))
}

/// Read the `atlas.live_reconstruct` boolean from the agent config at `path`.
/// Pure (the path is injected) so it is unit-tested. Disabled on any read/parse
/// error or an absent block, the same fail-closed default as [`atlas_enabled_in_yaml`].
fn atlas_live_in_yaml(path: &str) -> bool {
    #[derive(serde::Deserialize, Default)]
    struct Raw {
        #[serde(default)]
        atlas: Option<AtlasSection>,
    }
    #[derive(serde::Deserialize, Default)]
    struct AtlasSection {
        #[serde(default)]
        live_reconstruct: bool,
    }
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    serde_norway::from_str::<Raw>(&text)
        .ok()
        .and_then(|r| r.atlas)
        .map(|a| a.live_reconstruct)
        .unwrap_or(false)
}

/// Build the live-reconstruction cadence config from the environment + agent
/// config. The cadence defaults (every 30 keyframes / 20 s / 8-keyframe floor)
/// come from [`LiveReconstructConfig::default`]; `enabled` is set from the gate,
/// and the thresholds can be tuned via env without a config-file dependency.
fn live_reconstruct_config() -> LiveReconstructConfig {
    let mut cfg = LiveReconstructConfig {
        enabled: atlas_live_enabled(),
        ..LiveReconstructConfig::default()
    };
    if let Some(v) = env_u64("ADOS_ATLAS_LIVE_EVERY_KEYFRAMES") {
        cfg.every_keyframes = v.max(1);
    }
    if let Some(s) = env_u64("ADOS_ATLAS_LIVE_INTERVAL_S") {
        cfg.interval_ms = (s as i64).saturating_mul(1000);
    }
    if let Some(v) = env_u64("ADOS_ATLAS_LIVE_MIN_KEYFRAMES") {
        cfg.min_keyframes = v.max(1);
    }
    cfg
}

/// Parse a `u64` env var, or `None` when unset / unparseable.
fn env_u64(key: &str) -> Option<u64> {
    std::env::var(key).ok().and_then(|v| v.trim().parse().ok())
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
    let work_root = PathBuf::from(env_or("ADOS_COMPUTE_WORK", "/var/ados/compute/work"));
    let bind = env_or("ADOS_COMPUTE_BIND", "127.0.0.1:8092");
    let node_id = resolve_node_id();
    let workers: u32 = env_or("ADOS_COMPUTE_WORKERS", "1").parse().unwrap_or(1);
    let retention_ms: i64 = env_or("ADOS_COMPUTE_RETENTION_S", "86400")
        .parse::<i64>()
        .unwrap_or(86_400)
        .saturating_mul(1000);

    // The base URL the GCS fetches artifacts from: the explicit override, else
    // derived from the bind (the node hostname stands in for a wildcard bind so
    // the URL is reachable off-box). The artifact host matches the mDNS target.
    let public_base = derive_public_base(
        &bind,
        std::env::var("ADOS_COMPUTE_PUBLIC_URL").ok().as_deref(),
        Some(&ados_compute::mdns::system_hostname()),
    );

    if db != ":memory:" {
        if let Some(parent) = std::path::Path::new(&db).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
    }
    // The work root holds keyframe datasets and reconstruction artifacts.
    let _ = std::fs::create_dir_all(&work_root);

    let store = JobStore::open(&db)?;
    // The reconstructor picks the real backend per job (Brush when installed),
    // falling back to the mock (CI / no-GPU), and writes artifacts under work_root.
    let scheduler = Scheduler::new(
        store,
        Arc::new(SelectingReconstructor::new(work_root.clone())),
        Arc::new(MockDetector),
    );
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
        let wr = work_root.clone();
        let pb = public_base.clone();
        tokio::spawn(async move { worker_loop(ws, wr, pb).await });
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
    // The artifact server hands reconstruction outputs (.ply / .rrd / .tif / .jpg)
    // to the GCS over the LAN, path-jailed to the work root, on the same listener.
    let mut router = build_router(state.clone(), auth).merge(artifact_router(work_root.clone()));

    // Atlas world-model receiver. INERT unless atlas is enabled (Rule 46 single
    // canonical gate): when on, mount POST /api/atlas/event alongside the compute
    // job API on the same listener, and drain decoded events into the job queue
    // (a bagged capture-state submits the reconstruct job the workers pick up).
    // When off, neither the route nor the drain task exists, so a non-atlas
    // workstation node is byte-unchanged.
    if atlas_enabled() {
        let live_config = live_reconstruct_config();
        let (atlas_tx, atlas_rx) = mpsc::channel::<AtlasEvent>(ATLAS_EVENT_CHANNEL_CAP);
        router = router.merge(atlas_event_router(atlas_tx));
        let rs = state.clone();
        let wr = work_root.clone();
        tokio::spawn(async move { atlas_receiver_loop(atlas_rx, rs, wr, live_config).await });
        tracing::info!(
            channel_cap = ATLAS_EVENT_CHANNEL_CAP,
            live_reconstruct = live_config.enabled,
            "atlas enabled: world-model event receiver mounted at POST /api/atlas/event"
        );
    }

    // Permissive CORS so the GCS can read this listener cross-origin from the
    // browser on an HTTP origin (the compute-client fetches `:8092` directly on
    // http; the proxy is HTTPS-only). Matches ados-control's `:8080`. Outermost
    // layer: OPTIONS preflights are answered ahead of the per-route pairing gate.
    let router = router.layer(CorsLayer::permissive());

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
/// during the backend run wins inside `finalize`. A real `file://` artifact under
/// the work root is rewritten to a fetchable LAN URL (the GCS reads it as the
/// output URL) before finalize, keeping the local path for any pipeline chaining.
async fn worker_loop(state: Arc<Mutex<Engine>>, work_root: PathBuf, public_base: String) {
    loop {
        let (prepared, reconstructor, detector) = {
            let engine = state.lock().await;
            let prepared = engine.scheduler().claim_and_prepare(now_ms());
            let (reconstructor, detector) = engine.scheduler().backends();
            (prepared, reconstructor, detector)
        };
        match prepared {
            Ok(Prepared::Ready { job, input }) => {
                let mut result =
                    Scheduler::run_backend(&*reconstructor, &*detector, &job, &input, now_ms());
                // Write the Rerun world-model .rrd from the real capture + the
                // reconstruction geometry so the GCS World viewer renders real data
                // (camera trajectory + the reconstructed point cloud). Reconstruct
                // jobs only; an offload job has no world model. Best-effort: a write
                // fault is logged and the job still completes with its other output.
                if let PreparedInput::Reconstruct(dataset) = &input {
                    let input_path = dataset
                        .meta
                        .get("input_path")
                        .and_then(|v| v.as_str())
                        .map(std::path::Path::new);
                    let geometry = result
                        .outputs
                        .first()
                        .map(|o| (o.kind.as_str(), o.uri.as_str()));
                    match build_rerun_output(&work_root, &job.id, input_path, geometry, now_ms()) {
                        Ok(Some(rerun_out)) => result.outputs.push(rerun_out),
                        Ok(None) => {}
                        Err(e) => {
                            tracing::warn!(job = %job.id, error = %e, "rerun world-model write failed")
                        }
                    }
                }
                for output in &mut result.outputs {
                    rewrite_output_to_artifact_url(output, &work_root, &public_base);
                }
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

/// Every 5 s, snapshot the engine heartbeat + the host-GPU block and write them
/// to the sidecar the cloud relay folds into the agent heartbeat. The GPU read
/// shells out to `system_profiler`/`powermetrics`, so it runs on a blocking
/// thread to keep the async runtime responsive; a join failure degrades to an
/// all-null GPU block. Best-effort: a store or write error is logged, never fatal
/// (the relay treats an absent/stale sidecar as "no compute state", the honest
/// reading).
async fn heartbeat_loop(state: Arc<Mutex<Engine>>) {
    loop {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let hb = {
            let engine = state.lock().await;
            engine.heartbeat()
        };
        match hb {
            Ok(hb) => {
                let gpu = tokio::task::spawn_blocking(ados_compute::gpu::sample)
                    .await
                    .unwrap_or_default();
                if let Err(e) = write_compute_heartbeat(&hb, gpu, now_ms()) {
                    tracing::warn!(error = %e, "compute heartbeat sidecar write failed");
                }
            }
            Err(e) => tracing::warn!(error = %e, "compute heartbeat snapshot failed"),
        }
    }
}

/// Drain the Atlas event receiver into the job queue. One [`AtlasIngest`] lives
/// for the task: it persists each keyframe's image to the work-root dataset (no
/// store, no lock) and, on the terminal `Bagged` state, finalizes the dataset
/// (writes `transforms.json`) and yields the reconstruct job. The disk writes run
/// lock-free; the engine lock is held only briefly for the store submit (the same
/// lock-briefly discipline the worker uses). A malformed frame is swallowed; a
/// real filesystem or store fault is logged and the loop continues. The loop ends
/// when the event channel closes (the receiver router dropped its senders), which
/// only happens on shutdown.
///
/// When live reconstruction is enabled, the loop ALSO runs the per-session cadence
/// ([`run_live_cycles`]) on a tick and after each keyframe: it periodically
/// snapshots the growing capture and enqueues a real reconstruct so the world
/// model updates during the flight, not only at the end. When disabled, it is the
/// event-only drain (final bag reconstruct only) — byte-unchanged.
async fn atlas_receiver_loop(
    mut rx: mpsc::Receiver<AtlasEvent>,
    state: Arc<Mutex<Engine>>,
    work_root: PathBuf,
    live_config: LiveReconstructConfig,
) {
    let mut ingest = AtlasIngest::with_live_config(work_root, live_config);

    if !live_config.enabled {
        while let Some(event) = rx.recv().await {
            drain_event(&mut ingest, &state, &event).await;
        }
        tracing::info!("atlas receiver loop ended (event channel closed)");
        return;
    }

    let mut tick = tokio::time::interval(Duration::from_secs(LIVE_CADENCE_TICK_SECS));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            maybe_event = rx.recv() => {
                let Some(event) = maybe_event else { break };
                drain_event(&mut ingest, &state, &event).await;
                // A keyframe may have hit the count trigger; check promptly.
                run_live_cycles(&mut ingest, &state).await;
            }
            _ = tick.tick() => {
                // The interval trigger + the skip-while-running reconcile.
                run_live_cycles(&mut ingest, &state).await;
            }
        }
    }
    tracing::info!("atlas receiver loop ended (event channel closed)");
}

/// Handle one received Atlas event: persist a keyframe or, on the terminal bag,
/// enqueue the final reconstruct. A malformed frame is swallowed; a real disk or
/// store fault is logged and the caller continues.
async fn drain_event(ingest: &mut AtlasIngest, state: &Arc<Mutex<Engine>>, event: &AtlasEvent) {
    match ingest.step(event, now_ms()) {
        Ok(Some((dataset, job))) => {
            let keyframes_received = ingest.keyframes_seen();
            let submitted = {
                let engine = state.lock().await;
                submit_reconstruct_job(engine.scheduler().store(), &dataset, &job)
            };
            match submitted {
                Ok(job_id) => tracing::info!(
                    job = %job_id,
                    keyframes_received,
                    "atlas capture bagged: reconstruct job enqueued"
                ),
                Err(e) => tracing::error!(error = %e, "atlas reconstruct submit failed"),
            }
        }
        Ok(None) => {}
        Err(e) => tracing::error!(error = %e, "atlas ingest disk write failed"),
    }
}

/// Drive the live-reconstruction cadence: reconcile the skip-while-running guard
/// against the store (a cycle whose job reached a terminal state, or vanished,
/// releases the guard), then enqueue a fresh periodic reconstruct for every
/// session now due. The store is the source of truth for "is the cycle done", so
/// cycles coalesce instead of piling up. A snapshot or submit fault is logged; the
/// cadence keeps running.
async fn run_live_cycles(ingest: &mut AtlasIngest, state: &Arc<Mutex<Engine>>) {
    // 1. Release the guard for any session whose in-flight cycle finished.
    let in_flight = ingest.in_flight_cycles();
    if !in_flight.is_empty() {
        let finished: Vec<String> = {
            let engine = state.lock().await;
            let store = engine.scheduler().store();
            in_flight
                .into_iter()
                .filter_map(|(session, job_id)| match store.get_job(&job_id) {
                    Ok(Some(job)) if is_terminal(job.state) => Some(session),
                    // The job was purged (retention) — treat as finished so the
                    // session is never stuck with a guard that never releases.
                    Ok(None) => Some(session),
                    Ok(Some(_)) => None, // still queued / running: keep the guard
                    Err(e) => {
                        tracing::warn!(error = %e, "live cycle reconcile read failed");
                        None
                    }
                })
                .collect()
        };
        for session in finished {
            ingest.note_cycle_finished(&session);
        }
    }

    // 2. Enqueue a fresh periodic reconstruct for every session now due.
    match ingest.due_reconstructs(now_ms()) {
        Ok(jobs) => {
            for (dataset, job) in jobs {
                let submitted = {
                    let engine = state.lock().await;
                    submit_reconstruct_job(engine.scheduler().store(), &dataset, &job)
                };
                let keyframes = dataset
                    .meta
                    .get("keyframes")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                match submitted {
                    Ok(id) => tracing::info!(
                        job = %id,
                        keyframes,
                        "atlas live reconstruct cycle enqueued"
                    ),
                    Err(e) => tracing::error!(error = %e, "atlas live reconstruct submit failed"),
                }
            }
        }
        Err(e) => tracing::error!(error = %e, "atlas live snapshot write failed"),
    }
}

/// Whether a job has reached a terminal state (the live cadence's
/// skip-while-running guard releases on a terminal in-flight cycle).
fn is_terminal(state: ComputeJobState) -> bool {
    matches!(
        state,
        ComputeJobState::Completed | ComputeJobState::Failed | ComputeJobState::Cancelled
    )
}

#[cfg(test)]
mod tests {
    use super::{atlas_enabled_in_yaml, atlas_live_in_yaml, derive_node_id};

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

    #[test]
    fn live_reconstruct_gate_reads_its_own_key_and_defaults_off() {
        // Opt-in: the key must be present and true; atlas.enabled alone does NOT
        // turn on live reconstruction.
        let (_d1, on) = write_cfg("atlas:\n  enabled: true\n  live_reconstruct: true\n");
        assert!(atlas_live_in_yaml(on.to_str().unwrap()));

        let (_d2, only_enabled) = write_cfg("atlas:\n  enabled: true\n");
        assert!(
            !atlas_live_in_yaml(only_enabled.to_str().unwrap()),
            "atlas enabled without live_reconstruct leaves live off"
        );

        let (_d3, off) = write_cfg("atlas:\n  live_reconstruct: false\n");
        assert!(!atlas_live_in_yaml(off.to_str().unwrap()));

        assert!(
            !atlas_live_in_yaml("/nonexistent/ados/config.yaml"),
            "a missing config reads live off (fail-closed)"
        );
    }
}
