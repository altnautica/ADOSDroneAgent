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

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::watch;

use std::sync::Mutex;

use ados_plugin_host::{Paths, PluginSupervisor};

use ados_cloud::config::CloudConfig;
use ados_cloud::ground_station::{bridge as gs_bridge, CloudRelayBridge};
use ados_cloud::loops::{aux_status, beacon, command_poll, enrichment, heartbeat};
use ados_cloud::mqtt::{run_webrtc_signaling, MavlinkMqttRelay, MspMqttRelay};
use ados_cloud::{dispatch, pairing::PairingState, plugin_publish, plugin_update};
use ados_plugin_host::download::DownloadSource;

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

/// The auto-pair bind role (`drone` | `gs`), read from the relay's one
/// profile discrimination ([`CloudConfig::wire_profile`]).
fn auto_pair_role(config: &CloudConfig) -> String {
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
fn should_emit(api_key: Option<&str>, convex_url: &str) -> bool {
    api_key.is_some() && !convex_url.is_empty()
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
        let (board_name, board_tier, ..) = board_base();
        let board_id = (board_name != "unknown").then_some(board_name);
        let tier = (1..=4).contains(&board_tier).then_some(board_tier as u8);
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
        .use_preconfigured_tls(ados_cloud::tls::client_config())
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
        ados_cloud::spawn_log_push_watcher(
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
fn spawn_plugin_auto_update(
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
    let (board_name, ..) = board_base();
    let board = (board_name != "unknown").then_some(board_name);
    tokio::spawn(ados_plugin_host::auto_update::run_daily_loop(
        supervisor,
        Arc::new(source),
        board,
        shutdown,
    ))
}

/// The HAL board sidecar the API process persists once per boot. Read here so the
/// native base carries the real board identity (name/tier/soc/arch) instead of
/// "unknown", even before the enrichment loop's first write.
const BOARD_SIDECAR: &str = "/run/ados/board.json";

/// The schema version this build reads for the board sidecar. Must match the
/// `board` entry in the shared contract registry (asserted by a test).
const BOARD_SIDECAR_VERSION: u16 = 1;

/// Board identity for the heartbeat base, read from [`BOARD_SIDECAR`]. Falls back
/// to "unknown"/0/"" when the file is absent or malformed — the same degraded
/// shape the loop emitted before, but truthful whenever the board has been
/// detected (the normal case once the API service has served one status).
fn board_base() -> (String, i64, String, String, f64, bool) {
    let parsed: Option<serde_json::Value> = std::fs::read_to_string(BOARD_SIDECAR)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok());
    // Best-effort schema-drift signal: an older/foreign board file with no
    // `version` key reads back as 0 and warns, but is still used. Never a reject.
    if let Some(v) = parsed.as_ref() {
        let got = v
            .get("version")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as u16;
        ados_protocol::sidecar::check_sidecar_version("board", got, BOARD_SIDECAR_VERSION);
    }
    let obj = parsed.as_ref().and_then(|v| v.as_object());
    let s = |k: &str| {
        obj.and_then(|o| o.get(k))
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    let tier = obj
        .and_then(|o| o.get("tier"))
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(0);
    let npu_tops = obj
        .and_then(|o| o.get("npu_tops"))
        .and_then(serde_json::Value::as_f64)
        .unwrap_or(0.0);
    // The board profile's CPU-ONNX local-inference declaration; absent on an
    // older sidecar ⇒ false (the perception tier is unchanged there).
    let local_inference = obj
        .and_then(|o| o.get("has_local_inference"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    (
        s("name").unwrap_or_else(|| "unknown".to_string()),
        tier,
        s("soc").unwrap_or_default(),
        s("arch").unwrap_or_default(),
        npu_tops,
        local_inference,
    )
}

/// Spawn the heartbeat loop: when paired, POST the enriched payload every 5 s.
///
/// This is the node's ONLY `/agent/status` producer. On a ground station it
/// folds the relay state the GS bridge publishes on `relay_state` (uplink,
/// broker session, throttle, forwarding flags, vehicle telemetry) onto the same
/// document, so the relay block and the board/radio/service enrichment reach
/// the row together instead of two producers overwriting each other's absences.
fn spawn_heartbeat(
    config: Arc<CloudConfig>,
    http: Arc<reqwest::Client>,
    convex_url: String,
    relay_state: watch::Receiver<Option<gs_bridge::GsHeartbeat>>,
    drone_link: watch::Receiver<Option<Arc<AtomicBool>>>,
    mut shutdown: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    let started = std::time::Instant::now();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(heartbeat::HEARTBEAT_INTERVAL);
        // The previous tick's /proc/stat sample, kept across ticks so the native
        // enrichment producer reports a true inter-tick CPU delta (omitted on the
        // first tick, which has no prior sample to delta against).
        let mut prev_cpu: Option<enrichment::CpuSample> = None;
        let mut link = heartbeat::CloudLinkTracker::default();
        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() { break; }
                }
                _ = tick.tick() => {
                    let pairing = PairingState::load();
                    let api_key = pairing.api_key();
                    if !should_emit(api_key, &convex_url) {
                        write_cloud_link(&link, api_key.is_some(), &convex_url, None);
                        continue;
                    }
                    let api_key = api_key.expect("should_emit gates on api_key being Some");
                    let (
                        board_name,
                        board_tier,
                        board_soc,
                        board_arch,
                        board_npu_tops,
                        board_local_inference,
                    ) = board_base();
                    // The profile in WIRE form. Posting the internal form told
                    // the cloud `ground_station` or `auto`, neither of which the
                    // fleet view classifies.
                    let base = heartbeat::HeartbeatBase {
                        device_id: config.agent.device_id.clone(),
                        version: env!("CARGO_PKG_VERSION").to_string(),
                        profile: Some(config.wire_profile()),
                        role: None,
                        uptime_seconds: started.elapsed().as_secs() as i64,
                        board_name,
                        board_tier,
                        board_soc,
                        board_arch,
                        board_npu_tops,
                        board_local_inference,
                    };
                    // Live status (resources + FC link + service fleet) built in
                    // Rust from the real sources each tick, folded over the base.
                    // A blocking call (reads /proc, the state socket, shells
                    // systemctl) — keep it off the async reactor on the blocking
                    // pool, carrying the CPU sample in and back out.
                    let (enrich, next_cpu) = tokio::task::spawn_blocking(move || {
                        let mut cpu = prev_cpu;
                        let enrich = enrichment::build_native_enrichment(&mut cpu);
                        (enrich, cpu)
                    })
                    .await
                    .unwrap_or((serde_json::Value::Null, prev_cpu));
                    prev_cpu = next_cpu;
                    let mut body = heartbeat::build_payload(&base, Some(&enrich));
                    // Fold the ground-station relay block, when the bridge has
                    // published one. Absent on a drone, so the drone payload is
                    // byte-unchanged. The bridge owns `profile`/`role` for a GS
                    // (it reads the live mesh role, which this loop does not),
                    // and `deviceId`/`version`/`uptimeSeconds` were already
                    // re-asserted from the base by `build_payload`, so the fold
                    // cannot divert the row's identity.
                    fold_relay_state(&mut body, relay_state.borrow().as_ref());
                    let outcome = heartbeat::post_heartbeat(&http, &convex_url, api_key, &body).await;
                    link.record(&outcome, now_epoch_ms());
                    // A GS reports its bridge's session; a drone its relay's.
                    let broker = relay_state
                        .borrow()
                        .as_ref()
                        .map(|r| r.mqtt_connected)
                        .or_else(|| {
                            drone_link
                                .borrow()
                                .as_ref()
                                .map(|f| f.load(std::sync::atomic::Ordering::Acquire))
                        });
                    write_cloud_link(&link, true, &convex_url, broker);
                }
            }
        }
    })
}

