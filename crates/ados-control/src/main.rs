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
//!
//! `ados-control --apply-config-side-effects <old.yaml>` is a one-shot mode for
//! the other config writers: it compares `<old.yaml>` (the document before a
//! write) with the config now on disk, applies the config side effects (enable
//! markers and unit kicks), prints the report as JSON and exits.

use std::path::Path;

use anyhow::{anyhow, Context, Result};

/// The one-shot side-effect mode's flag.
const APPLY_SIDE_EFFECTS_FLAG: &str = "--apply-config-side-effects";

/// Apply the side effects of a write from `old_path` to the node's config.
fn apply_config_side_effects(old_path: &Path) -> Result<()> {
    let new_path = Path::new(ados_control::config::CONFIG_YAML);
    let read = |path: &Path| {
        ados_config::schema::read_document(path)
            .map(Option::unwrap_or_default)
            .map_err(|e| anyhow!("{}: {e}", path.display()))
    };
    let old = read(old_path)?;
    let new = read(new_path)?;
    let etc_dir = new_path.parent().unwrap_or(Path::new("/etc/ados"));
    let report = ados_config::side_effects::apply(&old, &new, etc_dir, &run_command);
    println!(
        "{}",
        serde_json::to_string(&report).context("serialising the report")?
    );
    Ok(())
}

/// Run one side-effect command for its exit status (stdio discarded). Every
/// command is a `systemctl --no-block` call, so it returns promptly; this
/// one-shot CLI mode has no reactor to stall.
fn run_command(program: &str, args: &[&str]) -> bool {
    std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

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
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() == Some(APPLY_SIDE_EFFECTS_FLAG) {
        let old = args
            .next()
            .ok_or_else(|| anyhow!("{APPLY_SIDE_EFFECTS_FLAG} needs the previous config path"))?;
        return apply_config_side_effects(Path::new(&old));
    }
    tracing::info!(
        socket = ados_control::paths::CONTROL_SOCKET,
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
