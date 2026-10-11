//! `ados-crsf` binary — the CRSF / ExpressLRS RC control lane service.
//!
//! Opens the pinned USB-serial RC transmitter module at 420 kbaud, transmits
//! the packed RC channels frame at the configured cadence, decodes returned
//! telemetry, serves the command socket, and writes the lane state sidecar
//! every second. Idles harmlessly (with an honest `disabled` sidecar) when
//! the lane is not opted in or this node's profile does not run it, and shuts
//! down cleanly on SIGTERM/SIGINT.
//!
//! ## Port ownership per `radio.crsf.mode`
//!
//! The module's port has exactly one owner, decided by the configured mode:
//!
//! - **`crsf_rc`** — THIS service owns the pinned device (CRSF framing at
//!   420 kbaud); the MAVLink router excludes the pin from FC candidacy.
//! - **`mavlink`** — the module runs its native MAVLink mode: a MAVLink byte
//!   carrier (the module firmware owns the CRSF air protocol internally). The
//!   MAVLink router ingests the carrier as its FC source — the pinned device
//!   at the fixed MAVLink-mode baud, or the WiFi backpack's UDP listen — so
//!   this service NEVER opens the port. It stands by at state `ready` with
//!   `mode: "mavlink"` reported: alive, honest about why no RC is transmitted,
//!   and reclaiming the port on the next reload that flips the mode back. The
//!   router reads telemetry up but keeps the host->FC command-down direction
//!   gated closed by default (`radio.crsf.mavlink_command_enabled`), so the
//!   source is telemetry-only until that marker is set. The command socket is
//!   not served (channel injection has no lane to land on and reads `503` at
//!   the control plane).
//! - **`airport`** — a generic serial data pipe with no ADOS owner yet; the
//!   lane reads `disabled` with the mode reported.
//!
//! SIGHUP is the in-process config reload (the unit's `ExecReload`): the
//! current bring-up tears down cleanly, `/etc/ados/config.yaml` is re-read,
//! and every gate (opt-in, profile, mode, device pin) re-runs — so enabling
//! or re-pinning the lane through the config surface applies without
//! dropping the unit.

use ados_protocol::shutdown::Shutdown;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{watch, Mutex, Notify};

use crate::cmdsock::{self, CmdState};
use crate::config::{profile_is_drone, CrsfLaneConfig, LaneMode};
use crate::link::{derive_state, LaneState, LinkInputs};
use crate::sidecar::{build_stats_value, write_stats_sidecar, StatsInputs};
use crate::sources::{ChannelSourceMode, SourceMerge};
use crate::transport::{open_serial, run_rx, run_tx, TelemetryState, WireCounters, CRSF_BAUD};

const CONFIG_YAML: &str = "/etc/ados/config.yaml";
const PROFILE_CONF: &str = "/etc/ados/profile.conf";
/// Sidecar heartbeat cadence while the lane runs.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);
/// Backoff between bring-up attempts (no device, open failure, port death).
const RETRY_INTERVAL: Duration = Duration::from_secs(5);
/// Sidecar refresh cadence while the lane idles in a terminal state
/// (disabled / wrong profile / non-RC mode). The body does not change; the
/// rewrite keeps the file's mtime fresh so a staleness-gated reader can tell
/// a live idle lane from a dead service's orphaned file. Half the tightest
/// consumer window (the heartbeat's 10 s gate), so an idle lane never flaps
/// in and out of the heartbeat at the staleness boundary.
const IDLE_REFRESH_INTERVAL: Duration = Duration::from_secs(5);

/// The `ados-crsf` binary's entry point.
pub fn run() -> std::process::ExitCode {
    serve();
    std::process::ExitCode::SUCCESS
}

