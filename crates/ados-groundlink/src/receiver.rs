//! WFB receiver: FEC-combine fragments from the local NIC + remote relays.
//!
//! `wfb_rx -p 0 -c 127.0.0.1 -u 5600 -a <listen_port> [<drone_iface>...]`
//! aggregates fragments arriving on the local monitor adapters AND from relays
//! forwarding over batman-adv into the aggregator UDP port, FEC-combines them,
//! and emits the decoded stream to localhost UDP 5600 where the existing
//! mediamtx-gs pipeline republishes it. The stdout `PKT` stats line drives
//! `fragments_after_dedup` / `fec_repaired` / `output_kbps`; the `RX_ANT` lines
//! before it drive the per-relay `fragments` (see [`rx_ant`]).
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
use crate::process_spawn::{GsWfbProcess, Stdout};

use fold::{fold_aggregator_pkt, fold_relay_fragments};

/// Per-relay liveness grace before a silent relay is aged out of the map.
/// Mirrors the Python `_RELAY_GRACE_MS = 4000`.
const RELAY_GRACE_MS: i64 = 4000;
/// State-write + churn-poll cadence. Mirrors the Python `_POLL_INTERVAL_S = 2.0`.
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

/// The receiver's published state (the `wfb-receiver.json` shape, byte-identical
/// to the Python `_write_state`). Relays are flattened to a list on write.
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

impl Default for ReceiverState {
    fn default() -> Self {
        Self {
            role: "receiver".to_string(),
            drone_iface: String::new(),
            listen_port: 5800,
            accept_local_nic: true,
            mesh_iface: "bat0".to_string(),
            relays: Vec::new(),
            fragments_after_dedup: 0,
            fec_repaired: 0,
            output_kbps: 0,
            up: false,
            mesh_adapter: None,
        }
    }
}

impl ReceiverState {
    /// Atomically write the state to `wfb-receiver.json` (Contract-E path). The
    /// run-dir path resolves via `run_path` (honouring the `ADOS_RUN_DIR`
    /// override); the write itself is delegated to `write_to` so tests can target
    /// an explicit temp path without mutating process-global env.
    pub fn write(&self) -> std::io::Result<()> {
        self.write_to(Path::new(&crate::paths::run_path("wfb-receiver.json")))
    }

    /// Atomically write the state to an explicit sidecar path. The path seam that
    /// lets a test write into its own temp dir without touching `ADOS_RUN_DIR`.
    pub fn write_to(&self, path: &Path) -> std::io::Result<()> {
        ados_protocol::sidecar::write_json_atomic(path, self, 0o644)
    }

    /// Write the state file AND ship the same body to the logging store as a
    /// single `gs.receiver_state` event. The struct is persisted to disk
    /// directly, so the on-disk sidecar stays byte-identical to `write()`; the
    /// JSON value is built only for the store event (the nested `relays` array
    /// round-trips through `json_object_to_fields`). Best-effort: an absent
    /// logging daemon drops the event without disturbing the poll loop; an I/O
    /// error on the file write is surfaced to the caller exactly as `write()`.
    pub fn write_and_emit(
        &self,
        ingest: Option<&ados_protocol::logd::emitter::IngestEmitter>,
    ) -> std::io::Result<()> {
        self.write_and_emit_to(
            Path::new(&crate::paths::run_path("wfb-receiver.json")),
            ingest,
        )
    }

    /// [`write_and_emit`](Self::write_and_emit) against an explicit sidecar path.
    /// The path seam that keeps a test's best-effort write inside its own temp
    /// tree without mutating the process-global `ADOS_RUN_DIR`.
    pub fn write_and_emit_to(
        &self,
        path: &Path,
        ingest: Option<&ados_protocol::logd::emitter::IngestEmitter>,
    ) -> std::io::Result<()> {
        let res = ados_protocol::sidecar::write_json_atomic(path, self, 0o644);
        if let Some(em) = ingest {
            let v = serde_json::to_value(self).unwrap_or(serde_json::Value::Null);
            em.emit_event(
                "gs.receiver_state",
                ados_protocol::logd::Level::Info,
                crate::wfb_rx::stats::json_object_to_fields(&v),
            );
        }
        res
    }
}

