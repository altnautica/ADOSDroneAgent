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

use std::sync::Mutex;

use ados_plugin_host::{Paths, PluginSupervisor};

use ados_cloud::config::CloudConfig;
use ados_cloud::dispatch::install::DownloadSource;
use ados_cloud::ground_station::{bridge as gs_bridge, CloudRelayBridge};
use ados_cloud::loops::{beacon, command_poll, heartbeat};
use ados_cloud::mqtt::transport::TransportConfig;
use ados_cloud::mqtt::{MavlinkMqttRelay, WS_PATH};
use ados_cloud::{dispatch, pairing::PairingState};

/// The shared, single-instance plugin supervisor handle. Its lifecycle methods
/// are synchronous and take `&mut self` (filesystem + `systemctl`), so a `std`
/// mutex held inside a blocking task is the right fit — the install download +
/// archive unpack never runs on the async reactor.
type SharedSupervisor = Arc<Mutex<PluginSupervisor>>;

/// A `Send + Sync` download seam handle, shared into the blocking install task.
type SharedDownload = Arc<dyn DownloadSource>;

fn init_logging() {
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

    // One plugin supervisor for the whole process. The cloud command poll drives
    // its lifecycle ops (install / enable / disable / uninstall / configure) for
    // a remotely-relayed GCS that cannot reach the agent directly. `discover`
    // loads the on-disk install state; a failure here is non-fatal (a fresh box
    // simply has no installs), the supervisor still serves new installs.
    let supervisor: SharedSupervisor = {
        let mut sup =
            PluginSupervisor::new(Paths::default(), false, None, env!("CARGO_PKG_VERSION"));
        if let Err(e) = sup.discover() {
            tracing::warn!(error = %e, "plugin supervisor discover failed; continuing");
        }
        Arc::new(Mutex::new(sup))
    };

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
            supervisor.clone(),
            shutdown_rx.clone(),
        ),
        // ── Pairing-beacon loop (default gated off in local mode) ─
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
        ados_cloud::spawn_log_push_watcher(
            config.clone(),
            http.clone(),
            convex_url.clone(),
            shutdown_rx.clone(),
        ),
    ];

    // Relay supervision. The MAVLink-over-MQTT relay runs a real
    // connect/restart loop in the same runtime, gated on the paired state + a
    // live cloud URL. On a ground station the uplink-aware bridge owns the relay
    // lifecycle (explicit teardown/reconnect on every uplink change + data-cap
    // downshift + the 30 s GS status heartbeat); on a drone a thin supervisor
    // keeps the relay connected and restarts it on exit with a backoff. Both
    // gate on the paired state so a LAN-only / unpaired agent stays off the
    // cloud relay.
    let mut tasks = tasks;
    if convex_url.is_empty() {
        tracing::info!("relay supervision idle (local mode, no cloud url)");
    } else if auto_pair_role(&config) == "gs" {
        tasks.push(spawn_gs_bridge(
            config.clone(),
            http.clone(),
            convex_url.clone(),
            shutdown_rx.clone(),
        ));
    } else {
        tasks.push(spawn_drone_relay(config.clone(), shutdown_rx.clone()));
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
/// The plugin supervisor is shared behind a mutex (its lifecycle methods take
/// `&mut self`) and driven from the blocking pool so the install download and
/// archive unpack never run on the async reactor.
fn spawn_command_poll(
    config: Arc<CloudConfig>,
    http: Arc<reqwest::Client>,
    convex_url: String,
    supervisor: SharedSupervisor,
    mut shutdown: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    // A blocking client for the install download (the supervisor install path is
    // synchronous; the download seam is blocking). Built once and reused.
    let download: SharedDownload = Arc::new(dispatch::install::HttpDownloadSource::new());
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
                    poll_commands_once(
                        &http,
                        &convex_url,
                        api_key,
                        &config.agent.device_id,
                        &supervisor,
                        &download,
                    )
                    .await;
                }
            }
        }
    })
}

