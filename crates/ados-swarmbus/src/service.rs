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
use tokio::sync::watch;

use crate::bus::SwarmBus;
use crate::config::SwarmBusConfig;
use crate::crypto::{FleetKeyWatch, SwarmCipher};
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

/// Consecutive beacon injection failures after which the transmit loop gives up
/// and the supervisor reopens the radio. At the 500 ms beacon period this is
/// about five seconds: long enough to ride out a momentarily full driver queue,
/// short enough that a wedged TX path is noticed before neighbours drop this node
/// for long.
const TX_FAILURE_LIMIT: u32 = 10;

/// How often the slot-to-device-id join and the fleet key file are re-read.
/// Pairing and binding are human-scale events, so this is deliberately far slower
/// than the publish rate.
const REGISTRY_REFRESH: Duration = Duration::from_secs(10);

/// The latest vehicle-state snapshot, with the instant it was RECEIVED.
///
/// The timestamp is what makes the snapshot's *arrival* age measurable. It is
/// NOT the age of the position in it: the MAVLink router publishes on an
/// unconditional cadence off `state.sock`, so `received` keeps advancing on a
/// vehicle whose GPS has stopped while its heartbeat continues. The router
/// therefore also publishes `position_age_ms` — how long since it last decoded a
/// POSITION — and both ages have to clear the window before this node's body is
/// broadcast.
#[derive(Debug, Clone)]
struct StateSnapshot {
    value: Value,
    received: Instant,
}

/// Shared latest vehicle-state snapshot.
type SharedState = Arc<Mutex<Option<StateSnapshot>>>;

/// Age of the FC's position fix as the producer reported it, or `None` when the
/// snapshot does not state one.
///
/// Absence is NOT freshness. A snapshot with no `position_age_ms` is a producer
/// that has never decoded a POSITION (the field is `null` until the first one),
/// and the caller must treat that as an untrusted fix rather than as "no reason
/// to worry" — the whole hazard here is a plausible-looking frozen fix.
fn fix_age(snapshot: &StateSnapshot) -> Option<std::time::Duration> {
    snapshot
        .value
        .get("position_age_ms")
        .and_then(Value::as_u64)
        .map(std::time::Duration::from_millis)
}

/// The snapshot this node may broadcast as its own state, paired with whether
/// its position fix may be radiated, or `None` when there is no snapshot fresh
/// enough to stand for it at all. Pure, so both gates are testable without a
/// socket or a radio.
///
/// Two independent ages clear [`OWN_STATE_STALE`], because they fail
/// independently and mean different things:
///
/// - **Arrival age** (`now - received`) catches the MAVLink router dying. Then
///   nothing in the snapshot is current, so there is no body at all and the
///   beacon goes out carrying only the slot.
/// - **Fix age** (`position_age_ms`) catches the far more likely partial stall:
///   the FC keeps heartbeating, the router keeps publishing at its unconditional
///   cadence, and only the position inside the snapshot stops moving. Every
///   neighbour dead-reckons a beacon's position and velocity FORWARD, so a frozen
///   fix must not be radiated — but the armed, mode, hero, emergency and
///   precedence readings in the same snapshot are still live and still go out.
fn beacon_body(snapshot: Option<&StateSnapshot>, now: Instant) -> Option<(&Value, bool)> {
    let snapshot = snapshot?;
    if now.saturating_duration_since(snapshot.received) >= OWN_STATE_STALE {
        return None;
    }
    // Fail closed on an unstated age: a producer that cannot say how old its fix
    // is has not earned the fleet's trust in that fix.
    let fix_trusted = fix_age(snapshot).is_some_and(|age| age < OWN_STATE_STALE);
    Some((&snapshot.value, fix_trusted))
}

/// This node's beacon for the snapshot held at `now`: [`beacon_body`]'s two
/// gates applied through [`beacon_from_state`].
fn own_beacon(
    snapshot: Option<&StateSnapshot>,
    now: Instant,
    slot: u8,
    seq_ms: u16,
) -> crate::beacon::SwarmBeacon {
    match beacon_body(snapshot, now) {
        Some((value, fix_trusted)) => beacon_from_state(Some(value), fix_trusted, slot, seq_ms),
        None => beacon_from_state(None, false, slot, seq_ms),
    }
}

