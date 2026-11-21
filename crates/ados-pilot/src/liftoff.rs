//! Steady take-off: an ArduPilot multirotor launch lifts off level even when
//! the airframe is not balanced.
//!
//! ArduPilot resets the rate-loop integrators every loop until a GUIDED or
//! AUTO take-off leaves the ground, so an airframe whose centre of gravity
//! sits off the thrust centre pivots on its skids as the thrust rises and
//! leaves the ground tilted. In GUIDED with an attitude target (with
//! `GUID_OPTIONS` bit 3 the target's thrust is thrust) the integrators stay
//! live on the ground once the motors spin, so the pilot runs the take-off
//! itself:
//!
//! 1. **Spool**: idle thrust at the resting attitude until every motor spins
//!    steadily; the resting attitude is captured (slope-aware).
//! 2. **Level**: with a learned trim, at half hover thrust, lean the
//!    attitude target toward it until the rate integrators hold it.
//! 3. **Find**: thrust fast to most of hover, then slowly on, level, until a
//!    side starts to unload (or, balanced, it lifts off). The thrust a side
//!    unloaded at is the unloading thrust.
//! 4. **Bracket**: at [`BRACKET_MARGIN`] of hover below the unloading
//!    thrust both skids stay loaded, so the attitude target can lean the
//!    rate-loop integrators to any value without the airframe moving. Each
//!    edge says which side of the trim the integrator was on (a side tilted
//!    up: too much). From a bound on one side the next value is
//!    [`BRACKET_STEP`] past it; between bounds on both sides it is their
//!    midpoint. After each wind the airframe settles level, then a probe
//!    ramps the thrust until a side unloads again. Bounds closer than
//!    [`BRACKET_TOL`] give the trim (their midpoint), on each axis that
//!    unloaded. A probe that lifts the airframe before a side unloads has
//!    the trim close enough: it climbs out at the commit's thrust.
//! 5. **Commit**: with the trim wound in, the thrust steps in
//!    [`COMMIT_RAMP_MS`] to [`COMMIT_STEP`] of hover above the unloading
//!    thrust (less, where the busiest motor would pass
//!    [`COMMIT_MOTOR`]), level, so the airframe leaves the ground before
//!    a small trim error can tip it. No lift-off in [`COMMIT_WAIT_MS`], or a
//!    tilt past [`COMMIT_TILT_DEG`], cuts the thrust and brackets again,
//!    once. Less than [`COMMIT_MIN`] of hover of motor headroom stops the
//!    launch: no climb margin. A tilt past [`HARD_DEG`] on the ground cuts
//!    the thrust to [`HARD_FRAC`] of hover; past [`ABORT_DEG`], or a motor
//!    held at the top of its range, stops the launch.
//! 6. **Climb**: level, clear of the ground, then a position target that
//!    starts where the airframe is and moving as it moves, climbing to the
//!    launch height.
//!
//! This module is the state machine only: [`Liftoff::step`] takes what the
//! FC reports and returns the frame to send. The pilot owns the I/O and the
//! operator's stops (`core/actions/liftoff.rs`).

#[cfg(any(test, feature = "dst"))]
pub mod sim;
#[cfg(test)]
mod tests;

use std::collections::VecDeque;

use crate::airframe::{LiftoffOutcome, Trim};
use crate::fc::{Attitude, LocalPos};

/// The FC parameters the lift-off reads or sets.
pub const PARAMS: &[&str] = &[
    "GUID_OPTIONS",
    "GCS_PID_MASK",
    "MOT_THST_HOVER",
    "MOT_PWM_MIN",
    "MOT_PWM_MAX",
    "FRAME_CLASS",
];
/// `GUID_OPTIONS` bit 3: SET_ATTITUDE_TARGET thrust is thrust.
pub use ados_protocol::mavlink::GUID_OPTIONS_THRUST;
/// `GCS_PID_MASK` bits 0 and 1: PID_TUNING for roll and pitch.
pub const GCS_PID_ROLL_PITCH: u32 = 0b11;

