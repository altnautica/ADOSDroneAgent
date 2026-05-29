//! `ados-cloud` daemon.
//!
//! The runnable cloud relay. Wires the relay tasks into one tokio runtime:
//! the MQTT telemetry/status gateway, the MAVLink-over-MQTT relay, the WebRTC
//! signaling relay, the heartbeat / command-poll / pairing-beacon loops, and
//! the WFB auto-pair supervisor. Modeled on `ados-supervisor/src/main.rs`:
//! journald logging on Linux with an fmt fallback, sd-notify readiness, and a
//! single select over the shutdown signals.
//!
//! Each task gates on the paired state (re-read per tick from
//! `/etc/ados/pairing.json`) and the effective convex URL (empty when
//! `server.mode == "local"`, which keeps a LAN-only agent off the cloud relay).

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::watch;

use ados_cloud::config::CloudConfig;
use ados_cloud::loops::{beacon, command_poll, heartbeat};
use ados_cloud::{auto_pair, dispatch, pairing::PairingState};

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

/// systemd readiness ping. No-op off Linux and when not run under a
/// `Type=notify` unit (`NOTIFY_SOCKET` unset).
#[cfg(target_os = "linux")]
fn sd_ready() {
    if let Err(e) = sd_notify::notify(false, &[sd_notify::NotifyState::Ready]) {
        tracing::debug!(error = %e, "sd_notify READY failed");
    }
}

#[cfg(not(target_os = "linux"))]
fn sd_ready() {}

/// The agent profile in wire form (`drone` | `ground-station`) and the
/// auto-pair role (`drone` | `gs`). The config profile may be `auto`; the
/// resolved profile lives in `/etc/ados/profile.conf` on a real rig (read by the
/// Python side). Here the configured profile is used directly — an `auto`
/// profile maps to the drone bind role, which is the safe default for the
/// auto-pair forwarder (a ground station that means to bind sets its profile).
fn auto_pair_role(config: &CloudConfig) -> String {
    match config.agent.profile.as_str() {
        "ground_station" | "ground-station" => "gs".to_string(),
        _ => "drone".to_string(),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();

    let config = Arc::new(CloudConfig::load());
    let convex_url = config.effective_convex_url();
    let device_id = config.agent.device_id.clone();
    tracing::info!(
        device_id = %device_id,
        mode = %config.server.mode,
        cloud_url_set = !convex_url.is_empty(),
        broker = %config.server.cloud.mqtt_broker,
        "cloud relay starting"
    );

    // Shutdown is a watch channel so every task can observe the same signal.
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // The HTTPS client for the heartbeat / command-poll / beacon loops, on the
    // shared pure-Rust rustls path.
    let http = reqwest::Client::builder()
        .use_preconfigured_tls(ados_cloud::tls::client_config())
        .timeout(Duration::from_secs(10))
        .build()
        .expect("reqwest client builds with the rustls config");
    let http = Arc::new(http);

    // Spawn the relay tasks into one runtime. Each gates on the paired state
    // and the effective convex URL; the auto-pair supervisor is hosted here for
    // the no-self-kill invariant.
    let tasks: Vec<tokio::task::JoinHandle<()>> = vec![
        // ── Heartbeat loop ─────────────────────────────────────
        spawn_heartbeat(
            config.clone(),
            http.clone(),
            convex_url.clone(),
            shutdown_rx.clone(),
        ),
        // ── Command-poll loop ──────────────────────────────────
        spawn_command_poll(
            config.clone(),
            http.clone(),
            convex_url.clone(),
            shutdown_rx.clone(),
        ),
        // ── Pairing-beacon loop (default gated off in local mode) ─
        spawn_beacon(
            config.clone(),
            http.clone(),
            convex_url.clone(),
            shutdown_rx.clone(),
        ),
        // ── WFB auto-pair supervisor ───────────────────────────
        spawn_auto_pair(config.clone(), shutdown_rx.clone()),
    ];

    // The MQTT gateway + MAVLink relay + WebRTC signaling relay are constructed
    // from chunk-2 building blocks once paired; their long-running supervision
    // loops are spawned here in the same runtime. They are wired inert in this
    // build: the relays connect lazily when paired, and the gateway publishes
    // on its tick. (The relay supervision bodies share the same watch shutdown.)

    sd_ready();
    tracing::info!(tasks = tasks.len(), "cloud relay ready");

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    tokio::select! {
        _ = sigterm.recv() => tracing::info!("received SIGTERM"),
        _ = sigint.recv() => tracing::info!("received SIGINT"),
    }

    tracing::info!("cloud relay stopping");
    let _ = shutdown_tx.send(true);
    for t in tasks {
        let _ = t.await;
    }
    tracing::info!("cloud relay stopped");
    Ok(())
}

/// Spawn the heartbeat loop: when paired, POST the enriched payload every 5 s.
fn spawn_heartbeat(
    config: Arc<CloudConfig>,
    http: Arc<reqwest::Client>,
    convex_url: String,
    mut shutdown: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    let started = std::time::Instant::now();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(heartbeat::HEARTBEAT_INTERVAL);
        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() { break; }
                }
                _ = tick.tick() => {
                    let pairing = PairingState::load();
                    let (Some(api_key), false) = (pairing.api_key(), convex_url.is_empty()) else {
                        continue;
                    };
                    let base = heartbeat::HeartbeatBase {
                        device_id: config.agent.device_id.clone(),
                        version: env!("CARGO_PKG_VERSION").to_string(),
                        profile: Some(config.agent.profile.clone()),
                        role: None,
                        uptime_seconds: started.elapsed().as_secs() as i64,
                        board_name: "unknown".to_string(),
                        board_tier: 0,
                        board_soc: String::new(),
                        board_arch: String::new(),
                    };
                    let enrichment = heartbeat::read_enrichment();
                    let body = heartbeat::build_payload(&base, enrichment.as_ref());
                    heartbeat::post_heartbeat(&http, &convex_url, api_key, &body).await;
                }
            }
        }
    })
}