/// Resolves once shutdown has been requested, immediately if it already was.
///
/// Shutdown is a latched `watch` flag rather than a `Notify`: a notification sent
/// before a loop reaches its await point would be lost, and a stop that lands
/// during startup would then hang the service until systemd kills it. A dropped
/// sender also counts as shutdown, since nothing could ever request it again.
async fn cancelled(cancel: &watch::Receiver<bool>) {
    let mut rx = cancel.clone();
    let _ = rx.wait_for(|stop| *stop).await;
}

/// Run the service until `cancel` is set to `true`.
pub async fn run(cfg: SwarmBusConfig, cancel: watch::Receiver<bool>) {
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
    let (state, state_reader) = spawn_state_reader(cfg.state_socket_path(), cancel.clone());

    let publish = tokio::spawn(publish_loop(
        cfg.clone(),
        table.clone(),
        publisher,
        cancel.clone(),
    ));

    // Radio-bound work, restarted whenever the radio goes away (an adapter reset, a
    // monitor-mode flap) or the fleet key changes. The table survives across
    // restarts: a neighbour heard before the flap is still there, and ages out on
    // its own if it is not.
    let radio = tokio::spawn(radio_supervisor(
        cfg.clone(),
        table.clone(),
        state,
        cancel.clone(),
    ));

    cancelled(&cancel).await;
    publish.abort();
    radio.abort();
    state_reader.abort();
    let _ = std::fs::remove_file(&swarm_sock);
    tracing::info!("ados-swarmbus stopped");
}

/// Why the radio-bound loops were torn down.
enum Reopen {
    Stop,
    RadioLost,
    TxFailed,
    Rekeyed,
}

/// Open the radio, run the transmit and receive loops on it, and reopen it if it
/// fails or the fleet key changes.
///
/// The key file is re-read every [`REGISTRY_REFRESH`]. A bind or a pair rewrites
/// it under a running service, and a bus left on the old key would count every
/// re-keyed peer as a bad tag while they counted it the same way. On a change the
/// cipher is rebuilt with [`SwarmCipher::rekeyed`], which keeps this node's nonce
/// prefix and counter, and the bus is reopened on it.
async fn radio_supervisor(
    cfg: SwarmBusConfig,
    table: Arc<Mutex<NeighborTable>>,
    state: SharedState,
    cancel: watch::Receiver<bool>,
) {
    let mut keys = FleetKeyWatch::new(ados_radio::paths::DRONE_KEY);
    let mut cipher = Arc::new(SwarmCipher::new(keys.key()));
    // The prefix survives every re-key below, so recording it once is enough.
    table.lock().set_own_sender(cipher.sender_prefix());
    loop {
        // A key that changed while the radio was down is picked up before reopening.
        if let Some(key) = keys.poll() {
            cipher = Arc::new(cipher.rekeyed(&key));
        }
        let bus = match open_bus(&cfg, &cipher, &cancel).await {
            Some(b) => Arc::new(b),
            None => return,
        };
        tracing::info!(
            iface = bus.iface(),
            fleet_id = bus.fleet_id(),
            slot = bus.slot(),
            "swarm bus open"
        );
        table.lock().set_radio_iface(Some(bus.iface().to_string()));

        let mut rx = tokio::spawn(recv_loop(bus.clone(), table.clone()));
        // A ground station receives only. Slot 0 is not an aircraft, so it has no
        // position to broadcast and must never appear in a neighbour table.
        let mut tx = (!cfg.is_ground_station()).then(|| {
            tokio::spawn(transmit_loop(
                bus.clone(),
                cfg.fleet_slot,
                table.clone(),
                state.clone(),
            ))
        });

        let outcome = loop {
            tokio::select! {
                _ = cancelled(&cancel) => break Reopen::Stop,
                // The receive loop only returns on a socket error, which means the
                // adapter went away. Drop both loops and reopen.
                _ = &mut rx => break Reopen::RadioLost,
                // The transmit loop returns only after a run of injection
                // failures: the receive socket may still be bound, but this node
                // has stopped radiating, so reopen the whole bus.
                _ = transmit_ended(&mut tx) => {
                    tx = None;
                    break Reopen::TxFailed;
                }
                _ = tokio::time::sleep(REGISTRY_REFRESH) => {
                    if let Some(key) = keys.poll() {
                        cipher = Arc::new(cipher.rekeyed(&key));
                        break Reopen::Rekeyed;
                    }
                }
            }
        };
        table.lock().set_radio_iface(None);
        rx.abort();
        if let Some(tx) = tx {
            // Wait the transmitter out, so the old cipher seals nothing after the
            // rebuilt one took over its counter.
            tx.abort();
            let _ = tx.await;
        }
        match outcome {
            Reopen::Stop => return,
            Reopen::Rekeyed => tracing::info!("swarm fleet key changed; reopening the bus"),
            Reopen::RadioLost | Reopen::TxFailed => {
                if matches!(outcome, Reopen::RadioLost) {
                    tracing::warn!("swarm radio receive ended; reopening");
                } else {
                    tracing::warn!("swarm beacon injection keeps failing; reopening");
                }
                tokio::select! {
                    _ = cancelled(&cancel) => return,
                    _ = tokio::time::sleep(RADIO_RETRY) => {}
                }
            }
        }
    }
}

