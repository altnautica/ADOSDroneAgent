//! WFB relay: FEC-forward fragments to a receiver over batman-adv.
//!
//! Ports `wfb_relay.py`'s FEC supervision. The drone-facing RTL8812 adapter
//! runs `wfb_rx -p 0 -f <receiver_ip>:<port>` to forward video fragments to the
//! receiver; `wfb-relay.json` is written atomically. In forwarder mode `wfb_rx`
//! prints no stats (`vendor/wfb-ng/src/rx.hpp`: `Forwarder::dump_stats` is
//! empty), so the fragment counters are published as `null` — unmeasured, never
//! a fabricated zero; the receiver's combined counters are the measurement.
//!
//! Discovery is Rust-native: the relay browses `_ados-receiver._tcp` on `bat0`
//! each poll via [`crate::mdns::resolve_receiver`] and forwards to the resolved
//! `(ip, port)`, filtering to the mesh `/24` so it never picks a receiver on the
//! shared LAN. On a receiver change the old forwarder is terminated (SIGTERM,
//! 3s grace, SIGKILL) and a fresh one spawned; a forwarder that exits on its own
//! is respawned on the next pass; a receiver-loss grace window marks the link
//! down, emits `receiver_unreachable` across the cross-process event seam, and
//! tears the forwarder down.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use ados_protocol::shutdown::Shutdown;
use serde_json::json;
use tokio::sync::Mutex;

use crate::gs_config::GroundStationConfig;
use crate::mesh_events;
use crate::process_spawn::{GsWfbProcess, Stdout};

/// Receiver-loss grace window: how long the relay tolerates the receiver
/// dropping off mDNS before it marks the link down. Mirrors the Python
/// `_RECEIVER_LOST_GRACE_S = 15.0`.
const RECEIVER_LOST_GRACE_MS: i64 = 15_000;
/// Poll cadence for re-resolving the receiver and republishing state. Mirrors
/// the Python `_POLL_INTERVAL_S = 2.0`.
const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// mDNS resolve timeout per poll.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(3);
/// Forwarder graceful-shutdown grace before SIGKILL. Mirrors the Python
/// `wait_for(proc.wait(), timeout=3.0)` between terminate and kill.
const FORWARDER_GRACE: Duration = Duration::from_secs(3);
/// Fixed retry for drone-facing adapter detection while none is usable. No cap:
/// an adapter plugged in after boot is picked up within one interval.
const ADAPTER_RETRY: Duration = Duration::from_secs(5);
/// The forwarder's stderr log (diagnostics only; a file never fills the way an
/// unread pipe does).
const FORWARDER_LOG: &str = "/run/ados/wfb-gs-forwarder.log";

/// The relay's published state (the `wfb-relay.json` shape, byte-identical to
/// the Python `_write_state`). `Deserialize` round-trips the on-disk file shape
/// for parity assertions and any reader that loads the sidecar back.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RelayState {
    pub role: String,
    pub drone_iface: String,
    pub receiver_ip: Option<String>,
    pub receiver_port: i64,
    pub receiver_last_seen_ms: i64,
    /// Fragments captured off-air. `None`: the forwarder reports no counters.
    pub fragments_seen: Option<i64>,
    /// Fragments forwarded to the receiver. `None`: unmeasured, as above.
    pub fragments_forwarded: Option<i64>,
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
            fragments_seen: None,
            fragments_forwarded: None,
            up: false,
            mesh_iface: "bat0".to_string(),
        }
    }
}

impl RelayState {
    /// Atomically write the state to `wfb-relay.json` (Contract-E path). The
    /// run-dir path resolves via `run_path` (honouring the `ADOS_RUN_DIR`
    /// override); the write itself is delegated to `write_to` so tests can target
    /// an explicit temp path without mutating process-global env.
    pub fn write(&self) -> std::io::Result<()> {
        self.write_to(Path::new(&crate::paths::run_path("wfb-relay.json")))
    }

    /// Atomically write the state to an explicit sidecar path. The path seam that
    /// lets a test write into its own temp dir without touching `ADOS_RUN_DIR`.
    pub fn write_to(&self, path: &Path) -> std::io::Result<()> {
        crate::sidecars::write_json_atomic(path, self, 0o644)
    }

