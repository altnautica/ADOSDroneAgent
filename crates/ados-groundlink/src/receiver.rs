//! WFB receiver: FEC-combine fragments from the local NIC + remote relays.
//!
//! `wfb_rx -p 0 -c 127.0.0.1 -u 5600 -a <listen_port> [<drone_iface>...]`
//! aggregates fragments arriving on the local monitor adapters AND from relays
//! forwarding over batman-adv into the aggregator UDP port, FEC-combines them,
//! and emits the decoded stream to localhost UDP 5600 where the existing
//! mediamtx-gs pipeline republishes it. The stdout `PKT` stats line drives
//! `fragments_after_dedup` / `fec_repaired` / `output_kbps`; the `RX_ANT` lines
//! before it drive the per-relay `fragments` (see the `rx_ant` module).
//! `wfb-receiver.json` is written atomically.
//!
//! Discovery is Rust-native: the receiver advertises `_ados-receiver._tcp` on
//! `bat0` via [`crate::mdns::advertise_receiver`] (held for the loop lifetime,
//! unregistered on shutdown) so relays resolve it. The relay-churn task reads
//! batman-adv neighbor MACs (`batctl n -H`) on the mesh interface to populate
//! and age the per-relay map, emitting `relay_connected` on first sight and
//! `relay_disconnected` past the grace window. A relay listed in the mesh
//! revocation file is never shown as a relay. The aggregator subprocess
//! lifecycle, the stats tail, the churn watcher, and the state file are all
//! owned here.

mod fold;
mod rx_ant;

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ados_protocol::shutdown::Shutdown;
use serde_json::json;
use tokio::sync::Mutex;

use crate::gs_config::GroundStationConfig;
use crate::mesh_events;
use ados_proc::{GroupChild, StdoutMode};

use fold::{fold_aggregator_pkt, fold_relay_fragments};

mod aggregator;
mod state;
pub use aggregator::{aggregate_args, parse_receiver_stats_line, spawn_aggregator};
use aggregator::{tail_aggregator_stats, wait_for_link_change, watch_aggregator};
use state::watch_relay_churn;

/// Per-relay liveness grace before a silent relay is aged out of the map.
const RELAY_GRACE_MS: i64 = 4000;
/// State-write + churn-poll cadence.
const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// Aggregator graceful-shutdown grace before SIGKILL.
const AGGREGATOR_GRACE: Duration = Duration::from_secs(3);
/// Fixed retry between aggregator bring-up attempts (spawn failure, exit,
/// stall). No cap: a receiver keeps trying for as long as the unit runs.
const RESPAWN_INTERVAL: Duration = Duration::from_secs(5);
/// How long the aggregator may go without printing a stats line before it is
/// judged wedged. `wfb_rx` prints one every second whether or not anything
/// arrives, so a flat line count with a live process is a stopped loop.
const STATS_SILENCE_WINDOW: Duration = Duration::from_secs(30);

/// The receiver's published state (the `wfb-receiver.json` shape). Relays are
/// flattened to a list on write.
/// `Deserialize` round-trips the on-disk file shape for parity assertions and
/// any reader that loads the sidecar back.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ReceiverState {
    pub role: String,
    pub drone_iface: String,
    pub listen_port: i64,
    pub accept_local_nic: bool,
    pub mesh_iface: String,
    pub relays: Vec<RelayStats>,
    pub fragments_after_dedup: i64,
    pub fec_repaired: i64,
    pub output_kbps: i64,
    pub up: bool,
    /// The WFB adapter the role resolver left to the mesh carrier (role
    /// `mesh`), which the mesh manager binds to batman-adv. `None` when no
    /// adapter has that role and the mesh manager keeps its own pick.
    #[serde(default)]
    pub mesh_adapter: Option<String>,
}

/// Per-relay fragment stats (one entry in the `relays` list). `fragments` is
/// the running count of packets the aggregator received from this relay, or
/// `null` while the relay's mesh address has not been resolved (unmeasured).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RelayStats {
    pub mac: String,
    pub last_seen_ms: i64,
    pub fragments: Option<i64>,
}

