//! `ados-control` daemon — the native HTTP control surface.
//!
//! The runnable daemon. Binds the trusted local Unix socket and the LAN TCP
//! port, serves the shared `/api/*` (+ `/healthz`) Router on each, and shuts
//! down cleanly on `SIGTERM`/`SIGINT`. It answers the agent's control API with
//! no Python runtime.
//!
//! Modeled on the sibling daemons: journald logging on Linux with an fmt
//! fallback off Linux or outside a journald unit, and systemd readiness notify.
//! The binary is functional but ships dark — no supervisor registration and no
//! systemd unit enable it yet, so it has no effect at the install layer until
//! that wiring lands. The crate is inert.

use anyhow::Result;

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
    tracing::info!(
        socket = ados_control::paths::CONTROL_SOCKET,
        tcp_port = ados_control::paths::CONTROL_TCP_PORT,
        "control API starting"
    );
    match ados_control::run_daemon().await {
        Ok(()) => {
            tracing::info!("control API exited cleanly");
            Ok(())
        }
        Err(e) => {
            tracing::error!(error = %e, "control API fatal error");
            Err(e)
        }
    }
}