/// Resolves when the transmit task ends; never, on a node that runs none.
async fn transmit_ended(tx: &mut Option<tokio::task::JoinHandle<()>>) {
    match tx {
        Some(handle) => {
            let _ = handle.await;
        }
        None => std::future::pending().await,
    }
}

/// Counts consecutive beacon injection failures. One success clears the run.
#[derive(Debug, Default)]
struct TxFailures {
    consecutive: u32,
}

impl TxFailures {
    /// Record one send outcome; `true` once the run of failures reaches
    /// [`TX_FAILURE_LIMIT`] and the transmitter should give the radio up.
    fn record(&mut self, sent: bool) -> bool {
        if sent {
            self.consecutive = 0;
            return false;
        }
        self.consecutive += 1;
        self.consecutive >= TX_FAILURE_LIMIT
    }
}

/// Resolve an interface and open the bus, retrying until it works or `cancel`
/// fires.
async fn open_bus(
    cfg: &SwarmBusConfig,
    cipher: &Arc<SwarmCipher>,
    cancel: &watch::Receiver<bool>,
) -> Option<SwarmBus> {
    loop {
        match resolve_interface(cfg) {
            Some(iface) => {
                match SwarmBus::open(&iface, cfg.fleet_id, cfg.fleet_slot, cipher.clone()) {
                    Ok(bus) => return Some(bus),
                    Err(e) => tracing::warn!(
                        %iface, error = %e,
                        "swarm_radio_open_failed: is the adapter in monitor mode?"
                    ),
                }
            }
            None => tracing::debug!("swarm_radio_interface_unknown: waiting for the radio manager"),
        }
        tokio::select! {
            _ = cancelled(cancel) => return None,
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

/// Broadcast this node's beacon at the jittered beacon rate. Returns after
/// [`TX_FAILURE_LIMIT`] consecutive injection failures so the supervisor reopens
/// the radio; an isolated failure (a full driver queue) is only a dropped beacon.
async fn transmit_loop(
    bus: Arc<SwarmBus>,
    slot: u8,
    table: Arc<Mutex<NeighborTable>>,
    state: SharedState,
) {
    let started = Instant::now();
    let mut failures = TxFailures::default();
    loop {
        // Fresh jitter every transmission, not once at startup: a fleet powered up
        // together must not stay in lockstep.
        tokio::time::sleep(beacon_delay(random_word())).await;
        // A snapshot whose ARRIVAL is older than the window is not this node's
        // state at all: the beacon carries only the slot. A live snapshot whose FC
        // POSITION FIX is older than the window keeps its armed, mode, hero,
        // emergency and precedence readings but radiates no position, velocity or
        // GPS_OK, because every receiving drone dead-reckons those FORWARD from
        // the moment the beacon arrives: a frozen fix would read across the fleet
        // as a node still flying on a track it left behind.
        let held = state.lock().clone();
        let now = Instant::now();
        if let Some(snapshot) = held.as_ref() {
            if !beacon_body(Some(snapshot), now).is_some_and(|(_, fix_trusted)| fix_trusted) {
                // Which age tripped, because they mean different faults: a
                // stalled router versus a live router publishing a dead fix.
                tracing::debug!(
                    arrival_age_ms =
                        now.saturating_duration_since(snapshot.received).as_millis() as u64,
                    fix_age_ms = fix_age(snapshot).map(|d| d.as_millis() as u64),
                    "swarm_beacon_own_state_stale"
                );
            }
        }
        // Sender uptime, truncated to 16 bits. It wraps every 65.5 s, which is far
        // longer than the staleness window it feeds.
        let seq_ms = started.elapsed().as_millis() as u16;
        let beacon = own_beacon(held.as_ref(), now, slot, seq_ms);
        match bus.broadcast(&beacon).await {
            Ok(()) => {
                table.lock().record_tx();
                failures.record(true);
            }
            Err(e) => {
                tracing::debug!(error = %e, "swarm_beacon_tx_failed");
                if failures.record(false) {
                    tracing::warn!(
                        error = %e,
                        failures = TX_FAILURE_LIMIT,
                        "swarm_beacon_tx_failing"
                    );
                    return;
                }
            }
        }
    }
}

/// Prune and publish the table at the beacon rate.
async fn publish_loop(
    cfg: SwarmBusConfig,
    table: Arc<Mutex<NeighborTable>>,
    publisher: Arc<IpcBroadcast>,
    cancel: watch::Receiver<bool>,
) {
    let mut device_ids = load_device_ids(Path::new(FLEET_REGISTRY_PATH));
    let mut last_registry_read = Instant::now();
    loop {
        tokio::select! {
            _ = cancelled(&cancel) => return,
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
        publisher.broadcast(encode_line(&payload).into()).await;
    }
}

/// Read the vehicle-state socket into a shared cell, reconnecting forever.
///
/// The MAVLink router owns vehicle state; this is a read-only subscriber to the same
/// `state.sock` every other consumer uses, so the beacon reports the same numbers the
/// telemetry surfaces do. An absent socket leaves the cell empty and the beacon goes
/// out with no position and no condition bits, which reads correctly as "on the bus,
/// no fix".
fn spawn_state_reader(
    socket_path: String,
    cancel: watch::Receiver<bool>,
) -> (SharedState, tokio::task::JoinHandle<()>) {
    let shared: SharedState = Arc::new(Mutex::new(None));
    let writer = shared.clone();
    let task = tokio::spawn(async move {
        loop {
            let connect = connect_with_retry(&socket_path, 5, Duration::from_millis(300));
            let mut stream = tokio::select! {
                _ = cancelled(&cancel) => return,
                s = connect => match s {
                    Ok(s) => s,
                    Err(_) => {
                        tokio::select! {
                            _ = cancelled(&cancel) => return,
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
                    _ = cancelled(&cancel) => return,
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
                _ = cancelled(&cancel) => return,
                _ = tokio::time::sleep(Duration::from_millis(500)) => {}
            }
        }
    });
    (shared, task)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A run of injection failures ends the transmitter; a single success in the
    /// run clears it, so an occasional full queue never tears the radio down.
    #[test]
    fn only_a_consecutive_run_of_tx_failures_gives_the_radio_up() {
        let mut f = TxFailures::default();
        for _ in 0..TX_FAILURE_LIMIT - 1 {
            assert!(!f.record(false));
        }
        assert!(!f.record(true), "a success clears the run");
        for _ in 0..TX_FAILURE_LIMIT - 1 {
            assert!(!f.record(false));
        }
        assert!(f.record(false), "the limit-th consecutive failure gives up");
    }

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
    /// this node's state. Every receiving drone dead-reckons the beacon forward,
    /// so a frozen snapshot radiates as continued motion the aircraft is not
    /// performing.
    #[test]
    fn a_stale_snapshot_is_not_broadcast_as_this_nodes_state() {
        let held = StateSnapshot {
            value: serde_json::json!({
                "armed": true,
                "mode": "GUIDED",
                "position": {"lat": 12.34, "lon": 56.78, "alt_rel": 40.0},
                "velocity": {"vx": 8.0, "vy": 0.0, "vz": 0.0},
                // A live fix: the router decoded a POSITION a moment ago.
                "position_age_ms": 40,
            }),
            received: Instant::now(),
        };

        // Fresh: the full body goes out, which is the whole point of the bus.
        let fresh = own_beacon(Some(&held), held.received, 3, 0);
        assert_ne!(fresh.lat, 0);
        assert_ne!(fresh.vx_cms, 0);
        assert!(fresh.armed() && fresh.guided());

        // One window later the producer itself has gone quiet: nothing in the
        // snapshot is current. The node stays on the bus carrying only its slot.
        let stale = own_beacon(Some(&held), held.received + OWN_STATE_STALE, 3, 0);
        assert_eq!((stale.lat, stale.lon, stale.vx_cms), (0, 0, 0));
        assert_eq!(stale.status, 0, "no reading from a dead producer");
        assert_eq!(stale.slot, 3, "the node is still on the bus");
    }

    /// A node whose GPS has died but whose flight controller keeps heartbeating
    /// must not keep radiating its last fix to the formation — and must keep
    /// radiating everything else it knows.
    ///
    /// The arrival-age gate cannot see this case: the MAVLink router is alive and
    /// publishing on its unconditional cadence, so every snapshot looks fresh.
    /// Only the producer-reported fix age distinguishes it. A frozen fix is
    /// withheld, because every receiving drone dead-reckons the position and
    /// velocity in the beacon FORWARD. The armed, guided, hero, emergency and
    /// precedence readings are live, and zeroing them broadcast an armed drone in
    /// hard separation as a disarmed one on `hold`.
    #[test]
    fn a_frozen_fix_under_a_live_producer_withholds_only_the_position() {
        use crate::ModePrecedence;

        let flying = serde_json::json!({
            "armed": true,
            "mode": "GUIDED",
            "position": {"lat": 12.34, "lon": 56.78, "alt_rel": 40.0},
            "velocity": {"vx": 8.0, "vy": 0.0, "vz": 0.0},
            "gps": {"fix_type": 3},
            "video_profile": "hero",
            "swarm_emergency": true,
            "swarm_precedence": "hard-separation",
        });

        // The producer is alive: this snapshot arrived just now. The ONLY
        // difference between the two cases below is how old the FC's position
        // fix is.
        let with_fix_age = |ms: u64| StateSnapshot {
            value: {
                let mut v = flying.clone();
                v["position_age_ms"] = serde_json::json!(ms);
                v
            },
            received: Instant::now(),
        };
        let now = Instant::now();

        let good = own_beacon(Some(&with_fix_age(50)), now, 4, 0);
        assert_ne!(good.lat, 0, "a live fix is broadcast");
        assert_ne!(good.vx_cms, 0);
        assert!(good.gps_ok());
        assert!(good.armed() && good.guided() && good.hero() && good.emergency());

        let frozen = with_fix_age(OWN_STATE_STALE.as_millis() as u64 + 500);
        let suppressed = own_beacon(Some(&frozen), now, 4, 0);
        assert_eq!(suppressed.lat, 0, "a frozen fix must not be radiated");
        assert_eq!(suppressed.lon, 0);
        assert_eq!(suppressed.alt_dm, 0);
        assert_eq!(
            suppressed.vx_cms, 0,
            "a velocity the neighbours dead-reckon forward is the actual hazard"
        );
        assert!(
            !suppressed.gps_ok(),
            "GPS_OK on a dead fix is the bit that makes a neighbour trust it"
        );
        assert!(suppressed.armed(), "still armed");
        assert!(suppressed.guided(), "still in its mode");
        assert!(suppressed.hero(), "still the hero");
        assert!(suppressed.emergency(), "still in override");
        assert_eq!(suppressed.precedence(), ModePrecedence::HardSeparation);
        assert_eq!(suppressed.slot, 4, "the node is still on the bus");
    }

    /// A producer that does not state a fix age has not earned the fleet's
    /// trust in its position. Absence must fail CLOSED for the position: reading
    /// it as "no reason to worry" is exactly how a frozen fix gets radiated.
    #[test]
    fn a_snapshot_with_no_stated_fix_age_radiates_no_position() {
        let no_age = StateSnapshot {
            value: serde_json::json!({
                "armed": true,
                "position": {"lat": 12.34, "lon": 56.78},
                "gps": {"fix_type": 3},
            }),
            received: Instant::now(),
        };
        let b = own_beacon(Some(&no_age), Instant::now(), 2, 0);
        assert_eq!((b.lat, b.lon), (0, 0));
        assert!(!b.gps_ok());
        assert!(b.armed());

        // An explicit null (the producer has never decoded a POSITION) reads the
        // same way, and must never read as age zero.
        let never_fixed = StateSnapshot {
            value: serde_json::json!({"armed": true, "position_age_ms": null}),
            received: Instant::now(),
        };
        assert_eq!(
            beacon_body(Some(&never_fixed), Instant::now()).map(|(_, t)| t),
            Some(false)
        );
    }

    /// Both boundaries are exclusive on the near side, matching the control
    /// loop's own reading of the same window.
    #[test]
    fn the_freshness_window_is_the_shared_one() {
        let held = StateSnapshot {
            value: serde_json::json!({"armed": true, "position_age_ms": 0}),
            received: Instant::now(),
        };
        let just_inside = held.received + OWN_STATE_STALE - Duration::from_millis(1);
        assert_eq!(
            beacon_body(Some(&held), just_inside).map(|(_, t)| t),
            Some(true)
        );
        assert!(beacon_body(Some(&held), held.received + OWN_STATE_STALE).is_none());
        assert!(beacon_body(None, Instant::now()).is_none());

        let at_edge = |ms: u64| StateSnapshot {
            value: serde_json::json!({"position_age_ms": ms}),
            received: held.received,
        };
        let edge = OWN_STATE_STALE.as_millis() as u64;
        let trusted = |s: &StateSnapshot| beacon_body(Some(s), held.received).map(|(_, t)| t);
        assert_eq!(trusted(&at_edge(edge - 1)), Some(true));
        assert_eq!(trusted(&at_edge(edge)), Some(false));
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
        let (cancel, cancel_rx) = watch::channel(false);
        let (shared, _reader) = spawn_state_reader(sock.to_string_lossy().into_owned(), cancel_rx);

        server
            .broadcast(
                encode_v2(&serde_json::json!({"armed": true}))
                    .unwrap()
                    .into(),
            )
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
        let _ = cancel.send(true);
    }

    /// A stop requested before the service reached its own await point must still
    /// stop it. With a non-latched notification, a stop that lands during startup
    /// was lost and the service ran on until systemd killed it, leaving
    /// `swarm.sock` behind.
    #[tokio::test]
    async fn a_stop_requested_during_startup_still_stops_the_service() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = SwarmBusConfig {
            ground_station: true,
            interface: "nonexistent-swarm-iface0".to_string(),
            fleet_slot: ados_radio::config::SLOT_GROUND,
            socket_dir: dir.path().to_string_lossy().into_owned(),
            ..SwarmBusConfig::default()
        };
        let (cancel, cancel_rx) = watch::channel(false);
        // The stop lands before `run` has registered anything.
        cancel.send(true).unwrap();
        let finished =
            tokio::time::timeout(Duration::from_secs(5), run(cfg.clone(), cancel_rx)).await;
        assert!(finished.is_ok(), "the service must honour an early stop");
        assert!(
            !Path::new(&cfg.swarm_socket_path()).exists(),
            "shutdown removes the socket"
        );
    }
}
