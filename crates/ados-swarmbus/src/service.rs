//! The daemon: three loops over one radio.
//!
//! - **Receive**, always. Every authenticated beacon folds into the shared table.
//!   This is the loop that makes the bus decentralized: it runs identically on a
//!   drone and on a ground station, and neither depends on the other.
//! - **Transmit**, drones only. A beacon filled from the flight controller's state
//!   every 500 ms plus jitter. A ground station is not an aircraft: it has no
//!   position to report and slot 0 never appears in anyone's neighbour table.
//! - **Publish**, always. Prune the stale entries, then broadcast the table on
//!   `swarm.sock` at the beacon rate for `ados-control` to serve.
//!
//! The radio is opened in a retry loop rather than as a startup precondition,
//! because the interface legitimately does not exist yet: the radio manager selects
//! and monitor-modes an adapter on its own schedule, and a swarm bus that gave up on
//! first failure would need a manual restart after every cold boot.

use parking_lot::Mutex;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ados_protocol::ipc::{connect_with_retry, IpcBroadcast};
use ados_protocol::state::{read_state_value, STATE_V2_MAX_FRAME};
use serde_json::Value;
use tokio::sync::Notify;

use crate::bus::SwarmBus;
use crate::config::SwarmBusConfig;
use crate::fleet_join::{load_device_ids, FLEET_REGISTRY_PATH};
use crate::ingest::Ingest;
use crate::neighbors::NeighborTable;
use crate::publish::{encode_line, neighbors_payload};
use crate::schedule::{beacon_delay, random_word, BEACON_PERIOD};
use crate::vehicle::beacon_from_state;

/// Per-client queue depth on the swarm socket. Small on purpose: a consumer that
/// falls a second behind on position data wants the newest frame, not a backlog.
const SWARM_QUEUE_DEPTH: usize = 8;

/// How long to wait before retrying a radio open.
const RADIO_RETRY: Duration = Duration::from_secs(5);

/// How often the slot-to-device-id join is re-read. Pairing is a human-scale event,
/// so this is deliberately far slower than the publish rate.
const REGISTRY_REFRESH: Duration = Duration::from_secs(10);

/// Shared latest vehicle-state snapshot.
type SharedState = Arc<Mutex<Option<Value>>>;

/// Run the service until `cancel` fires.
pub async fn run(cfg: SwarmBusConfig, cancel: Arc<Notify>) {
    let table = Arc::new(Mutex::new(NeighborTable::new(cfg.fleet_slot)));

    // The publish socket comes up first and unconditionally. A bus with no radio
    // still answers `GET /api/swarm/neighbors` with an empty table and zeroed
    // counters, which is the honest report and is distinguishable from an absent
    // service (which answers with a null fleet id).
    let swarm_sock = cfg.swarm_socket_path();
    let publisher = match IpcBroadcast::bind(&swarm_sock, SWARM_QUEUE_DEPTH, true, None).await {
        Ok((p, _)) => Arc::new(p),
        Err(e) => {
            tracing::error!(path = %swarm_sock, error = %e, "swarm_sock_bind_failed");
            return;
        }
    };

    // The own-beacon source. Started on a ground station too, harmlessly: it simply
    // never connects, and the transmit loop that would read it does not run.
    let state = spawn_state_reader(cfg.state_socket_path(), cancel.clone());

    let key = crate::crypto::resolve_fleet_key();
    let publish = tokio::spawn(publish_loop(
        cfg.clone(),
        table.clone(),
        publisher,
        cancel.clone(),
    ));

    // Radio-bound work, restarted whenever the radio goes away (an adapter reset, a
    // monitor-mode flap). The table survives across restarts: a neighbour heard
    // before the flap is still there, and ages out on its own if it is not.
    let radio = tokio::spawn(radio_supervisor(
        cfg.clone(),
        key,
        table.clone(),
        state,
        cancel.clone(),
    ));

    cancel.notified().await;
    publish.abort();
    radio.abort();
    let _ = std::fs::remove_file(&swarm_sock);
    tracing::info!("ados-swarmbus stopped");
}

/// Open the radio, run the transmit and receive loops on it, and reopen it if it
/// fails.
async fn radio_supervisor(
    cfg: SwarmBusConfig,
    key: [u8; 32],
    table: Arc<Mutex<NeighborTable>>,
    state: SharedState,
    cancel: Arc<Notify>,
) {
    loop {
        let bus = match open_bus(&cfg, &key, &cancel).await {
            Some(b) => Arc::new(b),
            None => return,
        };
        tracing::info!(
            iface = bus.iface(),
            fleet_id = bus.fleet_id(),
            slot = bus.slot(),
            "swarm bus open"
        );

        let mut rx = tokio::spawn(recv_loop(bus.clone(), table.clone()));
        // A ground station receives only. Slot 0 is not an aircraft, so it has no
        // position to broadcast and must never appear in a neighbour table.
        let tx = (!cfg.is_ground_station()).then(|| {
            tokio::spawn(transmit_loop(
                bus.clone(),
                cfg.fleet_slot,
                table.clone(),
                state.clone(),
            ))
        });

        let reopen = tokio::select! {
            _ = cancel.notified() => false,
            // The receive loop only returns on a socket error, which means the
            // adapter went away. Drop both loops and reopen.
            _ = &mut rx => true,
        };
        rx.abort();
        if let Some(tx) = tx {
            tx.abort();
        }
        if !reopen {
            return;
        }
        tracing::warn!("swarm radio receive ended; reopening");
        tokio::select! {
            _ = cancel.notified() => return,
            _ = tokio::time::sleep(RADIO_RETRY) => {}
        }
    }
}

