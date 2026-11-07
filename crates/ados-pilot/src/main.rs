//! `ados-pilot` service entry point.

use ados_pilot::config::{config_path, Settings};
use ados_pilot::runtime::Daemon;

#[tokio::main]
async fn main() {
    {
        use ados_protocol::logd::layer::LogdLayer;
        use tracing_subscriber::prelude::*;
        let filter =
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
        let _ = tracing_subscriber::registry()
            .with(filter)
            .with(tracing_subscriber::fmt::layer())
            .with(LogdLayer::new("ados-pilot"))
            .try_init();
    }
    let path = config_path();
    let settings = Settings::load(&path);
    tracing::info!(
        control_mode = settings.control.mode.as_str(),
        profile = ?settings.pilot.profile,
        avoid = ?settings.pilot.avoid,
        "pilot_service_starting"
    );
    let daemon = Daemon::new(settings, path);
    let run = daemon.run(None);
    tokio::select! {
        r = run => {
            if let Err(e) = r {
                tracing::error!(error = %e, "pilot_service_failed");
                std::process::exit(1);
            }
        }
        _ = shutdown() => tracing::info!("pilot_service_stopping"),
    }
}

async fn shutdown() {
    use tokio::signal::unix::{signal, SignalKind};
    match signal(SignalKind::terminate()) {
        Ok(mut term) => {
            tokio::select! {
                _ = term.recv() => {}
                _ = tokio::signal::ctrl_c() => {}
            }
        }
        Err(_) => {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}
