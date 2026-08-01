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
use crate::vehicle::{beacon_from_state, OWN_STATE_STALE};

/// Per-client queue depth on the swarm socket. Small on purpose: a consumer that
/// falls a second behind on position data wants the newest frame, not a backlog.
const SWARM_QUEUE_DEPTH: usize = 8;

/// How long to wait before retrying a radio open.
const RADIO_RETRY: Duration = Duration::from_secs(5);

/// How often the slot-to-device-id join is re-read. Pairing is a human-scale event,
/// so this is deliberately far slower than the publish rate.
const REGISTRY_REFRESH: Duration = Duration::from_secs(10);

/// The latest vehicle-state snapshot, with the instant it was RECEIVED.
///
/// The timestamp is what makes the snapshot's age measurable at all. Without it
/// the cell holds a value that looks identical whether the flight controller
/// published it a moment ago or stopped publishing a minute ago, and the beacon
/// loop reading that cell had no way to ask.
#[derive(Debug, Clone)]
struct StateSnapshot {
    value: Value,
    received: Instant,
}

/// Shared latest vehicle-state snapshot.
type SharedState = Arc<Mutex<Option<StateSnapshot>>>;

/// The snapshot body this node may broadcast as its own state, or `None` when
/// there is none fresh enough to stand for it. Pure, so the window is testable
/// without a socket or a radio.
fn beacon_body(snapshot: Option<&StateSnapshot>, now: Instant) -> Option<&Value> {
    snapshot
        .filter(|s| now.saturating_duration_since(s.received) < OWN_STATE_STALE)
        .map(|s| &s.value)
}

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
        // A snapshot older than the window is not broadcast as this node's
        // state. Every receiving drone dead-reckons the position and velocity in
        // a beacon FORWARD from the moment it arrives, so a frozen fix does not
        // read across the fleet as a node gone quiet — it reads as a node still
        // flying, on a track it left behind. Dropping the body (rather than the
        // beacon) keeps this node visible on the bus with no position and no
        // condition bits, which is exactly the reading a receiver needs: present,
        // not locatable.
        let held = state.lock().clone();
        let body = beacon_body(held.as_ref(), Instant::now());
        if held.is_some() && body.is_none() {
            tracing::debug!("swarm_beacon_own_state_stale");
        }
        // Sender uptime, truncated to 16 bits. It wraps every 65.5 s, which is far
        // longer than the staleness window it feeds.
        let seq_ms = started.elapsed().as_millis() as u16;
        let beacon = beacon_from_state(body, slot, seq_ms);
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
                    Ok(Some(value)) => {
                        *writer.lock() = Some(StateSnapshot {
                            value,
                            received: Instant::now(),
                        });
                    }
                    // The stream ended or failed. Clear the cell rather than
                    // leaving the last snapshot standing: the reconnect loop
                    // below may take seconds, and a held value with no producer
                    // behind it is the frozen-fix case arriving by a different
                    // route. The age gate would catch it, but the honest state
                    // while there is no producer is no state.
                    Ok(None) | Err(_) => {
                        *writer.lock() = None;
                        break;
                    }
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

    /// A snapshot whose producer has stopped must not keep being broadcast as
    /// this node's position. Every receiving drone dead-reckons the beacon
    /// forward, so a frozen fix radiates as continued motion the aircraft is not
    /// performing, with the armed and offboard bits still set.
    #[test]
    fn a_stale_snapshot_is_not_broadcast_as_this_nodes_state() {
        use crate::beacon::{STATUS_ARMED, STATUS_GUIDED};
        use crate::vehicle::beacon_from_state;

        let held = StateSnapshot {
            value: serde_json::json!({
                "armed": true,
                "mode": "GUIDED",
                "position": {"lat": 12.34, "lon": 56.78, "alt_rel": 40.0},
                "velocity": {"vx": 8.0, "vy": 0.0, "vz": 0.0},
            }),
            received: Instant::now(),
        };

        // Fresh: the full body goes out, which is the whole point of the bus.
        let fresh = beacon_from_state(beacon_body(Some(&held), held.received), 3, 0);
        assert_ne!(fresh.lat, 0);
        assert_ne!(fresh.vx_cms, 0);
        assert_eq!(fresh.status & STATUS_ARMED, STATUS_ARMED);
        assert_eq!(fresh.status & STATUS_GUIDED, STATUS_GUIDED);

        // One window later the producer has gone quiet. The node stays on the
        // bus, but carries no position, no velocity and no condition bits, so a
        // receiver reads it as present and not locatable rather than moving.
        let stale = beacon_from_state(
            beacon_body(Some(&held), held.received + OWN_STATE_STALE),
            3,
            0,
        );
        assert_eq!(stale.lat, 0);
        assert_eq!(stale.lon, 0);
        assert_eq!(stale.vx_cms, 0);
        assert_eq!(
            stale.status, 0,
            "a dead fix must not radiate armed/offboard"
        );
        assert_eq!(stale.slot, 3, "the node is still on the bus");
    }

    /// The boundary is exclusive on the near side, matching the control loop's
    /// own reading of the same window.
    #[test]
    fn the_freshness_window_is_the_shared_one() {
        let held = StateSnapshot {
            value: serde_json::json!({"armed": true}),
            received: Instant::now(),
        };
        let just_inside = held.received + OWN_STATE_STALE - Duration::from_millis(1);
        assert!(beacon_body(Some(&held), just_inside).is_some());
        assert!(beacon_body(Some(&held), held.received + OWN_STATE_STALE).is_none());
        assert!(beacon_body(None, Instant::now()).is_none());
    }

    /// The state reader used to break out of its read loop leaving the last
    /// snapshot in the cell, so a flight controller that stopped publishing left
    /// its final reading standing while the reader reconnected.
    #[tokio::test]
    async fn a_closed_state_stream_clears_the_held_snapshot() {
        use ados_protocol::ipc::IpcBroadcast;
        use ados_protocol::state::encode_v2;

        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("state.sock");
        let (server, _in) = IpcBroadcast::bind(&sock, 32, true, None).await.unwrap();
        let cancel = Arc::new(Notify::new());
        let shared = spawn_state_reader(sock.to_string_lossy().into_owned(), cancel.clone());

        server
            .broadcast(encode_v2(&serde_json::json!({"armed": true})).unwrap())
            .await;
        for _ in 0..100 {
            if shared.lock().is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(shared.lock().is_some(), "the snapshot must be held");

        // The producer goes away.
        drop(server);
        for _ in 0..100 {
            if shared.lock().is_none() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            shared.lock().is_none(),
            "a snapshot with no producer behind it must not stay held"
        );
        cancel.notify_waiters();
    }
}