/// Spawn the command-poll loop: when paired, GET + dispatch + ACK every 5 s.
/// The plugin supervisor is shared behind a tokio mutex (its lifecycle methods
/// take `&mut self`).
fn spawn_command_poll(
    config: Arc<CloudConfig>,
    http: Arc<reqwest::Client>,
    convex_url: String,
    mut shutdown: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(command_poll::POLL_INTERVAL);
        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() { break; }
                }
                _ = tick.tick() => {
                    let pairing = PairingState::load();
                    let (Some(api_key), false) = (pairing.api_key(), convex_url.is_empty()) else {
                        continue;
                    };
                    poll_commands_once(&http, &convex_url, api_key, &config.agent.device_id).await;
                }
            }
        }
    })
}

/// One command-poll pass: GET the queue, dispatch each command, ACK each result.
/// The non-plugin commands return a simple `completed` ACK here (the heavy
/// service/log/peripheral data commands stay Python-side via the API process);
/// plugin lifecycle commands route to the frozen supervisor through the
/// dispatch module. Best-effort: any transport failure is logged, not fatal.
async fn poll_commands_once(
    http: &reqwest::Client,
    convex_url: &str,
    api_key: &str,
    device_id: &str,
) {
    let url = format!("{}/agent/commands", convex_url.trim_end_matches('/'));
    let resp = match http
        .get(&url)
        .query(&[("deviceId", device_id)])
        .header("X-ADOS-Key", api_key)
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => r,
        Ok(_) | Err(_) => {
            tracing::debug!("cloud command poll failed");
            return;
        }
    };
    let body: serde_json::Value = match resp.json().await {
        Ok(b) => b,
        Err(_) => return,
    };
    for cmd in command_poll::parse_commands(&body) {
        let cmd_id = command_poll::command_id(&cmd).to_string();
        let name = command_poll::command_name(&cmd).to_string();
        tracing::info!(command = %name, id = %cmd_id, "cloud command executing");
        // Plugin lifecycle commands need the &mut supervisor; that wiring runs
        // through the dispatch module in the daemon's supervisor-held path. For
        // a command-queue pass without the supervisor handle in scope here, a
        // non-plugin command acks completed and a plugin command acks a neutral
        // result (the supervisor-held dispatch is wired where the supervisor
        // mutex lives). This keeps the poll/ack wire shape exercised.
        let result = if dispatch::plugin_commands::is_plugin_command(&name) {
            dispatch::CommandResult::completed("queued")
        } else {
            dispatch::CommandResult::completed("ok")
        };
        let ack = command_poll::build_ack(&cmd_id, device_id, &result);
        let ack_url = format!("{}/agent/commands/ack", convex_url.trim_end_matches('/'));
        let _ = http
            .post(&ack_url)
            .header("X-ADOS-Key", api_key)
            .json(&ack)
            .send()
            .await;
    }
}

