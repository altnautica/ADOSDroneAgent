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

mod state_reader;

mod beacon_gate;
mod loops;
mod radio;
mod replay;

use beacon_gate::*;
use loops::*;
pub use radio::*;
use replay::*;

use parking_lot::Mutex;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ados_protocol::ipc::IpcBroadcast;
use serde_json::Value;
use tokio::sync::watch;

use crate::bus::SwarmBus;
use crate::config::SwarmBusConfig;
use crate::crypto::{FleetKeyWatch, SwarmCipher};
use crate::fleet_join::{load_device_ids, FLEET_REGISTRY_PATH};
use crate::ingest::Ingest;
use crate::neighbors::{NeighborTable, SenderMarks};
use crate::publish::{encode_line, neighbors_payload};
use crate::radio::{Transport, LOOPBACK_IFACE};
use crate::schedule::{beacon_delay, random_word, BEACON_PERIOD};
use crate::vehicle::beacon_from_state;

use state_reader::spawn_state_reader;

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

/// How often the replay window is written out when it changed. Frames accepted
/// inside the last tick before a crash are the only ones a restart forgets.
const REPLAY_PERSIST_PERIOD: Duration = Duration::from_secs(1);

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
    // The replay window from the previous run is restored before the radio half
    // starts, so the first frame this run hears is judged against it.
    let replay_path = std::path::PathBuf::from(&cfg.replay_state_path);
    let table = Arc::new(Mutex::new(NeighborTable::with_sender_marks(
        cfg.fleet_slot,
        SenderMarks::load(&replay_path),
    )));

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

    let persist = tokio::spawn(replay_persist_loop(
        replay_path.clone(),
        table.clone(),
        cancel.clone(),
    ));

    cancelled(&cancel).await;
    publish.abort();
    radio.abort();
    state_reader.abort();
    // Not aborted: it returns between writes, so no write is in flight when the
    // final one below runs.
    let _ = persist.await;
    // The last accepts of this run, so a clean restart forgets none of them.
    persist_replay(&replay_path, &table);
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
/// Counts consecutive beacon injection failures. One success clears the run.
#[derive(Debug, Default)]
struct TxFailures {
    consecutive: u32,
}

#[cfg(test)]
mod tests;
