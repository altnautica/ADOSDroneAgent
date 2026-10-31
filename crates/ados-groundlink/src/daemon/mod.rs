//! The ground-station data-plane service. The `[[bin]]` is a thin wrapper over [`run`].
//!
//! Dispatches on the mesh role: `direct` runs the standalone WFB receive
//! manager (`receive::receive_loop`); `relay` forwards drone fragments to a
//! receiver over batman-adv; `receiver` aggregates the local NIC + remote relay
//! forwards and republishes the combined FEC stream. Each role runs as its own
//! systemd unit (`ados-wfb-rx` / `ados-wfb-relay` / `ados-wfb-receiver`), which
//! passes its role on `--role`. The supervisor decides which unit runs; a unit
//! started while the `/etc/ados/mesh/role` sentinel names another role exits
//! cleanly without touching the adapter.
//!
//! Direct-role detail: per generation it spawns the data RX + both control
//! planes, starts the video fan-out and the presence emit/listen loops as
//! sub-services, and runs the stats reader, the valid-packet watchdog, and the
//! stdout-silence zombie watchdog concurrently. When the data RX exits (or a
//! watchdog terminates it), the generation ends and the loop respawns with a
//! bounded backoff.
//!
//! Adapter detection for the direct receive plane takes the already-prepared
//! interface from config; the relay/receiver roles run the shared radio
//! selector themselves (adapter detect + monitor mode) before spawning their
//! forwarder/aggregator. The rx-key pairing gate and regulatory-domain/tx-power
//! application stay where they were.

mod direct;
mod generation;
mod ground_video;
mod receive;
mod services;

use ados_protocol::shutdown::Shutdown;
use std::process::ExitCode;
use std::sync::atomic::AtomicU32;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::Mutex;

use ados_radio::config::WfbConfig;
use ados_radio::link_quality::LinkStats;

use crate::wfb_rx::{
    self, DataRxHandle, IwChannelSetter, SharedValidCounter, SlotReceivers, SystemClock,
    WfbRxManager,
};
use crate::{
    fanout, mesh, presence, receiver, relay, FleetRegistry, GsPresenceCache,
    FLEET_RECONCILE_INTERVAL, FLEET_REGISTRY_PATH,
};
use direct::run_direct;

const CONFIG_YAML: &str = "/etc/ados/config.yaml";
const RX_KEY: &str = ados_radio::paths::WFB_RX_KEY;

/// How often the unpaired-state sidecar is refreshed while the pairing gate
/// blocks. Comfortably inside every reader's staleness window, and far below the
/// 5 s poll cadence, so an unpaired ground station stays visible without writing
/// to the card twelve times a minute.
const UNPAIRED_SIDECAR_REFRESH: Duration = Duration::from_secs(20);

/// Fixed retry between receive-plane attempts: adapter detect, the regulatory
/// gate, the chain spawn and the generation restart all wait exactly this long.
///
/// Flat, with no ceiling and no attempt cap. The ladder this replaces doubled to
/// a 30 s ceiling, so a ground station whose RTL adapter re-enumerated after a
/// USB glitch — or whose regulatory gate transiently blocked — left the operator
/// staring at a black video panel for up to half a minute after the hardware was
/// already back.
const RETRY_INTERVAL: Duration = Duration::from_secs(5);

/// The cadence every role loop stamps its progress marker at, and therefore the
/// interval the systemd watchdog budget is derived from.
///
/// The ping is COUPLED to that marker rather than free-running, for the same
/// reason as the supervisor's: liveness for this unit is not "the process
/// exists" and not "the tokio runtime still schedules tasks". A wedged role loop
/// satisfies both while the receive plane, the channel acquirer and the hop
/// follower have all stopped — which is precisely the failure that previously
/// needed a power cycle or SSH, because the unit was `Type=simple` and nothing
/// could observe it. A free-running ping would reproduce that hole with a green
/// check on top.
const PROGRESS_INTERVAL: Duration = Duration::from_secs(5);

/// The run role the service dispatches on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Direct,
    Relay,
    Receiver,
}