/// Resolve an interface and open the bus, retrying until it works or `cancel`
/// fires.
async fn open_bus(cfg: &SwarmBusConfig, key: &[u8; 32], cancel: &Notify) -> Option<SwarmBus> {
    loop {
        match resolve_interface(cfg) {
            Some(iface) => match SwarmBus::open(&iface, cfg.fleet_id, cfg.fleet_slot, key) {
                Ok(bus) => return Some(bus),
                Err(e) => tracing::warn!(
                    %iface, error = %e,
                    "swarm_radio_open_failed: is the adapter in monitor mode?"
                ),
            },
            None => tracing::debug!("swarm_radio_interface_unknown: waiting for the radio manager"),
        }
        tokio::select! {
            _ = cancel.notified() => return None,
            _ = tokio::time::sleep(RADIO_RETRY) => {}
        }
    }
}

/// The monitor interface to inject on: the operator's config pin first, then the
/// live selection from the radio service's sidecar.
///
/// The sidecar is the authoritative source in practice — `video.wfb.interface` is
/// usually empty, and the adapter the radio manager actually selected is the only
/// one carrying this fleet's traffic. Injecting on a different interface would
/// produce a bus nobody hears, with no error anywhere.
pub fn resolve_interface(cfg: &SwarmBusConfig) -> Option<String> {
    if !cfg.interface.trim().is_empty() {
        return Some(cfg.interface.trim().to_string());
    }
    interface_from_sidecar(Path::new(ados_radio::paths::WFB_STATS_JSON))
}

/// Read the live interface out of the radio service's `wfb-stats.json`.
pub fn interface_from_sidecar(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    let iface = v.get("interface")?.as_str()?.trim();
    (!iface.is_empty()).then(|| iface.to_string())
}

/// Fold every captured frame into the table until the socket fails.
async fn recv_loop(bus: Arc<SwarmBus>, table: Arc<Mutex<NeighborTable>>) {
    loop {
        match bus.recv_into(&table).await {
            Ok(Ingest::Rejected(reason)) => {
                tracing::trace!(?reason, "swarm frame rejected");
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(error = %e, "swarm_radio_recv_failed");
                return;
            }
        }
    }
}

/// Broadcast this node's beacon at the jittered beacon rate.
async fn transmit_loop(
    bus: Arc<SwarmBus>,
    slot: u8,
    table: Arc<Mutex<NeighborTable>>,
    state: SharedState,
) {
    let started = Instant::now();
    loop {
        // Fresh jitter every transmission, not once at startup: a fleet powered up
        // together must not stay in lockstep.
        tokio::time::sleep(beacon_delay(random_word())).await;
        let snapshot = state.lock().clone();
        // Sender uptime, truncated to 16 bits. It wraps every 65.5 s, which is far
        // longer than the staleness window it feeds.
        let seq_ms = started.elapsed().as_millis() as u16;
        let beacon = beacon_from_state(snapshot.as_ref(), slot, seq_ms);
        match bus.broadcast(&beacon).await {
            Ok(()) => table.lock().record_tx(),
            // A full driver queue is a dropped beacon, not a fault: the next one is
            // 500 ms away and the receiver's dead reckoning covers the gap.
            Err(e) => tracing::debug!(error = %e, "swarm_beacon_tx_failed"),
        }
    }
}

/// Prune and publish the table at the beacon rate.
async fn publish_loop(
    cfg: SwarmBusConfig,
    table: Arc<Mutex<NeighborTable>>,
    publisher: Arc<IpcBroadcast>,
    cancel: Arc<Notify>,
) {
    let mut device_ids = load_device_ids(Path::new(FLEET_REGISTRY_PATH));
    let mut last_registry_read = Instant::now();
    loop {
        tokio::select! {
            _ = cancel.notified() => return,
            _ = tokio::time::sleep(BEACON_PERIOD) => {}
        }
        if last_registry_read.elapsed() >= REGISTRY_REFRESH {
            device_ids = load_device_ids(Path::new(FLEET_REGISTRY_PATH));
            last_registry_read = Instant::now();
        }
        let now = Instant::now();
        let payload = {
            let mut guard = table.lock();
            // Prune before publishing so a consumer never sees an entry the table
            // has already decided is dead.
            guard.prune(now);
            neighbors_payload(cfg.fleet_id, &guard, &device_ids, now)
        };
        publisher.broadcast(encode_line(&payload)).await;
    }
}

