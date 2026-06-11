//! Native HTTP control surface for the agent.
//!
//! The same status/pairing/command API the GCS speaks, served on the trusted
//! local Unix socket and the LAN TCP port with no Python runtime. It answers the
//! agent's `/api/*` (+ `/healthz`) routes byte-identically to the FastAPI
//! surface, so the same GCS works against either. On the full agent it binds an
//! alternate socket + port and is inert (the GCS still uses FastAPI on `:8080`);
//! on the headless profile it binds `:8080` itself.
//!
//! This crate carries the dual-listener serve loop ([`serve`]), the LAN-edge
//! auth ([`auth`]), the shared app state ([`state`]), the route Router
//! ([`routes`]), and the daemon lifecycle ([`run_with_paths`]). The binary is
//! functional but ships dark — no supervisor registration and no systemd unit
//! enable it yet — until the install layer wires it.

pub mod auth;
pub mod config;
pub mod ipc;
pub mod pairing_store;
pub mod profile;
pub mod routes;
pub mod serve;
pub mod state;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::oneshot;

use crate::auth::PairingState;
use crate::ipc::state_client::{default_state_socket, StateIpcClient};
use crate::routes::build_router;
use crate::serve::{bind_tcp, bind_unix, serve_tcp, serve_unix, tcp_app, unix_app};
use crate::state::{AppState, PairingPaths};

/// Canonical runtime paths. The control socket lives under `/run/ados` (tmpfs);
/// the pairing-state file lives under `/etc/ados` (persistent). The TCP port is
/// the alternate LAN plane on the full agent (the GCS still uses FastAPI on
/// `:8080`); the unit overrides it to `:8080` on the headless profile.
pub mod paths {
    /// Control socket: the trusted local plane (the on-box CLI).
    pub const CONTROL_SOCKET: &str = "/run/ados/control.sock";
    /// The alternate TCP port on the full agent. Inert: the GCS uses the FastAPI
    /// surface on `:8080` until the bench cutover rebinds this surface there.
    pub const CONTROL_TCP_PORT: u16 = 8082;
}

/// How often the daemon pings the systemd watchdog while running. Comfortably
/// under the unit's `WatchdogSec` (a ~3x margin) so a single missed tick from a
/// brief scheduler stall does not trip a restart, but a genuinely wedged async
/// runtime does. Mirrors the sibling daemons.
pub const WATCHDOG_PING_INTERVAL: Duration = Duration::from_secs(10);

/// Resolved paths a daemon run needs: the two listener addresses it owns, the
/// pairing-state file the LAN-edge auth reads, and the vehicle-state socket the
/// status/telemetry routes read.
#[derive(Debug, Clone)]
pub struct DaemonPaths {
    /// The control socket the trusted local plane binds.
    pub control_socket: PathBuf,
    /// The TCP port the LAN plane binds.
    pub control_tcp_port: u16,
    /// The agent pairing-state file the LAN-edge auth reads AND the pairing
    /// routes read + write. Injectable so a test points it at a tempdir.
    pub pairing_path: PathBuf,
    /// The vehicle-state socket the status + telemetry routes read. Injectable so
    /// a test points it at a mock-IPC socket in a tempdir.
    pub state_socket: PathBuf,
    /// The agent config (`/etc/ados/config.yaml`) the pairing-info route projects
    /// for device identity, profile, and the radio peer. Injectable for tests.
    pub config_path: PathBuf,
    /// The WFB key directory (`/etc/ados/wfb`); the presence of `tx.key`/`rx.key`
    /// is the pairing-info `radio_paired` signal. Injectable for tests.
    pub wfb_key_dir: PathBuf,
    /// The WFB bind-session sentinel (`/run/ados/bind-state.json`) the
    /// pairing-info route folds into `bind_state`. Injectable for tests.
    pub bind_state_path: PathBuf,
}