impl Role {
    /// Parse a role token, returning `None` for anything that is not one of the
    /// three known values (so the caller can distinguish "unknown" from a real
    /// role and log accordingly).
    fn from_token(token: &str) -> Option<Self> {
        match token {
            "direct" => Some(Self::Direct),
            "relay" => Some(Self::Relay),
            "receiver" => Some(Self::Receiver),
            _ => None,
        }
    }
}

/// The role this process runs, from its `--role <value>` / `--role=<value>`
/// argument. The unit decides the role, not the sentinel: a unit that ran
/// whatever role the sentinel named put a second plane on the adapter after a
/// reboot into another role. A missing or unknown value is a unit-file error,
/// so it fails loud instead of guessing.
fn resolve_role() -> Result<Role, String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    resolve_role_from(&args)
}

/// Pure role-resolution core (test seam).
fn resolve_role_from(args: &[String]) -> Result<Role, String> {
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let value = if arg == "--role" {
            it.next().map(String::as_str)
        } else {
            arg.strip_prefix("--role=")
        };
        if let Some(v) = value {
            return Role::from_token(v).ok_or_else(|| {
                format!("unknown --role {v:?}; expected direct, relay or receiver")
            });
        }
    }
    Err("--role is required: direct, relay or receiver".to_string())
}

/// Whether the role sentinel names this unit's role. A role unit started while
/// the node runs another role (pulled up with the supervisor at boot, kicked by
/// the installer, a stray `systemctl start`) must not touch the adapter the
/// other role's plane drives.
fn sentinel_admits(unit_role: Role, sentinel: &str) -> bool {
    Role::from_token(sentinel) == Some(unit_role)
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
                .with(LogdLayer::new("ados-groundlink"))
                .try_init();
            return;
        }
    }

    let _ = tracing_subscriber::registry()
        .with(EnvFilter::new(&filter))
        .with(tracing_subscriber::fmt::layer())
        .with(LogdLayer::new("ados-groundlink"))
        .try_init();
}

/// Whether this process runs the ground media server (`--ground-video`, the
/// `ados-mediamtx-gs` unit) instead of a receive-plane role.
fn wants_ground_video(args: &[String]) -> bool {
    args.iter().any(|a| a == "--ground-video")
}

/// Run the ground mediamtx until SIGTERM/SIGINT.
async fn run_ground_video() -> Result<()> {
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    let shutdown = Shutdown::new();
    let run_dir = ados_protocol::paths::run_dir();
    let task = tokio::spawn(ground_video::run(shutdown.clone(), run_dir));
    tokio::select! {
        _ = sigterm.recv() => tracing::info!("received SIGTERM"),
        _ = sigint.recv() => tracing::info!("received SIGINT"),
    }
    shutdown.trigger();
    let _ = task.await;
    Ok(())
}

/// Run the ground-station data-plane, the ground media server, the mesh
/// pairing service or the mesh bring-up to completion.
#[tokio::main]
pub async fn run() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(mode) = services::service_mode(&args) {
        return services::run(mode).await;
    }
    match run_service().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Error: {e:?}");
            ExitCode::FAILURE
        }
    }
}

