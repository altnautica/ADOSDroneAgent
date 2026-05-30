//! WFB relay: FEC-forward fragments to a receiver over batman-adv.
//!
//! Ports `wfb_relay.py`'s FEC supervision. The drone-facing RTL8812 adapter
//! runs `wfb_rx -p 0 -f <receiver_ip>:<port>` to forward video fragments to the
//! receiver; the stderr `PKT` stats line drives the `fragments_seen` /
//! `fragments_forwarded` counters; `wfb-relay.json` is written atomically.
//!
//! Discovery is Rust-native: the relay browses `_ados-receiver._tcp` on `bat0`
//! each poll via [`crate::mdns::resolve_receiver`] and forwards to the resolved
//! `(ip, port)`, filtering to the mesh `/24` so it never picks a receiver on the
//! shared LAN. On a receiver change the old forwarder is terminated (SIGTERM,
//! 3s grace, SIGKILL) and a fresh one spawned; a receiver-loss grace window
//! marks the link down, emits `receiver_unreachable` across the cross-process
//! event seam, and tears the forwarder down. The FEC subprocess lifecycle, the
//! stats tail, the state file, and the event emit are all owned here.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use ados_radio::config::WfbConfig;
use serde_json::json;
use tokio::sync::Mutex;

use crate::gs_config::GroundStationConfig;
use crate::mesh_events;
use crate::process_spawn::GsWfbProcess;

/// Receiver-loss grace window: how long the relay tolerates the receiver
/// dropping off mDNS before it marks the link down. Mirrors the Python
/// `_RECEIVER_LOST_GRACE_S = 15.0`.
const RECEIVER_LOST_GRACE_MS: i64 = 15_000;
/// Poll cadence for re-resolving the receiver and republishing state. Mirrors
/// the Python `_POLL_INTERVAL_S = 2.0`.
const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// mDNS resolve timeout per poll. Mirrors the Python `_resolve_receiver`
/// `timeout=3.0`.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(3);
/// Forwarder graceful-shutdown grace before SIGKILL. Mirrors the Python
/// `wait_for(proc.wait(), timeout=3.0)` between terminate and kill.
const FORWARDER_GRACE: Duration = Duration::from_secs(3);

/// The relay's published state (the `wfb-relay.json` shape, byte-identical to
/// the Python `_write_state`).
#[derive(Debug, Clone, serde::Serialize)]
pub struct RelayState {
    pub role: String,
    pub drone_iface: String,
    pub receiver_ip: Option<String>,
    pub receiver_port: i64,
    pub receiver_last_seen_ms: i64,
    pub fragments_seen: i64,
    pub fragments_forwarded: i64,
    pub up: bool,
    pub mesh_iface: String,
}

impl Default for RelayState {
    fn default() -> Self {
        Self {
            role: "relay".to_string(),
            drone_iface: String::new(),
            receiver_ip: None,
            receiver_port: 5800,
            receiver_last_seen_ms: 0,
            fragments_seen: 0,
            fragments_forwarded: 0,
            up: false,
            mesh_iface: "bat0".to_string(),
        }
    }
}

impl RelayState {
    /// Atomically write the state to `wfb-relay.json` (Contract-E path). Honours
    /// the `ADOS_RUN_DIR` test override via `run_path`.
    pub fn write(&self) -> std::io::Result<()> {
        let path = crate::paths::run_path("wfb-relay.json");
        crate::sidecars::write_json_atomic(Path::new(&path), self, 0o644)
    }
}

/// Build the `wfb_rx -f` FEC-forward args for the drone-facing adapter. Uses the
/// rx key (decrypts the drone uplink). Mirrors `_launch_wfb_rx_forward`.
pub fn forward_args(
    drone_iface: &str,
    receiver_ip: &str,
    receiver_port: u16,
    rx_key: &Path,
) -> Vec<String> {
    vec![
        "-p".into(),
        "0".into(),
        "-f".into(),
        format!("{receiver_ip}:{receiver_port}"),
        "-K".into(),
        rx_key.to_string_lossy().into_owned(),
        drone_iface.into(),
    ]
}

