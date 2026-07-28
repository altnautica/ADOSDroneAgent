//! Mode arbitration, the 10 Hz loop, and the failsafe gates.
//!
//! The only stateful object in the crate. It holds what a safety layer genuinely
//! needs latched — the hard-override dwell and its climb datum, the staleness
//! deadline, the operator directive's expiry — converts geodetic fixes into the
//! local frame the laws work in, and arbitrates.
//!
//! # Precedence, highest first
//!
//! 1. HARD SEPARATION. Engaged on a geometric breach of the hard radius, or when
//!    the closing-rate barrier had to refuse the behaviour layer's command. It
//!    discards the horizontal solution and climbs to a slot-indexed offset.
//! 2. OPERATOR direct command.
//! 3. FORMATION keeping.
//! 4. FLOCKING.
//! 5. HOLD — nothing is driving.
//!
//! # Failsafes
//!
//! This layer adds none and replaces none. ArduPilot's `FS_GCS_ENABLE` /
//! `FS_LONG_ACTN` and the geofence are the backstop and act independently of it.
//! What this layer does is get out of the way: it emits NOTHING unless the swarm
//! is enabled, the vehicle is armed, the FC reports GUIDED, and the neighbour
//! table has been non-empty inside [`crate::NEIGHBOR_STALE`]. A layer with no
//! fresh neighbour data has no basis to fly and hands the vehicle back to the FC.
//!
//! Operator authority is untouched by that gate: a GCS command reaches the FC
//! through the router's client-passthrough path, not through here. The `Operator`
//! precedence level is for a SWARM-level directive (a fleet reposition) being
//! arbitrated against the autonomy layers.

use std::time::{Duration, Instant};

use crate::barrier::{self, BARRIER_GAIN_HZ};
use crate::config::SwarmControlConfig;
use crate::flocking::{self, FlockTuning};
use crate::formation::{self, Formation, FormationAnchor, FormationName, FORMATION_GAIN};
use crate::geo::{GeoOrigin, Ned};
use crate::neighbor::NeighborState;
use crate::precedence::ModePrecedence;
use crate::separation::{self, hard_override, HardBreach};
use crate::setpoint::{Setpoint, MAX_COMMAND_SPEED_MPS};

/// Control loop rate. Ten times the 2 Hz beacon rate: the loop runs against
/// dead-reckoned neighbour positions, so the predict/correct split is what lets a
/// 2 Hz beacon drive a 10 Hz controller without a staircase.
pub const CONTROL_HZ: f64 = 10.0;

/// Control loop period.
pub const CONTROL_PERIOD: Duration = Duration::from_millis(100);

/// How long an operator directive stays in force without being re-issued.
///
/// Matched to the neighbour staleness window: a directive whose issuer has gone
/// quiet must not keep outranking the autonomy layers indefinitely.
pub const OPERATOR_DIRECTIVE_TTL: Duration = Duration::from_secs(3);

/// Minimum time the hard override stays engaged once triggered.
///
/// Without it the override chatters at 10 Hz: engaging zeroes the horizontal
/// command, which removes the blocked-closure that triggered it, which releases
/// the override, which lets the behaviour layer close again. One second is long
/// enough for the geometry to actually change.
pub const HARD_DWELL: Duration = Duration::from_secs(1);

/// Proportional gain on an operator reposition, 1/s.
pub const OPERATOR_GAIN: f64 = 1.0;

/// Commands under this magnitude are treated as no command at all while nothing
/// is driving, m/s. Keeps a drone in `hold` genuinely hands-off instead of
/// dribbling millimetre-per-second setpoints at the FC ten times a second.
pub const COMMAND_DEADBAND_MPS: f64 = 0.05;

/// The operator-commandable behaviour mode (`swarm.mode`).
///
/// `hard-separation` and `operator` are deliberately absent: those are precedence
/// levels the arbitration DERIVES, not modes anybody can ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SwarmMode {
    #[default]
    Hold,
    Flocking,
    Formation,
}