    /// Write the state file AND ship the same body to the logging store as a
    /// single `gs.relay_state` event. The struct is persisted to disk directly,
    /// so the on-disk sidecar stays byte-identical to `write()`; the JSON value
    /// is built only for the store event. Best-effort: an absent logging daemon
    /// drops the event without disturbing the poll loop, and an I/O error on the
    /// file write is surfaced to the caller exactly as `write()` does.
    pub fn write_and_emit(
        &self,
        ingest: Option<&ados_protocol::logd::emitter::IngestEmitter>,
    ) -> std::io::Result<()> {
        self.write_and_emit_to(Path::new(&crate::paths::run_path("wfb-relay.json")), ingest)
    }

    /// [`write_and_emit`](Self::write_and_emit) against an explicit sidecar path.
    /// The path seam that keeps a test's best-effort write inside its own temp
    /// tree without mutating the process-global `ADOS_RUN_DIR`.
    pub fn write_and_emit_to(
        &self,
        path: &Path,
        ingest: Option<&ados_protocol::logd::emitter::IngestEmitter>,
    ) -> std::io::Result<()> {
        let res = crate::sidecars::write_json_atomic(path, self, 0o644);
        if let Some(em) = ingest {
            let v = serde_json::to_value(self).unwrap_or(serde_json::Value::Null);
            em.emit_event(
                "gs.relay_state",
                ados_protocol::logd::Level::Info,
                crate::wfb_rx::stats::json_object_to_fields(&v),
            );
        }
        res
    }
}

/// Build the `wfb_rx -f` FEC-forward args for the drone-facing adapter. Uses the rx key
/// (decrypts the drone uplink).
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
/// drone-facing adapter, in its own process group (setsid/killpg). The
/// forwarder prints no stats; stderr goes to its log file.
pub async fn spawn_forwarder(
    drone_iface: &str,
    receiver_ip: &str,
    receiver_port: u16,
) -> std::io::Result<GsWfbProcess> {
    let rx_key = Path::new(ados_radio::paths::WFB_RX_KEY);
    let args = forward_args(drone_iface, receiver_ip, receiver_port, rx_key);
    GsWfbProcess::spawn("wfb_rx", &args, Stdout::Null, Some(FORWARDER_LOG)).await
}

/// True when the receiver should be treated as lost: a previously-seen
/// receiver has gone silent past the grace window while the link was up.
/// Mirrors the Python `stale_ms > _RECEIVER_LOST_GRACE_S * 1000 and state.up`.
fn receiver_is_stale(last_seen_ms: i64, was_up: bool, now_ms: i64) -> bool {
    last_seen_ms > 0 && was_up && (now_ms - last_seen_ms) > RECEIVER_LOST_GRACE_MS
}

/// True when a held forwarder has exited on its own, so the pass must forget it
/// and let the receiver reconcile spawn a fresh one. Pure over the observation.
fn forwarder_needs_respawn(held: bool, running: bool) -> bool {
    held && !running
}