/// Build the `wfb_rx -a` aggregator args. With `accept_local_nic` every local
/// receiving adapter is appended so its fragments are aggregated too; without
/// it the receiver trusts only relay forwards.
///
/// `link_id` is the transmitting drone's (`link_id(fleet_id, slot)`). The
/// aggregator checks every session against the `channel_id` built from it, so
/// without `-i` it sat on wfb-ng's default link id 0 and accepted no fleet
/// drone's stream.
pub fn aggregate_args(
    drone_ifaces: &[String],
    listen_port: u16,
    accept_local_nic: bool,
    rx_key: &Path,
    link_id: u32,
) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "-p".into(),
        "0".into(),
        "-i".into(),
        link_id.to_string(),
        "-c".into(),
        "127.0.0.1".into(),
        "-u".into(),
        "5600".into(),
        "-a".into(),
        listen_port.to_string(),
        "-K".into(),
        rx_key.to_string_lossy().into_owned(),
    ];
    if accept_local_nic {
        args.extend(drone_ifaces.iter().cloned());
    }
    args
}

/// The aggregator's stderr log. Its stats go to stdout (read below); stderr
/// carries only diagnostics, and a file never fills the way an unread pipe does.
const AGGREGATOR_LOG: &str = "/run/ados/wfb-gs-aggregator.log";

/// Spawn the FEC-combine aggregator in its own process group (setsid/killpg).
/// stdout is piped: `wfb_rx` prints its per-interval `PKT` stats line there
/// (`vendor/wfb-ng/src/rx.cpp` `Aggregator::dump_stats`, `IPC_MSG` = stdout).
pub async fn spawn_aggregator(
    drone_ifaces: &[String],
    listen_port: u16,
    accept_local_nic: bool,
    link_id: u32,
) -> std::io::Result<GsWfbProcess> {
    let rx_key = Path::new(ados_radio::paths::WFB_RX_KEY);
    let args = aggregate_args(drone_ifaces, listen_port, accept_local_nic, rx_key, link_id);
    GsWfbProcess::spawn("wfb_rx", &args, Stdout::Piped, Some(AGGREGATOR_LOG)).await
}

