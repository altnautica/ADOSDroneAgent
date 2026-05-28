//! Entry point. Resolves the agent profile, starts the gated services, then
//! drives a single serial loop over the monitor tick, hot-plug events, and the
//! shutdown signals. Owning the service state on one task means no lock is held
//! across a `systemctl` await.

use std::time::Duration;

use anyhow::Result;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::mpsc;

use ados_supervisor::{config::AgentConfig, hotplug, lifecycle::Supervisor, sdnotify};

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

    let mut supervisor = Supervisor::new(config);
    supervisor.start().await;

    // Tell systemd we are up (no-op off Linux / outside a notify unit).
    sdnotify::ready();

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

    supervisor.stop().await;
    Ok(())
}