async fn run_service() -> Result<()> {
    // The ground media server is its own unit and carries no role.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if wants_ground_video(&args) {
        init_logging();
        tracing::info!("ground media server starting");
        return run_ground_video().await;
    }

    init_logging();
    // Resolved before readiness: a unit started without a usable role must fail
    // its start, never report ready and run a guessed role on the adapter.
    let role = match resolve_role() {
        Ok(role) => role,
        Err(e) => {
            tracing::error!(error = %e, "ground-station data-plane has no role");
            anyhow::bail!(e);
        }
    };

    // Not this node's role: report ready (a notify unit that exits before READY
    // is a failed start) and exit 0, the status each role unit's
    // `RestartPreventExitStatus=0` never restarts.
    let sentinel = mesh::get_current_role();
    if !sentinel_admits(role, &sentinel) {
        tracing::info!(?role, sentinel = %sentinel, "role unit does not match the node role; exiting");
        ados_supervisor::sdnotify::ready();
        return Ok(());
    }

    // Publish this service's config-status sidecar so a malformed `ground_station:`
    // config block surfaces on the remote Health view, not just in the log. Read
    // once at startup; the role loops re-read the (unchanged) file as they consume
    // it. Best-effort — never blocks startup.
    crate::GroundStationConfig::publish_config_status(std::path::Path::new(CONFIG_YAML));

    // Tell systemd we are up (reuses the orchestrator's notify shim).
    ados_supervisor::sdnotify::ready();

    // The systemd watchdog keep-alive, fed only while the running role loop
    // keeps stamping progress. Each role marks it at its own stage boundaries;
    // see [`PROGRESS_INTERVAL`] for why the ping is coupled rather than free.
    let progress = ados_supervisor::sdnotify::MonitorProgress::new();
    ados_supervisor::sdnotify::spawn_watchdog_pinger(progress.clone(), PROGRESS_INTERVAL);

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;

    // The operator command socket runs for the whole service lifetime in every
    // role: gateway-preference and WFB pair-key install / unpair are operator
    // on-demand actions the native front forwards here (it has no in-process
    // Python pair manager to call). Spawned before the role dispatch so it is
    // reachable regardless of which role loop runs below.
    tokio::spawn(async {
        // Honour ADOS_RUN_DIR so a redirected runtime layout (a non-root dev host
        // or a test) places the socket alongside the other run-dir sockets.
        let sock = crate::paths::run_path("groundlink-cmd.sock");
        if let Err(e) = crate::cmdsock::serve(std::path::Path::new(&sock)).await {
            tracing::warn!(error = %e, path = %sock, "groundlink command socket exited");
        }
    });

    match role {
        Role::Relay => {
            tracing::info!("ground-station relay role starting");
            run_relay_or_receiver(true, progress, &mut sigterm, &mut sigint).await;
        }
        Role::Receiver => {
            tracing::info!("ground-station receiver role starting");
            run_relay_or_receiver(false, progress, &mut sigterm, &mut sigint).await;
        }
        Role::Direct => {
            run_direct(progress, &mut sigterm, &mut sigint).await?;
        }
    }

    tracing::info!("ground-station data-plane stopping");
    Ok(())
}

/// Run the relay (`is_relay`) or receiver loop until a shutdown signal. The
/// chosen loop owns its own adapter detect + monitor-mode + mDNS + state file;
/// a SIGTERM/SIGINT fires the shared latching `Shutdown` so the loop tears down cleanly.
async fn run_relay_or_receiver(
    is_relay: bool,
    progress: ados_supervisor::sdnotify::MonitorProgress,
    sigterm: &mut tokio::signal::unix::Signal,
    sigint: &mut tokio::signal::unix::Signal,
) {
    let shutdown = Shutdown::new();

    // Telemetry emitter for the relay/receiver branch: ships the mesh snapshot
    // and the relay/receiver state to the logging daemon as the durable read
    // source the REST layer reads back. Best-effort and non-blocking; the direct
    // path constructs its own inside the receive loop. A second instance in this
    // process is fine.
    let ingest = ados_protocol::logd::emitter::IngestEmitter::new("ados-groundlink");

    // Observability: publish the mesh snapshot (neighbors / gateways /
    // selected-gateway) so the REST layer + OLED see the fabric. This is the
    // same poll the direct path skips; the relay/receiver FEC supervision below
    // is independent of it.
    let role_label = if is_relay { "relay" } else { "receiver" };
    let snap = mesh::MeshSnapshot::new(role_label, "bat0", "802.11s");
    tokio::spawn(mesh::run_poll_loop(snap, Some(ingest.clone())));

    let role_task = {
        let shutdown = shutdown.clone();
        let ingest = Some(ingest.clone());
        tokio::spawn(async move {
            if is_relay {
                relay::run(shutdown, ingest, progress).await;
            } else {
                receiver::run(shutdown, ingest, progress).await;
            }
        })
    };
    tokio::select! {
        _ = role_task => {}
        _ = sigterm.recv() => {
            tracing::info!("received SIGTERM");
            shutdown.trigger();
        }
        _ = sigint.recv() => {
            tracing::info!("received SIGINT");
            shutdown.trigger();
        }
    }
    // Give the loop a moment to flush its down-state on signal-triggered exit.
    tokio::time::sleep(Duration::from_millis(200)).await;
}

#[cfg(test)]
mod tests;
