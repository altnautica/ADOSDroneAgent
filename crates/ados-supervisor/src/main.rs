//! Entry point. Resolves the agent profile, starts the gated services, then
//! drives a single serial loop over the monitor tick, hot-plug events, and the
//! shutdown signals. Owning the service state on one task means no lock is held
//! across a `systemctl` await.

use std::time::Duration;

use anyhow::Result;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::mpsc;

use std::sync::Arc;

use ados_supervisor::{
    auto_pair, bind, config::AgentConfig, hotplug, lifecycle::Supervisor, sdnotify,
};

const MONITOR_INTERVAL: Duration = Duration::from_secs(5);

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

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();

    let config = AgentConfig::load();
    tracing::info!(
        profile = %config.profile_wire,
        role = ?config.role,
        video_enabled = config.video_enabled,
        "resolved agent profile"
    );

    // Capture the bindable role before `config` is moved into the supervisor,
    // and a shutdown signal for the background tasks (auto-pair).
    let auto_pair_role = bind::BindRole::parse(&config.profile_wire);
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    // The bind orchestrator is shared between the monitor (which gates radio
    // auto-restart on a live bind) and the control socket task.
    let bind_orch = Arc::new(bind::orchestrator::BindOrchestrator::new());

    let mut supervisor = Supervisor::new(config, bind_orch.clone());
    supervisor.start().await;

    // Tell systemd we are up (no-op off Linux / outside a notify unit).
    sdnotify::ready();

    // Cross-process bind trigger seam: the FastAPI pairing route + the cloud
    // auto-pair supervisor forward start/cancel/status here over the control
    // socket. Always served (cheap); the Python side connects when it wants a
    // bind.
    {
        let bind_orch = bind_orch.clone();
        tokio::spawn(async move {
            let path = std::path::Path::new(bind::control::SUPERVISOR_SOCK);
            if let Err(e) = bind::control::serve(bind_orch, path).await {
                tracing::error!(error = %e, "bind control socket exited");
            }
        });
    }

    // Local-radio auto-pair: drive the first-boot bind in-process (moved out of
    // the cloud relay, which is idle in local-first mode). Bindable role only;
    // the orchestrator is shared so a manual / FastAPI bind still mutexes.
    if let Some(role) = auto_pair_role {
        let orch = bind_orch.clone();
        let shutdown = shutdown_rx.clone();
        tokio::spawn(auto_pair::run(orch, role, shutdown));
    }

    // Hot-plug poller runs on its own task and only forwards device-class
    // transitions; the supervisor state stays owned by this loop.
    let (tx, mut rx) = mpsc::channel(32);
    tokio::spawn(hotplug::run(tx, hotplug::poll_interval()));

    let mut tick = tokio::time::interval(MONITOR_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;

    loop {
        tokio::select! {
            _ = tick.tick() => {
                supervisor.monitor_pass().await;
                sdnotify::watchdog();
            }
            Some(kind) = rx.recv() => {
                supervisor.handle_hotplug(kind).await;
            }
            _ = sigterm.recv() => {
                tracing::info!("received SIGTERM");
                break;
            }
            _ = sigint.recv() => {
                tracing::info!("received SIGINT");
                break;
            }
        }
    }

    // Stop the background tasks (auto-pair) before tearing down the services.
    let _ = shutdown_tx.send(true);
    supervisor.stop().await;
    Ok(())
}