/// Spawn the FEC forwarder for `(receiver_ip, receiver_port)` on the
/// drone-facing adapter, in its own process group (setsid/killpg). stderr is
/// piped so the stats tail can read the `PKT` counters.
pub async fn spawn_forwarder(
    drone_iface: &str,
    receiver_ip: &str,
    receiver_port: u16,
) -> std::io::Result<GsWfbProcess> {
    let rx_key = Path::new(ados_radio::paths::WFB_RX_KEY);
    let args = forward_args(drone_iface, receiver_ip, receiver_port, rx_key);
    // stderr piped (the PKT stats land there); stdout discarded.
    GsWfbProcess::spawn_stderr_piped("wfb_rx", &args).await
}

/// Parse one `wfb_rx` stderr line for the relay fragment counters. Mirrors
/// `_tail_stats`: a `PKT` line carries `n_all:<seen>` and `n_out:<forwarded>`.
/// Returns `(seen, forwarded)` updates when present.
pub fn parse_relay_stats_line(line: &str) -> (Option<i64>, Option<i64>) {
    if !line.contains("PKT") {
        return (None, None);
    }
    let mut seen = None;
    let mut forwarded = None;
    for tok in line.split_whitespace() {
        if let Some(v) = tok.strip_prefix("n_all:") {
            seen = v.parse::<i64>().ok();
        } else if let Some(v) = tok.strip_prefix("n_out:") {
            forwarded = v.parse::<i64>().ok();
        }
    }
    (seen, forwarded)
}

/// The relay receiver port default (`ground_station.wfb_relay.receiver_port`).
/// Kept as a helper so the call site is explicit; the live value comes from
/// [`GroundStationConfig`].
pub fn default_receiver_port(_cfg: &WfbConfig) -> u16 {
    5800
}

/// True when the receiver should be treated as lost: a previously-seen
/// receiver has gone silent past the grace window while the link was up.
/// Mirrors the Python `stale_ms > _RECEIVER_LOST_GRACE_S * 1000 and state.up`.
fn receiver_is_stale(last_seen_ms: i64, was_up: bool, now_ms: i64) -> bool {
    last_seen_ms > 0 && was_up && (now_ms - last_seen_ms) > RECEIVER_LOST_GRACE_MS
}

/// Tail a forwarder's stderr, folding each `PKT` line into the shared state's
/// fragment counters. Mirrors the Python `_tail_stats`. Returns when the stderr
/// pipe closes (the forwarder exited).
async fn tail_forwarder_stats(stderr: tokio::process::ChildStderr, state: Arc<Mutex<RelayState>>) {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let (seen, forwarded) = parse_relay_stats_line(&line);
        if seen.is_some() || forwarded.is_some() {
            let mut s = state.lock().await;
            if let Some(v) = seen {
                s.fragments_seen = v;
            }
            if let Some(v) = forwarded {
                s.fragments_forwarded = v;
            }
        }
    }
}