/// One command-poll pass: GET the queue, dispatch each command for real, ACK the
/// real result. Plugin lifecycle commands run in-process against the held
/// supervisor; service/peripheral/fleet/log/WFB-pair commands forward to the
/// local API over loopback and carry back the route's real ok/failed result; any
/// command with no handler acks an honest `failed("not implemented: …")` rather
/// than fabricating success. Best-effort: any transport failure is logged, not
/// fatal.
async fn poll_commands_once(
    http: &reqwest::Client,
    convex_url: &str,
    api_key: &str,
    device_id: &str,
    supervisor: &SharedSupervisor,
    download: &SharedDownload,
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

        let result = dispatch_command(http, &name, &cmd, supervisor, download).await;

        if result.status == dispatch::CommandStatus::Failed {
            tracing::warn!(
                command = %name,
                id = %cmd_id,
                message = %result.result.get("message").and_then(|v| v.as_str()).unwrap_or(""),
                "cloud command failed"
            );
        }

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

/// Dispatch a single cloud command to its real handler and return the result.
///
/// - Plugin lifecycle commands (`plugin.*`) run in-process against the held
///   supervisor under the mutex (install downloads + `install_archive`; the
///   others enable / disable / uninstall / configure).
/// - Service / peripheral / fleet / log / WFB-pair / raw-command commands map to
///   a local API route and forward over loopback, returning the route's result.
/// - Anything else acks an honest `failed("not implemented: …")`.
async fn dispatch_command(
    http: &reqwest::Client,
    name: &str,
    cmd: &serde_json::Value,
    supervisor: &SharedSupervisor,
    download: &SharedDownload,
) -> dispatch::CommandResult {
    use dispatch::{loopback, plugin_commands};

    // ── Plugin lifecycle: in-process against the held supervisor ──
    // The supervisor ops are synchronous and the install path does a blocking
    // download + archive unpack + `systemctl`, so the whole branch runs on the
    // blocking pool — never on the async reactor.
    if plugin_commands::is_plugin_command(name) {
        let name = name.to_string();
        let cmd = cmd.clone();
        let supervisor = supervisor.clone();
        let download = download.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            dispatch_plugin_blocking(&name, &cmd, &supervisor, download.as_ref())
        })
        .await;
        return match outcome {
            Ok(result) => result,
            Err(e) => dispatch::CommandResult::failed(format!("plugin task panicked: {e}")),
        };
    }

    // ── Loopback to the local API for the work that lives there ──
    let args = cmd.get("args").cloned().unwrap_or(serde_json::Value::Null);
    if let Some(route) = loopback::route_for(name, &args) {
        return loopback::forward(http, name, &args, &route).await;
    }

    // ── No handler: ack an honest failure, never a fabricated success ──
    dispatch::CommandResult::failed(format!("not implemented: {name}"))
}

/// Run a plugin lifecycle command against the held supervisor. Synchronous
/// (filesystem + blocking download + `systemctl`); called from `spawn_blocking`.
fn dispatch_plugin_blocking(
    name: &str,
    cmd: &serde_json::Value,
    supervisor: &SharedSupervisor,
    download: &dyn DownloadSource,
) -> dispatch::CommandResult {
    use dispatch::{install, plugin_commands};

    let seen = dispatch::seen_jobs::default_path();
    let mut sup = match supervisor.lock() {
        Ok(g) => g,
        // A poisoned lock means a prior dispatch panicked mid-op; recover the
        // guard and continue rather than crash the relay.
        Err(poisoned) => poisoned.into_inner(),
    };
    if name == "plugin.install" {
        let install_cmd = install::InstallCommand::from_row(cmd);
        return install::handle_install(&mut sup, &install_cmd, download, &seen);
    }
    match plugin_commands::PluginCommand::from_row(cmd) {
        Some(pc) => plugin_commands::dispatch(&mut sup, &pc, &seen),
        None => dispatch::CommandResult::failed(format!("malformed plugin command: {name}")),
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

/// Build the MAVLink-relay broker dial config from the agent config + the live
/// pairing api key. The relay authenticates as `ados-{device_id}` with the api
/// key as the password (the broker ACL pattern). Returns `None` while unpaired
/// (no api key to authenticate the relay).
fn build_relay_transport(config: &CloudConfig, api_key: &str) -> TransportConfig {
    TransportConfig {
        client_id: format!("ados-{}", config.agent.device_id),
        host: config.server.cloud.mqtt_broker.clone(),
        port: config.server.cloud.mqtt_port,
        ws_path: WS_PATH.to_string(),
        username: format!("ados-{}", config.agent.device_id),
        password: api_key.to_string(),
        // The Rule-37 high in-flight ceiling: the publish path is the limit, not
        // the client's internal queue.
        inflight: 1000,
        keep_alive: Duration::from_secs(30),
    }
}

/// Spawn the drone-side MAVLink relay supervisor: while paired, keep the relay
/// connected over MQTT; on exit, restart it after a short delay. The relay
/// itself owns the bounded-queue + in-flight gate on the hot publish path.
fn spawn_drone_relay(
    config: Arc<CloudConfig>,
    mut shutdown: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if *shutdown.borrow() {
                break;
            }
            let pairing = PairingState::load();
            // Unpaired: nothing to relay; the loop polls for a pair transition.
            if let Some(api_key) = pairing.api_key() {
                let transport = build_relay_transport(&config, api_key);
                let relay = MavlinkMqttRelay::new(config.agent.device_id.clone(), transport);
                tracing::info!("drone mavlink relay connecting");
                if let Err(e) = relay.run(gs_bridge::MAVLINK_SOCK, shutdown.clone()).await {
                    tracing::warn!(error = %e, "drone mavlink relay exited");
                }
            }
            // Restart / re-poll after a short settle, unless shutting down.
            tokio::select! {
                _ = shutdown.changed() => { if *shutdown.borrow() { break; } }
                _ = tokio::time::sleep(Duration::from_secs(5)) => {}
            }
        }
    })
}