impl SwarmMode {
    pub const fn as_wire(self) -> &'static str {
        match self {
            Self::Hold => "hold",
            Self::Flocking => "flocking",
            Self::Formation => "formation",
        }
    }

    /// Parse `swarm.mode`. Anything unrecognised is `hold` — an unknown mode must
    /// never fly.
    pub fn from_wire(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "flocking" => Self::Flocking,
            "formation" => Self::Formation,
            _ => Self::Hold,
        }
    }
}

/// A swarm-level operator directive.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum OperatorDirective {
    /// Reposition to a geodetic point.
    Goto {
        lat_deg: f64,
        lon_deg: f64,
        alt_m: f64,
    },
    /// Stop where you are.
    Hold,
}

/// This drone's own state, as the router already holds it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OwnState {
    pub slot: u8,
    pub lat_deg: f64,
    pub lon_deg: f64,
    /// Altitude relative to home, metres — the frame every setpoint uses.
    pub alt_rel_m: f64,
    pub vn: f64,
    pub ve: f64,
    pub vd: f64,
    pub armed: bool,
    /// Whether the FC reports GUIDED. No setpoint is emitted otherwise.
    pub guided: bool,
}

impl Default for OwnState {
    fn default() -> Self {
        Self {
            slot: 1,
            lat_deg: 0.0,
            lon_deg: 0.0,
            alt_rel_m: 0.0,
            vn: 0.0,
            ve: 0.0,
            vd: 0.0,
            armed: false,
            guided: false,
        }
    }
}

/// A neighbour straight off the table: geodetic, already dead-reckoned forward to
/// `now` by the caller.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NeighborFix {
    pub slot: u8,
    pub lat_deg: f64,
    pub lon_deg: f64,
    pub alt_m: f64,
    pub vn: f64,
    pub ve: f64,
    /// Down-positive, MAVLink convention: a climbing neighbour is negative.
    pub vd: f64,
    pub status: u8,
}

/// Why a tick emitted nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Suppression {
    /// `swarm.enabled` is false.
    Disabled,
    /// The neighbour table has been empty for [`crate::NEIGHBOR_STALE`].
    NeighborsStale,
    /// The vehicle is not armed.
    NotArmed,
    /// The FC is not in GUIDED.
    NotGuided,
    /// Nothing is driving and the safety term has nothing to say.
    NothingToDo,
}

/// One tick's result.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ControlOutcome {
    pub setpoint: Option<Setpoint>,
    /// The level ACTUALLY driving, which is what rides beacon status bits 5..7.
    pub precedence: ModePrecedence,
    /// Whether beacon status bit 2 should be raised on the next beacon.
    pub emergency: bool,
    pub suppressed: Option<Suppression>,
}

/// Cheap counters for the status snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SwarmControlCounters {
    pub setpoints_emitted: u64,
    pub ticks_suppressed: u64,
    pub hard_engagements: u64,
}

#[derive(Debug, Clone, Copy)]
struct HardLatch {
    engaged_at: Instant,
    target_alt_m: f64,
    offender: u8,
}

/// The onboard autonomy loop.
pub struct SwarmController {
    enabled: bool,
    mode: SwarmMode,
    formation: Formation,
    flock: FlockTuning,
    barrier_gain: f64,
    /// Fleet target for the flocking virtual leader, geodetic.
    target: Option<(f64, f64, f64)>,
    operator: Option<(OperatorDirective, Instant)>,
    hard: Option<HardLatch>,
    last_neighbors_at: Option<Instant>,
    precedence: ModePrecedence,
    emergency: bool,
    counters: SwarmControlCounters,
    /// Reused per tick so the 10 Hz loop never allocates.
    scratch: Vec<NeighborState>,
}

