//! `ados-mavlink-router` binary.
//!
//! Owns the FC serial link and serves the MAVLink + state IPC sockets plus the
//! direct-GCS TCP/UDP proxies. Mirrors the Python `ados-mavlink` service
//! (`python -m ados.services.mavlink`): the IPC servers, the FC connection, the
//! 1 Hz companion heartbeat, the 10 Hz state publish, the adaptive stream
//! cadence, and the parameter sweep. The state-socket encoding (v1 newline-JSON
//! vs v2 length-prefixed msgpack) is selected by `ADOS_STATE_IPC_MSGPACK`, the
//! same flag the Python producer honours.

use std::sync::Arc;
use std::time::{Duration, Instant};

use ados_protocol::frame::{encode_frame, MAVLINK_MAX_FRAME};
use ados_protocol::ipc::IpcBroadcast;
use ados_protocol::state::{encode_v1, encode_v2};
use serde_json::{json, Map, Value};
use tokio::sync::{Mutex, Notify};

use ados_mavlink_router::config::MavlinkConfig;
use ados_mavlink_router::connection::FcConnection;
use ados_mavlink_router::param_cache::ParamCache;
use ados_mavlink_router::proxies::{run_tcp_proxy, run_udp_proxy, run_ws_proxy, WsProxyAuth};
use ados_mavlink_router::state::VehicleState;

const MAVLINK_QUEUE_DEPTH: usize = 256;
const STATE_QUEUE_DEPTH: usize = 32;
const TCP_PROXY_PORT: u16 = 5760;
const UDP_PROXY_PORTS: &[u16] = &[14550, 14551];

fn run_dir() -> String {
    std::env::var("ADOS_RUN_DIR").unwrap_or_else(|_| "/run/ados".to_string())
}

fn use_msgpack() -> bool {
    std::env::var("ADOS_STATE_IPC_MSGPACK").ok().as_deref() == Some("1")
}

/// Demo mode: drive synthetic telemetry instead of opening a serial FC. Enabled
/// by the `--demo` argument or `ADOS_MAVLINK_DEMO=1`. Off by default, so the
/// production unit (no argument, no env) keeps the serial path.
fn demo_enabled() -> bool {
    std::env::args().any(|a| a == "--demo")
        || std::env::var("ADOS_MAVLINK_DEMO").ok().as_deref() == Some("1")
}

/// TCP proxy bind port. Overridable via `ADOS_MAVLINK_TCP_PORT` (the parity
/// harness uses this to run a second instance without a port clash); defaults to
/// the standard port.
fn tcp_proxy_port() -> u16 {
    std::env::var("ADOS_MAVLINK_TCP_PORT")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(TCP_PROXY_PORT)
}

/// UDP proxy bind ports (comma-separated). Overridable via
/// `ADOS_MAVLINK_UDP_PORTS`; defaults to the standard ports. An empty or
/// unparseable override falls back to the defaults.
fn udp_proxy_ports() -> Vec<u16> {
    let parsed: Vec<u16> = std::env::var("ADOS_MAVLINK_UDP_PORTS")
        .ok()
        .map(|v| {
            v.split(',')
                .filter_map(|p| p.trim().parse::<u16>().ok())
                .collect()
        })
        .unwrap_or_default();
    if parsed.is_empty() {
        UDP_PROXY_PORTS.to_vec()
    } else {
        parsed
    }
}

/// WebSocket proxy bind port. `ADOS_MAVLINK_WS_PORT` overrides the configured
/// endpoint port when set (used by the parity harness); otherwise the first
/// enabled WebSocket endpoint from the config selects it.
fn ws_proxy_port(cfg: &MavlinkConfig) -> Option<u16> {
    if let Ok(v) = std::env::var("ADOS_MAVLINK_WS_PORT") {
        return v.trim().parse().ok();
    }
    cfg.websocket_port()
}