/// Idle thrust while the motors spool.
pub const SPOOL_THRUST: f32 = 0.02;
/// The motors must spin steadily this long.
const SPOOL_STEADY_MS: u64 = 500;
/// Without motor outputs, the spool is assumed done this long after it
/// began (ArduPilot's 2 s arming delay plus the spool-up).
const SPOOL_BLIND_MS: u64 = 3000;
const SPOOL_TIMEOUT_MS: u64 = 8000;
/// Spinning: every motor this far above its minimum, as a share of range.
const SPOOL_MIN_FRAC: f32 = 0.05;
/// …and within this many µs of each other.
const SPOOL_SPREAD_US: u16 = 15;
/// Thrust while a learned trim loads, as a share of hover.
const LOAD_FRAC: f32 = 0.5;
/// The attitude target's lean toward the trim while it loads, degrees.
const LOAD_LEAN_DEG: f32 = 8.0;
/// A load that has not reached its target by then ramps anyway.
const LOAD_TIMEOUT_MS: u64 = 8000;
/// The find ramp: fast to this share of hover in [`FAST_S`]. Below it no
/// airframe the balance check lets fly unloads a skid.
const K_FAST: f32 = 0.7;
const FAST_S: f32 = 0.5;
/// Then the thrust rises by this share of hover per second while the
/// airframe sits level, until a side unloads or it lifts, up to
/// [`THRUST_MAX`]. The hover estimate only paces the ramp: an untuned
/// `MOT_THST_HOVER` can be well short of the airframe's.
const EDGE_RATE: f32 = 0.15;
const THRUST_MAX: f32 = 0.9;
/// A side has started to unload: tilted this far from rest on its axis.
pub const EDGE_DEG: f32 = 0.5;
/// The bracket holds the thrust this share of hover below the highest
/// thrust a side unloaded at, so both skids are loaded while it winds.
/// Hover here is the estimate, or the highest unloading thrust where that
/// is higher (an untuned `MOT_THST_HOVER` reads well short).
const BRACKET_MARGIN: f32 = 0.12;
/// From a trim bound on one side only, the next integrator tried is this
/// far past it; each further edge on the same side halves the step (the
/// edges come closer to lift-off as the integrator nears the trim), down
/// to [`BRACKET_STEP_MIN`].
const BRACKET_STEP: f32 = 0.15;
const BRACKET_STEP_MIN: f32 = 0.02;
/// A tilt back under this share of [`EDGE_DEG`] has settled: the next tilt
/// past [`EDGE_DEG`] is a new edge.
const SETTLED_FRAC: f32 = 0.8;
/// The commit's thrust keeps the busiest motor under this share of its
/// range (the ground stop is [`SATURATION`]).
const COMMIT_MOTOR: f32 = 0.92;
/// After an edge the thrust drops and the airframe falls back onto its
/// skids and bounces past rest: no new edge counts for this long.
const EDGE_QUIET_MS: u64 = 1000;
/// A side unloading while the airframe already rises this fast (m/s) is
/// the lift-off starting, not a trim error: it commits.
const RISING_VZ_MPS: f32 = 0.05;
/// The trim is known once its bounds on an axis are this close; the
/// bracket's midpoint is the trim.
const BRACKET_TOL: f32 = 0.03;
/// More turns of an axis's edge from one side to the other than this, or
/// more edges in all than [`MAX_EDGES`], and the bracket gives up.
const MAX_REVERSALS: u32 = 8;
const MAX_EDGES: u32 = 16;
/// The attitude target's lean while it winds the rate-loop integrator to
/// the bracket's next value: proportional to what is left (rad per unit of
/// integrator), at most the axis's lean, which starts at
/// [`EDGE_LEAN_DEG`] and halves each time that axis's edge turns round.
const WIND_K: f32 = 0.5;
const EDGE_LEAN_DEG: f32 = 6.0;
const LEAN_MIN_DEG: f32 = 1.0;
/// Wound once within this of the value it winds to.
const WIND_TOL: f32 = 0.004;
/// A wind that has not got there by then is probed anyway.
const WIND_TIMEOUT_MS: u64 = 8000;
/// Level at the bracket thrust this long, back on both skids, before the
/// probe ramps (at [`EDGE_RATE`], until a side unloads or it lifts).
const SETTLE_MS: u64 = 500;
/// The commit: a thrust step to this share of hover above the highest
/// unloading thrust, in [`COMMIT_RAMP_MS`], bounded by the busiest motor
/// reaching [`COMMIT_MOTOR`]. With less than [`COMMIT_MIN`] of hover
/// left above the unloading thrust the launch stops: no climb margin.
const COMMIT_STEP: f32 = 0.25;
const COMMIT_MIN: f32 = 0.05;
const COMMIT_RAMP_MS: u64 = 60;
/// No lift-off this long after the commit began, or a tilt past
/// [`COMMIT_TILT_DEG`], cuts the thrust and brackets again, once.
const COMMIT_WAIT_MS: u64 = 800;
const COMMIT_TILT_DEG: f32 = 3.0;
const COMMIT_RETRIES: u32 = 1;
/// The busiest motor's output rises by this share of its range per unit of
/// thrust until a probe has measured it.
const MOTOR_SLOPE: f32 = 1.0;
/// A tilt past this cuts the thrust to [`HARD_FRAC`] of hover.
pub const HARD_DEG: f32 = 2.5;
const HARD_FRAC: f32 = 0.3;
/// An integrator bound past this: the trim is beyond what the rate loop
/// can hold.
const TRIM_MAX: f32 = 0.45;
/// Bracketing without rate-loop data for this long stops the launch:
/// nothing tells where the integrator is.
const NO_RATE_MS: u64 = 1000;
/// A tilt on the skids past this from rest stops the launch. The airframe
/// must still be able to fall back onto its skids once the motors stop: a
/// 50 mm nose-heavy quad stopped at 10.9° rolled over.
pub const ABORT_DEG: f32 = 6.0;
/// A motor above this share of its output range for [`SATURATION_MS`]
/// before lift-off stops the launch: at ArduPilot's default spin range and
/// thrust curve that is 95 % of the motor's thrust, with nothing left to
/// climb and steer. A 40 mm offset on a 2.2 kg quad peaks near 0.90 on
/// its skids; a 50 mm one reaches the top of the range.
pub const SATURATION: f32 = 0.925;
const SATURATION_MS: u64 = 150;
/// Once handed over, the climb stops speeding up while the busiest motor
/// is above this share of its range, and slows instead.
const CLIMB_SATURATION: f32 = 0.9;
/// Off the ground: risen this far, or climbing this fast (m, m/s). With a
/// downward rangefinder the rise is the rangefinder's alone: the FC's
/// height estimate creeps while the airframe rocks on its skids finding its
/// trim (0.08 m in 2 s on the bench), and taking that for a lift-off hands
/// a grounded airframe to the position controller.
const LIFT_RISE_M: f32 = 0.08;
const LIFT_VZ_MPS: f32 = 0.25;
/// The rangefinder height is the median of its readings over this long, so
/// its noise (±0.07 m on the bench) is not a rise.
const AGL_WINDOW_MS: u64 = 500;
/// …of at most this many readings, the newest kept.
const AGL_WINDOW_MAX: usize = 64;
/// Clear of the ground: hand over to the position controller at this rise,
/// after this long, or once tilted this far.
const HANDOVER_RISE_M: f32 = 0.12;
const CLIMB_MAX_MS: u64 = 800;
const CLIMB_TILT_DEG: f32 = 15.0;
/// The position target climbs at up to this rate, slowing into the launch
/// height. At the hand-over it starts from the climb rate the airframe has
/// and speeds up at [`CLIMB_ACCEL_MPS2`], so the position controller is
/// never asked for a step.
const CLIMB_MPS: f32 = 0.5;
const CLIMB_ACCEL_MPS2: f32 = 0.25;
const CLIMB_DECEL_MPS2: f32 = 0.5;
/// The slowest climb a saturating motor eases the target back to.
const CLIMB_MIN_MPS: f32 = 0.15;
/// At the hand-over the horizontal target starts at the airframe's own
/// drift and eases it to rest at this deceleration, so the position
/// controller answers with a small lean rather than a jump.
const DRIFT_DECEL_MPS2: f32 = 0.5;
/// The ground phases give up after this long.
const GROUND_TIMEOUT_MS: u64 = 60_000;

/// The pilot's side of a supervised launch: the FC set up for it before
/// arming, then the lift-off.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Supervision {
    /// When the FC setup began, and when it was last sent.
    pub prep: Option<(u64, u64)>,
    pub liftoff: Option<Liftoff>,
    /// The airframe's imbalance in operator words, added to a stop's detail.
    pub detail: Option<String>,
    /// When the pilot saw the lift-off stop, and whether its disarm (or
    /// land) has been sent.
    pub stopped_ms: Option<u64>,
    pub disarm_sent: bool,
}

