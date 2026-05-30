//! WFB receiver: FEC-combine fragments from the local NIC + remote relays.
//!
//! Ports `wfb_receiver.py`'s FEC supervision. `wfb_rx -p 0 -c 127.0.0.1 -u 5600
//! -a <listen_port> [<drone_iface>]` aggregates fragments arriving on the local
//! monitor adapter AND from relays forwarding over batman-adv into the
//! aggregator UDP port, FEC-combines them, and emits the decoded stream to
//! localhost UDP 5600 where the existing mediamtx-gs pipeline republishes it.
//! The stderr stats line drives `fragments_after_dedup` / `fec_repaired` /
//! `output_kbps`; `wfb-receiver.json` is written atomically.
//!
//! Discovery is Rust-native: the receiver advertises `_ados-receiver._tcp` on
//! `bat0` via [`crate::mdns::advertise_receiver`] (held for the loop lifetime,
//! unregistered on shutdown) so relays resolve it. The relay-churn task reads
//! batman-adv neighbor MACs (`batctl n -H`) on the mesh interface to populate
//! and age the per-relay map, emitting `relay_connected` on first sight and
//! `relay_disconnected` past the grace window. The aggregator subprocess
//! lifecycle, the stats tail, the churn watcher, and the state file are all
//! owned here.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tokio::sync::Mutex;

use crate::gs_config::GroundStationConfig;
use crate::mesh_events;
use crate::process_spawn::GsWfbProcess;

/// Per-relay liveness grace before a silent relay is aged out of the map.
/// Mirrors the Python `_RELAY_GRACE_MS = 4000`.
const RELAY_GRACE_MS: i64 = 4000;
/// State-write + churn-poll cadence. Mirrors the Python `_POLL_INTERVAL_S = 2.0`.
const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// Aggregator graceful-shutdown grace before SIGKILL.
const AGGREGATOR_GRACE: Duration = Duration::from_secs(3);

/// The receiver's published state (the `wfb-receiver.json` shape, byte-identical
/// to the Python `_write_state`). Relays are flattened to a list on write.
#[derive(Debug, Clone, serde::Serialize)]
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
}

/// Per-relay fragment stats (one entry in the `relays` list).
#[derive(Debug, Clone, serde::Serialize)]
pub struct RelayStats {
    pub mac: String,
    pub last_seen_ms: i64,
    pub fragments: i64,
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
        }
    }
}

impl ReceiverState {
    /// Atomically write the state to `wfb-receiver.json` (Contract-E path).
    /// Honours the `ADOS_RUN_DIR` test override via `run_path`.
    pub fn write(&self) -> std::io::Result<()> {
        let path = crate::paths::run_path("wfb-receiver.json");
        crate::sidecars::write_json_atomic(Path::new(&path), self, 0o644)
    }
}

/// Build the `wfb_rx -a` aggregator args. With `accept_local_nic` the local
/// monitor adapter is appended so its fragments are aggregated too; without it
/// the receiver trusts only relay forwards. Mirrors `_launch_wfb_rx_aggregate`.
pub fn aggregate_args(
    drone_iface: &str,
    listen_port: u16,
    accept_local_nic: bool,
    rx_key: &Path,
) -> Vec<String> {
    let mut args = vec![
        "-p".into(),
        "0".into(),
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
        args.push(drone_iface.into());
    }
    args
}

/// Spawn the FEC-combine aggregator in its own process group (setsid/killpg).
/// stderr is piped so the stats tail can read the combined counters.
pub async fn spawn_aggregator(
    drone_iface: &str,
    listen_port: u16,
    accept_local_nic: bool,
) -> std::io::Result<GsWfbProcess> {
    let rx_key = Path::new(ados_radio::paths::WFB_RX_KEY);
    let args = aggregate_args(drone_iface, listen_port, accept_local_nic, rx_key);
    GsWfbProcess::spawn_stderr_piped("wfb_rx", &args).await
}

