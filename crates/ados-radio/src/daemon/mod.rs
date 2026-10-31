//! The `ados-radio` service — the WFB TX service for the drone profile.
//!
//! Waits for the WFB TX key, selects the injection adapter, sets monitor mode,
//! spawns `wfb_tx` in its own process group (setsid + killpg, so a wfb binary
//! never outlives the service), runs the liveness watchdogs and the FHSS hop
//! supervisor UDP tasks, writes Contract E sidecars, and shuts down cleanly on
//! SIGTERM/SIGINT. The `[[bin]]` is a thin wrapper over [`run`].

// Focused sibling modules the orchestrator (`run` + `service::run_service`)
// drives. Each groups one concern; the service behaves identically.
mod bringup;
mod heartbeat;
mod hop_supervisor;
mod service;
mod sidecar;
mod signals;
mod status_emit;
mod tasks;
mod txrate;

use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use tokio::sync::watch;

use crate::adapter;
use crate::config::WfbConfig;
use signals::{wait_for_shutdown, wait_for_shutdown_flag};

const CONFIG_YAML: &str = "/etc/ados/config.yaml";
/// Poll interval while waiting for the WFB TX key (unpaired state).
const KEY_WAIT_INTERVAL: Duration = Duration::from_secs(5);
/// Minimum gap between two heartbeat regulatory reconciles. One reconcile runs
/// `iw reg set` with a bounded verify retry (~10 s worst case); a domain that
/// will not take must not re-run it on every 2 s tick.
const REG_RECONCILE_INTERVAL: Duration = Duration::from_secs(30);

/// Run the radio service (or the one-shot adapter list) to completion.
#[tokio::main]
pub async fn run() -> ExitCode {
    // One-shot adapter-list mode: scan once, print the JSON list to stdout, and
    // exit 0 WITHOUT entering the service loop. This is the seam a thin Python
    // shim and any pre-service caller invokes when no radio service is running
    // (e.g. the bind iface setup or the REST adapter endpoint on a fresh box).
    // No tracing init here — stdout must carry ONLY the JSON document.
    let args: Vec<String> = std::env::args().collect();
    if args
        .iter()
        .skip(1)
        .any(|a| a == "adapters" || a == "--list-adapters")
    {
        run_list_adapters().await;
        return ExitCode::SUCCESS;
    }

    {
        use ados_protocol::logd::layer::LogdLayer;
        use tracing_subscriber::prelude::*;

        // fmt as the primary sink (this binary has no journald layer) plus the
        // logd layer that ships records to the logging daemon's ingest socket;
        // the logd layer is best-effort and never blocks the service. This runs
        // only on the service path, after the one-shot adapter-list mode has
        // already returned (that mode must keep stdout pure JSON).
        let filter =
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
        let _ = tracing_subscriber::registry()
            .with(filter)
            .with(tracing_subscriber::fmt::layer())
            .with(LogdLayer::new("ados-radio"))
            .try_init();
    }
    tracing::info!("wfb_service_starting");

    let mut cfg = WfbConfig::load_from(Path::new(CONFIG_YAML));
    // Fold the operator-facing link preset onto the MCS/FEC trio before the
    // radio comes up so the data plane spawns at the preset's tunables. The
    // default `conservative` is a no-op (the explicit config stands).
    cfg.apply_link_preset();
    let cfg = cfg;

    // Profile gate: the WFB TX service is drone-only. On a ground station this
    // binary must idle (the GS runs ados-wfb-rx) so it doesn't clobber the GS's
    // wfb-stats.json. Defensive — the supervisor already profile-gates the unit.
    if crate::config::profile_is_ground_station(
        Path::new(CONFIG_YAML),
        Path::new(crate::config::PROFILE_CONF),
    ) {
        tracing::warn!("wfb_tx_idle_on_ground_station_profile");
        wait_for_shutdown().await;
        return ExitCode::SUCCESS;
    }

    // Fleet identity gate (below, after the signal task). A drone that radiates
    // on the wrong `link_id` shares a `channel_id` with whoever owns it — the
    // ground station, or another drone — and the wfb-ng `Aggregator` re-inits
    // its FEC decoder on every foreign session packet, so the two thrash each
    // other roughly once a second. That presents as unexplained link loss, never
    // as a config fault, so the radio REFUSES to come up on an unusable identity
    // rather than defaulting one. Parking (rather than exiting) means a slot
    // written by the pair flow is picked up on the next poll without a service
    // restart: a freshly bound drone self-heals.
    //
    // Shutdown is a latching watch flag, not a one-shot `Notify`: once SIGTERM
    // flips it to `true` the value STAYS set, so a select arm that loses a race
    // on the first signal (e.g. a watchdog task finishing in the same poll) still
    // sees the shutdown on the next loop iteration, and any later SIGTERM is a
    // no-op rather than a lost edge. Created before the fleet-identity park so a
    // SIGTERM that lands while the config is being re-read is never missed: the
    // one signal task listens for the whole process lifetime.
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);

    // ── Signal handler ────────────────────────────────────────────────────
    tokio::spawn(async move {
        wait_for_shutdown().await;
        // Send always succeeds while the receiver lives; if the service already
        // returned there is nothing left to signal.
        let _ = shutdown_tx.send(true);
    });

    let Some(cfg) = wait_for_fleet_identity(cfg, &mut shutdown_rx).await else {
        return ExitCode::SUCCESS;
    };

    service::run_service(&cfg, shutdown_rx).await;
    tracing::info!("wfb_service_stopped");
    ExitCode::SUCCESS
}

