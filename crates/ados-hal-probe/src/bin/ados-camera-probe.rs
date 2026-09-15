//! `ados-camera-probe` — boot-time apply-verify-auto-revert oneshot for the
//! CSI camera overlay.
//!
//! Runs once per boot (a systemd oneshot, gated by `ConditionPathExists` on
//! `/etc/ados/camera.probation`) to confirm the sensor the overlay was applied
//! for, or restore the pre-overlay boot config when nothing bound. The whole
//! decision lives in [`ados_hal_probe::camera_probe`]; this binary wires
//! logging and the default real paths.

use ados_hal_probe::camera_probe::{self, CameraProbePaths};

fn init_logging() {
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string());
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(&filter))
        .try_init();
}

fn main() {
    init_logging();
    let paths = CameraProbePaths::default();
    match camera_probe::run(&paths) {
        Ok(outcome) => tracing::info!(?outcome, "camera probe complete"),
        Err(e) => tracing::warn!(error = %e, "camera probe error"),
    }
    // Always exit 0: an unconfirmed sensor is a HANDLED outcome (the boot config
    // was restored), not a unit failure. A non-zero exit here would put the
    // oneshot in `failed` and add a scary state to a board that just healed
    // itself.
    std::process::exit(0);
}
