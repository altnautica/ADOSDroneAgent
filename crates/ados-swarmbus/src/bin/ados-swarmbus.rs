//! `ados-swarmbus` entry point: the decentralized swarm state bus.
//!
//! Resolves the fleet identity, refuses to run on an invalid one, then runs the
//! receive / transmit / publish loops until SIGTERM or SIGINT.
//!
//! The identity gate is the one hard failure here. A drone left on the ground
//! station's slot 0, or two drones sharing a slot, thrashes the wfb-ng FEC decoder
//! roughly once a second — which presents as unexplained link loss, not as a
//! configuration error. So a misprovisioned drone exits non-zero and does not
//! radiate: a `failed` unit is a diagnosable state, an aircraft quietly jamming its
//! own fleet is not.

use std::path::Path;
use std::sync::Arc;

use ados_swarmbus::config::{SwarmBusConfig, CONFIG_YAML};
use tokio::sync::Notify;

fn init_tracing() {
    use ados_protocol::logd::layer::LogdLayer;
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::EnvFilter;

    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string());

    // The logd layer ships records to the logging daemon's ingest socket alongside
    // the primary sink; it is best-effort and never blocks the service.
    #[cfg(target_os = "linux")]
    {
        if let Ok(journald) = tracing_journald::layer() {
            let _ = tracing_subscriber::registry()
                .with(EnvFilter::new(&filter))
                .with(journald)
                .with(LogdLayer::new("ados-swarmbus"))
                .try_init();
            return;
        }
    }

    let _ = tracing_subscriber::registry()
        .with(EnvFilter::new(&filter))
        .with(tracing_subscriber::fmt::layer())
        .with(LogdLayer::new("ados-swarmbus"))
        .try_init();
}

#[tokio::main]
async fn main() {
    init_tracing();

    let config = SwarmBusConfig::load_from(Path::new(CONFIG_YAML));
    tracing::info!(
        profile = ?config.profile,
        fleet_id = config.fleet_id,
        fleet_slot = config.fleet_slot,
        ground_station = config.is_ground_station(),
        "ados-swarmbus resolved config"
    );

    if let Some(err) = config.identity_error() {
        tracing::error!(
            error = %err,
            "ados-swarmbus refusing to start: a duplicate or unprovisioned fleet slot \
             thrashes the wfb-ng FEC decoder and presents as unexplained link loss"
        );
        std::process::exit(1);
    }

    let cancel = Arc::new(Notify::new());
    notify_ready();
    tracing::info!("ados-swarmbus ready");

    let run_cancel = cancel.clone();
    let mut handle = tokio::spawn(async move { ados_swarmbus::run(config, run_cancel).await });

    // Exit on a signal, or if the service loop ends unexpectedly. It only returns on
    // `cancel`, so an early finish means the daemon is alive-but-dead; surface it
    // with a non-zero exit so systemd's Restart=on-failure recovers it rather than
    // leaving a running unit that carries no beacons.
    tokio::select! {
        _ = wait_for_shutdown() => {
            tracing::info!("ados-swarmbus stopping");
            cancel.notify_waiters();
            let _ = handle.await;
        }
        res = &mut handle => {
            tracing::error!(result = ?res, "ados-swarmbus service loop exited unexpectedly");
            std::process::exit(1);
        }
    }
}

/// Tell systemd the service is up, so `Type=notify` units downstream of it order
/// correctly. A no-op off Linux and when the notify socket is absent.
fn notify_ready() {
    #[cfg(target_os = "linux")]
    {
        let _ = sd_notify::notify(false, &[sd_notify::NotifyState::Ready]);
    }
}

/// Resolve when the service receives SIGTERM or SIGINT.
async fn wait_for_shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(_) => return,
        };
        let mut int = match signal(SignalKind::interrupt()) {
            Ok(s) => s,
            Err(_) => return,
        };
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