#[tokio::main]
async fn main() {
    use ados_protocol::logd::layer::LogdLayer;
    use tracing_subscriber::prelude::*;

    // fmt as the primary sink (this binary has no journald layer) plus the logd
    // layer that ships records to the logging daemon's ingest socket; the logd
    // layer is best-effort and never blocks the service.
    let filter =
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer())
        .with(LogdLayer::new("ados-mavlink-router"))
        .try_init();
    tracing::info!("mavlink_router_starting");

    let cfg = MavlinkConfig::load();

    let state = Arc::new(Mutex::new(VehicleState::default()));
    let mut pc = ParamCache::default_path();
    if let Err(e) = pc.load() {
        tracing::warn!(error = %e, "param_cache_load_failed");
    }
    let params = Arc::new(Mutex::new(pc));

    let fc = FcConnection::new(cfg.clone(), state.clone(), params.clone());
    let cancel = Arc::new(Notify::new());

    let dir = run_dir();
    let mavlink_sock = format!("{dir}/mavlink.sock");
    let state_sock = format!("{dir}/state.sock");

    // MAVLink socket: fan FC frames out (256-deep), accept client commands inbound.
    let (mavlink_ipc, inbound) = match IpcBroadcast::bind(
        &mavlink_sock,
        MAVLINK_QUEUE_DEPTH,
        false,
        Some(MAVLINK_QUEUE_DEPTH),
    )
    .await
    {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(path = %mavlink_sock, error = %e, "mavlink_sock_bind_failed");
            return;
        }
    };
    let mavlink_ipc = Arc::new(mavlink_ipc);
    let mut inbound = inbound.expect("inbound channel requested");

    // State socket: replay last snapshot on connect (32-deep), no inbound.
    let (state_ipc, _) = match IpcBroadcast::bind(&state_sock, STATE_QUEUE_DEPTH, true, None).await
    {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(path = %state_sock, error = %e, "state_sock_bind_failed");
            return;
        }
    };
    let state_ipc = Arc::new(state_ipc);

    let started = Instant::now();
    let mut tasks = Vec::new();

    // FC connect + read loop. In demo mode a synthetic source feeds the same
    // fan-out, state, and proxy paths a serial FC would; the serial path is
    // untouched when demo mode is off (the default).
    let demo = demo_enabled();
    {
        let fc = fc.clone();
        let cancel = cancel.clone();
        if demo {
            tracing::info!("mavlink_router_demo_mode");
            tasks.push(tokio::spawn(async move { fc.run_demo(cancel).await }));
        } else {
            tasks.push(tokio::spawn(async move { fc.run(cancel).await }));
        }
    }

    // 1 Hz companion heartbeat.
    {
        let fc = fc.clone();
        let cancel = cancel.clone();
        tasks.push(tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            loop {
                tokio::select! {
                    _ = tick.tick() => fc.send_heartbeat().await,
                    _ = cancel.notified() => break,
                }
            }
        }));
    }

    // Adaptive stream cadence + parameter sweep (evaluated every second).
    {
        let fc = fc.clone();
        let cancel = cancel.clone();
        tasks.push(tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            loop {
                tokio::select! {
                    _ = tick.tick() => {
                        fc.tick_streams().await;
                        fc.tick_param_sweep().await;
                    }
                    _ = cancel.notified() => break,
                }
            }
        }));
    }

    // FC frames -> MAVLink socket clients. The socket contract is 4-byte
    // big-endian length-prefixed in both directions (the inbound reader decodes
    // the prefix), so each raw FC frame is framed before it is broadcast. The
    // proxies consume the raw frame stream directly and are unaffected.
    {
        let mavlink_ipc = mavlink_ipc.clone();
        let cancel = cancel.clone();
        let mut rx = fc.subscribe();
        tasks.push(tokio::spawn(async move {
            loop {
                tokio::select! {
                    frame = rx.recv() => match frame {
                        Ok(f) => match encode_frame(&f, MAVLINK_MAX_FRAME) {
                            Ok(framed) => mavlink_ipc.broadcast(framed).await,
                            Err(e) => tracing::warn!(error = %e, "mavlink_frame_encode_failed"),
                        },
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    },
                    _ = cancel.notified() => break,
                }
            }
        }));
    }

    // MAVLink socket client commands -> FC.
    {
        let fc = fc.clone();
        let cancel = cancel.clone();
        tasks.push(tokio::spawn(async move {
            loop {
                tokio::select! {
                    cmd = inbound.recv() => match cmd {
                        Some(data) => fc.send_bytes(&data).await,
                        None => break,
                    },
                    _ = cancel.notified() => break,
                }
            }
        }));
    }

    // 10 Hz state publish: vehicle snapshot + the service runtime extras.
    {
        let fc = fc.clone();
        let state = state.clone();
        let params = params.clone();
        let state_ipc = state_ipc.clone();
        let mavlink_ipc_stats = mavlink_ipc.clone();
        let cancel = cancel.clone();
        let msgpack = use_msgpack();
        tasks.push(tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(100));
            // Last reported eviction counts, so a fresh eviction logs once
            // rather than every 100 ms tick while the count sits unchanged.
            let mut last_mavlink_drops = 0u64;
            let mut last_state_drops = 0u64;
            loop {
                tokio::select! {
                    _ = tick.tick() => {
                        let mavlink_drops = mavlink_ipc_stats.dropped_clients();
                        let state_drops = state_ipc.dropped_clients();
                        // Surface a newly evicted slow consumer as a log line
                        // (also shipped to the logging daemon) so the eviction
                        // is not silent. The continuous signal rides the state
                        // snapshot below for the GCS.
                        if mavlink_drops > last_mavlink_drops {
                            tracing::warn!(
                                socket = "mavlink",
                                evicted = mavlink_drops - last_mavlink_drops,
                                total = mavlink_drops,
                                "ipc_slow_client_evicted"
                            );
                            last_mavlink_drops = mavlink_drops;
                        }
                        if state_drops > last_state_drops {
                            tracing::warn!(
                                socket = "state",
                                evicted = state_drops - last_state_drops,
                                total = state_drops,
                                "ipc_slow_client_evicted"
                            );
                            last_state_drops = state_drops;
                        }
                        let extras = build_extras(
                            &fc, &state, &params, started, mavlink_drops, state_drops,
                        )
                        .await;
                        let wire = { state.lock().await.to_wire_with(&extras) };
                        let encoded = if msgpack { encode_v2(&wire) } else { encode_v1(&wire) };
                        match encoded {
                            Ok(bytes) => state_ipc.broadcast(bytes).await,
                            Err(e) => tracing::warn!(error = %e, "state_encode_failed"),
                        }
                    }
                    _ = cancel.notified() => break,
                }
            }
        }));
    }

    // Direct-GCS proxies. Bind ports default to the standard values and are
    // overridable via env so a second instance (the parity harness) can run
    // alongside the first without a port clash.
    {
        let fc = fc.clone();
        let cancel = cancel.clone();
        let port = tcp_proxy_port();
        tasks.push(tokio::spawn(async move {
            run_tcp_proxy(fc, port, cancel).await
        }));
    }
    for port in udp_proxy_ports() {
        let fc = fc.clone();
        let cancel = cancel.clone();
        tasks.push(tokio::spawn(async move {
            run_udp_proxy(fc, port, cancel).await
        }));
    }
    if let Some(ws_port) = ws_proxy_port(&cfg) {
        let fc = fc.clone();
        let cancel = cancel.clone();
        // The direct WebSocket proxy carries raw MAVLink to/from the FC, so a
        // paired agent gates an off-box connection on the stored pairing key.
        // Enforcement is config-driven and defaults off (observe-only), so this
        // build does not change the data path until a bench session enables it.
        let auth = WsProxyAuth::from_config(cfg.ws_proxy_enforce_auth);
        tasks.push(tokio::spawn(async move {
            run_ws_proxy(fc, ws_port, auth, cancel).await
        }));
    }

    tracing::info!("mavlink_router_ready");
    wait_for_shutdown().await;
    tracing::info!("mavlink_router_stopping");
    cancel.notify_waiters();
    for t in tasks {
        let _ = t.await;
    }
    tracing::info!("mavlink_router_stopped");
}