/// The per-interval counters the receiver surfaces, off one aggregator stats
/// line: `<ts>\tPKT\t<p_all>:<b_all>:<dec_err>:<sess>:<data>:<uniq>:<fec_rec>:<lost>:<bad>:<out>:<b_out>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AggregatorPkt {
    /// Unique fragments after cross-source dedup this interval.
    pub unique: i64,
    /// Fragments FEC repaired this interval.
    pub fec_recovered: i64,
    /// Bytes emitted on the decoded output this interval.
    pub bytes_out: i64,
}

/// Run the receiver role to completion (until `shutdown` fires).
///
/// Detects the local drone-facing adapters (when `accept_local_nic`), spawns the
/// `wfb_rx` aggregator once, advertises `_ados-receiver._tcp` on `bat0`, then
/// runs the stats tail + relay-churn watcher + periodic state writer
/// concurrently. On shutdown (or aggregator exit) all tasks are cancelled, the
/// aggregator is terminated gracefully, the mDNS record is unregistered, and
/// `up=false` is persisted.
pub async fn run(
    shutdown: Shutdown,
    ingest: Option<ados_protocol::logd::emitter::IngestEmitter>,
    progress: ados_supervisor::sdnotify::MonitorProgress,
) {
    let cfg = GroundStationConfig::load_from(Path::new("/etc/ados/config.yaml"));
    let mesh_iface = cfg.mesh.bat_iface.clone();
    let service_type = cfg.wfb_relay.receiver_mdns_service.clone();
    let listen_port = cfg.wfb_receiver.listen_port;
    let accept_local_nic = cfg.wfb_receiver.accept_local_nic;

    let state = Arc::new(Mutex::new(ReceiverState {
        listen_port: listen_port as i64,
        accept_local_nic,
        mesh_iface: mesh_iface.clone(),
        ..Default::default()
    }));

    if !Path::new(ados_radio::paths::WFB_RX_KEY).exists() {
        tracing::warn!("wfb_receiver_keys_missing");
    }

    // Advertise on the mesh fabric so relays can resolve us. Held for the loop
    // lifetime; dropped (unregister + shutdown) on exit.
    let advert = crate::mdns::advertise_receiver(&service_type, &mesh_iface, listen_port);

    let churn_task = tokio::spawn(watch_relay_churn(state.clone(), mesh_iface.clone()));
    let writer_task = {
        let state = state.clone();
        let shutdown = shutdown.clone();
        let ingest = ingest.clone();
        tokio::spawn(async move {
            loop {
                // The receiver's steady-state work is this periodic state write;
                // it is therefore the honest progress stamp for the systemd
                // watchdog. A free-running ping would report a wedged receiver
                // as healthy, which is the hole the coupling exists to close.
                progress.mark();
                if let Err(e) = state.lock().await.write_and_emit(ingest.as_ref()) {
                    tracing::debug!(error = %e, "receiver_state_write_failed");
                }
                tokio::select! {
                    _ = shutdown.wait() => break,
                    _ = tokio::time::sleep(POLL_INTERVAL) => {}
                }
            }
        })
    };

    // Supervise the aggregator: bring it up, watch it, and on an exit or a
    // stalled stats stream tear it down and bring it up again on a fixed
    // interval. The local adapters are re-detected on every attempt that has
    // none, so an adapter plugged in after boot joins the aggregation.
    let mut drone_ifaces: Vec<String> = Vec::new();
    loop {
        if accept_local_nic && drone_ifaces.is_empty() {
            let plan = detect_local_adapters().await;
            drone_ifaces = plan.as_ref().map(|p| p.rx_ifaces()).unwrap_or_default();
            // The sidecar names the first local adapter (every one is
            // aggregated) and the adapter left to the mesh carrier, which the
            // mesh manager binds to batman-adv.
            let mut s = state.lock().await;
            s.drone_iface = drone_ifaces.first().cloned().unwrap_or_default();
            s.mesh_adapter = plan
                .as_ref()
                .and_then(|p| p.mesh())
                .map(|m| m.ifname().to_string());
        }
        // With no local adapter the receiver trusts only relay forwards (the
        // iface args are dropped by `aggregate_args`).
        let use_local = accept_local_nic && !drone_ifaces.is_empty();
        let Some(link_id) = crate::fleet_hero::served_video_link_id() else {
            // No registered drone: there is no link to aggregate.
            state.lock().await.up = false;
            tokio::select! {
                _ = shutdown.wait() => break,
                _ = tokio::time::sleep(RESPAWN_INTERVAL) => {}
            }
            continue;
        };
        match spawn_aggregator(&drone_ifaces, listen_port, use_local, link_id).await {
            Ok(mut aggregator) => {
                state.lock().await.up = true;
                let lines_seen = Arc::new(AtomicU64::new(0));
                let tail_task = aggregator.take_stdout().map(|out| {
                    tokio::spawn(tail_aggregator_stats(
                        out,
                        state.clone(),
                        lines_seen.clone(),
                    ))
                });
                let stopped = tokio::select! {
                    _ = shutdown.wait() => true,
                    reason = watch_aggregator(&mut aggregator, &lines_seen) => {
                        tracing::warn!(reason, "wfb_receiver_aggregator_down_respawning");
                        false
                    }
                    _ = wait_for_link_change(link_id) => {
                        tracing::info!(link_id, "wfb_receiver_served_drone_changed");
                        false
                    }
                };
                if let Some(t) = tail_task {
                    t.abort();
                }
                aggregator.terminate(AGGREGATOR_GRACE).await;
                state.lock().await.up = false;
                if stopped {
                    break;
                }
            }
            Err(e) => {
                tracing::error!(error = %e, "wfb_receiver_aggregator_spawn_failed");
                state.lock().await.up = false;
            }
        }
        tokio::select! {
            _ = shutdown.wait() => break,
            _ = tokio::time::sleep(RESPAWN_INTERVAL) => {}
        }
    }

    tracing::info!("wfb_receiver_stopping");
    churn_task.abort();
    writer_task.abort();
    if let Some(a) = &advert {
        a.shutdown();
    }
    state.lock().await.up = false;
    let _ = state.lock().await.write_and_emit(ingest.as_ref());
    // Restore the local monitor adapters to managed mode when any were
    // resolved (only with local-NIC aggregation). Empty when the receiver
    // trusts relay forwards alone, so there is nothing to restore in that case
    // (the mirror of the drone-side teardown).
    for iface in &drone_ifaces {
        tracing::info!(interface = %iface, "restoring local monitor adapter to managed mode");
        ados_radio::adapter::set_managed_mode(iface).await;
    }
    tracing::info!("wfb_receiver_stopped");
}