/// Resolves once the drone this ground station serves is no longer
/// `current`, so the aggregator can be rebuilt on the new link.
async fn wait_for_link_change(current: u32) {
    loop {
        tokio::time::sleep(POLL_INTERVAL).await;
        if crate::fleet_hero::served_video_link_id() != Some(current) {
            return;
        }
    }
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

/// Parse one aggregator stdout line, or `None` when it is not an 11-field `PKT`
/// stats line (the `RX_ANT` lines and anything else are ignored).
pub fn parse_receiver_stats_line(line: &str) -> Option<AggregatorPkt> {
    let mut cols = line.trim_end().split('\t');
    let _ts = cols.next()?;
    if cols.next()? != "PKT" {
        return None;
    }
    let f: Vec<&str> = cols.next()?.split(':').collect();
    if f.len() != 11 {
        return None;
    }
    Some(AggregatorPkt {
        unique: f[5].parse().ok()?,
        fec_recovered: f[6].parse().ok()?,
        bytes_out: f[10].parse().ok()?,
    })
}

/// Upsert the relays seen this poll (by batman-neighbor MAC) into `state`, refreshing
/// `last_seen_ms` and returning the MACs that were NOT present before (first sight →
/// caller emits `relay_connected`). Pure over the serialized `Vec<RelayStats>`. This is
/// the populate half the superseded packaged receiver never implemented: it only aged
/// relays out.
fn upsert_relays(state: &mut ReceiverState, macs: &[String], now_ms: i64) -> Vec<String> {
    let mut newly = Vec::new();
    for mac in macs {
        if let Some(r) = state.relays.iter_mut().find(|r| &r.mac == mac) {
            r.last_seen_ms = now_ms;
        } else {
            state.relays.push(RelayStats {
                mac: mac.clone(),
                last_seen_ms: now_ms,
                fragments: None,
            });
            newly.push(mac.clone());
        }
    }
    newly
}

/// Remove relays silent past `RELAY_GRACE_MS`, returning the aged-out MACs so the
/// caller emits `relay_disconnected`.
fn age_out_relays(state: &mut ReceiverState, now_ms: i64) -> Vec<String> {
    let mut removed = Vec::new();
    state.relays.retain(|r| {
        let stale = now_ms - r.last_seen_ms > RELAY_GRACE_MS;
        if stale {
            removed.push(r.mac.clone());
        }
        !stale
    });
    removed
}

/// Tail the aggregator's stdout, folding its stats into shared state and
/// counting stats lines (the delta counter the stall window checks). The
/// `RX_ANT` lines of an interval are buffered and folded into the per-relay
/// counts when that interval's `PKT` line arrives. Returns when the pipe
/// closes.
async fn tail_aggregator_stats(
    stdout: tokio::process::ChildStdout,
    state: Arc<Mutex<ReceiverState>>,
    lines_seen: Arc<AtomicU64>,
) {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let mesh_iface = state.lock().await.mesh_iface.clone();
    let mut lines = BufReader::new(stdout).lines();
    let mut interval_ant: Vec<String> = Vec::new();
    while let Ok(Some(line)) = lines.next_line().await {
        if let Some(pkt) = parse_receiver_stats_line(&line) {
            lines_seen.fetch_add(1, Ordering::Relaxed);
            let per_source = rx_ant::per_source_packets(&rx_ant::parse_rx_ant(
                interval_ant.iter().map(String::as_str),
            ));
            interval_ant.clear();
            let arp = rx_ant::read_arp(&mesh_iface);
            let mut s = state.lock().await;
            fold_aggregator_pkt(&mut s, pkt);
            fold_relay_fragments(&mut s, &per_source, &arp);
        } else if rx_ant::parse_rx_ant_line(&line).is_some() {
            interval_ant.push(line);
        }
    }
}

/// Read the batman-adv neighbor MACs on the mesh interface (`batctl n -H`).
/// These are the relays (and any other mesh peers) currently reachable on
/// `bat0`; the churn watcher folds them into the per-relay map. Returns an
/// empty vec on a batctl error so a wedged module never stalls the loop.
async fn neighbor_macs(_mesh_iface: &str) -> Vec<String> {
    let (rc, out, _e) =
        crate::mesh::batctl::run("batctl", &["n", "-H"], Duration::from_secs(3)).await;
    if rc != 0 {
        return Vec::new();
    }
    crate::mesh::batctl::parse_neighbors(&out, mesh_events::now_ms())
        .into_iter()
        .map(|n| n.mac)
        .collect()
}

/// One churn step over the relay map: drop revoked MACs (a revoked relay is
/// never presented as one), upsert the neighbor MACs seen this poll
/// (refreshing existing, adding new) and age out the silent ones. Returns the
/// `(newly_connected, disconnected)` MACs the caller emits; a revoked relay
/// already in the map is reported as disconnected. Pure over the serialized
/// state, so the upsert→emit decision is unit-testable without a running mesh
/// or the cross-process event seam.
fn churn_step(
    state: &mut ReceiverState,
    macs: &[String],
    revoked: &BTreeSet<String>,
    now_ms: i64,
) -> (Vec<String>, Vec<String>) {
    let is_revoked = |mac: &str| revoked.iter().any(|r| r.eq_ignore_ascii_case(mac));
    let mut removed = Vec::new();
    state.relays.retain(|r| {
        let gone = is_revoked(&r.mac);
        if gone {
            removed.push(r.mac.clone());
        }
        !gone
    });
    let allowed: Vec<String> = macs
        .iter()
        .filter(|m| !is_revoked(m.as_str()))
        .cloned()
        .collect();
    let newly = upsert_relays(state, &allowed, now_ms);
    removed.extend(age_out_relays(state, now_ms));
    (newly, removed)
}

/// The relay-churn watcher: each `POLL_INTERVAL`, read mesh neighbor MACs and
/// the revocation list, upsert the non-revoked neighbors into the relay map
/// (emit `relay_connected` for new), and age out the silent ones (emit
/// `relay_disconnected`). Runs until cancelled.
async fn watch_relay_churn(state: Arc<Mutex<ReceiverState>>, mesh_iface: String) {
    loop {
        tokio::time::sleep(POLL_INTERVAL).await;
        let macs = neighbor_macs(&mesh_iface).await;
        let revoked = crate::pairing::revocations::load();
        let now = mesh_events::now_ms();
        let (newly, removed) = {
            let mut s = state.lock().await;
            churn_step(&mut s, &macs, &revoked, now)
        };
        for mac in newly {
            mesh_events::emit(
                mesh_events::KIND_RELAY_CONNECTED,
                json!({ "relay_mac": mac }),
            );
        }
        for mac in removed {
            mesh_events::emit(
                mesh_events::KIND_RELAY_DISCONNECTED,
                json!({ "relay_mac": mac }),
            );
        }
    }
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
                aggregator.terminate_then_kill(AGGREGATOR_GRACE).await;
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

/// Resolve when the aggregator has exited or its stats stream has stayed flat
/// for [`STATS_SILENCE_WINDOW`]; returns the reason. One arm of the
/// supervision select.
async fn watch_aggregator(proc: &mut GsWfbProcess, lines_seen: &AtomicU64) -> &'static str {
    let mut last = lines_seen.load(Ordering::Relaxed);
    let mut last_advance = Instant::now();
    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if !proc.is_running() {
            return "exited";
        }
        let now_lines = lines_seen.load(Ordering::Relaxed);
        if now_lines > last {
            last = now_lines;
            last_advance = Instant::now();
        } else if last_advance.elapsed() >= STATS_SILENCE_WINDOW {
            return "stats_silent";
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::net::Ipv4Addr;

    #[test]
    fn aggregate_args_with_local_nic() {
        // wfb_rx -p 0 -i <link> -c 127.0.0.1 -u 5600 -a 5800 -K <rx.key> <iface>...
        let a = aggregate_args(
            &["wlan0".to_string(), "wlan1".to_string()],
            5800,
            true,
            Path::new("/etc/ados/wfb/rx.key"),
            ados_radio::config::link_id(1, 1),
        );
        assert_eq!(
            a,
            vec![
                "-p",
                "0",
                "-i",
                "257",
                "-c",
                "127.0.0.1",
                "-u",
                "5600",
                "-a",
                "5800",
                "-K",
                "/etc/ados/wfb/rx.key",
                "wlan0",
                "wlan1"
            ]
        );
    }

    #[test]
    fn aggregate_args_without_local_nic_drops_iface() {
        let a = aggregate_args(&["wlan0".to_string()], 5800, false, Path::new("/k"), 257);
        assert!(!a.contains(&"wlan0".to_string()));
        // The aggregator still listens on the relay forward port.
        let ai = a.iter().position(|x| x == "-a").unwrap();
        assert_eq!(a[ai + 1], "5800");
    }

    /// The aggregator's stats line is `wfb_rx`'s own eleven-field `PKT` line on
    /// stdout (`vendor/wfb-ng/src/rx.cpp:501`); nothing else carries counters.
    #[test]
    fn the_aggregator_stats_line_yields_dedup_fec_and_output() {
        assert_eq!(
            parse_receiver_stats_line(
                "1750000000000\tPKT\t120:180000:0:1:110:100:4:0:0:100:125000"
            ),
            Some(AggregatorPkt {
                unique: 100,
                fec_recovered: 4,
                bytes_out: 125_000,
            })
        );
        assert_eq!(
            parse_receiver_stats_line("999 PKT n_out:1500 fec_rec:12"),
            None
        );
        assert_eq!(
            parse_receiver_stats_line("1\tRX_ANT\t5745:1:20\t0\t1:-50:-49:-48:20:21:22"),
            None
        );
    }

    #[test]
    fn receiver_state_json_shape_flattens_relays() {
        let mut s = ReceiverState::default();
        s.relays.push(RelayStats {
            mac: "aa:bb:cc:dd:ee:ff".into(),
            last_seen_ms: 123,
            fragments: Some(500),
        });
        let v = serde_json::to_value(&s).unwrap();
        for k in [
            "role",
            "drone_iface",
            "listen_port",
            "accept_local_nic",
            "mesh_iface",
            "relays",
            "fragments_after_dedup",
            "fec_repaired",
            "output_kbps",
            "up",
            "mesh_adapter",
        ] {
            assert!(v.get(k).is_some(), "missing key {k}");
        }
        assert_eq!(v["relays"][0]["mac"], "aa:bb:cc:dd:ee:ff");
        assert_eq!(v["relays"][0]["fragments"], 500);
    }

    #[test]
    fn upsert_relays_adds_new_and_refreshes_existing() {
        let mut s = ReceiverState::default();
        // First sight of two relays → both reported new.
        let newly = upsert_relays(&mut s, &["aa".into(), "bb".into()], 1_000);
        assert_eq!(newly.len(), 2);
        assert_eq!(s.relays.len(), 2);
        // Re-sight one + a third → only the third is new, last_seen refreshed.
        let newly = upsert_relays(&mut s, &["aa".into(), "cc".into()], 5_000);
        assert_eq!(newly, vec!["cc".to_string()]);
        assert_eq!(s.relays.len(), 3);
        let aa = s.relays.iter().find(|r| r.mac == "aa").unwrap();
        assert_eq!(aa.last_seen_ms, 5_000);
    }

    #[test]
    fn age_out_relays_removes_only_stale() {
        let mut s = ReceiverState::default();
        upsert_relays(&mut s, &["fresh".into(), "stale".into()], 0);
        // Mark "fresh" as recently seen, leave "stale" old.
        upsert_relays(&mut s, &["fresh".into()], 10_000);
        let removed = age_out_relays(&mut s, 10_000 + 1);
        // "stale" last seen at 0, now 10_001 → > RELAY_GRACE_MS (4000) → removed.
        assert_eq!(removed, vec!["stale".to_string()]);
        assert_eq!(s.relays.len(), 1);
        assert_eq!(s.relays[0].mac, "fresh");
    }

    #[test]
    fn churn_step_drives_connect_then_disconnect() {
        // Drive the exact decision the churn watcher emits on: first sight of two
        // relays reports both as newly connected (→ relay_connected); a later
        // poll seeing only one ages the other out past the grace window (→
        // relay_disconnected) while the still-present one is neither.
        let mut s = ReceiverState::default();

        let (newly, removed) =
            churn_step(&mut s, &["aa".into(), "bb".into()], &BTreeSet::new(), 1_000);
        assert_eq!(newly, vec!["aa".to_string(), "bb".to_string()]);
        assert!(removed.is_empty());

        // Re-poll past "bb"'s grace window seeing only "aa".
        let later = 1_000 + RELAY_GRACE_MS + 1;
        let (newly, removed) = churn_step(&mut s, &["aa".into()], &BTreeSet::new(), later);
        assert!(newly.is_empty(), "aa already known → not newly connected");
        assert_eq!(removed, vec!["bb".to_string()]);
        // Only the live relay remains in the map.
        assert_eq!(s.relays.len(), 1);
        assert_eq!(s.relays[0].mac, "aa");
    }

    #[test]
    fn churn_step_no_neighbors_ages_out_all() {
        // An empty neighbor list past the grace window disconnects every relay.
        let mut s = ReceiverState::default();
        churn_step(&mut s, &["aa".into(), "bb".into()], &BTreeSet::new(), 0);
        let (newly, removed) = churn_step(&mut s, &[], &BTreeSet::new(), RELAY_GRACE_MS + 1);
        assert!(newly.is_empty());
        assert_eq!(removed.len(), 2);
        assert!(removed.contains(&"aa".to_string()));
        assert!(removed.contains(&"bb".to_string()));
        assert!(s.relays.is_empty());
    }

    #[test]
    fn age_out_then_reconnect_emits_again() {
        // A relay that ages out and later reappears is reported new again.
        let mut s = ReceiverState::default();
        upsert_relays(&mut s, &["r1".into()], 0);
        let removed = age_out_relays(&mut s, RELAY_GRACE_MS + 1);
        assert_eq!(removed, vec!["r1".to_string()]);
        let newly = upsert_relays(&mut s, &["r1".into()], RELAY_GRACE_MS + 2);
        assert_eq!(newly, vec!["r1".to_string()]);
    }

    /// Stats lines accumulate: the fragment and FEC totals sum the per-interval
    /// counts, the output rate is the latest interval's, and every stats line
    /// advances the line counter the stall window watches.
    #[tokio::test]
    async fn tail_folds_aggregator_counters() {
        let state = Arc::new(Mutex::new(ReceiverState::default()));
        let lines = Arc::new(AtomicU64::new(0));
        let script = "printf '1\\tPKT\\t10:0:0:0:10:8:1:0:0:8:1000\\n2\\tPKT\\t10:0:0:0:10:7:2:0:0:7:2000\\n'";
        let mut child = tokio::process::Command::new("sh")
            .args(["-c", script])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("spawn sh");
        tail_aggregator_stats(child.stdout.take().unwrap(), state.clone(), lines.clone()).await;
        let _ = child.wait().await;
        let s = state.lock().await;
        assert_eq!(s.fragments_after_dedup, 15);
        assert_eq!(s.fec_repaired, 3);
        assert_eq!(s.output_kbps, 16);
        assert_eq!(lines.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn receiver_fixture_round_trips_with_python_shape() {
        // The exact JSON the Python `_write_state` produced for a receiver
        // aggregating the local NIC plus one relay forward. Deserialize into the
        // Rust struct, then re-serialize and assert the key set + values are
        // preserved (no field drift), including the nested relay entry.
        let fixture = r#"{
            "role": "receiver",
            "drone_iface": "wlan1",
            "listen_port": 5800,
            "accept_local_nic": true,
            "mesh_iface": "bat0",
            "relays": [
                {"mac": "aa:bb:cc:dd:ee:ff", "last_seen_ms": 1717000000000, "fragments": 4096}
            ],
            "fragments_after_dedup": 8000,
            "fec_repaired": 24,
            "output_kbps": 4200,
            "up": true,
            "mesh_adapter": "wlan3"
        }"#;
        let s: ReceiverState = serde_json::from_str(fixture).expect("deserialize receiver fixture");
        assert_eq!(s.role, "receiver");
        assert_eq!(s.drone_iface, "wlan1");
        assert_eq!(s.listen_port, 5800);
        assert!(s.accept_local_nic);
        assert_eq!(s.mesh_iface, "bat0");
        assert_eq!(s.relays.len(), 1);
        assert_eq!(s.relays[0].mac, "aa:bb:cc:dd:ee:ff");
        assert_eq!(s.relays[0].last_seen_ms, 1_717_000_000_000);
        assert_eq!(s.relays[0].fragments, Some(4096));
        assert_eq!(s.fragments_after_dedup, 8000);
        assert_eq!(s.fec_repaired, 24);
        assert_eq!(s.output_kbps, 4200);
        assert!(s.up);
        assert_eq!(s.mesh_adapter.as_deref(), Some("wlan3"));

        // Round-trip back to the same key set + values as the fixture.
        let re = serde_json::to_value(&s).unwrap();
        let orig: serde_json::Value = serde_json::from_str(fixture).unwrap();
        assert_eq!(re, orig);
    }

    #[test]
    fn unmeasured_relay_serializes_null() {
        let mut s = ReceiverState::default();
        upsert_relays(&mut s, &["aa:bb:cc:dd:ee:01".into()], 1);
        let v = serde_json::to_value(&s).unwrap();
        assert!(v["relays"][0]["fragments"].is_null());
        assert!(v["relays"][0]
            .as_object()
            .unwrap()
            .contains_key("fragments"));
    }

    #[test]
    fn relay_fragments_count_only_resolved_relays() {
        let mut s = ReceiverState::default();
        upsert_relays(
            &mut s,
            &["aa:bb:cc:dd:ee:01".into(), "aa:bb:cc:dd:ee:02".into()],
            1,
        );
        let relay_ip = Ipv4Addr::new(192, 168, 1, 50);
        let arp = HashMap::from([(relay_ip, "aa:bb:cc:dd:ee:01".to_string())]);
        let per_source = HashMap::from([(relay_ip, 40u64)]);
        fold_relay_fragments(&mut s, &per_source, &arp);
        fold_relay_fragments(&mut s, &per_source, &arp);
        // A quiet interval still counts as measured: zero added.
        fold_relay_fragments(&mut s, &HashMap::new(), &arp);
        assert_eq!(s.relays[0].fragments, Some(80));
        // No resolved address: stays unmeasured.
        assert_eq!(s.relays[1].fragments, None);
    }

    #[test]
    fn revoked_mac_excluded_from_relays() {
        let mut s = ReceiverState::default();
        churn_step(
            &mut s,
            &["aa:bb:cc:dd:ee:01".into(), "aa:bb:cc:dd:ee:02".into()],
            &BTreeSet::new(),
            0,
        );
        let revoked = BTreeSet::from(["AA:BB:CC:DD:EE:02".to_string()]);
        let (newly, removed) = churn_step(
            &mut s,
            &["aa:bb:cc:dd:ee:01".into(), "aa:bb:cc:dd:ee:02".into()],
            &revoked,
            1_000,
        );
        assert!(newly.is_empty());
        assert_eq!(removed, vec!["aa:bb:cc:dd:ee:02".to_string()]);
        assert_eq!(s.relays.len(), 1);
        assert_eq!(s.relays[0].mac, "aa:bb:cc:dd:ee:01");
        // A revoked neighbour seen first never enters the map.
        let mut fresh = ReceiverState::default();
        let (newly, _) = churn_step(&mut fresh, &["aa:bb:cc:dd:ee:02".into()], &revoked, 0);
        assert!(newly.is_empty());
        assert!(fresh.relays.is_empty());
    }

    #[test]
    fn receiver_fixture_empty_relays_locks_list() {
        // A receiver with no relays seen yet writes `relays: []`; the Rust
        // `Vec<RelayStats>` must accept it as an empty list (not an error).
        let fixture = r#"{
            "role": "receiver",
            "drone_iface": "",
            "listen_port": 5800,
            "accept_local_nic": false,
            "mesh_iface": "bat0",
            "relays": [],
            "fragments_after_dedup": 0,
            "fec_repaired": 0,
            "output_kbps": 0,
            "up": false
        }"#;
        let s: ReceiverState = serde_json::from_str(fixture).expect("deserialize empty relays");
        // A sidecar written before the mesh adapter was published reads as none.
        assert!(s.mesh_adapter.is_none());
        assert!(s.relays.is_empty());
        assert!(!s.accept_local_nic);
        assert!(!s.up);
        let re = serde_json::to_value(&s).unwrap();
        assert!(re["relays"].as_array().unwrap().is_empty());
    }

    #[test]
    fn receiver_state_write_to_writes_the_sidecar() {
        // The write seam persists the state to the given path. No env mutation:
        // the temp path is threaded in explicitly, so this test cannot race any
        // other test under the parallel runner.
        let dir = tempfile::tempdir().unwrap();
        let mut s = ReceiverState {
            listen_port: 5800,
            up: true,
            ..Default::default()
        };
        upsert_relays(&mut s, &["aa:bb:cc:dd:ee:ff".into()], 42);
        let path = dir.path().join("wfb-receiver.json");
        s.write_to(&path).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&written).unwrap();
        assert_eq!(v["up"], true);
        assert_eq!(v["relays"][0]["mac"], "aa:bb:cc:dd:ee:ff");
    }

    #[tokio::test]
    async fn write_and_emit_enqueues_one_event_with_an_emitter_and_none_without() {
        // The emitting write ships exactly one gs.receiver_state event with an
        // emitter and nothing with None, regardless of the best-effort file
        // write result. The on-disk file path is covered by
        // `receiver_state_write_to_writes_the_sidecar`; this test threads the
        // temp path in explicitly (no `ADOS_RUN_DIR` mutation) so the best-effort
        // write stays inside its own temp tree and cannot race a sibling test.
        let dir = tempfile::tempdir().unwrap();
        let mut s = ReceiverState {
            listen_port: 5800,
            up: true,
            ..Default::default()
        };
        upsert_relays(&mut s, &["aa:bb:cc:dd:ee:ff".into()], 42);
        let path = dir.path().join("wfb-receiver.json");

        let emitter = ados_protocol::logd::emitter::IngestEmitter::with_socket(
            "ados-groundlink",
            dir.path().join("ingest.sock"),
        );
        let stats = emitter.stats();
        let _ = s.write_and_emit_to(&path, Some(&emitter));
        assert_eq!(stats.enqueued(), 1);

        let none_emitter = ados_protocol::logd::emitter::IngestEmitter::with_socket(
            "ados-groundlink",
            dir.path().join("ingest2.sock"),
        );
        let none_stats = none_emitter.stats();
        let _ = s.write_and_emit_to(&path, None);
        assert_eq!(none_stats.enqueued(), 0);
    }
}
