//! The 10 Hz onboard-autonomy loop: swarm neighbour table in,
//! `SET_POSITION_TARGET_GLOBAL_INT` out.
//!
//! Every control law lives in `ados-swarm` as a pure function; this file
//! is the plumbing that connects it to the two things it cannot own — the swarm
//! bus and the flight controller.
//!
//! # Why it lives in this crate
//!
//! The setpoint has to go out through [`super::FcConnection`]: that is the process
//! holding the FC link, the sequence counter and the writer. Putting the loop in
//! `ados-swarmbus` instead would mean a second command path to the autopilot, and
//! the whole point of this router is that there is exactly one.
//!
//! # Data in
//!
//! `/run/ados/swarm.sock` publishes the neighbour table as newline JSON at the
//! 2 Hz beacon rate. This loop runs at 10 Hz against the LAST payload,
//! dead-reckoning each neighbour forward by its age plus the time since that
//! payload arrived. That predict/correct split is exactly why a 2 Hz beacon can
//! drive a 10 Hz controller: the loop never sees a staircase, and the correction
//! arrives before the prediction has drifted.
//!
//! # Data out
//!
//! Two things, both consumed elsewhere:
//!
//! * The setpoint, straight to the FC, only while it reports GUIDED.
//! * The active precedence level and the emergency condition, published in this
//!   router's state snapshot as `swarm_precedence` / `swarm_emergency`, which
//!   `ados_swarmbus::vehicle::beacon_from_state` folds into the outgoing beacon.
//!   That is how a neighbour — and the operator screen — learns which layer is
//!   ACTUALLY flying this aircraft rather than which one it was told to.
//!
//! # What it never does
//!
//! No failsafe of its own, and none of ArduPilot's replaced. When the swarm is
//! disabled, the vehicle is disarmed, the FC is out of GUIDED, the neighbour
//! table has been empty for `NEIGHBOR_STALE`, or the autopilot has not yet
//! named its own system id in a HEARTBEAT, this loop emits NOTHING and the FC
//! holds on its own terms. Setpoints are addressed to that latched id, never
//! to a conventional 1/1: a fleet drone flies with its slot as its system id.
//!
//! # Human pilot in command
//!
//! The loop is an autonomous injector. Where the PIC arbiter runs and its
//! sidecar is present and fresh, every tick asks the same verdict the FC-write
//! injector gate uses (as injector `swarm`); while anyone else holds the claim
//! the loop emits nothing and publishes `swarm_precedence = "operator"`, so the
//! beacon shows a human is flying. Where no sidecar exists (a drone, which runs
//! no arbiter) the loop keeps the gates above alone.
//!
//! # Signed flight controllers
//!
//! This router does not sign. When the FC answers signed on a link where it
//! enforces signing, every setpoint would be dropped by the autopilot while the
//! counters said it was sent, so the layer is not eligible while
//! `agent_commands_unsigned_rejected_risk` is set (logged once per episode).
//!
//! # Config changes
//!
//! The `swarm:` block is re-read while the router runs. The loop checks the
//! config file's stamp every [`CONFIG_POLL`] and re-parses it only when the
//! stamp moved, so an operator who raises the separation envelope, changes the
//! formation or turns participation on or off gets the new behaviour within one
//! poll instead of at the next router restart. A changed block is applied to
//! the live controller through `SwarmController::apply_config`, which leaves an
//! engaged hard-separation latch alone.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use ados_protocol::ipc::{connect_with_retry, read_newline_line};
use ados_protocol::mavlink::{GuidedSetpoint, SetpointKind as WireSetpointKind};
use ados_protocol::shutdown::Shutdown;
use ados_swarm::{
    ControlOutcome, ModePrecedence, NeighborFix, OwnState, Setpoint, SwarmControlConfig,
    SwarmController, CONTROL_PERIOD,
};
use serde_json::Value;
use tokio::sync::{watch, Mutex};

use super::FcConnection;
use crate::state::VehicleState;

/// Largest swarm payload accepted. A 24-drone table is under 9 KB; this is a
/// generous bound that still refuses a runaway writer.
const MAX_SWARM_LINE: usize = 64 * 1024;

/// Reconnect backoff on the swarm socket. The swarm service legitimately starts
/// after this one, so a missing socket is normal rather than a fault.
const SWARM_RECONNECT: Duration = Duration::from_secs(2);

/// How often the config file is checked for a changed `swarm:` block. One
/// `stat` per period; the file is parsed only when its stamp moved.
const CONFIG_POLL: Duration = Duration::from_secs(2);

/// What the loop wants the state snapshot to say about it. Atomics, so the 10 Hz
/// snapshot publisher reads it without waiting on the control loop.
#[derive(Debug, Default)]
pub struct SwarmSetpointStatus {
    /// The active precedence level, as a `ModePrecedence` discriminant.
    precedence: AtomicU8,
    emergency: AtomicBool,
    setpoints_emitted: AtomicU64,
    ticks_suppressed: AtomicU64,
    hard_engagements: AtomicU64,
    /// Set while the layer is held off for the unsigned-command risk, so the
    /// hold is logged once per episode rather than every poll.
    signing_hold_logged: AtomicBool,
}