impl SwarmController {
    /// A controller from the `swarm:` config block and the registered slot set.
    pub fn new(cfg: &SwarmControlConfig, slots: &[u8]) -> Self {
        let mut c = Self {
            enabled: false,
            mode: SwarmMode::Hold,
            formation: Formation::built_in(
                FormationName::Line,
                slots,
                formation::DEFAULT_SPACING_M,
                FormationAnchor::Centroid,
            ),
            flock: FlockTuning::default(),
            barrier_gain: BARRIER_GAIN_HZ,
            target: None,
            operator: None,
            hard: None,
            last_neighbors_at: None,
            precedence: ModePrecedence::Hold,
            emergency: false,
            counters: SwarmControlCounters::default(),
            scratch: Vec::with_capacity(crate::neighbor::MAX_NEAREST),
        };
        c.apply_config(cfg, slots);
        c
    }

    /// Re-read the config. Safe to call on a live controller: it never touches a
    /// latch, so a config reload cannot silently disarm the safety layer
    /// mid-engagement.
    pub fn apply_config(&mut self, cfg: &SwarmControlConfig, slots: &[u8]) {
        self.enabled = cfg.enabled;
        self.mode = cfg.swarm_mode();
        self.flock = cfg.flock_tuning();
        self.formation = Formation::built_in(
            cfg.formation_name(),
            slots,
            cfg.spacing_m(),
            self.formation.anchor,
        );
    }

    pub fn set_mode(&mut self, mode: SwarmMode) {
        self.mode = mode;
    }

    pub fn mode(&self) -> SwarmMode {
        self.mode
    }

    /// Rebuild the formation table for a new shape, anchor or slot set.
    pub fn set_formation(
        &mut self,
        name: FormationName,
        slots: &[u8],
        spacing_m: f64,
        anchor: FormationAnchor,
    ) {
        self.formation = Formation::built_in(name, slots, spacing_m, anchor);
    }

    pub fn formation(&self) -> &Formation {
        &self.formation
    }

    /// The anchor the current table is measured from. Exposed so a caller
    /// regenerating the table for a changed slot set preserves the anchor instead
    /// of silently reverting it to the centroid.
    pub fn formation_anchor(&self) -> FormationAnchor {
        self.formation.anchor
    }

    /// The flocking virtual leader's goal, geodetic. `None` = free flocking.
    pub fn set_target(&mut self, target: Option<(f64, f64, f64)>) {
        self.target = target;
    }

    /// Issue a swarm-level operator directive. It expires after
    /// [`OPERATOR_DIRECTIVE_TTL`] unless re-issued.
    pub fn set_operator_directive(&mut self, directive: OperatorDirective, now: Instant) {
        self.operator = Some((directive, now));
    }

    pub fn clear_operator_directive(&mut self) {
        self.operator = None;
    }

    /// The level actually driving, as of the last tick.
    pub fn precedence(&self) -> ModePrecedence {
        self.precedence
    }

    /// Whether the next beacon should raise status bit 2.
    pub fn emergency(&self) -> bool {
        self.emergency
    }

    /// While the hard override is engaged: the slot that triggered it and the
    /// altitude it is climbing to. Diagnostic only — the arbitration never reads
    /// it back — but it is what turns "emergency" on the operator screen into
    /// "emergency, because of slot 7".
    pub fn hard_latch(&self) -> Option<(u8, f64)> {
        self.hard.map(|l| (l.offender, l.target_alt_m))
    }

    pub fn counters(&self) -> SwarmControlCounters {
        self.counters
    }