/// What the FC reports this tick.
#[derive(Debug, Clone, Copy, Default)]
pub struct Inputs {
    pub now_ms: u64,
    /// Fresh attitude, `None` while stale.
    pub att: Option<Attitude>,
    /// Fresh local position, `None` while stale.
    pub local: Option<LocalPos>,
    /// Fresh roll and pitch rate-loop I.
    pub i: Option<[f32; 2]>,
    /// The busiest motor's share of its output range and whether every motor
    /// spins steadily, `None` without fresh outputs.
    pub motors: Option<Motors>,
    /// A fresh downward rangefinder height, m.
    pub agl: Option<f32>,
}

/// The motor outputs, summarised.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Motors {
    /// The busiest motor's share of its range, 0..1.
    pub max: f32,
    /// The idlest motor's share of its range.
    pub min: f32,
    /// The spread between them, µs.
    pub spread_us: u16,
}

/// The frame to send.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Cmd {
    /// SET_ATTITUDE_TARGET (rad, thrust 0..1).
    Attitude {
        roll: f32,
        pitch: f32,
        yaw: f32,
        thrust: f32,
    },
    /// A position plus velocity target, local NED.
    Position {
        pos: [f32; 3],
        vel: [f32; 3],
        yaw: f32,
    },
}

/// Why a lift-off stopped on the ground.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// It tilted past [`ABORT_DEG`].
    Tipping,
    /// A motor stayed above [`SATURATION`].
    Saturated,
    /// The integrator wound past [`TRIM_MAX`] and a side still unloads.
    NoTrim,
    /// A side unloads and the FC sends no rate-loop data to bound the trim.
    NoRateData,
    /// The motors never spun steadily.
    Spool,
    /// The FC stopped reporting its attitude.
    FcLink,
    /// The operator's control link went silent.
    Link,
    /// The operator paused or stopped it.
    Operator,
    /// The ground phases ran past [`GROUND_TIMEOUT_MS`].
    Timeout,
}

impl Stop {
    /// The launch error code.
    pub fn code(self) -> &'static str {
        match self {
            Self::Tipping => "liftoff_tipping",
            Self::Saturated => "liftoff_saturated",
            Self::NoTrim => "liftoff_no_trim",
            Self::NoRateData => "liftoff_no_rate_data",
            Self::Spool => "liftoff_spool",
            Self::FcLink => "liftoff_fc_link",
            Self::Link => "liftoff_link",
            Self::Operator => "liftoff_operator",
            Self::Timeout => "liftoff_timeout",
        }
    }

    /// Why, in the operator's words: the detail under "Take-off stopped",
    /// the spec's `refusal.liftoff_<code>` after its "Take-off stopped. ".
    pub fn text(self) -> &'static str {
        match self {
            Self::Tipping => "The drone started to tip. Motors off.",
            Self::Saturated => "No climb margin. Lighten the drone or rebalance it.",
            Self::NoTrim => {
                "Could not find the drone's balance. Check the battery and payload are secure."
            }
            Self::NoRateData => {
                "The flight controller sends no rate data to balance the drone with. Motors off."
            }
            Self::Spool => "The motors did not spin up.",
            Self::FcLink => "The flight controller stopped reporting.",
            Self::Link => "The controller link was lost. Motors off.",
            Self::Operator => "Stopped by the operator. Motors off.",
            Self::Timeout => "Lift-off took too long. Motors off.",
        }
    }
}

/// The status line of a stopped take-off.
pub const STOPPED_TEXT: &str = "Take-off stopped";