/// Run the relay role to completion (until `shutdown` fires).
///
/// Detects the drone-facing adapter + monitor mode (via the shared radio
/// selector), then loops: re-resolve the receiver over mDNS on `bat0`; on a
/// receiver change tear down the old forwarder and spawn a fresh one (emitting
/// `relay_connected`); on receiver loss past the grace window mark the link
/// down and emit `receiver_unreachable`; write `wfb-relay.json` every poll. On
/// shutdown the forwarder is terminated gracefully and `up=false` is persisted.
pub async fn run(shutdown: Arc<tokio::sync::Notify>) {
    let cfg = GroundStationConfig::load_from(Path::new("/etc/ados/config.yaml"));
    let mesh_iface = cfg.mesh.bat_iface.clone();
    let service_type = cfg.wfb_relay.receiver_mdns_service.clone();

    let state = Arc::new(Mutex::new(RelayState {
        mesh_iface: mesh_iface.clone(),
        receiver_port: cfg.wfb_relay.receiver_port as i64,
        ..Default::default()
    }));

    // Detect the drone-facing adapter and put it into monitor mode (the shared
    // selector denies the control iface + AIC8800 and verifies the readback).
    let drone_iface = match resolve_drone_iface().await {
        Some(iface) => iface,
        None => {
            tracing::error!("wfb_relay_no_adapter");
            mesh_events::emit(
                mesh_events::KIND_WFB_ADAPTER_MISSING,
                json!({
                    "side": "relay",
                    "reason": "adapter_not_found",
                    "detail": "No monitor-capable WFB adapter detected on the relay node.",
                }),
            );
            // Persist a down state so the UI shows the fault, then idle until
            // shutdown rather than crash-loop the unit.
            {
                let mut s = state.lock().await;
                s.up = false;
            }
            let _ = state.lock().await.write();
            shutdown.notified().await;
            return;
        }
    };
    {
        let mut s = state.lock().await;
        s.drone_iface = drone_iface.clone();
    }

    if !Path::new(ados_radio::paths::WFB_RX_KEY).exists() {
        tracing::warn!("wfb_relay_keys_missing");
    }

    let mut forwarder: Option<GsWfbProcess> = None;
    let mut tail_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut current_receiver: Option<(String, u16)> = None;

    loop {
        let resolved =
            crate::mdns::resolve_receiver(&service_type, &mesh_iface, RESOLVE_TIMEOUT).await;
        let now = mesh_events::now_ms();

        if let Some((ip, port)) = resolved {
            {
                let mut s = state.lock().await;
                s.receiver_last_seen_ms = now;
            }
            if current_receiver.as_ref() != Some(&(ip.clone(), port)) {
                // Receiver changed: tear down the old forwarder, spawn fresh.
                if let Some(mut old) = forwarder.take() {
                    old.terminate_then_kill(FORWARDER_GRACE).await;
                }
                if let Some(t) = tail_task.take() {
                    t.abort();
                }
                {
                    let mut s = state.lock().await;
                    s.receiver_ip = Some(ip.clone());
                    s.receiver_port = port as i64;
                }
                match spawn_forwarder(&drone_iface, &ip, port).await {
                    Ok(mut proc) => {
                        if let Some(stderr) = proc.take_stderr() {
                            tail_task =
                                Some(tokio::spawn(tail_forwarder_stats(stderr, state.clone())));
                        }
                        forwarder = Some(proc);
                        {
                            let mut s = state.lock().await;
                            s.up = true;
                        }
                        current_receiver = Some((ip.clone(), port));
                        mesh_events::emit(
                            mesh_events::KIND_RELAY_CONNECTED,
                            json!({ "receiver_ip": ip, "receiver_port": port }),
                        );
                        tracing::info!(receiver = %ip, port, "wfb_relay_forwarding");
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "wfb_relay_spawn_failed");
                        let mut s = state.lock().await;
                        s.up = false;
                    }
                }
            }
        } else {
            // No receiver this poll: if we had one and the grace window passed,
            // mark the link down, emit the event, and tear the forwarder down.
            let (last_seen, was_up, last_ip) = {
                let s = state.lock().await;
                (s.receiver_last_seen_ms, s.up, s.receiver_ip.clone())
            };
            if receiver_is_stale(last_seen, was_up, now) {
                let stale = now - last_seen;
                {
                    let mut s = state.lock().await;
                    s.up = false;
                }
                mesh_events::emit(
                    mesh_events::KIND_RECEIVER_UNREACHABLE,
                    json!({ "last_receiver": last_ip, "stale_ms": stale }),
                );
                if let Some(mut old) = forwarder.take() {
                    old.terminate_then_kill(FORWARDER_GRACE).await;
                }
                if let Some(t) = tail_task.take() {
                    t.abort();
                }
                current_receiver = None;
                tracing::warn!(stale_ms = stale, "wfb_relay_receiver_unreachable");
            }
        }

        if let Err(e) = state.lock().await.write() {
            tracing::debug!(error = %e, "relay_state_write_failed");
        }

        tokio::select! {
            _ = shutdown.notified() => break,
            _ = tokio::time::sleep(POLL_INTERVAL) => {}
        }
    }

    // Clean shutdown: terminate the forwarder and persist the down state.
    if let Some(t) = tail_task.take() {
        t.abort();
    }
    if let Some(mut proc) = forwarder.take() {
        proc.terminate_then_kill(FORWARDER_GRACE).await;
    }
    {
        let mut s = state.lock().await;
        s.up = false;
    }
    let _ = state.lock().await.write();
    tracing::info!("wfb_relay_stopped");
}

