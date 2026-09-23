//! The 10 Hz onboard-autonomy loop: swarm neighbour table in,
//! `SET_POSITION_TARGET_GLOBAL_INT` out.
//!
//! Every control law lives in `ados-swarm-control` as a pure function; this file
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
//! disabled, the vehicle is disarmed, the FC is out of GUIDED, or the neighbour
//! table has been empty for `NEIGHBOR_STALE`, this loop emits NOTHING and the FC
//! holds on its own terms.
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
use ados_swarm_control::{
    fixes_from_payload, ControlOutcome, ModePrecedence, NeighborFix, OwnState, Setpoint,
    SwarmControlConfig, SwarmController, CONTROL_PERIOD,
};
use serde_json::Value;
use tokio::sync::{watch, Mutex, Notify};

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
}

impl SwarmSetpointStatus {
    /// The `mode_precedence` wire string for the snapshot.
    pub fn precedence_wire(&self) -> &'static str {
        ModePrecedence::from_status_bits(
            self.precedence.load(Ordering::Relaxed)
                << ados_swarmbus::beacon::STATUS_PRECEDENCE_SHIFT,
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

    fn publish(
        &self,
        level: ModePrecedence,
        emergency: bool,
        c: ados_swarm_control::SwarmControlCounters,
    ) {
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
        Some(SwarmControlConfig::load_from(&self.path))
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
    cancel: Arc<Notify>,
) {
    let mut source = SwarmConfigSource::new(PathBuf::from(config_path));
    let mut cfg = source.poll().unwrap_or_default();
    loop {
        while !eligible(&cfg) {
            tokio::select! {
                _ = cancel.notified() => return,
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

/// Feed the newest swarm payload into a watch channel.
///
/// A watch rather than an mpsc on purpose: the control loop wants the NEWEST
/// table, never a backlog, so a loop that fell behind must skip stale frames
/// rather than work through them. Reading in its own task also keeps the byte
/// reader out of the control loop's `select!`, where a cancelled read would drop
/// buffered bytes and mis-frame the next line.
async fn read_swarm_socket(
    path: String,
    tx: watch::Sender<Option<(Value, Instant)>>,
    cancel: Arc<Notify>,
) {
    loop {
        let Ok(mut stream) = connect_with_retry(&path, 1, SWARM_RECONNECT).await else {
            tokio::select! {
                _ = cancel.notified() => return,
                _ = tokio::time::sleep(SWARM_RECONNECT) => continue,
            }
        };
        loop {
            let line = tokio::select! {
                _ = cancel.notified() => return,
                r = read_newline_line(&mut stream, MAX_SWARM_LINE) => r,
            };
            match line {
                Ok(Some(buf)) => match serde_json::from_slice::<Value>(&buf) {
                    Ok(v) => {
                        // A send failure means the control loop is gone; so is the
                        // reason to keep reading.
                        if tx.send(Some((v, Instant::now()))).is_err() {
                            return;
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "swarm_payload_parse_failed"),
                },
                // EOF or an IO error: the publisher restarted. Reconnect rather
                // than giving up, and do NOT clear the last payload — the
                // controller's own staleness gate decides when it has gone off,
                // and it is the only place that decision belongs.
                Ok(None) | Err(_) => break,
            }
        }
        tokio::select! {
            _ = cancel.notified() => return,
            _ = tokio::time::sleep(SWARM_RECONNECT) => {}
        }
    }
}

/// Tick the controller and send what it asks for, re-reading the block every
/// [`CONFIG_POLL`].
async fn control_loop(
    fc: Arc<FcConnection>,
    state: Arc<Mutex<VehicleState>>,
    mut live: LiveSwarm,
    source: &mut SwarmConfigSource,
    rx: watch::Receiver<Option<(Value, Instant)>>,
    status: Arc<SwarmSetpointStatus>,
    cancel: Arc<Notify>,
) -> LoopExit {
    let mut fixes: Vec<NeighborFix> = Vec::new();
    let mut tick = tokio::time::interval(CONTROL_PERIOD);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut reload = tokio::time::interval(CONFIG_POLL);
    reload.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = cancel.notified() => return LoopExit::Cancelled,
            _ = reload.tick() => {
                if let Some(cfg) = source.poll() {
                    if !eligible(&cfg) {
                        return LoopExit::Ineligible(cfg);
                    }
                    live.reconfigure(cfg);
                }
                continue;
            }
            _ = tick.tick() => {}
        }
        let now = Instant::now();

        fixes.clear();
        if let Some((payload, at)) = rx.borrow().as_ref() {
            fixes_from_payload(payload, now.saturating_duration_since(*at), &mut fixes);
        }

        let own = {
            let s = state.lock().await;
            OwnState {
                slot: live.cfg.fleet_slot,
                lat_deg: s.lat,
                lon_deg: s.lon,
                alt_rel_m: s.alt_rel,
                vn: s.vx,
                ve: s.vy,
                vd: s.vz,
                armed: s.armed,
                // The FC's OWN report, never what this layer asked for. A
                // setpoint sent in a mode that does not accept one is either
                // ignored or, worse, latched. Which mode that is depends on the
                // firmware, so the name is checked against every commandable
                // one rather than against ArduPilot's alone -- comparing to
                // "GUIDED" meant a PX4 vehicle could never be commanded, since
                // PX4 has no mode by that name.
                guided: ados_protocol::accepts_offboard_setpoints(&s.mode),
                // How stale our OWN fix is. Read from the position stamp
                // specifically, never from `last_update`, which every frame
                // refreshes: a heartbeat arriving while the position has frozen
                // is precisely the case the controller has to refuse, and
                // `last_update` reports it as healthy.
                fix_age: s
                    .position_at
                    .map(|t| std::time::Instant::now().saturating_duration_since(t)),
            }
        };

        let out = live.tick(own, &fixes, now);
        status.publish(out.precedence, out.emergency, live.controller.counters());
        if let Some(setpoint) = out.setpoint {
            send_setpoint(&fc, &setpoint).await;
        }
    }
}

/// Turn a control-layer setpoint into MAVLink 86 and send it.
///
/// The message is built through `ados_protocol::mavlink::GuidedSetpoint`, which
/// validates the `type_mask` and the coordinate frame and refuses a NaN on an
/// active axis. That validation is the reason this goes through the shared builder
/// instead of constructing the payload here: a malformed setpoint must be refused
/// on this side of the wire, not diagnosed from the vehicle's behaviour.
async fn send_setpoint(fc: &Arc<FcConnection>, setpoint: &Setpoint) {
    let wire = GuidedSetpoint {
        kind: WireSetpointKind::GlobalInt,
        coordinate_frame: setpoint.coordinate_frame(),
        type_mask: setpoint.type_mask(),
        // The global message carries lat/lon already scaled by 1e7.
        x: setpoint.lat_e7 as f64,
        y: setpoint.lon_e7 as f64,
        z: setpoint.alt_m as f64,
        vx: setpoint.vn,
        vy: setpoint.ve,
        vz: setpoint.vd,
        afx: 0.0,
        afy: 0.0,
        afz: 0.0,
        yaw: 0.0,
        yaw_rate: 0.0,
    };
    // Target ids, not ours: the autopilot is system 1 / component 1
    // (MAV_COMP_ID_AUTOPILOT1) by MAVLink convention, and the router's own
    // identity goes in the header `send_msg` builds.
    let msg = match wire.build_message(1, 1) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(error = %e, kind = ?setpoint.kind, "swarm_setpoint_rejected");
            return;
        }
    };
    fc.send_msg(&msg).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_swarm_control::geo::Ned;

    #[test]
    fn the_status_block_round_trips_every_precedence_level() {
        let s = SwarmSetpointStatus::default();
        // The default must be the honest pre-Phase-5 value, since the snapshot is
        // published from tick zero.
        assert_eq!(s.precedence_wire(), "hold");
        assert!(!s.emergency());
        for level in ModePrecedence::ARBITRATION_ORDER {
            s.publish(
                level,
                level == ModePrecedence::HardSeparation,
                Default::default(),
            );
            assert_eq!(s.precedence_wire(), level.as_wire(), "{level:?}");
        }
        assert_eq!(
            ModePrecedence::from_wire(s.precedence_wire()),
            ModePrecedence::Hold
        );
    }

    #[test]
    fn the_status_block_publishes_the_controller_counters() {
        let s = SwarmSetpointStatus::default();
        s.publish(
            ModePrecedence::Flocking,
            true,
            ados_swarm_control::SwarmControlCounters {
                setpoints_emitted: 7,
                ticks_suppressed: 3,
                hard_engagements: 2,
            },
        );
        assert_eq!(s.setpoints_emitted(), 7);
        assert_eq!(s.ticks_suppressed(), 3);
        assert_eq!(s.hard_engagements(), 2);
        assert!(s.emergency());
    }

    #[test]
    fn a_velocity_setpoint_builds_a_valid_global_int_message() {
        let sp = Setpoint::velocity(Ned::new(1.5, -2.5, -1.0));
        let wire = GuidedSetpoint {
            kind: WireSetpointKind::GlobalInt,
            coordinate_frame: sp.coordinate_frame(),
            type_mask: sp.type_mask(),
            x: sp.lat_e7 as f64,
            y: sp.lon_e7 as f64,
            z: sp.alt_m as f64,
            vx: sp.vn,
            vy: sp.ve,
            vz: sp.vd,
            afx: 0.0,
            afy: 0.0,
            afz: 0.0,
            yaw: 0.0,
            yaw_rate: 0.0,
        };
        wire.validate()
            .expect("the control layer must not emit an invalid setpoint");
        // The plan's message: SET_POSITION_TARGET_GLOBAL_INT, id 86.
        let msg = wire.build_message(1, 1).expect("builds");
        let bytes = ados_protocol::mavlink::serialize_v2(
            ados_protocol::mavlink::MavHeader {
                system_id: 1,
                component_id: 1,
                sequence: 0,
            },
            &msg,
        )
        .expect("serializes");
        assert_eq!(crate::aux_tee::mavlink_message_id(&bytes), Some(86));
    }

    #[test]
    fn a_position_setpoint_also_builds_and_validates() {
        let sp = Setpoint::position(12.9716, 77.5946, 30.0);
        let wire = GuidedSetpoint {
            kind: WireSetpointKind::GlobalInt,
            coordinate_frame: sp.coordinate_frame(),
            type_mask: sp.type_mask(),
            x: sp.lat_e7 as f64,
            y: sp.lon_e7 as f64,
            z: sp.alt_m as f64,
            vx: sp.vn,
            vy: sp.ve,
            vz: sp.vd,
            afx: 0.0,
            afy: 0.0,
            afz: 0.0,
            yaw: 0.0,
            yaw_rate: 0.0,
        };
        wire.validate().expect("valid");
        assert!(wire.build_message(1, 1).is_ok());
    }

    /// Replace the file the way every config writer does: a temp file renamed
    /// over the original.
    fn replace(path: &Path, text: &str) {
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, text).unwrap();
        std::fs::rename(&tmp, path).unwrap();
    }

    const HOME: (f64, f64, f64) = (12.9716, 77.5946, 30.0);

    fn own_fresh() -> OwnState {
        OwnState {
            slot: 0,
            lat_deg: HOME.0,
            lon_deg: HOME.1,
            alt_rel_m: HOME.2,
            vn: 0.0,
            ve: 0.0,
            vd: 0.0,
            armed: true,
            guided: true,
            fix_age: Some(Duration::ZERO),
        }
    }

    /// Slot 2, `north_m` metres north of `own_fresh`, at the same altitude.
    fn neighbour_north(north_m: f64) -> NeighborFix {
        use ados_swarm_control::neighbor::{STATUS_ARMED, STATUS_GPS_OK, STATUS_GUIDED};
        let (lat, lon, alt) = ados_swarm_control::GeoOrigin::new(HOME.0, HOME.1, HOME.2)
            .to_geo(Ned::new(north_m, 0.0, 0.0));
        NeighborFix {
            slot: 2,
            lat_deg: lat,
            lon_deg: lon,
            alt_m: alt,
            vn: 0.0,
            ve: 0.0,
            vd: 0.0,
            status: STATUS_ARMED | STATUS_GUIDED | STATUS_GPS_OK,
            sender_order: std::cmp::Ordering::Equal,
        }
    }

    const ENABLED_SLOT_1: &str = "swarm:\n  enabled: true\nvideo:\n  wfb:\n    fleet_slot: 1\n";

    #[test]
    fn a_raised_separation_envelope_reaches_the_running_controller() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        replace(&path, ENABLED_SLOT_1);
        let mut source = SwarmConfigSource::new(path.clone());
        let mut live = LiveSwarm::new(source.poll().expect("first poll reads"));
        let near = [neighbour_north(6.0)];

        // 6 m is outside the default 4 m hard floor.
        let before = live.tick(own_fresh(), &near, Instant::now());
        assert!(!before.emergency, "{before:?}");

        // The operator raises the envelope past the neighbour's distance.
        replace(
            &path,
            "swarm:\n  enabled: true\n  separation:\n    radius_m: 20\n    hard_m: 10\n\
             video:\n  wfb:\n    fleet_slot: 1\n",
        );
        live.reconfigure(source.poll().expect("a rewritten file is re-read"));

        let after = live.tick(own_fresh(), &near, Instant::now());
        assert!(after.emergency, "{after:?}");
        assert_eq!(after.precedence, ModePrecedence::HardSeparation);
    }

    #[test]
    fn turning_participation_on_is_picked_up_without_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        replace(
            &path,
            "swarm:\n  enabled: false\nvideo:\n  wfb:\n    fleet_slot: 1\n",
        );
        let mut source = SwarmConfigSource::new(path.clone());
        assert!(!eligible(&source.poll().expect("first poll reads")));

        replace(&path, ENABLED_SLOT_1);
        let next = source.poll().expect("a rewritten file is re-read");
        assert!(eligible(&next));

        // And off again: the running loop stands down on the next poll.
        replace(
            &path,
            "swarm:\n  enabled: false\nvideo:\n  wfb:\n    fleet_slot: 1\n",
        );
        assert!(!eligible(
            &source.poll().expect("a rewritten file is re-read")
        ));
    }