/// Parse one aggregator stderr line for the combined counters. Mirrors
/// `_tail_stats`: a line containing `n_out:` carries the post-dedup count,
/// `fec_rec:` the repaired count, `bitrate_kbps:` the output rate. Returns
/// `(after_dedup, fec_repaired, output_kbps)` updates when present.
pub fn parse_receiver_stats_line(line: &str) -> (Option<i64>, Option<i64>, Option<i64>) {
    if !line.contains("n_out:") {
        return (None, None, None);
    }
    let mut after_dedup = None;
    let mut fec_repaired = None;
    let mut output_kbps = None;
    for tok in line.split_whitespace() {
        if let Some(v) = tok.strip_prefix("n_out:") {
            after_dedup = v.parse::<i64>().ok();
        } else if let Some(v) = tok.strip_prefix("fec_rec:") {
            fec_repaired = v.parse::<i64>().ok();
        } else if let Some(v) = tok.strip_prefix("bitrate_kbps:") {
            output_kbps = v.parse::<i64>().ok();
        }
    }
    (after_dedup, fec_repaired, output_kbps)
}

/// Upsert the relays seen this poll (by batman-neighbor MAC) into `state`,
/// refreshing `last_seen_ms` and returning the MACs that were NOT present
/// before (first sight → caller emits `relay_connected`). Pure over the
/// serialized `Vec<RelayStats>`. Mirrors the populate half the Python module
/// never implemented (its `_watch_relay_churn` only ages out).
fn upsert_relays(state: &mut ReceiverState, macs: &[String], now_ms: i64) -> Vec<String> {
    let mut newly = Vec::new();
    for mac in macs {
        if let Some(r) = state.relays.iter_mut().find(|r| &r.mac == mac) {
            r.last_seen_ms = now_ms;
        } else {
            state.relays.push(RelayStats {
                mac: mac.clone(),
                last_seen_ms: now_ms,
                fragments: 0,
            });
            newly.push(mac.clone());
        }
    }
    newly
}

/// Remove relays silent past `RELAY_GRACE_MS`, returning the aged-out MACs so
/// the caller emits `relay_disconnected`. Mirrors the Python `_watch_relay_churn`
/// stale-removal half.
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

/// Tail the aggregator's stderr, folding combined counters into shared state.
/// Mirrors the Python `_tail_stats`. Returns when the stderr pipe closes.
async fn tail_aggregator_stats(
    stderr: tokio::process::ChildStderr,
    state: Arc<Mutex<ReceiverState>>,
) {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let (dedup, fec, kbps) = parse_receiver_stats_line(&line);
        if dedup.is_some() || fec.is_some() || kbps.is_some() {
            let mut s = state.lock().await;
            if let Some(v) = dedup {
                s.fragments_after_dedup = v;
            }
            if let Some(v) = fec {
                s.fec_repaired = v;
            }
            if let Some(v) = kbps {
                s.output_kbps = v;
            }
        }
    }
}

/// Read the batman-adv neighbor MACs on the mesh interface (`batctl n -H`).
/// These are the relays (and any other mesh peers) currently reachable on
/// `bat0`; the churn watcher folds them into the per-relay map. Returns an
/// empty vec on a batctl error so a wedged module never stalls the loop.
async fn neighbor_macs(_mesh_iface: &str) -> Vec<String> {
    let (rc, out, _e) = crate::mesh::batctl::run(
        "batctl",
        &["n", "-H"],
        Duration::from_secs(3),
    )
    .await;
    if rc != 0 {
        return Vec::new();
    }
    crate::mesh::batctl::parse_neighbors(&out, mesh_events::now_ms())
        .into_iter()
        .map(|n| n.mac)
        .collect()
}