/// Rewrite the cloud-link sidecar for this tick. Best-effort: a tmpfs write
/// failure is logged and the next tick tries again.
fn write_cloud_link(
    link: &heartbeat::CloudLinkTracker,
    paired: bool,
    convex_url: &str,
    broker_connected: Option<bool>,
) {
    let record = link.link(
        paired,
        !convex_url.is_empty(),
        broker_connected,
        now_epoch_ms(),
    );
    if let Err(e) = ados_protocol::cloud_link::write_cloud_link(&record) {
        tracing::debug!(error = %e, "cloud_link_sidecar_write_failed");
    }
}

/// Fold the ground-station relay block onto the heartbeat body in place.
///
/// `None` (a drone, or a GS bridge that has not published yet) leaves the body
/// untouched, so the drone payload is byte-unchanged. The relay slice's keys are
/// already the camelCase wire names the status mutation declares, so they are
/// copied over verbatim — except the three identity keys, which the heartbeat
/// base owns and which a folded producer must never be able to divert.
fn fold_relay_state(body: &mut serde_json::Value, relay: Option<&gs_bridge::GsHeartbeat>) {
    const BASE_OWNED: [&str; 3] = ["deviceId", "version", "uptimeSeconds"];
    let Some(relay) = relay else { return };
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    let Ok(serde_json::Value::Object(slice)) = serde_json::to_value(relay) else {
        return;
    };
    for (k, v) in slice {
        if BASE_OWNED.contains(&k.as_str()) {
            continue;
        }
        obj.insert(k, v);
    }
}