/// Resolve + monitor-mode the local adapters for aggregation (the last one is
/// left to the mesh carrier when no roles are configured), emitting the
/// adapter-missing event on failure. `None` leaves the receiver on relay
/// forwards alone until the next attempt.
async fn detect_local_adapters() -> Option<ados_radio::adapter::AdapterPlan> {
    match crate::adapter_plan::resolve("", true).await {
        crate::adapter_plan::Resolution::Ready(plan) => Some(plan),
        crate::adapter_plan::Resolution::NoInjection(sel) => {
            tracing::warn!(iface = %sel.ifname, "wfb_receiver_monitor_mode_failed");
            mesh_events::emit(
                mesh_events::KIND_WFB_ADAPTER_MISSING,
                json!({
                    "side": "receiver",
                    "reason": "monitor_mode_failed",
                    "detail": format!("Could not put {} into monitor mode.", sel.ifname),
                }),
            );
            None
        }
        crate::adapter_plan::Resolution::NoAdapter => {
            // Local aggregation requested but no adapter: the receiver still
            // serves relay forwards, but the operator must know local
            // reception is gone.
            mesh_events::emit(
                mesh_events::KIND_WFB_ADAPTER_MISSING,
                json!({
                    "side": "receiver",
                    "reason": "adapter_not_found",
                    "detail": "No monitor-capable WFB adapter detected for local reception.",
                }),
            );
            None
        }
    }
}

#[cfg(test)]
mod tests;