/// One-shot adapter list: scan, print the JSON array to stdout, exit. The
/// document is the same `WifiAdapterInfo` list the service writes to the
/// adapters sidecar, so a caller gets identical data whether it reads the file
/// or invokes this mode.
async fn run_list_adapters() {
    let adapters = adapter::detect_wfb_adapters().await;
    match serde_json::to_string(&adapters) {
        Ok(s) => println!("{s}"),
        // Serialization of a Vec<WifiAdapterInfo> cannot fail in practice; emit
        // an empty array rather than nothing so the caller always parses JSON.
        Err(_) => println!("[]"),
    }
}

/// Park until this drone's configured fleet identity is usable, then return the
/// config to bring the radio up with. `None` means SIGTERM/SIGINT arrived while
/// parked (the latched `shutdown` watch flipped).
///
/// The first pass reuses the already-loaded `cfg` (the caller has folded the
/// link preset onto it); every retry re-reads `/etc/ados/config.yaml` and
/// re-applies the preset, so a slot written by the pair flow while this loop is
/// parked is picked up without a service restart.
async fn wait_for_fleet_identity(
    cfg: WfbConfig,
    shutdown: &mut watch::Receiver<bool>,
) -> Option<WfbConfig> {
    let mut cfg = cfg;
    let mut logged = false;
    loop {
        let Some(err) = crate::config::fleet_identity_error(
            cfg.fleet_id,
            cfg.fleet_slot,
            /* is_ground_station = */ false,
        ) else {
            if logged {
                tracing::info!(
                    fleet_id = cfg.fleet_id,
                    fleet_slot = cfg.fleet_slot,
                    "wfb_fleet_identity_recovered"
                );
                ados_config::write_config_status("radio", None);
            }
            return Some(cfg);
        };
        // Log + publish once per fault, not once per poll: the status sidecar is
        // level-triggered and a 5 s log loop would bury the rest of the journal.
        if !logged {
            tracing::error!(
                fleet_id = cfg.fleet_id,
                fleet_slot = cfg.fleet_slot,
                reason = %err,
                "wfb_fleet_identity_invalid: radio parked, not radiating"
            );
            ados_config::write_config_status("radio", Some(&err.to_string()));
            logged = true;
        }
        tokio::select! {
            biased;
            _ = wait_for_shutdown_flag(shutdown) => return None,
            _ = tokio::time::sleep(KEY_WAIT_INTERVAL) => {}
        }
        cfg = WfbConfig::load_from(Path::new(CONFIG_YAML));
        cfg.apply_link_preset();
    }
}

#[cfg(test)]
mod tests;