#[tokio::main]
async fn serve() {
    {
        use ados_protocol::logd::layer::LogdLayer;
        use tracing_subscriber::prelude::*;

        // fmt as the primary sink plus the logd layer that ships records to
        // the logging daemon's ingest socket; the logd layer is best-effort
        // and never blocks the service.
        let filter =
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
        let _ = tracing_subscriber::registry()
            .with(filter)
            .with(tracing_subscriber::fmt::layer())
            .with(LogdLayer::new("ados-crsf"))
            .try_init();
    }
    tracing::info!("crsf_service_starting");

    // Shutdown is a latching watch flag, not a one-shot notify: once SIGTERM
    // flips it to true the value stays set, so a select arm that loses a race
    // on the first signal still sees the shutdown on the next loop iteration.
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    tokio::spawn(async move {
        wait_for_shutdown().await;
        let _ = shutdown_tx.send(true);
    });

    // SIGHUP = in-process config reload. `notify_one` stores a permit when no
    // arm is waiting, so a reload landing mid-teardown is not lost; a burst of
    // reloads coalesces into one re-read.
    let reload = Arc::new(Notify::new());
    #[cfg(unix)]
    {
        let reload = reload.clone();
        tokio::spawn(async move {
            use tokio::signal::unix::{signal, SignalKind};
            let Ok(mut hup) = signal(SignalKind::hangup()) else {
                return;
            };
            while hup.recv().await.is_some() {
                tracing::info!("crsf_config_reload_signal");
                reload.notify_one();
            }
        });
    }

    run_until_shutdown(
        Path::new(CONFIG_YAML),
        Path::new(PROFILE_CONF),
        shutdown_rx,
        reload,
    )
    .await;
    tracing::info!("crsf_service_stopped");
}

/// Why one service pass returned: the process is stopping, or a config
/// reload asked for a fresh pass over a re-read config.
#[derive(Debug, PartialEq, Eq)]
enum RunExit {
    Shutdown,
    Reload,
}

/// The reload loop: (re)read the config and run the service until shutdown.
/// A `Reload` exit tears the pass down cleanly and re-runs every gate against
/// the fresh config; `Shutdown` ends the process.
async fn run_until_shutdown(
    config_path: &Path,
    profile_conf: &Path,
    shutdown: watch::Receiver<bool>,
    reload: Arc<Notify>,
) {
    loop {
        if *shutdown.borrow() {
            return;
        }
        let cfg = CrsfLaneConfig::load_from(config_path);
        match run_service(
            &cfg,
            config_path,
            profile_conf,
            shutdown.clone(),
            reload.clone(),
        )
        .await
        {
            RunExit::Shutdown => return,
            RunExit::Reload => {
                tracing::info!("crsf_config_reloaded");
            }
        }
    }
}

/// One pass of the service under one loaded config. Owns the per-pass worker
/// set: the HID source (when the channel-source mode wants it) is scoped to
/// this pass via a latching stop flag, so a reload never leaks a second
/// reader onto the same gamepad.
async fn run_service(
    cfg: &CrsfLaneConfig,
    config_path: &Path,
    profile_conf: &Path,
    shutdown: watch::Receiver<bool>,
    reload: Arc<Notify>,
) -> RunExit {
    let (pass_over_tx, pass_over_rx) = watch::channel(false);
    let exit = run_service_pass(
        cfg,
        config_path,
        profile_conf,
        shutdown,
        reload,
        pass_over_rx,
    )
    .await;
    // Latch the pass-scope flag so per-pass workers (the HID source) stop.
    let _ = pass_over_tx.send(true);
    exit
}

mod pass;
use pass::run_service_pass;

/// Wait until the latched shutdown watch flips to `true`. Returns immediately
/// if it is already set (the latch never loses an edge) and on a closed
/// channel (the sender dropped — treat as shutdown).
async fn wait_for_shutdown_flag(shutdown: &mut watch::Receiver<bool>) {
    let _ = shutdown.wait_for(|s| *s).await;
}

/// Idle in a terminal gate state until shutdown or a config reload, rewriting
/// the honest sidecar on a slow cadence so its mtime keeps proving the
/// service is alive (a staleness-gated reader must be able to tell a live
/// idle lane from a dead one). `state` is the gate's verdict — `disabled`
/// when the lane does not run on this node, `ready` when it stands by with
/// the port handed to the MAVLink router in `mavlink` mode — and `inputs`
/// carries the gate's config-level facts (the lane mode, a relay role).
async fn idle_with_sidecar(
    state: LaneState,
    metrics: &ados_protocol::logd::emitter::IngestEmitter,
    shutdown: &mut watch::Receiver<bool>,
    reload: &Notify,
    inputs: &StatsInputs<'_>,
) -> RunExit {
    write_stats_sidecar(state, inputs, Some(metrics));
    loop {
        tokio::select! {
            biased;
            _ = wait_for_shutdown_flag(shutdown) => return RunExit::Shutdown,
            _ = reload.notified() => return RunExit::Reload,
            _ = tokio::time::sleep(IDLE_REFRESH_INTERVAL) => {
                write_stats_sidecar(state, inputs, Some(metrics));
            }
        }
    }
}

/// Resolve when SIGTERM or SIGINT is received.
async fn wait_for_shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
        let mut int = signal(SignalKind::interrupt()).expect("SIGINT handler");
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod shutdown_tests;