/// The relay-churn watcher: each `POLL_INTERVAL`, read mesh neighbor MACs,
/// upsert them into the relay map (emit `relay_connected` for new), and age out
/// the silent ones (emit `relay_disconnected`). Runs until cancelled.
async fn watch_relay_churn(state: Arc<Mutex<ReceiverState>>, mesh_iface: String) {
    loop {
        tokio::time::sleep(POLL_INTERVAL).await;
        let macs = neighbor_macs(&mesh_iface).await;
        let now = mesh_events::now_ms();
        let (newly, removed) = {
            let mut s = state.lock().await;
            let newly = upsert_relays(&mut s, &macs, now);
            let removed = age_out_relays(&mut s, now);
            (newly, removed)
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
/// Detects the local drone-facing adapter (when `accept_local_nic`), spawns the
/// `wfb_rx` aggregator once, advertises `_ados-receiver._tcp` on `bat0`, then
/// runs the stats tail + relay-churn watcher + periodic state writer
/// concurrently. On shutdown (or aggregator exit) all tasks are cancelled, the
/// aggregator is terminated gracefully, the mDNS record is unregistered, and
/// `up=false` is persisted.
pub async fn run(shutdown: Arc<tokio::sync::Notify>) {
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

    // Detect the local monitor adapter when local-NIC aggregation is enabled.
    let mut drone_iface = String::new();
    if accept_local_nic {
        match ados_radio::adapter::select_interface("").await {
            Some(sel) if sel.injection_ok => drone_iface = sel.ifname,
            Some(sel) => {
                tracing::warn!(iface = %sel.ifname, "wfb_receiver_monitor_mode_failed");
                mesh_events::emit(
                    mesh_events::KIND_WFB_ADAPTER_MISSING,
                    json!({
                        "side": "receiver",
                        "reason": "monitor_mode_failed",
                        "detail": format!("Could not put {} into monitor mode.", sel.ifname),
                    }),
                );
            }
            None => {
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
            }
        }
    }
    {
        let mut s = state.lock().await;
        s.drone_iface = drone_iface.clone();
    }

    if !Path::new(ados_radio::paths::WFB_RX_KEY).exists() {
        tracing::warn!("wfb_receiver_keys_missing");
    }

    // Advertise on the mesh fabric so relays can resolve us. Held for the loop
    // lifetime; dropped (unregister + shutdown) on exit.
    let advert = crate::mdns::advertise_receiver(&service_type, &mesh_iface, listen_port);

    // Spawn the aggregator once. With no local adapter the receiver trusts only
    // relay forwards (the iface arg is dropped by `aggregate_args`).
    let use_local = accept_local_nic && !drone_iface.is_empty();
    let mut aggregator = match spawn_aggregator(&drone_iface, listen_port, use_local).await {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(error = %e, "wfb_receiver_aggregator_spawn_failed");
            if let Some(a) = &advert {
                a.shutdown();
            }
            let mut s = state.lock().await;
            s.up = false;
            let _ = s.write();
            return;
        }
    };
    {
        let mut s = state.lock().await;
        s.up = true;
    }
    let _ = state.lock().await.write();

    let tail_task = aggregator
        .take_stderr()
        .map(|stderr| tokio::spawn(tail_aggregator_stats(stderr, state.clone())));
    let churn_task = tokio::spawn(watch_relay_churn(state.clone(), mesh_iface.clone()));
    let writer_task = {
        let state = state.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            loop {
                if let Err(e) = state.lock().await.write() {
                    tracing::debug!(error = %e, "receiver_state_write_failed");
                }
                tokio::select! {
                    _ = shutdown.notified() => break,
                    _ = tokio::time::sleep(POLL_INTERVAL) => {}
                }
            }
        })
    };

    // The role ends when shutdown fires or the aggregator exits.
    tokio::select! {
        _ = shutdown.notified() => {}
        _ = wait_aggregator_exit(&mut aggregator) => {
            tracing::warn!("wfb_receiver_aggregator_exited");
        }
    }

    tracing::info!("wfb_receiver_stopping");
    if let Some(t) = tail_task {
        t.abort();
    }
    churn_task.abort();
    writer_task.abort();
    aggregator.terminate_then_kill(AGGREGATOR_GRACE).await;
    if let Some(a) = &advert {
        a.shutdown();
    }
    {
        let mut s = state.lock().await;
        s.up = false;
    }
    let _ = state.lock().await.write();
    tracing::info!("wfb_receiver_stopped");
}