/// Epoch milliseconds from the system clock.
fn now_epoch_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The local API base the beacon posts the paired transition to over loopback.
/// The API process owns `pairing.json` writes (its `PairingManager.claim` uses
/// the same `pending_api_key` the beacon registered), so the relay never writes
/// the pairing file itself.
const LOCAL_API_BASE: &str = dispatch::loopback::LOCAL_API_BASE;

/// Spawn the pairing beacon: while UNPAIRED and only when the beacon is enabled
/// and a cloud URL is set, POST the pairing code to `{convex}/pairing/register`
/// every `beacon_interval` seconds. On a claim, persist the paired transition by
/// asking the local API process to claim (loopback `POST /api/pairing/claim`),
/// which writes `pairing.json` with the same `pending_api_key` the beacon
/// registered — so the cloud-frozen key matches the persisted key and no
/// heartbeat 401s after the claim. Best-effort throughout: a missing code, an
/// empty cloud response, or a loopback failure simply means the next tick
/// retries while still unpaired.
fn spawn_beacon(
    config: Arc<CloudConfig>,
    http: Arc<reqwest::Client>,
    convex_url: String,
    mut shutdown: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    let interval = Duration::from_secs(config.pairing.beacon_interval.max(1) as u64);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() { break; }
                }
                _ = tick.tick() => {
                    // Gate: the beacon runs only when enabled, a cloud URL is set,
                    // and the agent is still unpaired. Each re-checked per tick so
                    // a pair transition (or an operator toggle) stops it.
                    if !beacon::beacon_enabled(config.pairing.beacon_enabled)
                        || convex_url.is_empty()
                    {
                        continue;
                    }
                    let pairing = PairingState::load();
                    if pairing.is_paired() {
                        continue;
                    }
                    // The code + the stable pending key must both be present; skip
                    // the POST otherwise (the Convex handler 400s on an empty
                    // code, and an empty key would break the later claim).
                    let (Some(code), Some(api_key)) =
                        (pairing.pairing_code(), pairing.pending_api_key())
                    else {
                        continue;
                    };
                    beacon_register_once(
                        &http,
                        &convex_url,
                        &config,
                        code,
                        api_key,
                        pairing.code_expires_at_ms(),
                    )
                    .await;
                }
            }
        }
    })
}

