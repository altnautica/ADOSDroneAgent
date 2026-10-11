//! The `ados-mavlink-router` service body.
//!
//! Owns the FC serial link and serves the MAVLink + state IPC sockets plus the
//! direct-GCS TCP/UDP proxies. Mirrors the Python `ados-mavlink` service
//! (`python -m ados.services.mavlink`): the IPC servers, the FC connection, the
//! 1 Hz companion heartbeat, the 10 Hz state publish, the adaptive stream
//! cadence, and the parameter sweep. The state socket is published as
//! length-prefixed msgpack (v2), the versioned wire the shared reader
//! auto-detects.
//!
//! The binary's `main` is a thin call into [`run`]; everything the process
//! wires up at start lives here and in the sibling modules.

mod extras;
mod signals;
mod startup;
mod tasks;
mod wiring;

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Instant;

use ados_protocol::ipc::IpcBroadcast;
use ados_protocol::shutdown::Shutdown;
use tokio::sync::Mutex;

use crate::config::MavlinkConfig;
use crate::connection::FcConnection;
use crate::param_cache::ParamCache;
use crate::state::VehicleState;

use startup::{proxy_posture, MAVLINK_QUEUE_DEPTH, STATE_QUEUE_DEPTH};

/// The spawned service tasks, joined at shutdown.
type Tasks = Vec<tokio::task::JoinHandle<()>>;

/// Run the service to completion on a multi-threaded runtime.
pub fn run() -> ExitCode {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime")
        .block_on(serve());
    ExitCode::SUCCESS
}

async fn serve() {
    startup::init_tracing();
    tracing::info!("mavlink_router_starting");

    let cfg = MavlinkConfig::load();

    let state = Arc::new(Mutex::new(VehicleState::default()));
    let mut pc = ParamCache::default_path();
    if let Err(e) = pc.load() {
        tracing::warn!(error = %e, "param_cache_load_failed");
    }
    let params = Arc::new(Mutex::new(pc));

    let fc = FcConnection::new(cfg.clone(), state.clone(), params.clone());
    let cancel = Shutdown::new();

    let dir = ados_protocol::paths::run_dir()
        .to_string_lossy()
        .into_owned();
    let mavlink_sock = format!("{dir}/mavlink.sock");
    let msp_sock = format!("{dir}/msp.sock");
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
    let inbound = inbound.expect("inbound channel requested");

    // MSP socket: the sibling byte plane for an MSP FC (Betaflight/iNav), whose
    // FC->host bytes are raw MSP responses rather than MAVLink frames. The MAVLink
    // socket is fed only by the parsed frame lane, so it stays legitimately silent
    // for such an FC; this socket carries the raw byte lane instead so a downstream
    // consumer (the cloud relay) reaches a polling MSP GCS the same way it reaches
    // a MAVLink one. Length-prefixed both ways (256-deep), accepting protocol-
    // agnostic client commands inbound. Never parses the byte stream.
    let (msp_ipc, msp_inbound) = match IpcBroadcast::bind(
        &msp_sock,
        MAVLINK_QUEUE_DEPTH,
        false,
        Some(MAVLINK_QUEUE_DEPTH),
    )
    .await
    {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(path = %msp_sock, error = %e, "msp_sock_bind_failed");
            return;
        }
    };
    let msp_ipc = Arc::new(msp_ipc);
    let msp_inbound = msp_inbound.expect("msp inbound channel requested");

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
    let mut tasks: Tasks = Vec::new();

    tasks::spawn_core(&cfg, &fc, &cancel, &dir, &mut tasks);
    wiring::ipc::spawn_fc_to_mavlink(&fc, &mavlink_ipc, &cancel, &mut tasks);
    let aux = wiring::aux_link::spawn(&cfg, &fc, &cancel, &dir, &mut tasks).await;
    wiring::ipc::spawn_command_pumps(&fc, inbound, &msp_ipc, msp_inbound, &cancel, &mut tasks);

    // The proxy posture, shared by the state publish (which reads the live
    // listening flags) and the proxy tasks (which set them).
    let posture = proxy_posture(&cfg);

    tasks::spawn_gate_feed(&fc, &cancel, &mut tasks);
    wiring::state::spawn(
        &fc,
        &state,
        &params,
        &dir,
        &mavlink_ipc,
        &state_ipc,
        started,
        &aux,
        &posture,
        &cancel,
        &mut tasks,
    );
    wiring::proxies::spawn(&cfg, &fc, &posture, &cancel, &mut tasks);

    tracing::info!("mavlink_router_ready");
    signals::wait_for_shutdown().await;
    tracing::info!("mavlink_router_stopping");
    cancel.trigger();
    for t in tasks {
        let _ = t.await;
    }
    tracing::info!("mavlink_router_stopped");
}