    /// One control tick.
    pub fn tick(&mut self, own: &OwnState, fixes: &[NeighborFix], now: Instant) -> ControlOutcome {
        let origin = GeoOrigin::new(own.lat_deg, own.lon_deg, own.alt_rel_m);
        self.scratch.clear();
        for f in fixes {
            let pos = origin.to_ned(f.lat_deg, f.lon_deg, f.alt_m);
            if !pos.is_finite() {
                continue;
            }
            self.scratch.push(NeighborState::new(
                f.slot,
                pos,
                Ned::new(f.vn, f.ve, f.vd),
                f.status,
            ));
        }
        if !self.scratch.is_empty() {
            self.last_neighbors_at = Some(now);
        } else if self.last_neighbors_at.is_none() {
            // First tick with nothing heard yet: start the staleness clock now
            // rather than treating an unstarted swarm as already stale.
            self.last_neighbors_at = Some(now);
        }

        if !self.enabled {
            return self.suppress(Suppression::Disabled, false);
        }
        let stale = self
            .last_neighbors_at
            .is_none_or(|t| now.saturating_duration_since(t) >= crate::NEIGHBOR_STALE);
        if stale {
            // Never fly on stale data. The FC holds; its own failsafes stand.
            self.hard = None;
            return self.suppress(Suppression::NeighborsStale, false);
        }
        if !own.armed {
            self.hard = None;
            return self.suppress(Suppression::NotArmed, false);
        }

        let sep = self.flock.separation;
        let breach = separation::hard_breach(&self.scratch, &sep);
        let repulsion = separation::repulsion(&self.scratch, &sep);
        let (behaviour_level, behaviour) = self.behaviour_command(own, now);
        let repelled = if matches!(behaviour_level, ModePrecedence::Flocking) {
            // The flocking law already carries the repulsive term.
            behaviour
        } else {
            behaviour + repulsion
        };
        let bounded = barrier::constrain(
            repelled,
            Ned::new(own.vn, own.ve, own.vd),
            &self.scratch,
            &sep,
            self.barrier_gain,
        );
        // Escalate on a PIN, never on an ordinary trim: the barrier trims
        // constantly inside a dense lattice, and treating every trim as an
        // emergency freezes the whole fleet in a hold-and-climb.
        self.update_hard_latch(own, now, breach, bounded.pinned_by);

        // The proximity alarm is raised whenever the geometry is breached, even
        // when no setpoint can be emitted: the operator needs to see a near miss
        // regardless of what mode the FC happens to be in.
        self.emergency = breach.is_some() || self.hard.is_some();

        if !own.guided {
            return self.suppress(Suppression::NotGuided, self.emergency);
        }

        if let Some(latch) = self.hard {
            self.precedence = ModePrecedence::HardSeparation;
            self.counters.setpoints_emitted += 1;
            // The behaviour layer's horizontal SOLUTION is abandoned, but the
            // repulsive term is not part of that solution — it is the safety
            // layer's own, and dropping it would leave two pinned vehicles with
            // nothing but residual velocity between them. Horizontal only, so it
            // cannot fight the deterministic climb.
            let escape = repulsion.horizontal() + hard_override(own.alt_rel_m, latch.target_alt_m);
            // The override's own command goes through the barrier too. Its climb is
            // a deconfliction manoeuvre against ONE offender, and a fleet is not a
            // pair: an unconstrained climb is a vehicle flying up into whoever
            // happens to be above it. If every direction is refused the vehicle
            // simply holds, which is the correct floor for a last resort.
            let command = barrier::constrain(
                escape,
                Ned::new(own.vn, own.ve, own.vd),
                &self.scratch,
                &sep,
                self.barrier_gain,
            )
            .command;
            return ControlOutcome {
                setpoint: Some(Setpoint::velocity(command)),
                precedence: ModePrecedence::HardSeparation,
                emergency: true,
                suppressed: None,
            };
        }

        if behaviour_level == ModePrecedence::Hold && bounded.command.norm() < COMMAND_DEADBAND_MPS
        {
            return self.suppress(Suppression::NothingToDo, self.emergency);
        }
        self.precedence = behaviour_level;
        self.counters.setpoints_emitted += 1;
        ControlOutcome {
            setpoint: Some(Setpoint::velocity(bounded.command)),
            precedence: behaviour_level,
            emergency: self.emergency,
            suppressed: None,
        }
    }