/// Detect and monitor-mode the drone-facing adapter via the shared radio
/// selector. Returns the interface name on success. The selector denies the
/// control iface + AIC8800 and verifies the monitor-mode readback (4× retry).
async fn resolve_drone_iface() -> Option<String> {
    let selected = ados_radio::adapter::select_interface("").await?;
    if selected.injection_ok {
        Some(selected.ifname)
    } else {
        tracing::warn!(iface = %selected.ifname, "wfb_relay_monitor_mode_failed");
        mesh_events::emit(
            mesh_events::KIND_WFB_ADAPTER_MISSING,
            json!({
                "side": "relay",
                "reason": "monitor_mode_failed",
                "detail": format!("Could not put {} into monitor mode.", selected.ifname),
            }),
        );
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forward_args_match_python() {
        // wfb_rx -p 0 -f <ip>:<port> -K <rx.key> <iface>
        let a = forward_args("wlan0", "10.0.0.5", 5800, Path::new("/etc/ados/wfb/rx.key"));
        assert_eq!(
            a,
            vec![
                "-p",
                "0",
                "-f",
                "10.0.0.5:5800",
                "-K",
                "/etc/ados/wfb/rx.key",
                "wlan0"
            ]
        );
    }

    #[test]
    fn parse_pkt_line_pulls_n_all_and_n_out() {
        let line = "12345 PKT n_all:1000 n_out:980 fec_rec:5";
        let (seen, fwd) = parse_relay_stats_line(line);
        assert_eq!(seen, Some(1000));
        assert_eq!(fwd, Some(980));
    }

    #[test]
    fn non_pkt_line_is_ignored() {
        let (seen, fwd) = parse_relay_stats_line("some random wfb_rx log");
        assert!(seen.is_none());
        assert!(fwd.is_none());
    }

    #[test]
    fn relay_state_json_shape() {
        let s = RelayState {
            drone_iface: "wlan0".into(),
            receiver_ip: Some("10.0.0.5".into()),
            up: true,
            ..Default::default()
        };
        let v = serde_json::to_value(&s).unwrap();
        for k in [
            "role",
            "drone_iface",
            "receiver_ip",
            "receiver_port",
            "receiver_last_seen_ms",
            "fragments_seen",
            "fragments_forwarded",
            "up",
            "mesh_iface",
        ] {
            assert!(v.get(k).is_some(), "missing key {k}");
        }
        assert_eq!(v["role"], "relay");
        assert_eq!(v["receiver_ip"], "10.0.0.5");
    }

    #[test]
    fn receiver_staleness_decision() {
        // No prior receiver → never stale.
        assert!(!receiver_is_stale(0, true, 1_000_000));
        // Up but within grace → not stale.
        assert!(!receiver_is_stale(100_000, true, 110_000));
        // Up and past grace → stale.
        assert!(receiver_is_stale(
            100_000,
            true,
            100_000 + RECEIVER_LOST_GRACE_MS + 1
        ));
        // Already down → not re-fired.
        assert!(!receiver_is_stale(
            100_000,
            false,
            100_000 + RECEIVER_LOST_GRACE_MS + 1
        ));
    }

    #[tokio::test]
    async fn tail_folds_pkt_lines_into_shared_state() {
        // Drive a child whose stderr emits two PKT lines, then prove the tail
        // task folded the counters into the shared RelayState.
        #[cfg(target_os = "linux")]
        {
            use std::sync::Arc;
            use tokio::sync::Mutex;

            let state = Arc::new(Mutex::new(RelayState::default()));
            // `sh -c 'printf ... 1>&2'` writes the PKT stats to stderr.
            let script = "printf 'X PKT n_all:100 n_out:90\\nX PKT n_all:200 n_out:185\\n' 1>&2";
            let mut proc =
                GsWfbProcess::spawn_stderr_piped("sh", &["-c".to_string(), script.to_string()])
                    .await
                    .expect("spawn sh");
            let stderr = proc.take_stderr().expect("stderr piped");
            tail_forwarder_stats(stderr, state.clone()).await;

            let s = state.lock().await;
            assert_eq!(s.fragments_seen, 200);
            assert_eq!(s.fragments_forwarded, 185);
            proc.kill().await;
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = receiver_is_stale(0, false, 0);
        }
    }

    #[test]
    fn relay_state_write_honours_run_dir_override() {
        // SAFETY: serialized within this single-threaded test.
        let dir = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("ADOS_RUN_DIR", dir.path());
        }
        let s = RelayState {
            drone_iface: "wlan1".into(),
            up: true,
            ..Default::default()
        };
        s.write().unwrap();
        let written = std::fs::read_to_string(dir.path().join("wfb-relay.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&written).unwrap();
        assert_eq!(v["drone_iface"], "wlan1");
        assert_eq!(v["up"], true);
        unsafe {
            std::env::remove_var("ADOS_RUN_DIR");
        }
    }
}