/// One beacon registration pass: build + POST the `/pairing/register` body, and
/// on a claimed response, drive the local claim over loopback. Best-effort.
async fn beacon_register_once(
    http: &reqwest::Client,
    convex_url: &str,
    config: &CloudConfig,
    code: &str,
    api_key: &str,
    code_expires_at: Option<i64>,
) {
    let (board_name, board_tier, _soc, _arch, _npu, _local_inf) = board_base();
    let inputs = beacon::BeaconInputs {
        device_id: config.agent.device_id.clone(),
        pairing_code: code.to_string(),
        api_key: api_key.to_string(),
        name: config.agent.name.clone(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        board_name,
        board_tier,
        local_ip: String::new(),
        code_expires_at,
    };
    let body = beacon::build_beacon_body(&inputs);
    let url = format!("{}/pairing/register", convex_url.trim_end_matches('/'));
    let resp = match http.post(&url).json(&body).send().await {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => {
            tracing::debug!(status = r.status().as_u16(), "pairing beacon rejected");
            return;
        }
        Err(e) => {
            tracing::debug!(error = %e, "pairing beacon failed");
            return;
        }
    };
    let reply: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(_) => return,
    };
    if !beacon::response_claimed(&reply) {
        return;
    }
    // The cloud auto-matched / already-claimed this code. Persist the paired
    // transition through the API process so it owns the pairing.json write (it
    // claims with the same pending_api_key we just registered).
    let owner = reply
        .get("userId")
        .and_then(|v| v.as_str())
        .or_else(|| reply.get("ownerId").and_then(|v| v.as_str()))
        .unwrap_or("cloud")
        .to_string();
    let claim_url = format!("{LOCAL_API_BASE}/api/pairing/claim");
    match http
        .post(&claim_url)
        .json(&serde_json::json!({ "user_id": owner }))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => {
            tracing::info!(owner = %owner, "beacon_claimed");
        }
        Ok(r) => {
            // A non-2xx (e.g. a 409 already-paired race) is benign: the next tick
            // re-reads the pair state and stops beaconing once it is paired.
            tracing::debug!(status = r.status().as_u16(), "beacon local claim non-2xx");
        }
        Err(e) => {
            tracing::debug!(error = %e, "beacon local claim failed");
        }
    }
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
    let download: SharedDownload = Arc::new(ados_plugin_host::download::HttpDownloadSource::new());
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(command_poll::POLL_INTERVAL);
        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() { break; }
                }
                _ = tick.tick() => {
                    let pairing = PairingState::load();
                    let api_key = pairing.api_key();
                    if !should_emit(api_key, &convex_url) {
                        continue;
                    }
                    let api_key = api_key.expect("should_emit gates on api_key being Some");
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

        let seen = dispatch::seen_jobs::default_path();
        let result = dispatch_command(http, &name, &cmd, supervisor, download, &seen).await;

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
    seen: &std::path::Path,
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
        let seen = seen.to_path_buf();
        let outcome = tokio::task::spawn_blocking(move || {
            dispatch_plugin_blocking(&name, &cmd, &supervisor, download.as_ref(), &seen)
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
    seen: &std::path::Path,
) -> dispatch::CommandResult {
    use dispatch::{install, plugin_commands};

    let mut sup = match supervisor.lock() {
        Ok(g) => g,
        // A poisoned lock means a prior dispatch panicked mid-op; recover the
        // guard and continue rather than crash the relay.
        Err(poisoned) => poisoned.into_inner(),
    };
    if name == "plugin.install" {
        let install_cmd = install::InstallCommand::from_row(cmd);
        return install::handle_install(&mut sup, &install_cmd, download, seen);
    }
    match plugin_commands::PluginCommand::from_row(cmd) {
        Some(pc) => plugin_commands::dispatch(&mut sup, &pc, seen),
        None => dispatch::CommandResult::failed(format!("malformed plugin command: {name}")),
    }
}

/// Spawn the drone-side relay supervisor: while paired and a broker is
/// configured for this posture, keep the MAVLink relay, the MSP byte plane and
/// the WebRTC signaling lane connected over MQTT; on exit, restart them after a
/// short fixed delay. The relays own the bounded-queue + in-flight gate on the
/// hot publish path.
fn spawn_drone_relay(
    config: Arc<CloudConfig>,
    link: watch::Sender<Option<Arc<AtomicBool>>>,
    mut shutdown: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if *shutdown.borrow() {
                break;
            }
            let pairing = PairingState::load();
            // Unpaired, or a posture with no broker: nothing to relay; the loop
            // polls for a pair transition.
            if let Some(transport) = pairing
                .api_key()
                .and_then(|key| config.relay_transport(None, key))
            {
                let device_id = config.agent.device_id.clone();
                // The MSP byte plane and the WebRTC signaling lane run alongside
                // the MAVLink frame plane on the same broker, each on its own
                // ClientID. They share the loop's shutdown watch and are stopped
                // when the MAVLink relay returns so each iteration spawns a clean
                // set.
                let msp_relay = MspMqttRelay::new(device_id.clone(), transport.clone());
                let msp_shutdown = shutdown.clone();
                let msp_task = tokio::spawn(async move {
                    if let Err(e) = msp_relay.run(gs_bridge::MSP_SOCK, msp_shutdown).await {
                        tracing::warn!(error = %e, "drone msp relay exited");
                    }
                });
                let signaling_cfg = transport.clone();
                let signaling_id = device_id.clone();
                let signaling_shutdown = shutdown.clone();
                let signaling_task = tokio::spawn(async move {
                    // A drone has no data cap: every offer is served.
                    let video_allowed = Arc::new(AtomicBool::new(true));
                    if let Err(e) = run_webrtc_signaling(
                        &signaling_id,
                        &signaling_cfg,
                        video_allowed,
                        signaling_shutdown,
                    )
                    .await
                    {
                        tracing::warn!(error = %e, "drone webrtc signaling exited");
                    }
                });
                let relay = MavlinkMqttRelay::new(device_id, transport);
                tracing::info!("drone mavlink relay connecting");
                if let Err(e) = relay
                    .run_observed(gs_bridge::MAVLINK_SOCK, shutdown.clone(), Some(&link))
                    .await
                {
                    tracing::warn!(error = %e, "drone mavlink relay exited");
                }
                // The MAVLink relay returned (shutdown or exit): stop the sibling
                // lanes too so a fresh loop iteration spawns a clean set.
                msp_task.abort();
                signaling_task.abort();
                // The session ended with the relay; report no session.
                let _ = link.send(None);
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
/// the 30 s relay-state republish. Runs only while paired; re-checks the pair
/// state between bridge runs.
///
/// The bridge does NOT POST `/agent/status`: it publishes its relay state on
/// `relay_state_tx` and the heartbeat loop — the node's single producer — folds
/// it. Two producers on one row is not a merge; each tick's absences overwrote
/// the other's readings.
fn spawn_gs_bridge(
    config: Arc<CloudConfig>,
    relay_state_tx: watch::Sender<Option<gs_bridge::GsHeartbeat>>,
    mut shutdown: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // The live vehicle-state reader enriches the GS relay telemetry.
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
            if let Some(transport) = pairing
                .api_key()
                .and_then(|key| config.relay_transport(None, key))
            {
                let mut bridge = CloudRelayBridge::new(
                    config.agent.device_id.clone(),
                    pairing.owner_id.clone(),
                    transport,
                )
                .with_state_source(state_reader.clone())
                .with_relay_state_sink(relay_state_tx.clone());
                bridge.run(shutdown.clone()).await;
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
    use ados_cloud::dispatch::CommandStatus;
    use ados_plugin_host::download::{DownloadBody, DownloadError, DownloadSource};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// A no-network download source (never consulted on the no-plugin paths).
    struct NoSource;
    impl DownloadSource for NoSource {
        fn open(&self, _url: &str) -> Result<DownloadBody, DownloadError> {
            Err(DownloadError::Unparseable)
        }
    }

    fn no_source() -> SharedDownload {
        Arc::new(NoSource)
    }

    /// A per-test seen-jobs ring under a temp dir, never the node's real one.
    fn seen_path() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("_seen_jobs.json");
        (dir, path)
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
            control_dir: dir.join("plugin-host"),
            loopback_guard_state: dir.join("plugin-loopback-guard.json"),
            socket_dir: dir.join("sockets"),
            token_secret: dir.join("secrets/plugin-token-secret"),
            runner: dir.join("bin/ados-plugin-runner"),
            run_dir: dir.join("run"),
            data_root: dir.join("plugin-data"),
            device_id_file: dir.join("device-id"),
        };
        Arc::new(Mutex::new(PluginSupervisor::new(
            paths, false, None, "1.0.0",
        )))
    }

    #[test]
    fn board_sidecar_version_matches_registry() {
        assert_eq!(
            BOARD_SIDECAR_VERSION,
            ados_protocol::contracts::sidecar_version("board").unwrap()
        );
    }

    #[test]
    fn live_cloud_supervisor_enforces_signing() {
        // The cloud install path is wired live (the command poll drives
        // install_archive), so the process-wide supervisor MUST enforce
        // signatures. Build it exactly as `main` does and assert the secure
        // default holds with no env override.
        let prev = std::env::var("ADOS_PLUGIN_REQUIRE_SIGNED").ok();
        std::env::remove_var("ADOS_PLUGIN_REQUIRE_SIGNED");
        let dir = std::env::temp_dir().join(format!("ados-cloud-signed-{}", std::process::id()));
        let paths = Paths {
            install_dir: dir.join("plugins"),
            unit_dir: dir.join("units"),
            state_path: dir.join("state/plugin-state.json"),
            log_dir: dir.join("logs"),
            control_dir: dir.join("plugin-host"),
            loopback_guard_state: dir.join("plugin-loopback-guard.json"),
            socket_dir: dir.join("sockets"),
            token_secret: dir.join("secrets/plugin-token-secret"),
            runner: dir.join("bin/ados-plugin-runner"),
            run_dir: dir.join("run"),
            data_root: dir.join("plugin-data"),
            device_id_file: dir.join("device-id"),
        };
        let sup = PluginSupervisor::production(paths, None, env!("CARGO_PKG_VERSION"))
            .with_ungrantable_caps(ados_plugin_host::realhost::RealHost::ungrantable_caps());
        assert!(
            sup.require_signed(),
            "the live cloud install supervisor must require signed archives"
        );
        if let Some(v) = prev {
            std::env::set_var("ADOS_PLUGIN_REQUIRE_SIGNED", v);
        }
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

    /// Serve exactly one HTTP request on loopback, answering `body` as JSON,
    /// and hand back the raw request head the client sent.
    async fn capture_one_request(
        body: &'static str,
    ) -> (String, tokio::sync::oneshot::Receiver<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut head = Vec::new();
            let mut buf = [0u8; 4096];
            while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = sock.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                head.extend_from_slice(&buf[..n]);
            }
            let _ = tx.send(String::from_utf8_lossy(&head).into_owned());
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        });
        (format!("http://{addr}"), rx)
    }

    #[tokio::test]
    async fn the_command_poll_sends_the_key_as_a_header_and_only_the_device_id_as_a_query() {
        let (base, head) = capture_one_request(r#"{"commands":[]}"#).await;
        let http = test_client();
        poll_commands_once(
            &http,
            &base,
            "k-secret",
            "dev-7",
            &supervisor(),
            &no_source(),
        )
        .await;
        let head = head.await.unwrap();
        let request_line = head.lines().next().unwrap();
        assert!(
            request_line.starts_with("GET /agent/commands?deviceId=dev-7 "),
            "{request_line}"
        );
        assert!(
            !request_line.contains("k-secret"),
            "the key leaked into the URL"
        );
        assert!(
            head.to_ascii_lowercase().contains("x-ados-key: k-secret"),
            "{head}"
        );
    }

    #[tokio::test]
    async fn unknown_command_acks_failed_not_completed() {
        // The catch-all must never fabricate success for a command with no
        // handler. No HTTP is issued on this path (route_for returns None).
        let http = test_client();
        let sup = supervisor();
        let (_dir, seen) = seen_path();
        let cmd = serde_json::json!({"_id": "c1", "command": "totally_unknown"});
        let r = dispatch_command(&http, "totally_unknown", &cmd, &sup, &no_source(), &seen).await;
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
        let (_dir, seen) = seen_path();
        let cmd = serde_json::json!({"_id": "c2", "command": "restart_service", "args": {}});
        let r = dispatch_command(&http, "restart_service", &cmd, &sup, &no_source(), &seen).await;
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
        let (_dir, seen) = seen_path();
        let cmd = serde_json::json!({
            "_id": "c3",
            "command": "plugin.enable",
            "args": {"pluginId": "com.example.never-installed", "jobId": "j-unknown"}
        });
        let r = dispatch_command(&http, "plugin.enable", &cmd, &sup, &no_source(), &seen).await;
        assert_eq!(r.status, CommandStatus::Failed);
    }

    #[test]
    fn should_emit_requires_both_an_api_key_and_a_cloud_url() {
        // Paired but no cloud URL (local mode) → off.
        assert!(!should_emit(Some("k"), ""));
        // Cloud URL but unpaired → off.
        assert!(!should_emit(None, "https://relay.example/convex"));
        // Paired AND a live cloud URL → on.
        assert!(should_emit(Some("k"), "https://relay.example/convex"));
    }

    fn config_with_profile(profile: &str) -> CloudConfig {
        let mut config = CloudConfig::default();
        config.agent.profile = profile.to_string();
        config
    }

    #[test]
    fn the_advertised_profile_is_the_wire_form_and_agrees_with_the_bind_role() {
        // The internal form reached the cloud verbatim, so a default install
        // advertised `auto` and a ground station advertised `ground_station` —
        // neither of which the fleet view classifies.
        assert_eq!(
            config_with_profile("ground_station").wire_profile(),
            "ground-station"
        );
        assert_eq!(
            config_with_profile("ground-station").wire_profile(),
            "ground-station"
        );
        assert_eq!(config_with_profile("drone").wire_profile(), "drone");
        assert_eq!(
            config_with_profile("workstation").wire_profile(),
            "workstation"
        );
        assert_eq!(config_with_profile("compute").wire_profile(), "compute");
        // `auto` / empty / unknown resolve to the drone form, which is also the
        // bind role's safe default — one discrimination, so the two agree.
        for raw in ["auto", "", "nonsense"] {
            assert_eq!(config_with_profile(raw).wire_profile(), "drone");
        }
        for raw in ["ground_station", "ground-station"] {
            assert_eq!(auto_pair_role(&config_with_profile(raw)), "gs");
        }
        for raw in ["drone", "auto", "", "workstation"] {
            assert_eq!(auto_pair_role(&config_with_profile(raw)), "drone");
        }
    }

    #[test]
    fn the_relay_block_folds_onto_the_one_heartbeat_without_diverting_identity() {
        // The GS relay state reaches the row through the single producer. It
        // may set the relay + profile/role columns, but never the identity the
        // heartbeat base owns — a folded producer that could rewrite `deviceId`
        // would file a ground station's status under another node.
        let mut body = serde_json::json!({
            "deviceId": "gs-1",
            "version": "9.9.9",
            "uptimeSeconds": 4242,
            "boardName": "rpi4b",
            "profile": "ground-station",
        });
        let relay = gs_bridge::GsHeartbeat {
            device_id: "impostor".to_string(),
            version: "0.0.0".to_string(),
            uptime_seconds: 1,
            profile: "ground-station".to_string(),
            role: Some("receiver".to_string()),
            uplink: "wlan0".to_string(),
            mqtt_connected: true,
            throttle_state: "warn_80".to_string(),
            forwarding_video: true,
            forwarding_telemetry: true,
            ts_ms: 1234,
            telemetry: Some(serde_json::json!({"armed": true})),
        };
        fold_relay_state(&mut body, Some(&relay));
        // The relay block landed, under the camelCase keys the mutation declares.
        assert_eq!(body["uplink"], "wlan0");
        assert_eq!(body["mqttConnected"], true);
        assert_eq!(body["throttleState"], "warn_80");
        assert_eq!(body["forwardingVideo"], true);
        assert_eq!(body["forwardingTelemetry"], true);
        assert_eq!(body["tsMs"], 1234);
        assert_eq!(body["role"], "receiver");
        assert_eq!(body["telemetry"]["armed"], true);
        // The board enrichment the other producer used to wipe survives.
        assert_eq!(body["boardName"], "rpi4b");
        // Identity is the base's, not the folded slice's.
        assert_eq!(body["deviceId"], "gs-1");
        assert_eq!(body["version"], "9.9.9");
        assert_eq!(body["uptimeSeconds"], 4242);
    }

    #[test]
    fn a_drone_heartbeat_is_untouched_by_the_relay_fold() {
        // No bridge published anything (a drone, or a GS before its first
        // republish): the payload must be byte-identical.
        let before = serde_json::json!({"deviceId": "d1", "boardName": "rock-5c-lite"});
        let mut body = before.clone();
        fold_relay_state(&mut body, None);
        assert_eq!(body, before);
    }
}