/// Read the vehicle-state socket into a shared cell, reconnecting forever.
///
/// The MAVLink router owns vehicle state; this is a read-only subscriber to the same
/// `state.sock` every other consumer uses, so the beacon reports the same numbers the
/// telemetry surfaces do. An absent socket leaves the cell empty and the beacon goes
/// out with no position and no condition bits, which reads correctly as "on the bus,
/// no fix".
fn spawn_state_reader(socket_path: String, cancel: Arc<Notify>) -> SharedState {
    let shared: SharedState = Arc::new(Mutex::new(None));
    let writer = shared.clone();
    tokio::spawn(async move {
        loop {
            let connect = connect_with_retry(&socket_path, 5, Duration::from_millis(300));
            let mut stream = tokio::select! {
                _ = cancel.notified() => return,
                s = connect => match s {
                    Ok(s) => s,
                    Err(_) => {
                        tokio::select! {
                            _ = cancel.notified() => return,
                            _ = tokio::time::sleep(Duration::from_millis(500)) => {}
                        }
                        continue;
                    }
                },
            };
            let mut reader =
                tokio::io::BufReader::with_capacity(STATE_V2_MAX_FRAME.min(64 * 1024), &mut stream);
            loop {
                let frame = tokio::select! {
                    _ = cancel.notified() => return,
                    f = read_state_value(&mut reader) => f,
                };
                match frame {
                    Ok(Some(v)) => {
                        *writer.lock() = Some(v);
                    }
                    Ok(None) | Err(_) => break,
                }
            }
            tokio::select! {
                _ = cancel.notified() => return,
                _ = tokio::time::sleep(Duration::from_millis(500)) => {}
            }
        }
    });
    shared
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The config pin wins when set, because an operator who names an interface
    /// means it.
    #[test]
    fn a_configured_interface_pin_takes_precedence() {
        let pinned = |iface: &str| SwarmBusConfig {
            interface: iface.to_string(),
            ..SwarmBusConfig::default()
        };
        assert_eq!(
            resolve_interface(&pinned("wlan9")).as_deref(),
            Some("wlan9")
        );
        // Whitespace is trimmed rather than passed to the kernel as a name.
        assert_eq!(
            resolve_interface(&pinned("  wlan9  ")).as_deref(),
            Some("wlan9")
        );
    }

    /// An empty pin must fall through to the sidecar, not resolve to `""` — an empty
    /// interface name would open a socket bound to nothing and produce a bus nobody
    /// hears, with no error anywhere.
    #[test]
    fn an_empty_pin_falls_through_rather_than_resolving_to_an_empty_name() {
        let cfg = SwarmBusConfig::default();
        assert_eq!(cfg.interface, "", "the shipped default is empty");
        // With no sidecar on this host the resolution is None, never Some("").
        assert_ne!(resolve_interface(&cfg).as_deref(), Some(""));
        let blank = SwarmBusConfig {
            interface: "   ".to_string(),
            ..SwarmBusConfig::default()
        };
        assert_ne!(resolve_interface(&blank).as_deref(), Some(""));
    }

    #[test]
    fn the_sidecar_supplies_the_live_interface_and_degrades_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wfb-stats.json");

        assert_eq!(interface_from_sidecar(&path), None, "absent file");

        std::fs::write(&path, r#"{"state":"linked","interface":"wlan1"}"#).unwrap();
        assert_eq!(interface_from_sidecar(&path).as_deref(), Some("wlan1"));

        // The radio manager writes an empty interface before it has selected an
        // adapter; that is "not yet", not a name.
        std::fs::write(&path, r#"{"state":"scanning","interface":""}"#).unwrap();
        assert_eq!(interface_from_sidecar(&path), None);

        for bad in [
            r#"{"state":"linked"}"#,
            r#"{"interface":null}"#,
            r#"{"interface":42}"#,
            "not json",
            "",
        ] {
            std::fs::write(&path, bad).unwrap();
            assert_eq!(interface_from_sidecar(&path), None, "{bad}");
        }
    }

    /// The publish cadence must match the beacon rate: publishing slower would make
    /// the operator's fleet view lag the aircraft, and faster would burn CPU
    /// re-serializing an unchanged table.
    #[test]
    fn the_publish_cadence_is_the_beacon_rate() {
        assert_eq!(BEACON_PERIOD, Duration::from_millis(500));
        assert!(
            REGISTRY_REFRESH > BEACON_PERIOD * 10,
            "pairing is a human-scale event; do not re-read the registry per publish"
        );
    }
}