impl SwarmSetpointStatus {
    /// The `mode_precedence` wire string for the snapshot.
    pub fn precedence_wire(&self) -> &'static str {
        ModePrecedence::from_status_bits(
            self.precedence.load(Ordering::Relaxed) << ados_swarm::STATUS_PRECEDENCE_SHIFT,
        )
        .as_wire()
    }

    /// Whether the separation layer has taken the vehicle — beacon status bit 2.
    pub fn emergency(&self) -> bool {
        self.emergency.load(Ordering::Relaxed)
    }

    pub fn setpoints_emitted(&self) -> u64 {
        self.setpoints_emitted.load(Ordering::Relaxed)
    }

    pub fn ticks_suppressed(&self) -> u64 {
        self.ticks_suppressed.load(Ordering::Relaxed)
    }

    pub fn hard_engagements(&self) -> u64 {
        self.hard_engagements.load(Ordering::Relaxed)
    }

    fn publish(&self, level: ModePrecedence, emergency: bool, c: ados_swarm::SwarmControlCounters) {
        self.precedence.store(level as u8, Ordering::Relaxed);
        self.emergency.store(emergency, Ordering::Relaxed);
        self.setpoints_emitted
            .store(c.setpoints_emitted, Ordering::Relaxed);
        self.ticks_suppressed
            .store(c.ticks_suppressed, Ordering::Relaxed);
        self.hard_engagements
            .store(c.hard_engagements, Ordering::Relaxed);
    }

    /// The layer stood down: nothing is flying the aircraft from here, so the
    /// beacon must not keep advertising the last level or a stale emergency.
    fn stand_down(&self) {
        self.precedence
            .store(ModePrecedence::Hold as u8, Ordering::Relaxed);
        self.emergency.store(false, Ordering::Relaxed);
    }

    /// A human holds the PIC claim: nothing is flown from here, and the beacon
    /// says the operator is flying.
    fn stand_down_for_operator(&self) {
        self.precedence
            .store(ModePrecedence::Operator as u8, Ordering::Relaxed);
        self.emergency.store(false, Ordering::Relaxed);
    }
}

/// One version of the config file. Every writer replaces the file atomically
/// (write a temp file, rename it over), so the inode moves on every write even
/// when the length and a coarse mtime do not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileStamp {
    modified: Option<SystemTime>,
    len: u64,
    ino: u64,
}

impl FileStamp {
    fn of(path: &Path) -> Option<Self> {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::metadata(path).ok()?;
        Some(Self {
            modified: meta.modified().ok(),
            len: meta.len(),
            ino: meta.ino(),
        })
    }
}

/// The `swarm:` block, re-read when the config file changes.
struct SwarmConfigSource {
    path: PathBuf,
    /// `None` before the first read; `Some(None)` when the file was absent.
    stamp: Option<Option<FileStamp>>,
}

impl SwarmConfigSource {
    fn new(path: PathBuf) -> Self {
        Self { path, stamp: None }
    }

    /// The block, when the file changed since the last call. The first call
    /// always reads.
    fn poll(&mut self) -> Option<SwarmControlConfig> {
        let stamp = FileStamp::of(&self.path);
        if self.stamp == Some(stamp) {
            return None;
        }
        self.stamp = Some(stamp);
        Some(load_swarm_config(&self.path))
    }
}

/// Whether a block puts this node under the layer at all.
fn eligible(cfg: &SwarmControlConfig) -> bool {
    if !cfg.enabled {
        tracing::debug!("swarm_setpoint_disabled");
        return false;
    }
    if cfg.fleet_slot == 0 {
        // Slot 0 is the ground station. A node without a drone slot has no station
        // in a formation and no deconfliction ordering, so it must not fly the
        // layer rather than fly it with a guessed identity.
        tracing::warn!("swarm_setpoint_no_fleet_slot: video.wfb.fleet_slot is 0");
        return false;
    }
    true
}

/// Whether the unsigned-command risk holds the layer off. Logs once when the
/// hold starts and once when it clears.
async fn signing_holds(fc: &FcConnection, status: &SwarmSetpointStatus) -> bool {
    let holds = fc.agent_commands_unsigned_rejected_risk().await == Some(true);
    let was = status.signing_hold_logged.swap(holds, Ordering::Relaxed);
    if holds && !was {
        tracing::warn!("swarm_setpoint_held_fc_rejects_unsigned_commands");
    } else if !holds && was {
        tracing::info!("swarm_setpoint_signing_hold_cleared");
    }
    holds
}

/// The running controller and the block it was built from.
struct LiveSwarm {
    cfg: SwarmControlConfig,
    controller: SwarmController,
    slots: Vec<u8>,
}