    #[test]
    fn an_unchanged_file_is_not_reparsed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        replace(&path, ENABLED_SLOT_1);
        let mut source = SwarmConfigSource::new(path.clone());
        assert!(source.poll().is_some());
        assert!(source.poll().is_none());
        // An absent file reads as the default block once, then stays quiet.
        std::fs::remove_file(&path).unwrap();
        assert_eq!(source.poll(), Some(SwarmControlConfig::default()));
        assert!(source.poll().is_none());
    }

    #[test]
    fn a_ground_station_slot_disqualifies_the_layer() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        // Enabled, but no drone slot: slot 0 is the ground station.
        std::fs::write(&cfg, "swarm:\n  enabled: true\n").unwrap();
        assert!(!eligible(&SwarmControlConfig::load_from(&cfg)));
        // With a slot assigned it is eligible.
        std::fs::write(
            &cfg,
            "swarm:\n  enabled: true\nvideo:\n  wfb:\n    fleet_slot: 4\n",
        )
        .unwrap();
        assert!(eligible(&SwarmControlConfig::load_from(&cfg)));
    }

    #[test]
    fn standing_down_clears_the_advertised_level() {
        let s = SwarmSetpointStatus::default();
        s.publish(ModePrecedence::HardSeparation, true, Default::default());
        s.stand_down();
        assert_eq!(s.precedence_wire(), "hold");
        assert!(!s.emergency());
    }
}
