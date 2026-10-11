//! Delta-counter liveness: proof of WORK, not proof of process.
//!
//! `systemctl is-active` answers "has the process exited?", which is the wrong
//! question for a daemon that is alive and doing nothing. A MAVLink router
//! whose serial reader has wedged, a swarm beacon whose 2 Hz publish loop has
//! stopped, a CRSF lane holding the last stick frame, a vision engine that has
//! stopped consuming the camera ring — each keeps its unit `active` forever
//! while the lane it owns is dead. The supervisor's only liveness judgement
//! used to be `is-active`, so every one of those was invisible.
//!
//! So a supervised unit also has to show it did its work. Each judged unit
//! publishes a cumulative count of the work its lane exists to do, the
//! supervisor samples it once per monitor pass, and a count that has not
//! advanced across the whole [`STALL_WINDOW`] while the unit reports active is
//! a stall, treated exactly like a death.
//!
//! The process's own I/O counters (`/proc/<pid>/io` `rchar`/`wchar`) cannot
//! stand in for that. They follow only the `read(2)`/`write(2)` file path, so
//! the socket `send`/`recv` traffic that carries most of these lanes never
//! reaches them, and the timer-driven writes that do (a status sidecar, a state
//! publish) keep them moving on a lane that has stopped. So each unit is judged
//! on its own counter, and only while its lane has work to do:
//!
//! * `ados-mavlink`: frames decoded off the FC link (`fc_frames_decoded` on
//!   its state snapshot), while that link's transport is open
//!   ([`router_work_counter`]).
//! * `ados-swarmbus`: beacons sent plus beacons accepted (the `counters` of
//!   the table it publishes on `swarm.sock`), while its radio is open and it
//!   either transmits (a drone slot) or hears a neighbour
//!   ([`swarm_work_counter`]).
//! * `ados-crsf`: RC frames written to the module (`tx_frames_total` in
//!   `crsf-stats.json`), while a channel source is live. With no live source
//!   the lane sends nothing so the receiver's failsafe runs; that is idle, not
//!   wedged ([`crsf_work_counter`]).
//! * `ados-vision`: frames consumed from its inputs (`vision-status.json`),
//!   while one of them is delivering ([`read_vision_work_counter`]).
//!
//! Three rules keep this from becoming a false-positive generator, which on a
//! flight node would be worse than the defect it fixes:
//!
//! 1. **No reading, no verdict.** A counter that cannot be read at all (an
//!    absent socket or sidecar, a lane with nothing to do) yields
//!    [`WorkVerdict::Unknown`], never a stall.
//! 2. **A gap is not flatness.** A missing sample clears the baseline instead
//!    of carrying the old timestamp forward, so a transient read failure cannot
//!    age into a stall verdict.
//! 3. **Only chosen units.** [`WORK_PROVEN_UNITS`] lists the four lanes whose
//!    silence is a real loss of capability. Units that are legitimately idle
//!    for long stretches are not judged this way.
//!
//! The sidecars are read without an age gate on purpose: each is rewritten by
//! its unit about once a second, so a file that stopped changing belongs to a
//! process that stopped running its own timers, and its frozen count with a
//! live lane reads as flat, which is a stall.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ados_protocol::paths::run_dir;
use serde_json::Value;

// `tokio::time::Instant`, not `std::time::Instant`: identical in production,
// but a paused-clock test can drive the stall window deterministically instead
// of sleeping through 30 s of real time. Same reason `sdnotify` uses it.
use tokio::time::Instant;

/// How long a unit's work counter may stay flat before it is judged stalled.
///
/// Sized against what each supervised lane does when healthy, so the quietest
/// of them still clears it comfortably: the MAVLink router decodes the FC's
/// ≥1 Hz HEARTBEAT, the swarm bus beacons at 2 Hz, the CRSF lane writes RC
/// frames at its packet rate while a source is live, and the vision engine
/// drops an input that has delivered nothing for 10 s. 30 s is six monitor
/// passes at the 5 s tick — long enough that a scheduling hiccup or one slow
/// pass cannot manufacture a stall.
pub const STALL_WINDOW: Duration = Duration::from_secs(30);

/// The units whose `active` state is not accepted as proof of work.
///
/// Each owns a lane whose silent death is invisible in `systemctl` and costly
/// in the air:
/// * `ados-mavlink` — the only command-and-control path to the flight
///   controller.
/// * `ados-swarmbus` — the 2 Hz state beacon onboard separation reads.
/// * `ados-crsf` — the RC control lane.
/// * `ados-vision` — the detection stream the follow / designate plugins and
///   world-model capture consume.
///
/// `ados-video`, `ados-wfb`, `ados-wfb-rx` and `ados-logd` are deliberately
/// absent: each already asserts its own byte/packet deltas one layer down in
/// its own daemon, and a second, coarser judgement here would only add a way
/// to restart a unit its own watchdog knows is healthy.
pub const WORK_PROVEN_UNITS: &[&str] = &[ROUTER_UNIT, SWARMBUS_UNIT, CRSF_UNIT, VISION_UNIT];