/// A launch error the lift-off raised before it began, in the operator's
/// words.
pub fn error_text(code: &str) -> Option<&'static str> {
    match code {
        "liftoff_fc_params" => Some(
            "Take-off stopped. The flight controller did not take the steady take-off settings (GUID_OPTIONS, GCS_PID_MASK).",
        ),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Phase {
    Spool {
        since: u64,
        steady: Option<u64>,
    },
    Level {
        since: u64,
    },
    /// Thrust rising level until a side unloads (or it lifts).
    Find,
    /// The bracket: winding the integrators to their next values at the
    /// bracket thrust, level there, then a probe ramp until a side unloads.
    Wind {
        since: u64,
    },
    Settle {
        since: u64,
    },
    Probe,
    /// The thrust step off the skids, from `from` to `to`.
    Commit {
        since: u64,
        from: f32,
        to: f32,
    },
    Climb {
        since: u64,
    },
    /// The position target: where it is (`ne`, height `zt`), the drift it
    /// still carries (`vne`, m/s) and its climb rate (`vz`, m/s up).
    Hold {
        ne: [f32; 2],
        zt: f32,
        vne: [f32; 2],
        vz: f32,
    },
    Stopped(Stop),
}

/// What the bracket knows about the trim, per axis (roll, pitch).
#[derive(Debug, Clone, Copy, PartialEq)]
struct Bracket {
    /// Integrators a side unloaded at, too low (`lo`) or too high (`hi`):
    /// the trim lies between.
    lo: [Option<f32>; 2],
    hi: [Option<f32>; 2],
    /// The integrator each axis winds to next (`None`: left where it is).
    goal: [Option<f32>; 2],
    /// Each axis's lean cap (degrees), the side its last edge was on, and
    /// how many edges in a row it has had on that side.
    lean_deg: [f32; 2],
    side: [f32; 2],
    same: [u32; 2],
    /// The highest thrust a side unloaded at, and the thrust it winds at.
    t_u: f32,
    t_b: f32,
    /// The busiest motor's rise per unit of thrust, once a probe measured
    /// it, and where the running probe began (thrust, busiest motor).
    slope: Option<f32>,
    probe_from: Option<(f32, f32)>,
    /// A new edge counts once the tilt has settled since the last one.
    armed: bool,
    /// No edge counts before this time (the bounce after one).
    quiet_until: u64,
    /// Edges that turned an axis round, and commits that cut and bracketed
    /// again.
    reversals: u32,
    commits: u32,
}

impl Default for Bracket {
    fn default() -> Self {
        Self {
            lo: [None; 2],
            hi: [None; 2],
            goal: [None; 2],
            lean_deg: [EDGE_LEAN_DEG; 2],
            side: [0.0; 2],
            same: [0; 2],
            t_u: 0.0,
            t_b: 0.0,
            slope: None,
            probe_from: None,
            armed: true,
            quiet_until: 0,
            reversals: 0,
            commits: 0,
        }
    }
}

impl Bracket {
    /// The trim is known on `ax`: bounds on both sides, close enough.
    fn converged(&self, ax: usize) -> bool {
        matches!((self.lo[ax], self.hi[ax]), (Some(l), Some(h)) if h - l <= BRACKET_TOL)
    }

    /// Every axis that has unloaded is converged.
    fn done(&self) -> bool {
        let seen = |ax: usize| self.lo[ax].is_some() || self.hi[ax].is_some();
        (0..2).any(seen) && (0..2).all(|ax| !seen(ax) || self.converged(ax))
    }

    /// The integrator `ax` winds to next: the bounds' midpoint, else a step
    /// past the one bound. `None` past [`TRIM_MAX`].
    fn next(&self, ax: usize) -> Option<f32> {
        let halvings = self.same[ax].saturating_sub(2).min(8);
        let step = (BRACKET_STEP / (1u32 << halvings) as f32).max(BRACKET_STEP_MIN);
        let v = match (self.lo[ax], self.hi[ax]) {
            (Some(l), Some(h)) => 0.5 * (l + h),
            (Some(l), None) => l + step,
            (None, Some(h)) => h - step,
            (None, None) => return self.goal[ax],
        };
        let beyond = self.lo[ax].is_some_and(|l| l >= TRIM_MAX)
            || self.hi[ax].is_some_and(|h| h <= -TRIM_MAX);
        (!beyond).then(|| v.clamp(-TRIM_MAX, TRIM_MAX))
    }

    /// Hover for the bracket's margins: the estimate, at least the highest
    /// unloading thrust.
    fn hover_ref(&self, hover: f32) -> f32 {
        hover.max(self.t_u)
    }

    /// The thrust it winds at, below the highest unloading thrust.
    fn rebase(&mut self, hover: f32) {
        self.t_b = (self.t_u - BRACKET_MARGIN * self.hover_ref(hover)).max(HARD_FRAC * hover);
    }
}

/// One supervised lift-off.
#[derive(Debug, Clone, PartialEq)]
pub struct Liftoff {
    phase: Phase,
    /// The take-off point and height goal, local NED.
    start: Option<LocalPos>,
    alt_m: f32,
    hover: f32,
    /// The trim it started with came from earlier flights.
    learned: bool,
    /// A learned trim still loading on each axis, and the side it leans to.
    target: [f32; 2],
    loading: [bool; 2],
    lean: [f32; 2],
    /// The trim search, and how many edges it has seen.
    bracket: Bracket,
    edges: u32,
    /// Since when the bracket has run without rate-loop data.
    blind_since: Option<u64>,
    rest: Option<(f32, f32)>,
    yaw: f32,
    thrust: f32,
    tilt_deg: f32,
    began_ms: u64,
    last_ms: u64,
    /// The busiest motor seen on the ground, and since when it has been
    /// above [`SATURATION`].
    max_motor: f32,
    saturated_since: Option<u64>,
    /// The rangefinder height at rest, and whether it has left the ground.
    rest_agl: Option<f32>,
    lifted: bool,
    /// The rangefinder's readings over the last [`AGL_WINDOW_MS`].
    agl_window: VecDeque<(u64, f32)>,
    /// What it found at lift-off, taken once by the pilot.
    outcome: Option<LiftoffOutcome>,
}

impl Liftoff {
    /// A lift-off to `alt_m` above the take-off point, with the learned
    /// `trim` (`None`: it finds the trim on the ground) and the hover thrust
    /// `hover`.
    pub fn new(trim: Option<Trim>, hover: f32, alt_m: f32, now_ms: u64) -> Self {
        let target = trim.map_or([0.0; 2], |t| t.i);
        Self {
            phase: Phase::Spool {
                since: now_ms,
                steady: None,
            },
            start: None,
            alt_m,
            hover: hover.clamp(0.15, 0.8),
            learned: trim.is_some(),
            target,
            loading: target.map(|v| v.abs() > 0.01),
            // The integrators start from zero after arming.
            lean: target.map(f32::signum),
            bracket: Bracket::default(),
            edges: 0,
            blind_since: None,
            rest: None,
            yaw: 0.0,
            thrust: SPOOL_THRUST,
            tilt_deg: 0.0,
            began_ms: now_ms,
            last_ms: now_ms,
            max_motor: 0.0,
            saturated_since: None,
            rest_agl: None,
            lifted: false,
            agl_window: VecDeque::new(),
            outcome: None,
        }
    }

    /// The take-off point, local NED.
    pub fn start(&self) -> Option<LocalPos> {
        self.start
    }

    /// The rangefinder height now: the median of its readings over the last
    /// [`AGL_WINDOW_MS`] (at most [`AGL_WINDOW_MAX`]), `None` without any.
    fn agl_now(&self) -> Option<f32> {
        let mut buf = [0.0f32; AGL_WINDOW_MAX];
        let n = self.agl_window.len().min(AGL_WINDOW_MAX);
        if n == 0 {
            return None;
        }
        for (slot, &(_, a)) in buf.iter_mut().zip(self.agl_window.iter().rev()) {
            *slot = a;
        }
        let v = &mut buf[..n];
        v.sort_unstable_by(f32::total_cmp);
        Some(v[n / 2])
    }

    /// The rangefinder height at rest, once spooled.
    pub fn rest_agl(&self) -> Option<f32> {
        self.rest_agl
    }

    /// It has left the ground at some point.
    pub fn lifted_ever(&self) -> bool {
        self.lifted
    }

    /// Still on the ground: nothing has lifted yet.
    pub fn on_ground(&self) -> bool {
        matches!(
            self.phase,
            Phase::Spool { .. }
                | Phase::Level { .. }
                | Phase::Find
                | Phase::Wind { .. }
                | Phase::Settle { .. }
                | Phase::Probe
                | Phase::Commit { .. }
        )
    }

    /// Why it stopped, once it has.
    pub fn stopped(&self) -> Option<Stop> {
        match self.phase {
            Phase::Stopped(s) => Some(s),
            _ => None,
        }
    }

    /// Handed over to the position controller.
    pub fn handed_over(&self) -> bool {
        matches!(self.phase, Phase::Hold { .. })
    }

    /// The FC's estimate stepped by `d` (north, east, down; an EKF position
    /// reset) while the airframe stayed put: the take-off point and the
    /// position target move with it.
    pub fn rebase(&mut self, d: [f32; 3]) {
        if let Some(s) = self.start.as_mut() {
            s.n += d[0];
            s.e += d[1];
            s.d += d[2];
        }
        if let Phase::Hold { ne, zt, .. } = &mut self.phase {
            *ne = [ne[0] + d[0], ne[1] + d[1]];
            *zt += d[2];
        }
    }

    /// What it found at lift-off or at a saturation stop, once.
    pub fn take_outcome(&mut self) -> Option<LiftoffOutcome> {
        self.outcome.take()
    }

    /// Stop on the ground (the operator, a lost link, the FC gone quiet).
    /// Does nothing once off the ground.
    pub fn stop(&mut self, why: Stop) {
        if self.on_ground() {
            self.phase = Phase::Stopped(why);
        }
    }

    /// One tick: the frame to send, `None` once stopped.
    pub fn step(&mut self, inp: &Inputs) -> Option<Cmd> {
        let now = inp.now_ms;
        let dt = (now.saturating_sub(self.last_ms) as f32 / 1000.0).clamp(0.0, 0.1);
        self.last_ms = now;
        if let Phase::Stopped(_) = self.phase {
            return None;
        }
        let Some(att) = inp.att else {
            self.stop(Stop::FcLink);
            return self.hold_or_none();
        };
        if self.start.is_none() {
            self.start = inp.local;
            self.yaw = att.yaw;
        }
        let (r0, p0) = self.rest.unwrap_or((att.roll, att.pitch));
        let tilt = [(att.roll - r0).to_degrees(), (att.pitch - p0).to_degrees()];
        self.tilt_deg = tilt[0].abs().max(tilt[1].abs());
        if let Some(a) = inp.agl {
            if self.agl_window.len() == AGL_WINDOW_MAX {
                self.agl_window.pop_front();
            }
            self.agl_window.push_back((now, a));
        }
        while self
            .agl_window
            .front()
            .is_some_and(|&(t, _)| now.saturating_sub(t) > AGL_WINDOW_MS)
        {
            self.agl_window.pop_front();
        }
        let ekf_rise = match (self.start, inp.local) {
            (Some(s), Some(l)) => s.d - l.d,
            _ => 0.0,
        };
        // The rangefinder decides once it has a rest height to rise from: its
        // median, or, for a fast lift the median would see late, its newest
        // reading risen together with the FC's climb rate (neither the
        // estimate's creep nor the rangefinder's noise alone is enough).
        let agl_rise = self.rest_agl.zip(self.agl_now()).map(|(r0, a)| a - r0);
        let rise = agl_rise.unwrap_or(ekf_rise);
        let climbing = inp.local.is_some_and(|l| l.vd < -LIFT_VZ_MPS);
        let lifted = match agl_rise {
            Some(r) => {
                r > LIFT_RISE_M
                    || (climbing
                        && inp
                            .agl
                            .zip(self.rest_agl)
                            .is_some_and(|(a, r0)| a - r0 > LIFT_RISE_M))
            }
            None => ekf_rise > LIFT_RISE_M || climbing,
        };
        if self.on_ground() {
            if let Some(m) = inp.motors {
                self.max_motor = self.max_motor.max(m.max);
            }
            let saturating = self.rest.is_some() && inp.motors.is_some_and(|m| m.max > SATURATION);
            self.saturated_since = saturating.then(|| self.saturated_since.unwrap_or(now));
            if let Some(stop) = self.ground_stop(lifted, now) {
                self.phase = Phase::Stopped(stop);
                if stop == Stop::Saturated {
                    self.outcome = Some(self.outcome_now(inp, true));
                }
                return None;
            }
        }
        let hover = self.hover;
        let level_yaw = self.yaw;
        let att_cmd = |roll, pitch, thrust| Cmd::Attitude {
            roll,
            pitch,
            yaw: level_yaw,
            thrust,
        };
        match self.phase {
            Phase::Spool { since, steady } => {
                let spinning = inp
                    .motors
                    .map(|m| m.min > SPOOL_MIN_FRAC && m.spread_us <= SPOOL_SPREAD_US);
                let done = match spinning {
                    Some(true) => {
                        let s = steady.unwrap_or(now);
                        self.phase = Phase::Spool {
                            since,
                            steady: Some(s),
                        };
                        now.saturating_sub(s) >= SPOOL_STEADY_MS
                    }
                    Some(false) => {
                        self.phase = Phase::Spool {
                            since,
                            steady: None,
                        };
                        false
                    }
                    None => now.saturating_sub(since) >= SPOOL_BLIND_MS,
                };
                if done {
                    self.rest = Some((att.roll, att.pitch));
                    self.rest_agl = self.agl_now();
                    self.phase = if self.loading.iter().any(|&l| l) {
                        Phase::Level { since: now }
                    } else {
                        Phase::Find
                    };
                } else if now.saturating_sub(since) > SPOOL_TIMEOUT_MS {
                    self.phase = Phase::Stopped(Stop::Spool);
                    return None;
                }
                Some(att_cmd(r0, p0, SPOOL_THRUST))
            }
            Phase::Level { since } => {
                self.thrust = (self.thrust + dt * hover).min(LOAD_FRAC * hover);
                if let Some(i) = inp.i {
                    for (ax, &iv) in i.iter().enumerate() {
                        // Loaded once the integrator reached its target.
                        if self.loading[ax] && (iv - self.target[ax]) * self.lean[ax] >= 0.0 {
                            self.loading[ax] = false;
                        }
                    }
                } else {
                    self.loading = [false; 2];
                }
                if !self.loading.iter().any(|&l| l) || now.saturating_sub(since) > LOAD_TIMEOUT_MS {
                    self.loading = [false; 2];
                    self.phase = Phase::Find;
                }
                let lean = LOAD_LEAN_DEG.to_radians();
                let roll = r0
                    + if self.loading[0] {
                        self.lean[0] * lean
                    } else {
                        0.0
                    };
                let pitch = p0
                    + if self.loading[1] {
                        self.lean[1] * lean
                    } else {
                        0.0
                    };
                Some(att_cmd(roll, pitch, self.thrust))
            }
            Phase::Find => {
                if lifted {
                    return self.lift_off(inp, rise, dt, now);
                }
                if self.edge_seen(tilt, now) {
                    if Self::rising(inp) {
                        return self.rising_commit(inp, now);
                    }
                    return self.on_edge(inp, tilt, now);
                }
                let fast = K_FAST * hover;
                let rate = if self.thrust < fast {
                    fast / FAST_S
                } else {
                    EDGE_RATE * hover
                };
                self.thrust = (self.thrust + dt * rate).min(THRUST_MAX);
                Some(att_cmd(r0, p0, self.thrust))
            }
            Phase::Wind { since } => {
                if self.edge_seen(tilt, now) {
                    return self.on_edge(inp, tilt, now);
                }
                if let Some(stop) = self.blind(inp, now) {
                    self.phase = Phase::Stopped(stop);
                    return None;
                }
                self.toward_bracket_thrust(dt);
                let mut lean = [0.0f32; 2];
                let mut wound = true;
                if let Some(i) = inp.i {
                    for (ax, l) in lean.iter_mut().enumerate() {
                        if let Some(g) = self.bracket.goal[ax] {
                            let di = g - i[ax];
                            if di.abs() > WIND_TOL {
                                wound = false;
                                let cap = self.bracket.lean_deg[ax].to_radians();
                                *l = (di / WIND_K).clamp(-cap, cap);
                            }
                        }
                    }
                } else {
                    wound = false;
                }
                if wound || now.saturating_sub(since) > WIND_TIMEOUT_MS {
                    self.phase = Phase::Settle { since: now };
                    lean = [0.0; 2];
                }
                Some(att_cmd(r0 + lean[0], p0 + lean[1], self.thrust))
            }
            Phase::Settle { since } => {
                if self.edge_seen(tilt, now) {
                    return self.on_edge(inp, tilt, now);
                }
                if let Some(stop) = self.blind(inp, now) {
                    self.phase = Phase::Stopped(stop);
                    return None;
                }
                self.toward_bracket_thrust(dt);
                let settled =
                    self.tilt_deg < SETTLED_FRAC * EDGE_DEG && now >= self.bracket.quiet_until;
                if now.saturating_sub(since) >= SETTLE_MS && settled {
                    if self.bracket.done() {
                        return self.start_commit(inp, now);
                    }
                    self.bracket.probe_from = inp.motors.map(|m| (self.thrust, m.max));
                    self.phase = Phase::Probe;
                }
                Some(att_cmd(r0, p0, self.thrust))
            }
            Phase::Probe => {
                if lifted {
                    // Light enough to leave the ground before a side
                    // unloaded: the trim is close, and the climb-out takes
                    // the commit's thrust.
                    let to = self.commit_thrust(inp);
                    if self.no_climb_margin(inp, to) {
                        return None;
                    }
                    self.thrust = self.thrust.max(to);
                    return self.lift_off(inp, rise, dt, now);
                }
                if self.edge_seen(tilt, now) {
                    if Self::rising(inp) {
                        return self.rising_commit(inp, now);
                    }
                    return self.on_edge(inp, tilt, now);
                }
                if let Some(stop) = self.blind(inp, now) {
                    self.phase = Phase::Stopped(stop);
                    return None;
                }
                self.thrust = (self.thrust + dt * EDGE_RATE * hover).min(THRUST_MAX);
                Some(att_cmd(r0, p0, self.thrust))
            }
            Phase::Commit { since, from, to } => {
                if lifted {
                    return self.lift_off(inp, rise, dt, now);
                }
                let k = (now.saturating_sub(since) as f32 / COMMIT_RAMP_MS as f32).min(1.0);
                self.thrust = from + (to - from) * k;
                if self.tilt_deg > COMMIT_TILT_DEG || now.saturating_sub(since) > COMMIT_WAIT_MS {
                    return self.commit_failed(inp, tilt, now);
                }
                Some(att_cmd(r0, p0, self.thrust))
            }
            Phase::Climb { .. } | Phase::Hold { .. } => self.step_climb(inp, rise, dt),
            Phase::Stopped(_) => None,
        }
    }

    /// The tilt, saturation and timeout guards on the ground. The commit
    /// checked its motor margin before it began and runs its own limit
    /// ([`COMMIT_WAIT_MS`]): a motor near the top while it leaves the
    /// ground does not stop it.
    fn ground_stop(&self, lifted: bool, now: u64) -> Option<Stop> {
        if lifted {
            return None;
        }
        if self.rest.is_some() && self.tilt_deg > ABORT_DEG {
            return Some(Stop::Tipping);
        }
        let committing = matches!(self.phase, Phase::Commit { .. });
        if !committing
            && self
                .saturated_since
                .is_some_and(|s| now.saturating_sub(s) >= SATURATION_MS)
        {
            return Some(Stop::Saturated);
        }
        (now.saturating_sub(self.began_ms) > GROUND_TIMEOUT_MS).then_some(Stop::Timeout)
    }

    /// The level attitude target at `thrust`.
    fn level(&self, thrust: f32) -> Cmd {
        let (roll, pitch) = self.rest.unwrap_or((0.0, 0.0));
        Cmd::Attitude {
            roll,
            pitch,
            yaw: self.yaw,
            thrust,
        }
    }

    /// Off the skids: the outcome is taken and the climb begins.
    fn lift_off(&mut self, inp: &Inputs, rise: f32, dt: f32, now: u64) -> Option<Cmd> {
        self.lifted = true;
        self.outcome = Some(self.outcome_now(inp, false));
        self.phase = Phase::Climb { since: now };
        self.step_climb(inp, rise, dt)
    }

    /// A side has unloaded: a new edge once the tilt settled since the last
    /// and the bounce after it has passed.
    fn edge_seen(&mut self, tilt: [f32; 2], now: u64) -> bool {
        if now < self.bracket.quiet_until {
            return false;
        }
        if !self.bracket.armed {
            self.bracket.armed = self.tilt_deg < SETTLED_FRAC * EDGE_DEG;
            return false;
        }
        tilt.iter().any(|t| t.abs() > EDGE_DEG)
    }

    /// The FC's climb rate says the airframe is rising.
    fn rising(inp: &Inputs) -> bool {
        inp.local.is_some_and(|l| l.vd < -RISING_VZ_MPS)
    }

    /// A side unloads as the airframe starts to rise: the trim is close and
    /// the lift-off has begun, so it commits rather than dropping back.
    fn rising_commit(&mut self, inp: &Inputs, now: u64) -> Option<Cmd> {
        self.bracket.t_u = self.bracket.t_u.max(self.thrust);
        self.start_commit(inp, now)
    }

    /// Without rate-loop data for [`NO_RATE_MS`] the bracket stops.
    fn blind(&mut self, inp: &Inputs, now: u64) -> Option<Stop> {
        if inp.i.is_some() {
            self.blind_since = None;
            return None;
        }
        let since = *self.blind_since.get_or_insert(now);
        (now.saturating_sub(since) >= NO_RATE_MS).then_some(Stop::NoRateData)
    }

    /// The thrust back to the bracket thrust (cut past [`HARD_DEG`]).
    fn toward_bracket_thrust(&mut self, dt: f32) {
        let hover = self.hover;
        let t_b = self.bracket.t_b;
        if self.tilt_deg > HARD_DEG {
            self.thrust = self.thrust.min(HARD_FRAC * hover);
        } else if self.thrust > t_b {
            self.thrust = t_b;
        } else {
            self.thrust = (self.thrust + dt * K_FAST * hover / FAST_S).min(t_b);
        }
    }

    /// A side unloaded at the integrators `i`: the trim lies below an axis
    /// tilted up (positive) and above one tilted down. Sets each bounded
    /// axis's next integrator; `NoTrim` once a bound is past [`TRIM_MAX`].
    fn bound(&mut self, i: [f32; 2], tilt: [f32; 2]) -> Option<Stop> {
        let b = &mut self.bracket;
        for ax in 0..2 {
            let t = tilt[ax];
            if t.abs() <= EDGE_DEG {
                continue;
            }
            let (s, iv) = (t.signum(), i[ax]);
            if s > 0.0 {
                b.hi[ax] = Some(b.hi[ax].map_or(iv, |h| h.min(iv)));
                if b.lo[ax].is_some_and(|l| l >= iv) {
                    b.lo[ax] = None;
                }
            } else {
                b.lo[ax] = Some(b.lo[ax].map_or(iv, |l| l.max(iv)));
                if b.hi[ax].is_some_and(|h| h <= iv) {
                    b.hi[ax] = None;
                }
            }
            if b.side[ax] != 0.0 && b.side[ax] != s {
                b.lean_deg[ax] = (0.5 * b.lean_deg[ax]).max(LEAN_MIN_DEG);
                b.reversals += 1;
                b.same[ax] = 0;
            }
            b.side[ax] = s;
            b.same[ax] += 1;
        }
        if b.reversals > MAX_REVERSALS {
            return Some(Stop::NoTrim);
        }
        for ax in 0..2 {
            if b.lo[ax].is_some() || b.hi[ax].is_some() {
                match b.next(ax) {
                    Some(g) => b.goal[ax] = Some(g),
                    None => return Some(Stop::NoTrim),
                }
            }
        }
        None
    }

    /// An edge: the thrust drops back to the bracket thrust, below the
    /// highest unloading thrust, and the integrators wind to their next
    /// values.
    fn on_edge(&mut self, inp: &Inputs, tilt: [f32; 2], now: u64) -> Option<Cmd> {
        let hover = self.hover;
        self.bracket.armed = false;
        self.bracket.quiet_until = now + EDGE_QUIET_MS;
        self.edges += 1;
        if self.edges > MAX_EDGES {
            self.phase = Phase::Stopped(Stop::NoTrim);
            return None;
        }
        let b = &mut self.bracket;
        if let (Some((t0, m0)), Some(m)) = (b.probe_from.take(), inp.motors) {
            if self.thrust - t0 > 0.02 {
                b.slope = Some(((m.max - m0) / (self.thrust - t0)).max(0.2));
            }
        }
        b.t_u = b.t_u.max(self.thrust);
        b.rebase(hover);
        // Without rate-loop data nothing says where the integrator is: wind
        // nothing and stop if it stays missing.
        if let Some(i) = inp.i {
            if let Some(stop) = self.bound(i, tilt) {
                self.phase = Phase::Stopped(stop);
                return None;
            }
        }
        self.thrust = if self.tilt_deg > HARD_DEG {
            HARD_FRAC * hover
        } else {
            self.bracket.t_b
        };
        self.phase = Phase::Wind { since: now };
        Some(self.level(self.thrust))
    }

    /// The commit's thrust: [`COMMIT_STEP`] of hover above the highest
    /// unloading thrust, less where the busiest motor would pass
    /// [`COMMIT_MOTOR`].
    fn commit_thrust(&self, inp: &Inputs) -> f32 {
        let b = &self.bracket;
        let slope = b.slope.unwrap_or(MOTOR_SLOPE);
        let step_to = b.t_u + COMMIT_STEP * b.hover_ref(self.hover);
        let headroom = inp
            .motors
            .map(|m| self.thrust + (COMMIT_MOTOR - m.max) / slope);
        headroom.map_or(step_to, |h| h.min(step_to)).min(THRUST_MAX)
    }

    /// Less than [`COMMIT_MIN`] of hover between the highest unloading
    /// thrust and the commit's thrust `to`: the launch stops, saturated (no
    /// climb margin), with the busiest motor at the least thrust that would
    /// have lifted.
    fn no_climb_margin(&mut self, inp: &Inputs, to: f32) -> bool {
        let least = self.bracket.t_u + COMMIT_MIN * self.bracket.hover_ref(self.hover);
        if to >= least {
            return false;
        }
        if let Some(m) = inp.motors {
            let slope = self.bracket.slope.unwrap_or(MOTOR_SLOPE);
            self.max_motor = self.max_motor.max(m.max + slope * (least - self.thrust));
        }
        self.phase = Phase::Stopped(Stop::Saturated);
        self.outcome = Some(self.outcome_now(inp, true));
        true
    }

    /// The commit: a thrust step off the skids, as far above the highest
    /// unloading thrust as the motors allow.
    fn start_commit(&mut self, inp: &Inputs, now: u64) -> Option<Cmd> {
        let to = self.commit_thrust(inp);
        if self.no_climb_margin(inp, to) {
            return None;
        }
        self.phase = Phase::Commit {
            since: now,
            from: self.thrust,
            to,
        };
        Some(self.level(self.thrust))
    }

    /// A commit that tipped or did not lift: the thrust is cut and the
    /// bracket runs again (a tilt bounds the trim; no lift asks for more
    /// thrust), once. A motor held at the top through it is no climb margin.
    fn commit_failed(&mut self, inp: &Inputs, tilt: [f32; 2], now: u64) -> Option<Cmd> {
        let hover = self.hover;
        self.thrust = HARD_FRAC * hover;
        if self
            .saturated_since
            .is_some_and(|s| now.saturating_sub(s) >= SATURATION_MS)
        {
            self.phase = Phase::Stopped(Stop::Saturated);
            self.outcome = Some(self.outcome_now(inp, true));
            return None;
        }
        if self.bracket.commits >= COMMIT_RETRIES {
            self.phase = Phase::Stopped(Stop::NoTrim);
            return None;
        }
        self.bracket.commits += 1;
        self.bracket.armed = false;
        self.bracket.quiet_until = now + EDGE_QUIET_MS;
        let tipped = tilt.iter().any(|t| t.abs() > EDGE_DEG);
        match inp.i {
            Some(i) if tipped => {
                if let Some(stop) = self.bound(i, tilt) {
                    self.phase = Phase::Stopped(stop);
                    return None;
                }
            }
            _ => self.bracket.t_u += BRACKET_MARGIN * self.bracket.hover_ref(hover),
        }
        let b = &mut self.bracket;
        b.rebase(hover);
        self.phase = Phase::Wind { since: now };
        Some(self.level(self.thrust))
    }

    /// Off the skids: level, then the position target.
    fn step_climb(&mut self, inp: &Inputs, rise: f32, dt: f32) -> Option<Cmd> {
        if let Phase::Climb { since } = self.phase {
            let clear = rise > HANDOVER_RISE_M
                || inp.now_ms.saturating_sub(since) > CLIMB_MAX_MS
                || self.tilt_deg > CLIMB_TILT_DEG;
            match (clear, inp.local) {
                (true, Some(l)) => {
                    // Start the position target where the airframe is and
                    // moving as it moves, so the controller has no step.
                    self.phase = Phase::Hold {
                        ne: [l.n, l.e],
                        zt: l.d,
                        vne: [l.vn, l.ve],
                        vz: (-l.vd).clamp(0.0, CLIMB_MPS),
                    };
                }
                _ => {
                    // The lift-off thrust holds until the hand-over.
                    return Some(Cmd::Attitude {
                        roll: 0.0,
                        pitch: 0.0,
                        yaw: self.yaw,
                        thrust: self.thrust,
                    });
                }
            }
        }
        self.hold_or_none()
            .map(|c| self.climb_target(c, inp.motors, dt))
    }

    /// The position target once handed over: its drift eases to rest at
    /// [`DRIFT_DECEL_MPS2`]; its climb speeds up from the hand-over's climb
    /// rate to [`CLIMB_MPS`] (or slows, while a motor is near the top of its
    /// range), slowing into the launch height.
    fn climb_target(&mut self, c: Cmd, motors: Option<Motors>, dt: f32) -> Cmd {
        let Phase::Hold { ne, zt, vne, vz } = self.phase else {
            return c;
        };
        let vz = if motors.is_some_and(|m| m.max > CLIMB_SATURATION) {
            (vz - dt * CLIMB_ACCEL_MPS2).max(CLIMB_MIN_MPS.min(vz))
        } else {
            (vz + dt * CLIMB_ACCEL_MPS2).min(CLIMB_MPS)
        };
        let goal = self.start.map_or(0.0, |s| s.d) - self.alt_m;
        let left = (zt - goal).max(0.0);
        let v = vz.min((2.0 * CLIMB_DECEL_MPS2 * left).sqrt());
        let zt = (zt - v * dt).max(goal);
        let speed = vne[0].hypot(vne[1]);
        let vne = if speed > 0.0 {
            let k = (speed - dt * DRIFT_DECEL_MPS2).max(0.0) / speed;
            [vne[0] * k, vne[1] * k]
        } else {
            [0.0; 2]
        };
        let ne = [ne[0] + vne[0] * dt, ne[1] + vne[1] * dt];
        self.phase = Phase::Hold { ne, zt, vne, vz };
        Cmd::Position {
            pos: [ne[0], ne[1], zt],
            vel: [vne[0], vne[1], if zt > goal { -v } else { 0.0 }],
            yaw: self.yaw,
        }
    }

    fn hold_or_none(&self) -> Option<Cmd> {
        match self.phase {
            Phase::Hold { ne, zt, .. } => Some(Cmd::Position {
                pos: [ne[0], ne[1], zt],
                vel: [0.0; 3],
                yaw: self.yaw,
            }),
            _ => None,
        }
    }

    fn outcome_now(&self, inp: &Inputs, saturated: bool) -> LiftoffOutcome {
        LiftoffOutcome {
            i: if saturated { None } else { inp.i },
            thrust: self.thrust,
            max_motor: self.max_motor,
            saturated,
        }
    }

    /// The surfaces' view: the status line and the longer detail.
    pub fn view(&self) -> ados_protocol::pilot::Liftoff {
        let (phase, text, detail) = match self.phase {
            Phase::Spool { .. } => (
                "spool",
                "Spinning up",
                "Motors at idle, checking they're steady".to_string(),
            ),
            Phase::Level { .. } => (
                "level",
                "Setting trim",
                "Loading this airframe's learned balance".to_string(),
            ),
            Phase::Find => ("find", "Lifting off", String::new()),
            Phase::Wind { .. } | Phase::Probe => {
                ("bracket", "Checking balance", "Adjusting trim".to_string())
            }
            Phase::Settle { .. } => (
                "settle",
                "Checking balance",
                "Holding the balance it found".to_string(),
            ),
            Phase::Commit { .. } => ("commit", "Lifting off", String::new()),
            Phase::Climb { .. } => ("climb", "Airborne", self.climbing_to()),
            Phase::Hold { .. } => ("done", "Airborne", self.climbing_to()),
            Phase::Stopped(s) => ("stopped", STOPPED_TEXT, s.text().to_string()),
        };
        ados_protocol::pilot::Liftoff {
            phase: phase.into(),
            text: text.into(),
            detail,
            trim: if self.learned { "learned" } else { "probe" }.into(),
            tilt_deg: f64::from((self.tilt_deg * 10.0).round() / 10.0),
        }
    }

    fn climbing_to(&self) -> String {
        let h = (self.alt_m * 10.0).round() / 10.0;
        if h.fract() == 0.0 {
            format!("Climbing to {h:.0} m")
        } else {
            format!("Climbing to {h:.1} m")
        }
    }
}
