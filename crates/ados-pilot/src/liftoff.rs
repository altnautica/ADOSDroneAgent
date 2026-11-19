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
//! 3. **Ramp**: thrust fast to most of hover, then slowly on until lift-off,
//!    rising only while the airframe sits level on its skids. A side that
//!    starts to unload (the trim not known yet, or wrong) holds the thrust
//!    and leans the attitude target toward the low side, so the rate-loop
//!    integrator winds up against skid contact; the lean turns round when
//!    the other side unloads while the thrust climbs (never in the bounce
//!    after a cut, see [`FLIP_QUIET_MS`]). The airframe never pivots: a
//!    tilt past [`BACK_DEG`] steps the thrust back, past [`HARD_DEG`] cuts
//!    it to [`HARD_FRAC`] of hover. A tilt past [`ABORT_DEG`], or a motor
//!    held at the top of its range, stops the launch on the ground.
//! 4. **Climb**: level, clear of the ground, then a position target that
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
/// The fast ramp: to this share of hover in [`FAST_S`]. Below it no
/// airframe the balance check lets fly unloads a skid.
const K_FAST: f32 = 0.7;
const FAST_S: f32 = 0.5;
/// Then the thrust rises by this share of hover per second while the
/// airframe sits level, until it lifts, up to [`THRUST_MAX`]. The hover
/// estimate only paces the ramp: an untuned `MOT_THST_HOVER` can be well
/// short of the airframe's, and the ramp must not park it on its skids.
const EDGE_RATE: f32 = 0.15;
const THRUST_MAX: f32 = 0.9;
/// A side has started to unload: tilted this far from rest on its axis.
pub const EDGE_DEG: f32 = 0.5;
/// The attitude target's lean toward the low side while it does, degrees.
const EDGE_LEAN_DEG: f32 = 6.0;
/// Above this share of hover the lean only acts while a side is unloading,
/// so the airframe never leaves the ground leaning.
const LEAN_OFF: f32 = 0.97;
/// A tilt past this steps the thrust back by [`BACK_STEP`] of hover, once
/// per excursion…
pub const BACK_DEG: f32 = 1.0;
const BACK_STEP: f32 = 0.1;
/// …and past this cuts it to [`HARD_FRAC`] of hover.
pub const HARD_DEG: f32 = 2.5;
const HARD_FRAC: f32 = 0.3;
/// After a step back or a cut the airframe settles back onto its skids and
/// overshoots the other way: that bounce is not the other side unloading.
/// For this long after one, and while the thrust is under [`K_FAST`] of
/// hover, a lean never turns round, so the integrator keeps the trim it has
/// wound (turning round there unwound it and the ramp cycled on the skids).
const FLIP_QUIET_MS: u64 = 1000;
/// An integrator wound past this while a side still unloads: the trim is
/// beyond what the rate loop can hold.
const TRIM_MAX: f32 = 0.45;
/// Leaning without rate-loop data for this long stops the launch: nothing
/// bounds the integrator.
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
    Ramp,
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
    /// The side each axis leans to while a side unloads on the ramp (−1, 0,
    /// 1), how often a lean began or turned round, and since when the lean
    /// runs without rate-loop data.
    edge: [f32; 2],
    edges: u32,
    blind_since: Option<u64>,
    /// The thrust already stepped back for this excursion, and until when
    /// a lean may not turn round.
    backed: bool,
    flip_quiet_until: u64,
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
            edge: [0.0; 2],
            edges: 0,
            blind_since: None,
            backed: false,
            flip_quiet_until: 0,
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
            Phase::Spool { .. } | Phase::Level { .. } | Phase::Ramp
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
                        Phase::Ramp
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
                    self.phase = Phase::Ramp;
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
            Phase::Ramp => {
                if lifted {
                    self.lifted = true;
                    self.edge = [0.0; 2];
                    self.outcome = Some(self.outcome_now(inp, false));
                    self.phase = Phase::Climb { since: now };
                    return self.step_climb(inp, rise, dt);
                }
                if let Some(stop) = self.edge_track(inp, tilt, now) {
                    self.phase = Phase::Stopped(stop);
                    return None;
                }
                self.ramp_thrust(dt, now);
                let lean = EDGE_LEAN_DEG.to_radians();
                Some(att_cmd(
                    r0 + self.edge[0] * lean,
                    p0 + self.edge[1] * lean,
                    self.thrust,
                ))
            }
            Phase::Climb { .. } | Phase::Hold { .. } => self.step_climb(inp, rise, dt),
            Phase::Stopped(_) => None,
        }
    }

    /// The tilt, saturation and timeout guards on the ground.
    fn ground_stop(&self, lifted: bool, now: u64) -> Option<Stop> {
        if lifted {
            return None;
        }
        if self.rest.is_some() && self.tilt_deg > ABORT_DEG {
            return Some(Stop::Tipping);
        }
        if self
            .saturated_since
            .is_some_and(|s| now.saturating_sub(s) >= SATURATION_MS)
        {
            return Some(Stop::Saturated);
        }
        (now.saturating_sub(self.began_ms) > GROUND_TIMEOUT_MS).then_some(Stop::Timeout)
    }

    /// A side unloading on the ramp: lean toward it so the integrator winds
    /// up against skid contact, turning round when the other side unloads
    /// (outside the bounce after a cut, [`FLIP_QUIET_MS`]). Near lift-off
    /// thrust the lean only acts while a side unloads.
    fn edge_track(&mut self, inp: &Inputs, tilt: [f32; 2], now: u64) -> Option<Stop> {
        let quiet = now < self.flip_quiet_until || self.thrust < K_FAST * self.hover;
        for (ax, &t) in tilt.iter().enumerate() {
            if t.abs() > EDGE_DEG {
                let want = -t.signum();
                if quiet && self.edge[ax] == -want {
                    continue;
                }
                if self.edge[ax] != want {
                    self.edges += 1;
                }
                self.edge[ax] = want;
            } else if self.thrust > LEAN_OFF * self.hover {
                self.edge[ax] = 0.0;
            }
        }
        let leaning = self.edge.iter().any(|&e| e != 0.0);
        match inp.i {
            Some(i) => {
                self.blind_since = None;
                // Winding on past what the rate loop can hold.
                if (0..2).any(|ax| self.edge[ax] != 0.0 && i[ax] * self.edge[ax] > TRIM_MAX) {
                    return Some(Stop::NoTrim);
                }
            }
            None if leaning => {
                let since = *self.blind_since.get_or_insert(now);
                if now.saturating_sub(since) >= NO_RATE_MS {
                    return Some(Stop::NoRateData);
                }
            }
            None => self.blind_since = None,
        }
        None
    }

    /// The ramp's thrust: rising only while the airframe sits level, held
    /// while a side unloads, stepped back past [`BACK_DEG`], cut past
    /// [`HARD_DEG`]. A tilt past [`BACK_DEG`] keeps the [`FLIP_QUIET_MS`]
    /// window open until that long after it settles.
    fn ramp_thrust(&mut self, dt: f32, now: u64) {
        let hover = self.hover;
        let fast = K_FAST * hover;
        if self.tilt_deg > BACK_DEG {
            self.flip_quiet_until = now + FLIP_QUIET_MS;
        }
        if self.tilt_deg > HARD_DEG {
            self.thrust = self.thrust.min(HARD_FRAC * hover);
        } else if self.tilt_deg > BACK_DEG {
            if !self.backed {
                self.thrust = (self.thrust - BACK_STEP * hover).max(HARD_FRAC * hover);
                self.backed = true;
            }
        } else if self.tilt_deg <= EDGE_DEG {
            self.backed = false;
            let rate = if self.thrust < fast {
                fast / FAST_S
            } else {
                EDGE_RATE * hover
            };
            self.thrust = (self.thrust + dt * rate).min(THRUST_MAX);
        }
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
        let balancing = self.edge.iter().any(|&e| e != 0.0);
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
            Phase::Ramp if balancing => ("probe", "Checking balance", "Adjusting trim".to_string()),
            Phase::Ramp => ("lift", "Lifting off", String::new()),
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
