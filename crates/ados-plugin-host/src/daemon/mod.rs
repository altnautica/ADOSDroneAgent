//! The plugin-host daemon: the body behind the `ados-plugin-host` binary.
//!
//! The runnable plugin-host daemon. It discovers installed plugins, serves a
//! per-plugin Unix socket for every enabled subprocess plugin, and routes each
//! plugin's RPC calls to the real host services (telemetry, config, camera,
//! MAVLink send/subscribe, driver registration, process-spawn allowlist).
//! Mirrors the `ados-supervisor` / `ados-mavlink-router` binary shape: journald
//! logging on Linux with an fmt fallback, a `Type=notify` readiness ping, and a
//! single select loop over the shutdown signals.
//!
//! Token delivery: the daemon builds its [`TokenIssuer`] from a *persisted*
//! HMAC secret (0600 under `/etc/ados/secrets`), not a per-process random key,
//! so a runner started by its own systemd unit can present a token this daemon
//! verifies. When the daemon serves a plugin's socket it mints a fresh token
//! from that shared issuer and writes the 0600 token credential the unit loads
//! with `LoadCredential=`; the runner reads it from `$CREDENTIALS_DIRECTORY`
//! and connects.
//!
//! `ados-plugin-host guard-load` is the privileged pre-start every plugin unit
//! runs: it loads the loopback guard (with the current per-plugin listener
//! exceptions) and exits non-zero when it cannot, so no plugin starts
//! unguarded whatever the boot order.
//!
//! Lifecycle: the served set and the minted tokens are *maintained*, not
//! snapshotted at boot. A [`PluginReconciler`] re-reads the plugin state file
//! on a fixed two-second poll (and on demand, when a lifecycle controller pokes
//! `plugin.reconcile` on the control socket), binding sockets for newly enabled
//! plugins and tearing them down for disabled ones. A second loop re-mints
//! every served plugin's token at half the TTL. See `reconcile.rs` for what
//! each of those loops fixed — a freshly enabled plugin that ran inert, tokens
//! that died at ten minutes, and a revoke that was advisory until restart.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::Result;
use tokio::signal::unix::{signal, SignalKind};
use tokio::task::JoinHandle;

use crate::frame_link::{Declarations, FrameLink, Wire};
use crate::loopback_guard;
use crate::manifest::PluginManifest;
use crate::pic_gate::PicGate;
use crate::realhost::RealHost;
use crate::reconcile::PluginReconciler;
use crate::signing::{load_revocations, Revocations, PLUGIN_REVOCATIONS_PATH};
use crate::supervisor::Paths;
use crate::token_secret::TokenMint;
use crate::vehicle_events::VehicleArmedProbe;
use crate::vision_client::VisionClient;
use crate::{EventBus, PluginIpcServer, PluginSupervisor};
use ados_protocol::armed::ArmedProbe;
use ados_protocol::plugin::TokenIssuer;

mod startup;
mod tasks;

use startup::*;
use tasks::*;

/// The running agent semver, used by the supervisor's compatibility gate. The
/// `ADOS_AGENT_VERSION` env mirrors the Python `ados.__version__` source; the
/// crate version is the inert fallback when the env is unset.
fn agent_version() -> String {
    std::env::var("ADOS_AGENT_VERSION").unwrap_or_else(|_| env!("CARGO_PKG_VERSION").to_string())
}

fn init_logging() {
    use ados_protocol::logd::layer::LogdLayer;
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::EnvFilter;

    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string());

    // The logd layer ships records to the logging daemon's ingest socket
    // alongside the primary sink; it is best-effort and never blocks the service.
    #[cfg(target_os = "linux")]
    {
        if let Ok(journald) = tracing_journald::layer() {
            let _ = tracing_subscriber::registry()
                .with(EnvFilter::new(&filter))
                .with(journald)
                .with(LogdLayer::new("ados-plugin-host"))
                .try_init();
            return;
        }
    }

    let _ = tracing_subscriber::registry()
        .with(EnvFilter::new(&filter))
        .with(tracing_subscriber::fmt::layer())
        .with(LogdLayer::new("ados-plugin-host"))
        .try_init();
}

/// systemd readiness ping. No-op off Linux and when not run under a
/// `Type=notify` unit (`NOTIFY_SOCKET` unset).
#[cfg(target_os = "linux")]
fn sd_ready() {
    if let Err(e) = sd_notify::notify(&[sd_notify::NotifyState::Ready]) {
        tracing::debug!(error = %e, "sd_notify READY failed");
    }
}

#[cfg(not(target_os = "linux"))]
fn sd_ready() {}

/// Read a plugin's manifest off its unpacked install dir. Returns `None` when
/// the manifest is missing or unparseable (the daemon then skips serving that
/// plugin rather than failing the whole boot).
fn read_plugin_manifest(install_dir: &Path, plugin_id: &str) -> Option<PluginManifest> {
    let manifest_path = install_dir.join(plugin_id).join("manifest.yaml");
    let text = std::fs::read_to_string(&manifest_path).ok()?;
    match PluginManifest::from_yaml_text(&text) {
        Ok(m) => Some(m),
        Err(e) => {
            tracing::warn!(plugin_id, error = %e, "plugin manifest unreadable; skipping");
            None
        }
    }
}

/// Run the daemon to completion. Errors print like a `main` returning
/// `Result` and exit 1.
pub fn run() -> ExitCode {
    match serve() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Error: {e:?}");
            ExitCode::FAILURE
        }
    }
}

