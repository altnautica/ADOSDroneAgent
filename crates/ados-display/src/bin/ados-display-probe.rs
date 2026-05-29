//! `ados-display-probe` — boot-time apply-verify-auto-revert oneshot.
//!
//! Runs once per boot (a systemd oneshot, gated by `ConditionPathExists` on the
//! probation marker) to confirm or auto-revert a probationary SPI-LCD overlay.
//! The whole decision lives in [`ados_display::probe`]; this binary just wires
//! logging and the default real paths and returns 0.

use ados_display::probe::{self, ProbePaths};

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

fn main() {
    init_logging();
    let paths = ProbePaths::default();
    match probe::run(&paths) {
        Ok(outcome) => tracing::info!(?outcome, "display probe complete"),
        Err(e) => tracing::warn!(error = %e, "display probe error"),
    }
    // Always exit 0: an unconfirmed panel is a handled outcome, not a failure.
    std::process::exit(0);
}