impl Default for DaemonPaths {
    fn default() -> Self {
        // The env overrides mirror the sibling daemons so the unit can pin the
        // port per profile (`:8082` inert on the full agent, `:8080` headless)
        // and a test can redirect the socket/pairing paths without a real
        // `/run/ados` or `/etc/ados`.
        let control_socket = std::env::var("ADOS_CONTROL_SOCKET")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(paths::CONTROL_SOCKET));
        let control_tcp_port = std::env::var("ADOS_CONTROL_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(paths::CONTROL_TCP_PORT);
        let pairing_path = std::env::var("ADOS_PAIRING_JSON")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(auth::DEFAULT_PAIRING_PATH));
        // The state socket resolves under `ADOS_RUN_DIR` (the same override the
        // Python `ados.core.ipc` honours), defaulting to `/run/ados/state.sock`.
        let state_socket = default_state_socket();
        // The config path honours `ADOS_CONFIG` (the same override the sibling
        // crates use), defaulting to `/etc/ados/config.yaml`.
        let config_path = std::env::var("ADOS_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(config::CONFIG_YAML));
        // The WFB key dir is fixed at `/etc/ados/wfb` (no env override in the
        // Python source); injectable directly on `DaemonPaths` for tests.
        let wfb_key_dir = PathBuf::from("/etc/ados/wfb");
        // The bind-state sentinel resolves under `ADOS_RUN_DIR`, defaulting to
        // `/run/ados/bind-state.json` (matching the Python `BIND_STATE_SENTINEL`).
        let run_dir = std::env::var("ADOS_RUN_DIR").unwrap_or_else(|_| "/run/ados".to_string());
        let bind_state_path = Path::new(&run_dir).join("bind-state.json");
        Self {
            control_socket,
            control_tcp_port,
            pairing_path,
            state_socket,
            config_path,
            wfb_key_dir,
            bind_state_path,
        }
    }
}

/// systemd readiness ping. No-op off Linux and when not run under a
/// `Type=notify` unit (`NOTIFY_SOCKET` unset).
#[cfg(target_os = "linux")]
fn sd_ready() {
    if let Err(e) = sd_notify::notify(false, &[sd_notify::NotifyState::Ready]) {
        tracing::debug!(error = %e, "sd_notify READY failed");
    }
}

#[cfg(not(target_os = "linux"))]
fn sd_ready() {}

/// systemd stopping ping. No-op off Linux / outside a notify unit.
#[cfg(target_os = "linux")]
fn sd_stopping() {
    if let Err(e) = sd_notify::notify(false, &[sd_notify::NotifyState::Stopping]) {
        tracing::debug!(error = %e, "sd_notify STOPPING failed");
    }
}

#[cfg(not(target_os = "linux"))]
fn sd_stopping() {}

/// systemd watchdog keep-alive ping. No-op off Linux and when not run under a
/// `WatchdogSec`-armed `Type=notify` unit (`WATCHDOG_USEC` unset).
#[cfg(target_os = "linux")]
fn sd_watchdog() {
    let _ = sd_notify::notify(false, &[sd_notify::NotifyState::Watchdog]);
}

#[cfg(not(target_os = "linux"))]
fn sd_watchdog() {}

/// Run the daemon to completion: bind both listeners, serve the shared Router on
/// each, wait for `SIGTERM`/`SIGINT`, shut down cleanly. The production entry.
pub async fn run_daemon() -> Result<()> {
    run_with_paths(DaemonPaths::default(), shutdown_signal()).await
}