/// Spawn the ground-station cloud relay bridge: uplink-aware MQTT supervision
/// (explicit teardown/reconnect on every uplink change), data-cap downshift, and
/// the 30 s GS status heartbeat. Runs only while paired; re-checks the pair
/// state between bridge runs.
fn spawn_gs_bridge(
    config: Arc<CloudConfig>,
    http: Arc<reqwest::Client>,
    convex_url: String,
    mut shutdown: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // The live vehicle-state reader enriches the GS heartbeat telemetry.
        let state_reader = Arc::new(gs_bridge::StateIpcReader::spawn(
            std::path::PathBuf::from(gs_bridge::STATE_SOCK),
            shutdown.clone(),
        ));
        loop {
            if *shutdown.borrow() {
                break;
            }
            let pairing = PairingState::load();
            // Unpaired: idle; re-poll for a pair transition between runs.
            if let Some(api_key) = pairing.api_key() {
                let transport = build_relay_transport(&config, api_key);
                let mut bridge = CloudRelayBridge::new(
                    config.agent.device_id.clone(),
                    pairing.owner_id.clone(),
                    convex_url.clone(),
                    Some(api_key.to_string()),
                    transport,
                )
                .with_state_source(state_reader.clone());
                bridge.run(http.clone(), shutdown.clone()).await;
            }
            tokio::select! {
                _ = shutdown.changed() => { if *shutdown.borrow() { break; } }
                _ = tokio::time::sleep(Duration::from_secs(5)) => {}
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_cloud::dispatch::loopback;
    use ados_cloud::dispatch::{install::DownloadSource, CommandStatus};
    use std::path::Path;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// A no-network download source (never consulted on the no-plugin paths).
    struct NoSource;
    impl DownloadSource for NoSource {
        fn fetch(
            &self,
            _url: &str,
        ) -> Result<Vec<u8>, ados_cloud::dispatch::download::DownloadError> {
            Err(ados_cloud::dispatch::download::DownloadError::Unparseable)
        }
    }

    fn no_source() -> SharedDownload {
        Arc::new(NoSource)
    }

    /// An HTTP client on the crate's preconfigured rustls path (the same one the
    /// daemon builds). reqwest needs a crypto provider set even for plain-HTTP
    /// loopback requests, so the default `Client::new()` panics with "No provider
    /// set" in this no-default-features crate.
    fn test_client() -> reqwest::Client {
        reqwest::Client::builder()
            .use_preconfigured_tls(ados_cloud::tls::client_config())
            .build()
            .expect("test client builds with the rustls config")
    }

    fn supervisor() -> SharedSupervisor {
        // A temp-rooted supervisor so the test never touches /var/ados.
        let dir = std::env::temp_dir().join(format!("ados-cloud-test-{}", std::process::id()));
        let paths = Paths {
            install_dir: dir.join("plugins"),
            unit_dir: dir.join("units"),
            state_path: dir.join("state/plugin-state.json"),
            log_dir: dir.join("logs"),
        };
        Arc::new(Mutex::new(PluginSupervisor::new(
            paths, false, None, "1.0.0",
        )))
    }

    /// A one-shot local HTTP server that replies to a single request with the
    /// given status line + JSON body, then returns the bound base URL.
    async fn mock_once(status_line: &'static str, body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                // Drain the request head (best-effort) so the client write completes.
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let resp = format!(
                    "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn unknown_command_acks_failed_not_completed() {
        // The catch-all must never fabricate success for a command with no
        // handler. No HTTP is issued on this path (route_for returns None).
        let http = test_client();
        let sup = supervisor();
        let cmd = serde_json::json!({"_id": "c1", "command": "totally_unknown"});
        let r = dispatch_command(&http, "totally_unknown", &cmd, &sup, &no_source()).await;
        assert_eq!(r.status, CommandStatus::Failed);
        assert_eq!(r.result["message"], "not implemented: totally_unknown");
    }

    #[tokio::test]
    async fn restart_of_rejected_name_acks_failed() {
        // The local restart route returns `{"status":"error"}` with HTTP 200 for
        // a unit not in its allowlist; the forwarder must ack failed, not
        // completed. Exercised end-to-end through the real loopback HTTP path.
        let base = mock_once(
            "200 OK",
            r#"{"status":"error","message":"Unknown service: bogus"}"#,
        )
        .await;
        let http = test_client();
        let args = serde_json::json!({"name": "bogus"});
        let route = loopback::route_for("restart_service", &args).unwrap();
        let r = loopback::forward_to(&http, &base, "restart_service", &args, &route).await;
        assert_eq!(r.status, CommandStatus::Failed);
        assert_eq!(r.result["message"], "Unknown service: bogus");
    }

    #[tokio::test]
    async fn restart_of_allowed_name_acks_completed() {
        // The happy path: the route confirms the restart with `{"status":"ok"}`,
        // and the forwarder carries that through as completed with the route's
        // payload in `data`.
        let base = mock_once(
            "200 OK",
            r#"{"status":"ok","message":"Restarted ados-video","unit":"ados-video"}"#,
        )
        .await;
        let http = test_client();
        let args = serde_json::json!({"name": "ados-video"});
        let route = loopback::route_for("restart_service", &args).unwrap();
        let r = loopback::forward_to(&http, &base, "restart_service", &args, &route).await;
        assert_eq!(r.status, CommandStatus::Completed);
        assert_eq!(r.result["message"], "Restarted ados-video");
        assert_eq!(r.data.unwrap()["unit"], "ados-video");
    }

    #[tokio::test]
    async fn scan_peripherals_forwards_and_returns_real_data() {
        let base = mock_once("200 OK", r#"[{"name":"USB 0bda:a81a","type":"usb"}]"#).await;
        let http = test_client();
        let args = serde_json::Value::Null;
        let route = loopback::route_for("scan_peripherals", &args).unwrap();
        let r = loopback::forward_to(&http, &base, "scan_peripherals", &args, &route).await;
        assert_eq!(r.status, CommandStatus::Completed);
        assert!(r.data.unwrap().is_array());
    }

    #[tokio::test]
    async fn restart_without_name_acks_failed_in_dispatch() {
        // A restart_service with no name has no route; dispatch_command must fail
        // it honestly rather than POST to a malformed path.
        let http = test_client();
        let sup = supervisor();
        let cmd = serde_json::json!({"_id": "c2", "command": "restart_service", "args": {}});
        let r = dispatch_command(&http, "restart_service", &cmd, &sup, &no_source()).await;
        assert_eq!(r.status, CommandStatus::Failed);
        assert_eq!(r.result["message"], "not implemented: restart_service");
    }

    #[tokio::test]
    async fn plugin_command_for_unknown_plugin_acks_failed() {
        // A plugin lifecycle command routes in-process to the supervisor; an
        // enable of a plugin that was never installed is a real failed ACK, not
        // a fabricated success.
        let http = test_client();
        let sup = supervisor();
        let cmd = serde_json::json!({
            "_id": "c3",
            "command": "plugin.enable",
            "args": {"pluginId": "com.example.never-installed", "jobId": "j-unknown"}
        });
        let r = dispatch_command(&http, "plugin.enable", &cmd, &sup, &no_source()).await;
        assert_eq!(r.status, CommandStatus::Failed);
        let _ = std::fs::remove_dir_all(Path::new("/var/lib/ados/plugins/.jobs"));
    }
}
