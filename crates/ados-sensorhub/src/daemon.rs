//! `ados-sensorhub` service body: logging, signals, config (re)load.
//!
//! SIGTERM/SIGINT stop the service; SIGHUP re-reads the config and restarts
//! the link. With `transport: auto` and no hub attached at start the service
//! exits 0: its unit is gated on the hub's presence, so there is nothing to
//! wait for.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use ados_protocol::shutdown::Shutdown;
use tokio::sync::Notify;

use crate::config::{config_path, HubConfig};
use crate::link::{has_target, run_link, LinkParams};
use crate::rig::{rig_path, Extrinsics};
use crate::session::Stop;

/// The binary's entry point.
pub fn run() -> ExitCode {
    init_logging();
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!(error = %e, "tokio runtime failed to start");
            return ExitCode::FAILURE;
        }
    };
    rt.block_on(serve());
    ExitCode::SUCCESS
}

fn init_logging() {
    use ados_protocol::logd::layer::LogdLayer;
    use tracing_subscriber::prelude::*;

    let filter =
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer())
        .with(LogdLayer::new("ados-sensorhub"))
        .try_init();
}

async fn serve() {
    let shutdown = Shutdown::new();
    let reload = Arc::new(Notify::new());
    spawn_signals(shutdown.clone(), reload.clone());
    let tty_root = PathBuf::from(ados_hal_probe::sensor_hub::TTY_CLASS_DIR);
    loop {
        let cfg = HubConfig::load_from(&config_path());
        if !has_target(&cfg.transport, &tty_root) {
            tracing::info!("no sensor hub attached and no transport configured; idle");
            return;
        }
        let params = LinkParams {
            cfg,
            tty_root: tty_root.clone(),
            ext: Extrinsics::load(&rig_path()),
            socket: ados_sensorbus::socket_path(),
            sidecar: crate::state::sidecar_path(),
        };
        tracing::info!("sensorhub_service_starting");
        match run_link(params, shutdown.clone(), reload.clone()).await {
            Stop::Shutdown => break,
            Stop::Reload => tracing::info!("sensorhub config reloaded"),
        }
    }
    tracing::info!("sensorhub_service_stopped");
}

fn spawn_signals(shutdown: Shutdown, reload: Arc<Notify>) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        if let Ok(mut hup) = signal(SignalKind::hangup()) {
            tokio::spawn(async move {
                while hup.recv().await.is_some() {
                    reload.notify_one();
                }
            });
        }
        let stop = shutdown.clone();
        if let Ok(mut term) = signal(SignalKind::terminate()) {
            tokio::spawn(async move {
                term.recv().await;
                stop.trigger();
            });
        }
    }
    #[cfg(not(unix))]
    drop(reload);
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        shutdown.trigger();
    });
}