#[tokio::main]
async fn serve() -> Result<()> {
    init_logging();

    if std::env::args().nth(1).as_deref() == Some("guard-load") {
        return tokio::task::spawn_blocking(guard_load).await?;
    }

    // Every host path from its `ADOS_PLUGIN_*` / `ADOS_RUN_DIR` variable, so a
    // per-user install (the macOS workstation) or a test run points the whole
    // daemon at its own layout. The control socket binds in its own root-only
    // dir, apart from the per-plugin socket dir.
    let paths = Paths::from_env();
    let run = paths.run_dir.clone();
    let version = agent_version();
    let profile = ados_config::node_profile();
    // The board id and tier from the HAL board sidecar. The daemon used to pass
    // `None` for both, which left the compatibility gates inert on the
    // cloud-relay install path.
    let (board_id, board_tier) = PluginSupervisor::board_identity(&run.join("board.json"));

    tracing::info!(
        install_dir = %paths.install_dir.display(),
        socket_dir = %paths.socket_dir.display(),
        run_dir = %run.display(),
        profile = %profile,
        agent_version = %version,
        board_id = ?board_id,
        board_tier = ?board_tier,
        "plugin host daemon starting"
    );

    // The lifecycle controller refuses to grant a capability the default Rust
    // host cannot back (its host method returns not_implemented regardless of
    // wiring) so an operator never hands out a capability that can only error at
    // call time. `require_signed=true` is the safe production default for the
    // live install path. The board id and tier come from the HAL sidecar so the
    // cloud-relay install path applies the same `supported_boards` and
    // `min_tier` gates the LAN path does.
    let ungrantable = RealHost::ungrantable_caps();
    // Publish the same set for the LAN grant path, which runs in another
    // process and cannot derive it; it refuses these grants too.
    let ungrantable_sidecar = run.join(crate::realhost::UNGRANTABLE_CAPS_SIDECAR);
    if let Err(e) = crate::realhost::write_ungrantable_caps(&ungrantable_sidecar, &ungrantable) {
        tracing::warn!(path = %ungrantable_sidecar.display(), error = %e, "could not publish the ungrantable capability list");
    }
    // The armed verdict from the autopilot heartbeats the host reads. Until
    // one arrives a vehicle node counts as armed.
    let vehicle_armed = Arc::new(VehicleArmedProbe::new(is_vehicle_profile(&profile)));
    let mut supervisor = PluginSupervisor::production(paths.clone(), board_id, version)
        .with_profile(profile)
        .with_board_tier(board_tier)
        .with_accounts(crate::plugin_account::local_accounts())
        .with_ungrantable_caps(ungrantable);
    if let Err(e) = supervisor.discover() {
        tracing::error!(error = %e, "plugin discovery failed");
    }

    // Load the loopback guard before any plugin starts. Its verdict gates the
    // `network.outbound` grant and the rendered sandbox of every unit, so the
    // units are refreshed against it right after. Every plugin unit also
    // reloads it in its own pre-start.
    if let Err(e) = crate::plugin_account::ensure_groups() {
        tracing::error!(error = %e, "plugin groups could not be created");
    }
    let ruleset = guard_ruleset(&supervisor);
    let guard_state_path = paths.loopback_guard_state.clone();
    tokio::task::spawn_blocking(move || loopback_guard::install(&ruleset, &guard_state_path))
        .await?;

    // A plugin signed by a key, or installed from an archive, revoked since it
    // was installed is stopped before anything is refreshed or served, unless
    // the vehicle counts as armed: then the watch applies it on disarm.
    let mut revocations = Revocations::default();
    refresh_revocations(&supervisor.revocations_path(), &mut revocations);
    match supervisor.apply_revocations(&revocations, vehicle_armed.treated_as_armed()) {
        Ok(sweep) if !sweep.disabled.is_empty() => {
            tracing::warn!(disabled = ?sweep.disabled, "plugins disabled after a revocation")
        }
        Ok(_) => {}
        Err(e) => tracing::error!(error = %e, "could not apply the revocation list"),
    }
    supervisor.refresh_all_units();
    // Discovery's job here is the state reconciliation + tamper filter it runs
    // as a side effect; the reconciler reads state itself from now on.
    drop(supervisor);
    let revocation_watch =
        spawn_revocation_watch(paths.clone(), Arc::clone(&vehicle_armed), revocations);

    let daemon = wire(&paths, vehicle_armed).await;
    // The shared-secret issuer is owned by the daemon for the session lifetime;
    // it both verifies the runner's `hello` token and backs the mint that
    // writes each served plugin's token credential.
    let _ = &daemon.issuer;
    tracing::info!(
        served = daemon.reconciler.serving().len(),
        "plugin host daemon ready"
    );

    sd_ready();
    crate::vehicle_events::publish_host_event(
        &daemon.bus,
        crate::vehicle_events::TOPIC_AGENT_READY,
        rmpv::Value::Map(vec![]),
    );

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    tokio::select! {
        _ = sigterm.recv() => tracing::info!("received SIGTERM"),
        _ = sigint.recv() => tracing::info!("received SIGINT"),
    }

    tracing::info!("plugin host daemon stopping");
    revocation_watch.abort();
    daemon.shutdown();
    Ok(())
}

#[cfg(test)]
mod tests;
