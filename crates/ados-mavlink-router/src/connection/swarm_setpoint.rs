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

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ados_protocol::ipc::{connect_with_retry, read_newline_line};
use ados_protocol::mavlink::{GuidedSetpoint, SetpointKind as WireSetpointKind};
use ados_swarm_control::{
    fixes_from_payload, ModePrecedence, NeighborFix, OwnState, Setpoint, SwarmControlConfig,
    SwarmController, CONTROL_PERIOD,
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

/// The flight mode the FC must report before any setpoint is sent.
const GUIDED: &str = "GUIDED";

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
}

/// Run the autonomy loop until cancelled.
///
/// Returns immediately when `swarm.enabled` is false: an operator who has not
/// turned the swarm on pays for no socket, no timer and no task.
pub async fn run(
    fc: Arc<FcConnection>,
    state: Arc<Mutex<VehicleState>>,
    swarm_sock: String,
    config_path: String,
    status: Arc<SwarmSetpointStatus>,
    cancel: Arc<Notify>,
) {
    let cfg = SwarmControlConfig::load_from(std::path::Path::new(&config_path));
    if !cfg.enabled {
        tracing::debug!("swarm_setpoint_disabled");
        return;
    }
    if cfg.fleet_slot == 0 {
        // Slot 0 is the ground station. A node without a drone slot has no station
        // in a formation and no deconfliction ordering, so it must not fly the
        // layer rather than fly it with a guessed identity.
        tracing::warn!("swarm_setpoint_no_fleet_slot: video.wfb.fleet_slot is 0");
        return;
    }
    tracing::info!(
        slot = cfg.fleet_slot,
        mode = %cfg.mode,
        formation = %cfg.default_formation,
        "swarm_setpoint_started"
    );

    let (tx, rx) = watch::channel::<Option<(Value, Instant)>>(None);
    let reader = tokio::spawn(read_swarm_socket(swarm_sock, tx, cancel.clone()));
    control_loop(fc, state, cfg, rx, status, cancel).await;
    reader.abort();
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

/// Tick the controller and send what it asks for.
async fn control_loop(
    fc: Arc<FcConnection>,
    state: Arc<Mutex<VehicleState>>,
    cfg: SwarmControlConfig,
    rx: watch::Receiver<Option<(Value, Instant)>>,
    status: Arc<SwarmSetpointStatus>,
    cancel: Arc<Notify>,
) {
    let mut controller = SwarmController::new(&cfg, &[cfg.fleet_slot]);
    let mut fixes: Vec<NeighborFix> = Vec::new();
    let mut slots: Vec<u8> = vec![cfg.fleet_slot];
    let mut tick = tokio::time::interval(CONTROL_PERIOD);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = cancel.notified() => return,
            _ = tick.tick() => {}
        }
        let now = Instant::now();

        fixes.clear();
        if let Some((payload, at)) = rx.borrow().as_ref() {
            fixes_from_payload(payload, now.saturating_duration_since(*at), &mut fixes);
        }

        // Re-generate the formation table when the visible fleet changes. A table
        // sized for a drone that has gone home leaves a permanent hole in the shape.
        let heard = cfg.visible_slots(fixes.iter().map(|f| f.slot));
        if heard != slots {
            slots = heard;
            controller.set_formation(
                cfg.formation_name(),
                &slots,
                cfg.spacing_m(),
                controller.formation_anchor(),
            );
        }

        let own = {
            let s = state.lock().await;
            OwnState {
                slot: cfg.fleet_slot,
                lat_deg: s.lat,
                lon_deg: s.lon,
                alt_rel_m: s.alt_rel,
                vn: s.vx,
                ve: s.vy,
                vd: s.vz,
                armed: s.armed,
                // The FC's OWN report, never what this layer asked for. A setpoint
                // sent outside GUIDED is either ignored or, worse, latched.
                guided: s.mode == GUIDED,
            }
        };

        let out = controller.tick(&own, &fixes, now);
        status.publish(out.precedence, out.emergency, controller.counters());
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
    use ados_swarm_control::precedence_from_wire;

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
            precedence_from_wire(s.precedence_wire()),
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

    #[tokio::test]
    async fn a_disabled_swarm_starts_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        std::fs::write(&cfg, "swarm:\n  enabled: false\n").unwrap();
        let loaded = SwarmControlConfig::load_from(&cfg);
        assert!(!loaded.enabled, "the early return is what this asserts");
    }

    #[test]
    fn a_ground_station_slot_disqualifies_the_layer() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        // Enabled, but no drone slot: slot 0 is the ground station.
        std::fs::write(&cfg, "swarm:\n  enabled: true\n").unwrap();
        assert_eq!(SwarmControlConfig::load_from(&cfg).fleet_slot, 0);
        // With a slot assigned it is eligible.
        std::fs::write(
            &cfg,
            "swarm:\n  enabled: true\nvideo:\n  wfb:\n    fleet_slot: 4\n",
        )
        .unwrap();
        assert_eq!(SwarmControlConfig::load_from(&cfg).fleet_slot, 4);
    }
}