/// True when `unit` must prove work rather than merely being active.
pub fn requires_work_proof(unit: &str) -> bool {
    WORK_PROVEN_UNITS.contains(&unit)
}

/// The MAVLink router's unit.
pub const ROUTER_UNIT: &str = "ados-mavlink";

/// The swarm bus's unit.
pub const SWARMBUS_UNIT: &str = "ados-swarmbus";

/// The CRSF RC lane's unit.
pub const CRSF_UNIT: &str = "ados-crsf";

/// How long one read of a unit's socket (the router's state socket, the swarm
/// bus's table socket) may take. Both replay their latest publish on connect,
/// so a healthy unit answers at once.
const SOCKET_READ_TIMEOUT: Duration = Duration::from_secs(1);

/// The router's vehicle-state socket, honouring the `ADOS_RUN_DIR` override
/// the router binds under.
pub fn router_state_sock() -> PathBuf {
    run_dir().join("state.sock")
}

/// The vision engine's unit.
pub const VISION_UNIT: &str = "ados-vision";

/// The vision engine's work sidecar under the run directory.
pub fn vision_status_path() -> PathBuf {
    run_dir().join(ados_protocol::vision_status::VISION_STATUS_FILE)
}

/// The vision engine's work counter: the frames it consumed, while one of its
/// inputs is delivering. `None` (no verdict) when no input is delivering or
/// the sidecar is absent or unreadable.
///
/// The sidecar is not age-gated on purpose. The engine rewrites it every
/// second, so a file that stopped changing belongs to an engine that stopped
/// running its own timers; its frozen count reads as flat, which is a stall.
pub async fn read_vision_work_counter(path: &Path) -> Option<u64> {
    let text = tokio::fs::read_to_string(path).await.ok()?;
    serde_json::from_str::<ados_protocol::vision_status::VisionStatus>(&text)
        .ok()?
        .work_counter()
}

/// The router's work counter from one state snapshot: the cumulative frames
/// decoded off the FC link.
///
/// `None` (no verdict) unless the FC transport is open and the FC is one that
/// streams on its own. A closed transport means the router is between
/// reconnect attempts, which its own fixed-interval loop owns; an MSP board
/// (`fc_variant` set) is silent until a ground station polls it, so a flat
/// counter there is an idle link, not a wedged reader.
pub fn router_work_counter(snapshot: &Value) -> Option<u64> {
    if snapshot.get("transport_open").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    if snapshot.get("fc_variant").is_some_and(|v| !v.is_null()) {
        return None;
    }
    snapshot.get("fc_frames_decoded").and_then(Value::as_u64)
}

/// Read one snapshot from the router's state socket and lift its work counter
/// (see [`router_work_counter`]). `None` when the socket is absent, silent or
/// unreadable: no reading, no verdict.
pub async fn read_router_work_counter(sock: &Path) -> Option<u64> {
    let read = async {
        let mut stream = tokio::net::UnixStream::connect(sock).await.ok()?;
        ados_protocol::state::read_state_value(&mut stream)
            .await
            .ok()
            .flatten()
    };
    let snapshot = tokio::time::timeout(SOCKET_READ_TIMEOUT, read)
        .await
        .ok()
        .flatten()?;
    router_work_counter(&snapshot)
}

/// The swarm bus's table socket under the run directory.
pub fn swarm_sock() -> PathBuf {
    run_dir().join("swarm.sock")
}

/// The swarm bus's work counter from one published table: beacons sent plus
/// beacons accepted.
///
/// `None` (no verdict) unless the radio is open and the node has beacons to
/// move: a drone (any slot but the ground station's 0) transmits at 2 Hz
/// whenever its radio is open, while a ground station only receives, so it has
/// work only while a neighbour is being heard. A bus with no radio is the
/// radio supervisor's to reopen, not a stall.
pub fn swarm_work_counter(table: &Value) -> Option<u64> {
    if table.pointer("/radio/open").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let counters = table.get("counters")?;
    let tx = counters.get("beacons_tx")?.as_u64()?;
    let rx = counters.get("beacons_rx")?.as_u64()?;
    let transmits = table
        .get("slot")
        .and_then(Value::as_u64)
        .is_some_and(|slot| slot != 0);
    let hears = counters
        .get("neighbors_now")
        .and_then(Value::as_u64)
        .is_some_and(|n| n > 0);
    (transmits || hears).then_some(tx.saturating_add(rx))
}

