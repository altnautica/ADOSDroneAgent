//! Entry point for the ground-station data-plane service.
//!
//! Initializes logging (journald on Linux, fmt fallback elsewhere), signals
//! readiness to systemd, then runs the video UDP fan-out as the supervised
//! work. The channel-acquisition receive loop and the mesh role manager land in
//! a later chunk; this binary is the daemon skeleton they slot into.

use anyhow::Result;
use tokio::signal::unix::{signal, SignalKind};

use ados_groundlink::fanout;

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
        listen_port = fanout::INTERNAL_LISTEN_PORT,
        mediamtx_port = fanout::MEDIAMTX_PORT,
        lcd_port = fanout::LCD_PORT,
        "ground-station data-plane starting"
    );

    // Tell systemd we are up (no-op off Linux / outside a notify unit). Reuses
    // the orchestrator's notify shim so the readiness contract is identical.
    ados_supervisor::sdnotify::ready();

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;

    // Run the fan-out until a fatal socket error or a shutdown signal. The
    // receive loop + mesh manager will join this select in a later chunk.
    tokio::select! {
        res = fanout::run_default_fanout() => {
            if let Err(e) = res {
                tracing::error!(error = %e, "fanout exited with error");
            }
        }
        _ = sigterm.recv() => {
            tracing::info!("received SIGTERM");
        }
        _ = sigint.recv() => {
            tracing::info!("received SIGINT");
        }
    }

    tracing::info!("ground-station data-plane stopping");
    Ok(())
}