impl LiveSwarm {
    fn new(cfg: SwarmControlConfig) -> Self {
        let slots = vec![cfg.fleet_slot];
        let controller = SwarmController::new(&cfg, &slots);
        Self {
            cfg,
            controller,
            slots,
        }
    }

    /// Take a changed block into the running controller.
    fn reconfigure(&mut self, cfg: SwarmControlConfig) {
        if cfg == self.cfg {
            return;
        }
        tracing::info!(
            slot = cfg.fleet_slot,
            mode = %cfg.mode,
            formation = %cfg.default_formation,
            "swarm_setpoint_reconfigured"
        );
        // The next tick regenerates the table for the visible fleet.
        self.slots = vec![cfg.fleet_slot];
        self.controller.apply_config(&cfg, &self.slots);
        self.cfg = cfg;
    }

    /// One control tick. `own.slot` is overwritten with this node's slot.
    fn tick(&mut self, mut own: OwnState, fixes: &[NeighborFix], now: Instant) -> ControlOutcome {
        // Re-generate the formation table when the visible fleet changes. A table
        // sized for a drone that has gone home leaves a permanent hole in the shape.
        let heard = self.cfg.visible_slots(fixes.iter().map(|f| f.slot));
        if heard != self.slots {
            self.slots = heard;
            self.controller.set_formation(
                self.cfg.formation_name(),
                &self.slots,
                self.cfg.spacing_m(),
                self.controller.formation_anchor(),
            );
        }
        own.slot = self.cfg.fleet_slot;
        self.controller.tick(&own, fixes, now)
    }
}

/// Why the control loop returned.
enum LoopExit {
    Cancelled,
    /// The block no longer puts this node under the layer.
    Ineligible(SwarmControlConfig),
}

/// Run the autonomy loop until cancelled.
///
/// While `swarm.enabled` is false or no drone slot is assigned, the loop holds
/// no socket and no controller: it checks the config file every
/// [`CONFIG_POLL`] and starts when the block makes this node eligible.
pub async fn run(
    fc: Arc<FcConnection>,
    state: Arc<Mutex<VehicleState>>,
    swarm_sock: String,
    config_path: String,
    status: Arc<SwarmSetpointStatus>,
    cancel: Shutdown,
) {
    let mut source = SwarmConfigSource::new(PathBuf::from(config_path));
    let mut cfg = source.poll().unwrap_or_default();
    loop {
        while !eligible(&cfg) || signing_holds(&fc, &status).await {
            tokio::select! {
                _ = cancel.wait() => return,
                _ = tokio::time::sleep(CONFIG_POLL) => {}
            }
            if let Some(next) = source.poll() {
                cfg = next;
            }
        }
        tracing::info!(
            slot = cfg.fleet_slot,
            mode = %cfg.mode,
            formation = %cfg.default_formation,
            "swarm_setpoint_started"
        );

        let (tx, rx) = watch::channel::<Option<(Value, Instant)>>(None);
        let reader = tokio::spawn(read_swarm_socket(swarm_sock.clone(), tx, cancel.clone()));
        let exit = control_loop(
            fc.clone(),
            state.clone(),
            LiveSwarm::new(cfg),
            &mut source,
            rx,
            status.clone(),
            cancel.clone(),
        )
        .await;
        reader.abort();
        match exit {
            LoopExit::Cancelled => return,
            LoopExit::Ineligible(next) => {
                status.stand_down();
                tracing::info!("swarm_setpoint_stopped");
                cfg = next;
            }
        }
    }
}

mod control;
use control::*;

pub(crate) mod payload;
use payload::fixes_from_payload;

/// Load the `swarm:` block plus this node's `video.wfb.fleet_slot`.
///
/// A malformed block surfaces on the Health view through the config-status
/// sidecar rather than silently defaulting the autonomy layer off: a
/// genuinely-disabled swarm and a mis-parsed one must not look identical.
pub(crate) fn load_swarm_config(path: &Path) -> SwarmControlConfig {
    #[derive(Debug, Default, serde::Deserialize)]
    struct RawConfig {
        #[serde(default)]
        swarm: SwarmControlConfig,
        #[serde(default)]
        video: VideoSection,
    }
    #[derive(Debug, Default, serde::Deserialize)]
    struct VideoSection {
        #[serde(default)]
        wfb: WfbSection,
    }
    #[derive(Debug, Default, serde::Deserialize)]
    struct WfbSection {
        #[serde(default)]
        fleet_slot: u8,
    }
    let Ok(text) = std::fs::read_to_string(path) else {
        return SwarmControlConfig::default();
    };
    let (raw, err) = ados_config::yaml_reporting::<RawConfig>(&text, "swarm-control");
    ados_config::write_config_status("swarm-control", err.as_deref());
    let mut cfg = raw.swarm;
    cfg.fleet_slot = raw.video.wfb.fleet_slot;
    cfg
}

#[cfg(test)]
mod tests;
