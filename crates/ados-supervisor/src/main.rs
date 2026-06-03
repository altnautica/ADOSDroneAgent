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
    auto_pair, bind, config::AgentConfig, hotplug, lifecycle::Supervisor, mac_pin, sdnotify,
};

const MONITOR_INTERVAL: Duration = Duration::from_secs(5);

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
                .with(LogdLayer::new("ados-supervisor"))
                .try_init();
            return;
        }
    }

    let _ = tracing_subscriber::registry()
        .with(EnvFilter::new(&filter))
        .with(tracing_subscriber::fmt::layer())
        .with(LogdLayer::new("ados-supervisor"))
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

    // Reconcile the RTL8812EU operating-region driver options from the configured
    // posture (unrestricted by default). Idempotent: writes the modprobe options
    // file only when the resolved posture changed it. The reload is deferred at
    // start (a fresh module load picks up the on-disk options regardless), so this
    // never races the auto-pair/bind that follows.
    ados_supervisor::rtl_modprobe::reconcile_from_config(false);

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

    // Stable-MAC reconciler: pin a deterministic MAC on any onboard adapter
    // that randomizes its address each boot (no efuse MAC), so the box stops
    // churning its IP. Runs on every profile; inert when no such adapter exists.
    // Writes a next-boot .link only — never touches the live interface.
    tokio::spawn(mac_pin::run(shutdown_rx.clone()));

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
