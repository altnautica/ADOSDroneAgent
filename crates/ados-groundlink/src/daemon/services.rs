//! The two role-independent ground services this binary also runs:
//! `--pairing` (the `ados-mesh-pairing` unit) and `--mesh-bringup` (the
//! `ados-batman` unit).

use std::path::Path;
use std::process::ExitCode;

use tokio::signal::unix::{signal, SignalKind};

use super::{init_logging, CONFIG_YAML};
use crate::mesh::bringup::{
    request_direct_role, BringupError, BringupPaths, MeshBringup, SUPERVISE_INTERVAL,
};
use crate::pairing::{self, PairingEnv, PairingService};
use crate::GroundStationConfig;

/// Exit status for a bring-up that could not start; systemd restarts it.
const EXIT_SETUP_FAILED: u8 = 2;

/// Which of these services the arguments select, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ServiceMode {
    Pairing,
    MeshBringup,
}

pub(super) fn service_mode(args: &[String]) -> Option<ServiceMode> {
    args.iter().find_map(|a| match a.as_str() {
        "--pairing" => Some(ServiceMode::Pairing),
        "--mesh-bringup" => Some(ServiceMode::MeshBringup),
        _ => None,
    })
}

pub(super) async fn run(mode: ServiceMode) -> ExitCode {
    init_logging();
    match mode {
        ServiceMode::Pairing => run_pairing().await,
        ServiceMode::MeshBringup => run_mesh_bringup().await,
    }
}

async fn shutdown_signal() {
    let (Ok(mut term), Ok(mut int)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
    ) else {
        std::future::pending::<()>().await;
        return;
    };
    tokio::select! {
        _ = term.recv() => tracing::info!("received SIGTERM"),
        _ = int.recv() => tracing::info!("received SIGINT"),
    }
}

/// Serve the pairing RPC until SIGTERM. A stale socket that cannot be removed
/// fails the start.
async fn run_pairing() -> ExitCode {
    let socket = ados_protocol::paths::run_path("pairing");
    tracing::info!(socket = %socket.display(), "pairing_daemon_starting");
    let service = PairingService::new(PairingEnv::live());
    service.resume_relay_rekey().await;
    ados_supervisor::sdnotify::ready();
    match pairing::serve(&socket, service, shutdown_signal()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = %e, "pairing_daemon_socket_failed");
            ExitCode::from(EXIT_SETUP_FAILED)
        }
    }
}

/// Bring the mesh up and supervise it until SIGTERM. Exit 0 (never restarted)
/// when this node's role runs no mesh or a relay still lacks its identity.
async fn run_mesh_bringup() -> ExitCode {
    let role = crate::mesh::get_current_role();
    if role != "relay" && role != "receiver" {
        tracing::info!(role = %role, "mesh_not_applicable_for_role");
        return ExitCode::SUCCESS;
    }
    let config = Path::new(CONFIG_YAML);
    let cfg = GroundStationConfig::load_from(config);
    let device_id = pairing::env::agent_device_id(config);
    let mut mesh = MeshBringup::new(&role, cfg, device_id, BringupPaths::live());
    match mesh.setup().await {
        Ok(()) => {}
        Err(BringupError::IdentityMissing(reason)) => {
            // No invite delivered yet: ask the supervisor (the owner of role
            // transitions) for `direct` and exit cleanly instead of looping.
            tracing::warn!(reason = %reason, "mesh_identity_missing_requesting_direct");
            request_direct_role(&ados_protocol::paths::run_path("supervisor")).await;
            return ExitCode::SUCCESS;
        }
        Err(BringupError::Fatal(reason)) => {
            tracing::error!(reason = %reason, "mesh_setup_failed");
            return ExitCode::from(EXIT_SETUP_FAILED);
        }
    }
    ados_supervisor::sdnotify::ready();
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    let mut interval = tokio::time::interval(SUPERVISE_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    interval.tick().await;
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            _ = interval.tick() => mesh.tick().await,
        }
    }
    tracing::info!("mesh_bringup_stopping");
    mesh.teardown().await;
    ExitCode::SUCCESS
}