/// Build the runtime extras the state snapshot carries on top of the vehicle
/// fields.
///
/// `mavlink_drops` / `state_drops` are the monotonic slow-consumer eviction
/// counts from the two IPC servers, carried on the snapshot so a silently
/// pruned client is observable to Mission Control (it never sees an error of
/// its own when it is dropped for falling behind).
#[allow(clippy::too_many_arguments)]
async fn build_extras(
    fc: &Arc<FcConnection>,
    state: &Arc<Mutex<VehicleState>>,
    params: &Arc<Mutex<ParamCache>>,
    started: Instant,
    mavlink_drops: u64,
    state_drops: u64,
) -> Map<String, Value> {
    let cached = params.lock().await.count();
    let expected = state.lock().await.param_count;
    let params_blob = params.lock().await.get_all();
    let mut extras = Map::new();
    extras.insert("fc_connected".into(), json!(fc.connected()));
    extras.insert("fc_port".into(), json!(fc.port().await));
    extras.insert("fc_baud".into(), json!(fc.baud()));
    extras.insert(
        "service_uptime".into(),
        json!(started.elapsed().as_secs_f64()),
    );
    extras.insert("param_priming".into(), json!(fc.param_priming()));
    extras.insert(
        "param_sweep_timed_out".into(),
        json!(fc.param_sweep_timed_out()),
    );
    extras.insert(
        "param_sweep_send_failed".into(),
        json!(fc.param_sweep_send_failed()),
    );
    extras.insert("param_cached_count".into(), json!(cached));
    extras.insert("param_expected_count".into(), json!(expected));
    extras.insert("ipc_mavlink_drops".into(), json!(mavlink_drops));
    extras.insert("ipc_state_drops".into(), json!(state_drops));
    extras.insert("params".into(), Value::Object(params_blob));
    extras
}

/// Resolve when the service receives SIGTERM or SIGINT.
async fn wait_for_shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        let mut int = signal(SignalKind::interrupt()).expect("install SIGINT handler");
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