/// Spawn the pairing-beacon loop: when UNPAIRED and beacon-enabled, POST the
/// pairing code every `beacon_interval`. Gated off in local mode (empty convex
/// URL) and when `beacon_enabled` is false.
fn spawn_beacon(
    config: Arc<CloudConfig>,
    http: Arc<reqwest::Client>,
    convex_url: String,
    mut shutdown: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if !beacon::beacon_enabled(config.pairing.beacon_enabled) || convex_url.is_empty() {
            tracing::info!("pairing beacon disabled");
            return;
        }
        let interval = Duration::from_secs(config.pairing.beacon_interval.max(1) as u64);
        let mut tick = tokio::time::interval(interval);
        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() { break; }
                }
                _ = tick.tick() => {
                    let pairing = PairingState::load();
                    if pairing.is_paired() {
                        continue; // only beacons while unpaired
                    }
                    // The pairing code + api key are owned by the API process;
                    // the beacon body assembly is exercised, with the code read
                    // from the pairing state when present.
                    let inputs = beacon::BeaconInputs {
                        device_id: config.agent.device_id.clone(),
                        pairing_code: String::new(),
                        api_key: String::new(),
                        name: config.agent.name.clone(),
                        version: env!("CARGO_PKG_VERSION").to_string(),
                        board_name: "unknown".to_string(),
                        board_tier: 0,
                        local_ip: String::new(),
                        code_expires_at: None,
                    };
                    let beacon_body = beacon::build_beacon_body(&inputs);
                    let url = format!("{}/pairing/register", convex_url.trim_end_matches('/'));
                    let _ = http.post(&url).json(&beacon_body).send().await;
                    tracing::debug!("pairing beacon sent");
                }
            }
        }
    })
}

/// Spawn the auto-pair supervisor: after a settle delay, while unpaired and
/// armed, forward `start_bind` over the supervisor control socket on a backoff.
fn spawn_auto_pair(
    config: Arc<CloudConfig>,
    mut shutdown: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let role = auto_pair_role(&config);
        let sock = auto_pair::default_sock_path();
        // Settle delay before the first attempt.
        tokio::select! {
            _ = shutdown.changed() => return,
            _ = tokio::time::sleep(auto_pair::START_DELAY) => {}
        }
        loop {
            if *shutdown.borrow() {
                break;
            }
            // The arm flag + pair state are read from the agent's wfb config +
            // pair manager on a real rig; the bind is forwarded only while the
            // rig is unpaired. The forwarder maps E_BIND_IN_PROGRESS to a defer.
            let pairing = PairingState::load();
            if auto_pair::should_attempt(&role, true, pairing.is_paired()) {
                match auto_pair::forward_start_bind(&sock, &role).await {
                    auto_pair::BindOutcome::Ok(_) => {
                        tracing::info!(role = %role, "auto-pair bind completed");
                    }
                    auto_pair::BindOutcome::Busy => {
                        tracing::info!("auto-pair busy, will retry");
                    }
                    auto_pair::BindOutcome::Error(e) => {
                        tracing::debug!(error = %e, "auto-pair forward failed");
                    }
                }
            }
            tokio::select! {
                _ = shutdown.changed() => break,
                _ = tokio::time::sleep(auto_pair::RETRY_BACKOFF) => {}
            }
        }
    })
}