/// Run the relay role to completion (until `shutdown` fires).
///
/// Detects the drone-facing adapter + monitor mode (via the shared radio
/// selector), then loops: re-resolve the receiver over mDNS on `bat0`; on a
/// receiver change tear down the old forwarder and spawn a fresh one (emitting
/// `relay_connected`); on receiver loss past the grace window mark the link
/// down and emit `receiver_unreachable`; write `wfb-relay.json` every poll. On
/// shutdown the forwarder is terminated gracefully and `up=false` is persisted.
pub async fn run(
    shutdown: Shutdown,
    ingest: Option<ados_protocol::logd::emitter::IngestEmitter>,
    progress: ados_supervisor::sdnotify::MonitorProgress,
) {
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
    // With none usable, publish the down state and retry on a fixed interval,
    // stamping progress so a correctly parked relay is not restarted by the
    // systemd watchdog; an adapter plugged in later is picked up.
    let mut reported_missing = false;
    let drone_iface = loop {
        progress.mark();
        if let Some(iface) = resolve_drone_iface(!reported_missing).await {
            break iface;
        }
        if !reported_missing {
            tracing::error!("wfb_relay_no_adapter");
            reported_missing = true;
        }
        state.lock().await.up = false;
        let _ = state.lock().await.write_and_emit(ingest.as_ref());
        tokio::select! {
            _ = shutdown.wait() => return,
            _ = tokio::time::sleep(ADAPTER_RETRY) => {}
        }
    };
    state.lock().await.drone_iface = drone_iface.clone();

    if !Path::new(ados_radio::paths::WFB_RX_KEY).exists() {
        tracing::warn!("wfb_relay_keys_missing");
    }

    let mut forwarder: Option<GsWfbProcess> = None;
    let mut current_receiver: Option<(String, u16)> = None;

    loop {
        // One stamp per pass: mDNS resolve, forwarder reconcile, state write.
        progress.mark();
        // A forwarder that exited on its own is forgotten here, so the
        // receiver reconcile below spawns a fresh one on this same pass.
        let running = match forwarder.as_mut() {
            Some(p) => p.is_running(),
            None => false,
        };
        if forwarder_needs_respawn(forwarder.is_some(), running) {
            tracing::warn!("wfb_relay_forwarder_exited_respawning");
            forwarder = None;
            current_receiver = None;
            state.lock().await.up = false;
        }
        let resolved =
            crate::mdns::resolve_receiver(&service_type, &mesh_iface, RESOLVE_TIMEOUT).await;
        let now = mesh_events::now_ms();

        if let Some((ip, port)) = resolved {
            state.lock().await.receiver_last_seen_ms = now;
            if current_receiver.as_ref() != Some(&(ip.clone(), port)) {
                // Receiver changed (or the forwarder died): tear down the old
                // forwarder, spawn fresh.
                if let Some(mut old) = forwarder.take() {
                    old.terminate_then_kill(FORWARDER_GRACE).await;
                }
                {
                    let mut s = state.lock().await;
                    s.receiver_ip = Some(ip.clone());
                    s.receiver_port = port as i64;
                }
                match spawn_forwarder(&drone_iface, &ip, port).await {
                    Ok(proc) => {
                        forwarder = Some(proc);
                        state.lock().await.up = true;
                        current_receiver = Some((ip.clone(), port));
                        mesh_events::emit(
                            mesh_events::KIND_RELAY_CONNECTED,
                            json!({ "receiver_ip": ip, "receiver_port": port }),
                        );
                        tracing::info!(receiver = %ip, port, "wfb_relay_forwarding");
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "wfb_relay_spawn_failed");
                        state.lock().await.up = false;
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
                state.lock().await.up = false;
                mesh_events::emit(
                    mesh_events::KIND_RECEIVER_UNREACHABLE,
                    json!({ "last_receiver": last_ip, "stale_ms": stale }),
                );
                if let Some(mut old) = forwarder.take() {
                    old.terminate_then_kill(FORWARDER_GRACE).await;
                }
                current_receiver = None;
                tracing::warn!(stale_ms = stale, "wfb_relay_receiver_unreachable");
            }
        }

        if let Err(e) = state.lock().await.write_and_emit(ingest.as_ref()) {
            tracing::debug!(error = %e, "relay_state_write_failed");
        }

        tokio::select! {
            _ = shutdown.wait() => break,
            _ = tokio::time::sleep(POLL_INTERVAL) => {}
        }
    }

    // Clean shutdown: terminate the forwarder and persist the down state.
    if let Some(mut proc) = forwarder.take() {
        proc.terminate_then_kill(FORWARDER_GRACE).await;
    }
    {
        let mut s = state.lock().await;
        s.up = false;
    }
    let _ = state.lock().await.write_and_emit(ingest.as_ref());
    // Restore the drone-facing adapter to managed mode so the kernel /
    // NetworkManager can re-enumerate it instead of finding it stranded in
    // monitor mode after the unit stops (the mirror of the drone-side teardown).
    tracing::info!(interface = %drone_iface, "restoring drone-facing adapter to managed mode");
    ados_radio::adapter::set_managed_mode(&drone_iface).await;
    tracing::info!("wfb_relay_stopped");
}

/// Detect and monitor-mode the drone-facing adapter via the shared radio
/// selector. Returns the interface name on success. The selector denies the
/// control iface + AIC8800 and verifies the monitor-mode readback (4× retry).
/// `report` emits the adapter-missing event (the first failure only, so a
/// fixed-interval retry does not flood the event log).
async fn resolve_drone_iface(report: bool) -> Option<String> {
    let emit = |reason: &str, detail: String| {
        if report {
            mesh_events::emit(
                mesh_events::KIND_WFB_ADAPTER_MISSING,
                json!({ "side": "relay", "reason": reason, "detail": detail }),
            );
        }
    };
    let Some(selected) = ados_radio::adapter::select_interface("").await else {
        emit(
            "adapter_not_found",
            "No monitor-capable WFB adapter detected on the relay node.".to_string(),
        );
        return None;
    };
    if selected.injection_ok {
        Some(selected.ifname)
    } else {
        tracing::warn!(iface = %selected.ifname, "wfb_relay_monitor_mode_failed");
        emit(
            "monitor_mode_failed",
            format!("Could not put {} into monitor mode.", selected.ifname),
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

    /// A forwarder that exited on its own is respawned; a live one, or no
    /// forwarder at all, is left alone.
    #[test]
    fn an_exited_forwarder_is_respawned() {
        assert!(forwarder_needs_respawn(true, false));
        assert!(!forwarder_needs_respawn(true, true));
        assert!(!forwarder_needs_respawn(false, false));
    }

    /// The forwarder reports no counters, so a fresh relay state publishes
    /// them as null rather than a zero it never measured.
    #[test]
    fn fragment_counters_are_null_not_zero() {
        let v = serde_json::to_value(RelayState::default()).unwrap();
        assert!(v["fragments_seen"].is_null());
        assert!(v["fragments_forwarded"].is_null());
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

    #[test]
    fn relay_fixture_round_trips_with_python_shape() {
        // The exact JSON the Python `_write_state` produced for a relay forwarding
        // to a live receiver. Deserialize into the Rust struct, then re-serialize
        // and assert the key set + values are preserved (no field drift).
        let fixture = r#"{
            "role": "relay",
            "drone_iface": "wlan1",
            "receiver_ip": "10.42.0.5",
            "receiver_port": 5800,
            "receiver_last_seen_ms": 1717000000000,
            "fragments_seen": 12345,
            "fragments_forwarded": 12000,
            "up": true,
            "mesh_iface": "bat0"
        }"#;
        let s: RelayState = serde_json::from_str(fixture).expect("deserialize relay fixture");
        assert_eq!(s.role, "relay");
        assert_eq!(s.drone_iface, "wlan1");
        assert_eq!(s.receiver_ip.as_deref(), Some("10.42.0.5"));
        assert_eq!(s.receiver_port, 5800);
        assert_eq!(s.receiver_last_seen_ms, 1_717_000_000_000);
        assert_eq!(s.fragments_seen, Some(12345));
        assert_eq!(s.fragments_forwarded, Some(12000));
        assert!(s.up);
        assert_eq!(s.mesh_iface, "bat0");

        // Round-trip back to the same key set + values as the fixture.
        let re = serde_json::to_value(&s).unwrap();
        let orig: serde_json::Value = serde_json::from_str(fixture).unwrap();
        assert_eq!(re, orig);
    }

    #[test]
    fn relay_fixture_null_receiver_ip_locks_option() {
        // A relay with no resolved receiver writes `receiver_ip: null`; the Rust
        // `Option<String>` must accept it as `None` (not a deserialize error).
        let fixture = r#"{
            "role": "relay",
            "drone_iface": "wlan1",
            "receiver_ip": null,
            "receiver_port": 5800,
            "receiver_last_seen_ms": 0,
            "fragments_seen": 0,
            "fragments_forwarded": 0,
            "up": false,
            "mesh_iface": "bat0"
        }"#;
        let s: RelayState = serde_json::from_str(fixture).expect("deserialize null receiver_ip");
        assert!(s.receiver_ip.is_none());
        assert!(!s.up);
        // Re-serializing keeps `receiver_ip: null`.
        let re = serde_json::to_value(&s).unwrap();
        assert!(re["receiver_ip"].is_null());
    }

    #[test]
    fn relay_state_write_to_writes_the_sidecar() {
        // The write seam persists the state to the given path. No env mutation:
        // the temp path is threaded in explicitly, so this test cannot race any
        // other test under the parallel runner.
        let dir = tempfile::tempdir().unwrap();
        let s = RelayState {
            drone_iface: "wlan1".into(),
            up: true,
            ..Default::default()
        };
        let path = dir.path().join("wfb-relay.json");
        s.write_to(&path).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&written).unwrap();
        assert_eq!(v["drone_iface"], "wlan1");
        assert_eq!(v["up"], true);
    }

    #[tokio::test]
    async fn write_and_emit_enqueues_one_event_with_an_emitter_and_none_without() {
        // The emitting write ships exactly one gs.relay_state event when an
        // emitter is supplied and nothing with None, regardless of whether the
        // file write succeeds (it is best-effort). The emitter records every
        // enqueue independent of a listening daemon. The on-disk file path is
        // covered by `relay_state_write_to_writes_the_sidecar`; this test threads
        // the temp path in explicitly (no `ADOS_RUN_DIR` mutation) so it never
        // races a sibling test under the parallel runner.
        let dir = tempfile::tempdir().unwrap();
        let s = RelayState {
            drone_iface: "wlan1".into(),
            receiver_ip: Some("10.42.0.5".into()),
            up: true,
            ..Default::default()
        };
        let path = dir.path().join("wfb-relay.json");

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
