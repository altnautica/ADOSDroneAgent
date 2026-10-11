//! `ados-cloud` daemon.
//!
//! The runnable cloud relay. Wires the relay tasks into one tokio runtime:
//! the MAVLink-over-MQTT relay, the WebRTC
//! signaling relay, the heartbeat / command-poll loops, and the WFB auto-pair
//! supervisor. The pairing beacon is hosted in the API process (it owns the
//! pairing code + api key + claim). Modeled on `ados-supervisor/src/main.rs`:
//! journald logging on Linux with an fmt fallback, sd-notify readiness, and a
//! single select over the shutdown signals.
//!
//! Each task gates on the paired state (re-read per tick from
//! `/etc/ados/pairing.json`) and the effective convex URL (empty when
//! `server.mode == "local"`, which keeps a LAN-only agent off the cloud relay).

mod commands;
mod supervise;

use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::watch;

use std::sync::Mutex;

use ados_plugin_host::{Paths, PluginSupervisor};

use crate::config::CloudConfig;
use crate::ground_station::{bridge as gs_bridge, CloudRelayBridge};
use crate::loops::{aux_status, beacon, command_poll, enrichment, heartbeat};
use crate::mqtt::{run_webrtc_signaling, MavlinkMqttRelay, MspMqttRelay};
use crate::{dispatch, pairing::PairingState, plugin_publish, plugin_update, relay_lifecycle};
use ados_plugin_host::download::DownloadSource;

use commands::*;
use supervise::*;

/// Run the cloud relay daemon to completion. A startup error is printed the
/// same way a `main` returning `Err` prints it, with exit code 1.
pub fn run() -> std::process::ExitCode {
    match run_async() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Error: {e:?}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// The shared, single-instance plugin supervisor handle. Its lifecycle methods
/// are synchronous and take `&mut self` (filesystem + `systemctl`), so a `std`
/// mutex held inside a blocking task is the right fit — the install download +
/// archive unpack never runs on the async reactor.
pub(super) type SharedSupervisor = Arc<Mutex<PluginSupervisor>>;

/// A `Send + Sync` download seam handle, shared into the blocking install task.
pub(super) type SharedDownload = Arc<dyn DownloadSource>;

pub(super) fn init_logging() {
    use ados_protocol::logd::layer::LogdLayer;
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::EnvFilter;

    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string());

    // The logd layer ships records to the logging daemon's ingest socket
    // alongside the primary sink; it is best-effort and never blocks the service.
    #[cfg(target_os = "linux")]
    {
        if let Ok(journald) = tracing_journald::layer() {
            let _ = tracing_subscriber::registry()
                .with(EnvFilter::new(&filter))
                .with(journald)
                .with(LogdLayer::new("ados-cloud"))
                .try_init();
            return;
        }
    }

    let _ = tracing_subscriber::registry()
        .with(EnvFilter::new(&filter))
        .with(tracing_subscriber::fmt::layer())
        .with(LogdLayer::new("ados-cloud"))
        .try_init();
}

/// systemd readiness ping. No-op off Linux and when not run under a
/// `Type=notify` unit (`NOTIFY_SOCKET` unset).
#[cfg(target_os = "linux")]
pub(super) fn sd_ready() {
    if let Err(e) = sd_notify::notify(&[sd_notify::NotifyState::Ready]) {
        tracing::debug!(error = %e, "sd_notify READY failed");
    }
}

#[cfg(not(target_os = "linux"))]
pub(super) fn sd_ready() {}

/// The auto-pair bind role (`drone` | `gs`), read from the relay's one
/// profile discrimination ([`CloudConfig::wire_profile`]).
pub(super) fn auto_pair_role(config: &CloudConfig) -> String {
    if config.wire_profile() == "ground-station" {
        "gs".to_string()
    } else {
        "drone".to_string()
    }
}

/// Whether a per-tick relay POST may fire: the agent must be paired (have an api
/// key) AND have a live cloud URL (non-empty, i.e. an explicit cloud posture).
/// The single gate shared by the heartbeat + command-poll loops, so a LAN-only /
/// unpaired agent stays off the cloud relay.
pub(super) fn should_emit(api_key: Option<&str>, convex_url: &str) -> bool {
    api_key.is_some() && !convex_url.is_empty()
}