/// Read one table from the swarm bus's socket (it replays the latest on
/// connect) and lift its work counter (see [`swarm_work_counter`]). `None`
/// when the socket is absent, silent or unreadable.
pub async fn read_swarm_work_counter(sock: &Path) -> Option<u64> {
    use tokio::io::AsyncBufReadExt;
    let read = async {
        let stream = tokio::net::UnixStream::connect(sock).await.ok()?;
        let mut line = String::new();
        tokio::io::BufReader::new(stream)
            .read_line(&mut line)
            .await
            .ok()?;
        serde_json::from_str::<Value>(&line).ok()
    };
    let table = tokio::time::timeout(SOCKET_READ_TIMEOUT, read)
        .await
        .ok()
        .flatten()?;
    swarm_work_counter(&table)
}

/// The CRSF lane's stats sidecar under the run directory.
pub fn crsf_stats_path() -> PathBuf {
    run_dir().join("crsf-stats.json")
}

/// The CRSF lane's work counter from its stats sidecar: RC frames written to
/// the module, while a channel source is live (`channel_source` names one).
/// `None` (no verdict) otherwise: with no live source the lane deliberately
/// sends nothing, and a lane standing by in another mode carries no count.
pub fn crsf_work_counter(stats: &Value) -> Option<u64> {
    if !stats.get("channel_source").is_some_and(Value::is_string) {
        return None;
    }
    stats.get("tx_frames_total").and_then(Value::as_u64)
}

/// Read the CRSF lane's stats sidecar and lift its work counter (see
/// [`crsf_work_counter`]). `None` when the sidecar is absent or unreadable.
pub async fn read_crsf_work_counter(path: &Path) -> Option<u64> {
    let text = tokio::fs::read_to_string(path).await.ok()?;
    crsf_work_counter(&serde_json::from_str::<Value>(&text).ok()?)
}

/// The current work counter of a work-proven `unit`, read from wherever that
/// unit publishes it. `None` for any other unit, and whenever the unit's lane
/// has nothing to do.
pub async fn read_work_counter(unit: &str) -> Option<u64> {
    match unit {
        ROUTER_UNIT => read_router_work_counter(&router_state_sock()).await,
        SWARMBUS_UNIT => read_swarm_work_counter(&swarm_sock()).await,
        CRSF_UNIT => read_crsf_work_counter(&crsf_stats_path()).await,
        VISION_UNIT => read_vision_work_counter(&vision_status_path()).await,
        _ => None,
    }
}

/// The outcome of folding one counter reading into a unit's history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkVerdict {
    /// The counter moved, or it is flat but the window has not elapsed yet.
    Progressing,
    /// Flat for at least [`STALL_WINDOW`]. The unit is alive and idle on a lane
    /// that is never idle when healthy.
    Stalled { flat_for: Duration },
    /// No counter reading. Never acted on.
    Unknown,
}

#[derive(Debug, Clone, Copy)]
struct Sample {
    counter: u64,
    flat_since: Instant,
}

/// Per-unit work-counter history across monitor passes.
#[derive(Debug, Default)]
pub struct WorkProof {
    last: HashMap<String, Sample>,
}

impl WorkProof {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one counter reading for `unit` into its history and judge it.
    pub fn observe(&mut self, unit: &str, reading: Option<u64>, now: Instant) -> WorkVerdict {
        let Some(counter) = reading else {
            // A gap in the samples is not elapsed flatness.
            self.last.remove(unit);
            return WorkVerdict::Unknown;
        };
        match self.last.get_mut(unit) {
            Some(prev) if prev.counter == counter => {
                let flat_for = now.duration_since(prev.flat_since);
                if flat_for >= STALL_WINDOW {
                    WorkVerdict::Stalled { flat_for }
                } else {
                    WorkVerdict::Progressing
                }
            }
            Some(prev) => {
                // A counter that went BACKWARDS means the process was replaced
                // under the same unit name. Rebaseline rather than reading the
                // new, smaller value as a jump.
                prev.counter = counter;
                prev.flat_since = now;
                WorkVerdict::Progressing
            }
            None => {
                self.last.insert(
                    unit.to_string(),
                    Sample {
                        counter,
                        flat_since: now,
                    },
                );
                WorkVerdict::Progressing
            }
        }
    }

    /// Drop a unit's baseline. Called whenever the unit is restarted or goes
    /// inactive: the replacement process starts its counters at zero, and the
    /// stale baseline would otherwise be compared against them.
    pub fn forget(&mut self, unit: &str) {
        self.last.remove(unit);
    }
}

#[cfg(test)]
mod tests;
