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
use ados_mavlink_router::proxies::{run_tcp_proxy, run_udp_proxy, run_ws_proxy};
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

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
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

    // FC connect + read loop.
    {
        let fc = fc.clone();
        let cancel = cancel.clone();
        tasks.push(tokio::spawn(async move { fc.run(cancel).await }));
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
        let cancel = cancel.clone();
        let msgpack = use_msgpack();
        tasks.push(tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(100));
            loop {
                tokio::select! {
                    _ = tick.tick() => {
                        let extras = build_extras(&fc, &state, &params, started).await;
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

    // Direct-GCS proxies.
    {
        let fc = fc.clone();
        let cancel = cancel.clone();
        tasks.push(tokio::spawn(async move {
            run_tcp_proxy(fc, TCP_PROXY_PORT, cancel).await
        }));
    }
    for &port in UDP_PROXY_PORTS {
        let fc = fc.clone();
        let cancel = cancel.clone();
        tasks.push(tokio::spawn(async move {
            run_udp_proxy(fc, port, cancel).await
        }));
    }
    if let Some(ws_port) = cfg.websocket_port() {
        let fc = fc.clone();
        let cancel = cancel.clone();
        tasks.push(tokio::spawn(async move {
            run_ws_proxy(fc, ws_port, cancel).await
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

/// Build the 11 runtime extras the state snapshot carries on top of the vehicle
/// fields (mirrors __main__.py:78-133).
async fn build_extras(
    fc: &Arc<FcConnection>,
    state: &Arc<Mutex<VehicleState>>,
    params: &Arc<Mutex<ParamCache>>,
    started: Instant,
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