#[tokio::main]
async fn run_async() -> Result<()> {
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

    // One plugin supervisor for the whole process. The cloud command poll drives
    // its lifecycle ops (install / enable / disable / uninstall / configure) for
    // a remotely-relayed GCS that cannot reach the agent directly. `discover`
    // loads the on-disk install state; a failure here is non-fatal (a fresh box
    // simply has no installs), the supervisor still serves new installs.
    let supervisor: SharedSupervisor = {
        // The cloud install path is live (the command poll drives install_archive
        // for a remotely-relayed GCS), so signature enforcement MUST be on:
        // `production()` bakes require_signed=true by default. The grant path
        // refuses any capability the default Rust host cannot back so a remote
        // operator never grants a capability that can only error.
        //
        // The board id and tier come from the same HAL sidecar the heartbeat
        // base reads. Without them this path applied neither the
        // `supported_boards` gate nor the `min_tier` floor, so a plugin refused
        // over LAN installed cleanly when pushed through the cloud command
        // queue and then crash-looped on hardware it was never built for.
        let (board_id, tier) = PluginSupervisor::board_identity(Path::new(BOARD_SIDECAR));
        let mut sup =
            PluginSupervisor::production(Paths::from_env(), board_id, env!("CARGO_PKG_VERSION"))
                .with_profile(ados_config::node_profile())
                .with_board_tier(tier)
                .with_ungrantable_caps(ados_plugin_host::realhost::RealHost::ungrantable_caps());
        if let Err(e) = sup.discover() {
            tracing::warn!(error = %e, "plugin supervisor discover failed; continuing");
        }
        Arc::new(Mutex::new(sup))
    };

    // The HTTPS client for the heartbeat / command-poll loops, on the
    // shared pure-Rust rustls path.
    let http = reqwest::Client::builder()
        .use_preconfigured_tls(crate::tls::client_config())
        .timeout(Duration::from_secs(10))
        .build()
        .expect("reqwest client builds with the rustls config");
    let http = Arc::new(http);

    // The ground-station relay's forwarding state, published in-process by the
    // GS bridge and folded by the heartbeat loop. There is exactly ONE
    // `/agent/status` producer per node: the bridge used to POST its own 30 s
    // document alongside the heartbeat's 5 s one, and two producers on one row
    // is not a merge — each tick's absences overwrote the other's readings, so
    // the row alternated between a full board/radio/service shape and the small
    // relay one. Stays `None` forever on a drone, so the drone payload is
    // unchanged.
    let (relay_state_tx, relay_state_rx) = watch::channel::<Option<gs_bridge::GsHeartbeat>>(None);
    // The drone relay's confirmed broker-session flag, published once its
    // transport is dialed and cleared when the relay returns, so the
    // heartbeat's cloud-link sidecar reports the session, not the task.
    let (drone_link_tx, drone_link_rx) = watch::channel::<Option<Arc<AtomicBool>>>(None);

    // Spawn the relay tasks into one runtime. Each gates on the paired state
    // and the effective convex URL; the auto-pair supervisor is hosted here for
    // the no-self-kill invariant.
    let tasks: Vec<tokio::task::JoinHandle<()>> = vec![
        // ── Heartbeat loop ─────────────────────────────────────
        spawn_heartbeat(
            config.clone(),
            http.clone(),
            convex_url.clone(),
            relay_state_rx,
            drone_link_rx,
            shutdown_rx.clone(),
        ),
        // ── Command-poll loop ──────────────────────────────────
        spawn_command_poll(
            config.clone(),
            http.clone(),
            convex_url.clone(),
            supervisor.clone(),
            shutdown_rx.clone(),
        ),
        // ── Pairing beacon ─────────────────────────────────────
        // While unpaired and only when the beacon is enabled + a cloud URL is
        // set, register the pairing code with the cloud so a remote GCS can claim
        // by code. On a claim, the local API process owns the paired transition
        // (over loopback). Default-gated: a LAN-only agent never beacons.
        spawn_beacon(
            config.clone(),
            http.clone(),
            convex_url.clone(),
            shutdown_rx.clone(),
        ),
        // ── Log-window push watcher ────────────────────────────
        // Watches for an operator-triggered request to export a chosen log
        // window to the paired cloud account. Default-off, account-gated, and
        // re-checks the pair state + the operator opt-in per request, so an
        // unpaired / local-only agent never exports anything.
        crate::spawn_log_push_watcher(
            config.clone(),
            http.clone(),
            convex_url.clone(),
            shutdown_rx.clone(),
        ),
        // ── Auxiliary-lane status publisher ────────────────────
        // Push this node's compact status + identity over the radio's auxiliary
        // lane, so a ground station relaying it can describe it to an operator
        // who is paired only to the ground station. The radio link carries no
        // IP, so the node cannot be asked; it states. Rate-limited, bounded, and
        // silent once the lane is refused, so it can never disturb video.
        tokio::spawn(aux_status::run(config.clone(), shutdown_rx.clone())),
        // ── Plugin auto-update ─────────────────────────────────
        // The daily registry check over the installed plugins: silent patch or
        // minor updates through the signed-archive install, an
        // `update_available` notice for anything the operator must decide.
        // Skips every cycle while unpaired or with no cloud URL.
        spawn_plugin_auto_update(
            config.clone(),
            convex_url.clone(),
            supervisor.clone(),
            shutdown_rx.clone(),
        ),
        // ── Plugin revocation list ─────────────────────────────
        // Daily, alongside the update cycle: fetch the registry's revocation
        // list and merge it add-only into the local one the plugin host
        // enforces. A failure is logged and retried at the next cycle.
        tokio::spawn(crate::plugin_revocations::run_daily_loop(
            convex_url.clone(),
            std::path::PathBuf::from(crate::plugin_revocations::LOCAL_REVOCATIONS),
            shutdown_rx.clone(),
        )),
        // ── Plugin cloud publish ───────────────────────────────
        // Serve the root-only cloud-publish socket the plugin host forwards
        // gated `cloud.publish` / `cloud.records.put` calls to: stream messages
        // onto the broker at QoS 0 through a bounded drop-oldest queue, records
        // into the cloud with this device's key. Bound on every posture, so a
        // local-only or unpaired node answers with the reason.
        tokio::spawn(plugin_publish::run(
            config.clone(),
            http.clone(),
            convex_url.clone(),
            shutdown_rx.clone(),
        )),
    ];

    // Relay supervision. The MAVLink-over-MQTT relay runs a real
    // connect/restart loop in the same runtime, gated on the paired state + a
    // live cloud URL. On a ground station the uplink-aware bridge owns the relay
    // lifecycle (explicit teardown/reconnect on every uplink change + data-cap
    // downshift + the 30 s relay-state republish the heartbeat folds); on a
    // drone a thin supervisor keeps the relay connected and restarts it on exit
    // with a backoff. Both gate on the paired state so a LAN-only / unpaired
    // agent stays off the cloud relay.
    let mut tasks = tasks;
    if convex_url.is_empty() {
        tracing::info!("relay supervision idle (local mode, no cloud url)");
    } else if auto_pair_role(&config) == "gs" {
        tasks.push(spawn_gs_bridge(
            config.clone(),
            relay_state_tx,
            shutdown_rx.clone(),
        ));
    } else {
        tasks.push(spawn_drone_relay(
            config.clone(),
            drone_link_tx,
            shutdown_rx.clone(),
        ));
    }

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

/// Spawn the plugin auto-update engine and its notice publisher. Returns the
/// engine's handle; the publisher ends with the same shutdown signal.
pub(super) fn spawn_plugin_auto_update(
    config: Arc<CloudConfig>,
    convex_url: String,
    supervisor: SharedSupervisor,
    shutdown: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    let (source, notices) = plugin_update::CloudUpdateSource::new(convex_url);
    tokio::spawn(plugin_update::run_notice_publisher(
        config,
        notices,
        shutdown.clone(),
    ));
    let (board, _tier) = PluginSupervisor::board_identity(Path::new(BOARD_SIDECAR));
    // Unattended updates stop and restart plugin services, so they wait while
    // the vehicle is (or cannot be shown not to be) armed. Only a node that
    // carries or links a vehicle reads the state socket.
    let profile = ados_config::node_profile();
    let vehicle_profile = matches!(
        profile.as_str(),
        "drone" | "ground_station" | "ground-station"
    );
    let probe: Arc<dyn ados_protocol::armed::ArmedProbe> =
        ados_protocol::armed::StateSocketProbe::spawn(
            std::path::PathBuf::from(gs_bridge::STATE_SOCK),
            vehicle_profile,
        );
    tokio::spawn(ados_plugin_host::auto_update::run_daily_loop(
        supervisor,
        Arc::new(source),
        board,
        probe,
        shutdown,
    ))
}

#[cfg(test)]
mod tests;
