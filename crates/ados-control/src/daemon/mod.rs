//! The daemon entry for the `ados-control` binary.
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

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use tokio::sync::oneshot;

use crate::auth::PairingState;
use crate::ipc::logd_client::{default_logd_socket, LogdQueryClient};
use crate::ipc::mavlink_client::{default_mavlink_socket, MavlinkIpcClient};
use crate::ipc::state_client::{default_state_socket, StateIpcClient};
use crate::ipc::swarm_client::{default_swarm_socket, SwarmIpcClient};
use crate::routes::build_router;
use crate::serve::{bind_tcp, bind_unix, serve_tcp, serve_unix, tcp_app, unix_app};
use crate::state::{AppState, PairingPaths};

mod signals;
mod startup;
mod tasks;

use signals::*;
pub use startup::{DaemonPaths, WATCHDOG_PING_INTERVAL};
pub use tasks::run_with_paths;

/// The one-shot side-effect mode's flag.
const APPLY_SIDE_EFFECTS_FLAG: &str = "--apply-config-side-effects";

/// Apply the side effects of a write from `old_path` to the node's config.
fn apply_config_side_effects(old_path: &Path) -> Result<()> {
    let new_path = Path::new(crate::config::CONFIG_YAML);
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

/// The binary's entry: run the daemon (or the one-shot side-effect mode) and
/// map the outcome to the process exit code.
pub fn run() -> std::process::ExitCode {
    match serve() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Error: {e:?}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[tokio::main]
async fn serve() -> Result<()> {
    init_logging();
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() == Some(APPLY_SIDE_EFFECTS_FLAG) {
        let old = args
            .next()
            .ok_or_else(|| anyhow!("{APPLY_SIDE_EFFECTS_FLAG} needs the previous config path"))?;
        return apply_config_side_effects(Path::new(&old));
    }
    tracing::info!(
        socket = crate::paths::CONTROL_SOCKET,
        "control API starting"
    );
    match crate::run_daemon().await {
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

/// Run the daemon to completion: publish the node's `_ados._tcp` record, bind
/// both listeners, serve the shared Router on each, wait for `SIGTERM`/
/// `SIGINT`, shut down cleanly. The production entry.
///
/// The mDNS advert lives here rather than inside [`run_with_paths`] because it
/// is a property of the PROCESS, not of the lifecycle: one real node publishes
/// one record on the host's multicast group, while `run_with_paths` is the
/// parameterized form a test drives dozens of times concurrently against temp
/// sockets. A record is published only by a daemon that will actually answer
/// `/api/pairing/*` on the advertised port, and it is withdrawn before the
/// process exits.
pub async fn run_daemon() -> Result<()> {
    let paths = DaemonPaths::default();
    // `_ados._tcp`, on EVERY profile. The GCS Add-a-Node card browses this
    // service and probes whatever answers; without it a fresh drone or ground
    // station never appears in the discovered list and the operator has to
    // know and type a hostname. The SRV target is the same resolvable name
    // `/api/pairing/info` reports as `mdns_host`.
    let advert = crate::mdns::advertise(
        &crate::state::PairingPaths {
            config: paths.config_path.clone(),
            pairing_json: paths.pairing_path.clone(),
            wfb_key_dir: paths.wfb_key_dir.clone(),
            bind_state: paths.bind_state_path.clone(),
            profile_conf: paths.profile_conf_path.clone(),
            mesh_role: paths.mesh_role_path.clone(),
            relay_secret: paths.relay_secret_path.clone(),
        },
        paths.board_path.clone(),
        paths.control_tcp_port,
    );
    let result = run_with_paths(paths, shutdown_signal()).await;
    // Withdraw before exiting, so a GCS browsing mid-shutdown does not list a
    // node whose pairing front has already stopped answering.
    if let Some(advert) = advert {
        advert.shutdown();
    }
    result
}