    /// The active behaviour layer and its raw command, before the safety layer.
    fn behaviour_command(&self, own: &OwnState, now: Instant) -> (ModePrecedence, Ned) {
        let origin = GeoOrigin::new(own.lat_deg, own.lon_deg, own.alt_rel_m);
        if let Some((directive, issued)) = self.operator {
            if now.saturating_duration_since(issued) < OPERATOR_DIRECTIVE_TTL {
                let cmd = match directive {
                    OperatorDirective::Goto {
                        lat_deg,
                        lon_deg,
                        alt_m,
                    } => origin
                        .to_ned(lat_deg, lon_deg, alt_m)
                        .scale(OPERATOR_GAIN)
                        .clamp_norm(MAX_COMMAND_SPEED_MPS),
                    OperatorDirective::Hold => Ned::ZERO,
                };
                return (ModePrecedence::Operator, cmd);
            }
        }
        match self.mode {
            SwarmMode::Formation => formation::command(
                &self.formation,
                own.slot,
                &self.scratch,
                FORMATION_GAIN,
                MAX_COMMAND_SPEED_MPS,
            )
            .map(|c| (ModePrecedence::Formation, c))
            // No station or no resolvable anchor: decline rather than guess.
            .unwrap_or((ModePrecedence::Hold, Ned::ZERO)),
            SwarmMode::Flocking => (
                ModePrecedence::Flocking,
                flocking::command(
                    Ned::new(own.vn, own.ve, own.vd),
                    self.target
                        .map(|(lat, lon, alt)| origin.to_ned(lat, lon, alt)),
                    &self.scratch,
                    &self.flock,
                    MAX_COMMAND_SPEED_MPS,
                ),
            ),
            SwarmMode::Hold => (ModePrecedence::Hold, Ned::ZERO),
        }
    }

    /// Engage, hold or release the hard override.
    ///
    /// Two triggers. A GEOMETRIC breach of the hard radius is the literal one. A
    /// PINNED closure — the barrier leaving no usable closing allowance at all
    /// while the behaviour layer still wants to close — is the same condition seen
    /// one step earlier: the horizontal solution has been taken away, which is
    /// precisely when the deterministic vertical rule is the only thing left that
    /// can break the deadlock.
    fn update_hard_latch(
        &mut self,
        own: &OwnState,
        now: Instant,
        breach: Option<HardBreach>,
        pinned_by: Option<u8>,
    ) {
        let trigger = breach.map(|b| (b.slot, b.relative_alt_m)).or_else(|| {
            pinned_by.and_then(|slot| {
                self.scratch
                    .iter()
                    .find(|n| n.slot == slot)
                    .map(|n| (slot, -n.pos.d))
            })
        });
        match (self.hard, trigger) {
            (None, Some((offender, relative_alt_m))) => {
                self.counters.hard_engagements += 1;
                self.hard = Some(HardLatch {
                    engaged_at: now,
                    // Pinned to the HOLDER's altitude, so re-engagement recomputes
                    // the same number and the pair cannot ratchet each other
                    // upward. See `separation::climb_offset_m`.
                    target_alt_m: separation::hard_climb_target_m(
                        own.alt_rel_m,
                        own.slot,
                        offender,
                        relative_alt_m,
                    ),
                    offender,
                });
            }
            (Some(latch), None)
                if now.saturating_duration_since(latch.engaged_at) >= HARD_DWELL =>
            {
                self.hard = None;
            }
            _ => {}
        }
    }

    fn suppress(&mut self, why: Suppression, emergency: bool) -> ControlOutcome {
        self.precedence = ModePrecedence::Hold;
        self.emergency = emergency;
        self.counters.ticks_suppressed += 1;
        ControlOutcome {
            setpoint: None,
            precedence: ModePrecedence::Hold,
            emergency,
            suppressed: Some(why),
        }
    }
}

#[cfg(test)]
mod tests;
