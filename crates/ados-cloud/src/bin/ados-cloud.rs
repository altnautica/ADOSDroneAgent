//! `ados-cloud` binary skeleton.
//!
//! The runnable cloud relay daemon. For now it initializes logging, loads the
//! config, logs what it resolved, and exits cleanly — the long-running relay
//! tasks (MQTT gateway, MAVLink + signaling relays, heartbeat / command-poll /
//! beacon loops, the local auto-pair supervisor) are added incrementally on top
//! of this skeleton. Modeled on the `ados-supervisor` / `ados-mavlink-router`
//! binary shape: journald logging on Linux with an fmt fallback.

use anyhow::Result;

use ados_cloud::config::CloudConfig;

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

fn main() -> Result<()> {
    init_logging();

    let config = CloudConfig::load();
    tracing::info!(
        ota_channel = %config.ota.channel,
        ota_repo = %config.ota.github_repo,
        cloud_url_set = !config.server.cloud.url.is_empty(),
        "cloud relay starting (skeleton: tasks not yet wired)"
    );

    // The relay tasks are wired incrementally on top of this skeleton; until
    // then the daemon resolves its config, reports it, and exits cleanly so the
    // unit is installable and the config path is exercised.
    tracing::info!("cloud relay skeleton exiting cleanly");
    Ok(())
}