/// Poll the aggregator until it exits. One arm of the completion select.
async fn wait_aggregator_exit(proc: &mut GsWfbProcess) {
    loop {
        if !proc.is_running() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregate_args_with_local_nic() {
        // wfb_rx -p 0 -c 127.0.0.1 -u 5600 -a 5800 -K <rx.key> <iface>
        let a = aggregate_args("wlan0", 5800, true, Path::new("/etc/ados/wfb/rx.key"));
        assert_eq!(
            a,
            vec![
                "-p",
                "0",
                "-c",
                "127.0.0.1",
                "-u",
                "5600",
                "-a",
                "5800",
                "-K",
                "/etc/ados/wfb/rx.key",
                "wlan0"
            ]
        );
    }

    #[test]
    fn aggregate_args_without_local_nic_drops_iface() {
        let a = aggregate_args("wlan0", 5800, false, Path::new("/k"));
        assert!(!a.contains(&"wlan0".to_string()));
        // The aggregator still listens on the relay forward port.
        let ai = a.iter().position(|x| x == "-a").unwrap();
        assert_eq!(a[ai + 1], "5800");
    }

    #[test]
    fn parse_aggregator_stats_pulls_three_counters() {
        let line = "999 PKT n_out:1500 fec_rec:12 bitrate_kbps:4200";
        let (dedup, fec, kbps) = parse_receiver_stats_line(line);
        assert_eq!(dedup, Some(1500));
        assert_eq!(fec, Some(12));
        assert_eq!(kbps, Some(4200));
    }

    #[test]
    fn non_aggregator_line_ignored() {
        let (d, f, k) = parse_receiver_stats_line("starting up");
        assert!(d.is_none() && f.is_none() && k.is_none());
    }

    #[test]
    fn receiver_state_json_shape_flattens_relays() {
        let mut s = ReceiverState::default();
        s.relays.push(RelayStats {
            mac: "aa:bb:cc:dd:ee:ff".into(),
            last_seen_ms: 123,
            fragments: 500,
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
    fn age_out_then_reconnect_emits_again() {
        // A relay that ages out and later reappears is reported new again.
        let mut s = ReceiverState::default();
        upsert_relays(&mut s, &["r1".into()], 0);
        let removed = age_out_relays(&mut s, RELAY_GRACE_MS + 1);
        assert_eq!(removed, vec!["r1".to_string()]);
        let newly = upsert_relays(&mut s, &["r1".into()], RELAY_GRACE_MS + 2);
        assert_eq!(newly, vec!["r1".to_string()]);
    }

    #[tokio::test]
    async fn tail_folds_aggregator_counters() {
        #[cfg(target_os = "linux")]
        {
            let state = Arc::new(Mutex::new(ReceiverState::default()));
            let script = "printf 'X PKT n_out:1500 fec_rec:12 bitrate_kbps:4200\\n' 1>&2";
            let mut proc = GsWfbProcess::spawn_stderr_piped(
                "sh",
                &["-c".to_string(), script.to_string()],
            )
            .await
            .expect("spawn sh");
            let stderr = proc.take_stderr().expect("stderr piped");
            tail_aggregator_stats(stderr, state.clone()).await;
            let s = state.lock().await;
            assert_eq!(s.fragments_after_dedup, 1500);
            assert_eq!(s.fec_repaired, 12);
            assert_eq!(s.output_kbps, 4200);
            proc.kill().await;
        }
        #[cfg(not(target_os = "linux"))]
        {
            let mut s = ReceiverState::default();
            let _ = upsert_relays(&mut s, &[], 0);
        }
    }

    #[test]
    fn receiver_state_write_honours_run_dir_override() {
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: serialized within this single-threaded test.
        unsafe {
            std::env::set_var("ADOS_RUN_DIR", dir.path());
        }
        let mut s = ReceiverState {
            listen_port: 5800,
            up: true,
            ..Default::default()
        };
        upsert_relays(&mut s, &["aa:bb:cc:dd:ee:ff".into()], 42);
        s.write().unwrap();
        let written =
            std::fs::read_to_string(dir.path().join("wfb-receiver.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&written).unwrap();
        assert_eq!(v["up"], true);
        assert_eq!(v["relays"][0]["mac"], "aa:bb:cc:dd:ee:ff");
        unsafe {
            std::env::remove_var("ADOS_RUN_DIR");
        }
    }
}
