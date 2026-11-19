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
    // ADOS time: the simulator's feed on a node configured for
    // `simulation.clock: sim`, wall otherwise.
    let gate = ados_config::simulation::read_gate(&path);
    let clock = match ados_time::from_config(&gate, ados_time::FeedPeer::Pilot) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "pilot_sim_clock_bind_failed");
            std::process::exit(1);
        }
    };
    let mut daemon = Daemon::new(settings, path);
    daemon.clock = clock;
    let setup = ados_pilot::setup::service::start(&daemon);
    let run = daemon.run(Some(setup));
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