/// The lifecycle, parameterized over the paths and the stop trigger so tests can
/// drive a real bring-up + shutdown against temp sockets without sending a
/// process signal.
///
/// Both listeners are bound up front so a bind clash (the FastAPI surface
/// already on the port, a stale socket) surfaces here rather than inside a
/// spawned task — important for the inert dual-run, where a port collision is
/// the first thing the cutover must rule out.
pub async fn run_with_paths<F>(paths: DaemonPaths, shutdown: F) -> Result<()>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let pairing = Arc::new(PairingState::with_path(paths.pairing_path.clone()));

    // The vehicle-state reader the status + telemetry routes project. Its
    // background task connects to the state socket and reconnects on EOF / an
    // absent socket; an idle agent (no socket) leaves the snapshot empty, which
    // the routes degrade to rather than fail. The handle stops it on shutdown.
    let (state_client, state_handle) = StateIpcClient::spawn(paths.state_socket.clone());
    // The on-disk paths the pairing routes read + write. The pairing-state file
    // is the same one the LAN-edge auth reader watches, so the gate and the
    // claim/unpair writers agree on one file.
    let pairing_paths = PairingPaths {
        config: paths.config_path.clone(),
        pairing_json: paths.pairing_path.clone(),
        wfb_key_dir: paths.wfb_key_dir.clone(),
        bind_state: paths.bind_state_path.clone(),
    };
    let state = AppState::new(Arc::clone(&pairing), state_client, pairing_paths);

    // The Unix edge: the bare Router, no auth. The LAN edge: the same Router
    // wrapped with the rate-limit + auth layer keyed on the shared pairing
    // reader (so a route and the gate read one short-TTL-cached posture).
    let unix_router = unix_app(build_router(state.clone()));
    let tcp_router = tcp_app(build_router(state), Arc::clone(&pairing));

    // Bind both listeners up front so a bind failure surfaces here.
    let unix_listener = bind_unix(&paths.control_socket)
        .with_context(|| format!("bind control socket {}", paths.control_socket.display()))?;
    let tcp_listener = bind_tcp(paths.control_tcp_port).await?;
    tracing::info!(
        socket = %paths.control_socket.display(),
        tcp_port = paths.control_tcp_port,
        "control API listening"
    );

    // Two graceful-shutdown signals fan out from the single shutdown future.
    let (unix_stop_tx, unix_stop_rx) = oneshot::channel::<()>();
    let (tcp_stop_tx, tcp_stop_rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        shutdown.await;
        let _ = unix_stop_tx.send(());
        let _ = tcp_stop_tx.send(());
    });

    let unix = tokio::spawn(serve_unix(unix_listener, unix_router, unix_stop_rx));
    let tcp = tokio::spawn(serve_tcp(tcp_listener, tcp_router, tcp_stop_rx));

    sd_ready();

    // Run until the stop trigger fires, pinging the systemd watchdog on a fixed
    // cadence in between. A wedged async runtime stops pinging and systemd
    // restarts the unit; a healthy daemon keeps the timer fed.
    let mut watchdog = tokio::time::interval(WATCHDOG_PING_INTERVAL);
    watchdog.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // The first immediate tick fires right after READY; consume it so the
    // cadence stays a steady WATCHDOG_PING_INTERVAL.
    watchdog.tick().await;
    let serving = async {
        let _ = unix.await;
        let _ = tcp.await;
    };
    tokio::pin!(serving);
    loop {
        tokio::select! {
            _ = &mut serving => break,
            _ = watchdog.tick() => sd_watchdog(),
        }
    }
    sd_stopping();

    // Stop the state reader before exiting so its task does not outlive the run.
    state_handle.shutdown().await;

    // tmpfs cleanup: a stale socket path confuses a probing reader on restart.
    let _ = std::fs::remove_file(&paths.control_socket);
    tracing::info!("control API stopped");
    Ok(())
}

/// Resolve when the process receives `SIGTERM` or `SIGINT`. The production stop
/// trigger.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "failed to install SIGTERM handler");
                return;
            }
        };
        let mut sigint = match signal(SignalKind::interrupt()) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "failed to install SIGINT handler");
                return;
            }
        };
        tokio::select! {
            _ = sigterm.recv() => tracing::info!("received SIGTERM"),
            _ = sigint.recv() => tracing::info!("received SIGINT"),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("received interrupt");
    }
}

/// Hand a freshly-bound socket to the `ados` group so a non-root operator in
/// that group can reach the trusted local plane. The bind sets the mode to
/// `0o660`, which only grants the group once the group actually owns the file.
/// Best-effort: the installer creates the group, and when it is absent (a dev
/// host) this is a quiet no-op so bring-up stays automatic. Linux-only; a stub
/// elsewhere. Mirrors the logd helper.
#[cfg(target_os = "linux")]
pub(crate) fn set_ados_group(path: &Path) {
    match nix::unistd::Group::from_name("ados") {
        Ok(Some(g)) => {
            if let Err(err) = nix::unistd::chown(path, None, Some(g.gid)) {
                tracing::debug!(error = %err, path = %path.display(), "chgrp control socket failed");
            }
        }
        Ok(None) => {
            tracing::debug!("ados group not present; leaving socket group as-is");
        }
        Err(err) => {
            tracing::debug!(error = %err, "resolving ados group failed");
        }
    }
}

/// Non-Linux stub: socket group ownership is a Linux-only concern. Unused on a
/// dev host (the call site is itself Linux-gated), hence the allow.
#[cfg(not(target_os = "linux"))]
#[allow(dead_code)]
pub(crate) fn set_ados_group(_path: &Path) {}
