//! The supervised lift-off: an ArduPilot multirotor launch lifts off level
//! even when the airframe is not balanced.
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
//! 2. **Level**: at half hover thrust, lean the attitude target toward the
//!    airframe's learned trim until the rate integrators hold it.
//! 3. **Ramp**: thrust fast to most of hover, then slowly on until lift-off.
//!    A tip that begins on the skids (a trim not known, or wrong) backs the
//!    thrust down, settles and loads a better trim ([`probe`]); a pivot past
//!    the limit, or a motor at the top of its range, stops the launch.
//! 4. **Climb**: level, clear of the ground, then a position target at
//!    the lift-off point climbing to the launch height.
//!
//! This module is the state machine only: [`Liftoff::step`] takes what the
//! FC reports and returns the frame to send. The pilot owns the I/O and the
//! operator's stops (`core/actions/liftoff.rs`).

pub mod probe;
#[cfg(test)]
pub mod sim;
#[cfg(test)]
mod tests;

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
pub const GUID_OPTIONS_THRUST: u32 = 1 << 3;
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
/// Thrust while the trim loads, as a share of hover.
const LOAD_FRAC: f32 = 0.5;
/// The attitude target's lean toward the trim while it loads, degrees.
const LOAD_LEAN_DEG: f32 = 8.0;
/// A load that has not reached its target by then ramps anyway.
const LOAD_TIMEOUT_MS: u64 = 8000;
/// The fast ramp: to this share of hover in [`FAST_S`].
const K_FAST: f32 = 0.8;
const FAST_S: f32 = 0.5;
/// The slow ramp climbs (K_LIFT - K_FAST) × hover every [`RAMP_S`]…
const K_LIFT: f32 = 1.12;
const RAMP_S: f32 = 1.5;
/// …and never past this thrust.
const THRUST_MAX: f32 = 0.9;
/// A tip on the skids past this from rest backs the thrust down and probes.
pub const PROBE_DEG: f32 = 1.5;
/// Tries before the launch stops for a trim it cannot find.
pub const PROBE_MAX: u32 = 10;
/// Settled back onto the skids: within this of rest for [`SETTLE_MS`].
const SETTLE_DEG: f32 = 0.5;
const SETTLE_MS: u64 = 300;
/// A pivot on the skids past this from rest stops the launch.
pub const ABORT_DEG: f32 = 10.0;
/// A motor above this share of its range before lift-off stops the launch:
/// less than 10 % left to climb and steer.
pub const SATURATION: f32 = 0.9;
/// Off the ground: risen this far, or climbing this fast (m, m/s).
const LIFT_RISE_M: f32 = 0.08;
const LIFT_VZ_MPS: f32 = 0.25;
/// Clear of the ground: hand over to the position controller at this rise,
/// after this long, or once tilted this far.
const HANDOVER_RISE_M: f32 = 0.25;
const CLIMB_MAX_MS: u64 = 2000;
const CLIMB_TILT_DEG: f32 = 15.0;
/// The climb thrust eases back toward this share of hover.
const CLIMB_K: f32 = 1.03;
/// The position target climbs at this rate, slowing into the launch height.
const CLIMB_MPS: f32 = 0.5;
const CLIMB_DECEL_MPS2: f32 = 0.5;
/// The ground phases give up after this long.
const GROUND_TIMEOUT_MS: u64 = 60_000;

/// The pilot's side of a supervised launch: the FC set up for it before
/// arming, then the lift-off.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Supervision {
    /// When the FC setup began, and when it was last sent.
    pub prep: Option<(u64, u64)>,
    pub liftoff: Option<Liftoff>,
    /// The airframe's imbalance in operator words, added to a stop's text.
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
    /// It pivoted past [`ABORT_DEG`].
    Tipping,
    /// A motor reached [`SATURATION`].
    Saturated,
    /// [`PROBE_MAX`] tries found no trim.
    NoTrim,
    /// It tipped and the FC sends no rate-loop data to find the trim with.
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

    /// Why, in the operator's words.
    pub fn text(self) -> &'static str {
        match self {
            Self::Tipping => "Stopped on the ground: the drone started to tip over",
            Self::Saturated => "Stopped on the ground: a motor ran out of power before lift-off",
            Self::NoTrim => "Stopped on the ground: could not find the drone's balance; check the battery and payload are secure",
            Self::NoRateData => "Stopped on the ground: the drone tipped and the flight controller sends no rate data to level it",
            Self::Spool => "Stopped on the ground: the motors did not spin up",
            Self::FcLink => "Stopped: the flight controller stopped reporting",
            Self::Link => "Stopped on the ground: the controller link was lost",
            Self::Operator => "Stopped on the ground by the operator",
            Self::Timeout => "Stopped on the ground: lift-off took too long",
        }
    }
}

/// A launch error the lift-off raised before it began, in the operator's
/// words.
pub fn error_text(code: &str) -> Option<&'static str> {
    match code {
        "liftoff_fc_params" => Some(
            "the flight controller did not take the level lift-off settings (GUID_OPTIONS, GCS_PID_MASK)",
        ),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Phase {
    Spool { since: u64, steady: Option<u64> },
    Level { since: u64 },
    Ramp,
    Settle { since: Option<u64> },
    Climb { since: u64 },
    Hold { ne: [f32; 2], zt: f32 },
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
    /// The integrator each axis loads to, whether it still loads, and the
    /// side it leans to while it does.
    target: [f32; 2],
    loading: [bool; 2],
    lean: [f32; 2],
    bracket: [probe::Bracket; 2],
    probes: u32,
    rest: Option<(f32, f32)>,
    yaw: f32,
    thrust: f32,
    tilt_deg: f32,
    began_ms: u64,
    last_ms: u64,
    /// The busiest motor seen on the ground.
    max_motor: f32,
    /// The rangefinder height at rest, and whether it has left the ground.
    rest_agl: Option<f32>,
    lifted: bool,
    /// What it found at lift-off, taken once by the pilot.
    outcome: Option<LiftoffOutcome>,
}

impl Liftoff {
    /// A lift-off to `alt_m` above the take-off point, with the learned
    /// `trim` (`None`: probe) and the hover thrust `hover`.
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
            bracket: Default::default(),
            probes: 0,
            rest: None,
            yaw: 0.0,
            thrust: SPOOL_THRUST,
            tilt_deg: 0.0,
            began_ms: now_ms,
            last_ms: now_ms,
            max_motor: 0.0,
            rest_agl: None,
            lifted: false,
            outcome: None,
        }
    }

    /// The take-off point, local NED.
    pub fn start(&self) -> Option<LocalPos> {
        self.start
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
            Phase::Spool { .. } | Phase::Level { .. } | Phase::Ramp | Phase::Settle { .. }
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
        let (dr, dp) = (att.roll - r0, att.pitch - p0);
        self.tilt_deg = dr.abs().max(dp.abs()).to_degrees();
        let rise = match (self.start, inp.local) {
            (Some(s), Some(l)) => s.d - l.d,
            _ => 0.0,
        };
        let lifted = rise > LIFT_RISE_M || inp.local.is_some_and(|l| l.vd < -LIFT_VZ_MPS);
        if self.on_ground() {
            if let Some(m) = inp.motors {
                self.max_motor = self.max_motor.max(m.max);
            }
            if let Some(stop) = self.ground_stop(inp, lifted, now) {
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
                    self.rest_agl = inp.agl;
                    self.phase = self.after_settle(now);
                } else if now.saturating_sub(since) > SPOOL_TIMEOUT_MS {
                    self.phase = Phase::Stopped(Stop::Spool);
                    return None;
                }
                Some(att_cmd(r0, p0, SPOOL_THRUST))
            }
            Phase::Level { since } => {
                self.thrust = (self.thrust + dt * hover).min(LOAD_FRAC * hover);
                if let Some(i) = inp.i {
                    for ax in 0..2 {
                        // Loaded once the integrator reached its target.
                        if self.loading[ax] && (i[ax] - self.target[ax]) * self.lean[ax] >= 0.0 {
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
                    self.outcome = Some(self.outcome_now(inp, false));
                    self.phase = Phase::Climb { since: now };
                    return self.step_climb(inp, rise, dt);
                }
                if self.tilt_deg > PROBE_DEG {
                    return Some(self.probe(inp, (dr, dp), r0, p0));
                }
                let fast = K_FAST * hover;
                self.thrust = if self.thrust < fast {
                    (self.thrust + dt * fast / FAST_S).min(fast)
                } else {
                    (self.thrust + dt * (K_LIFT - K_FAST) * hover / RAMP_S).min(THRUST_MAX)
                };
                Some(att_cmd(r0, p0, self.thrust))
            }
            Phase::Settle { since } => {
                self.thrust = LOAD_FRAC * hover;
                if self.tilt_deg < SETTLE_DEG {
                    let s = since.unwrap_or(now);
                    self.phase = Phase::Settle { since: Some(s) };
                    if now.saturating_sub(s) >= SETTLE_MS {
                        self.phase = self.after_settle(now);
                    }
                } else {
                    self.phase = Phase::Settle { since: None };
                }
                Some(att_cmd(r0, p0, self.thrust))
            }
            Phase::Climb { .. } | Phase::Hold { .. } => self.step_climb(inp, rise, dt),
            Phase::Stopped(_) => None,
        }
    }

    /// The pivot, saturation and timeout guards on the ground.
    fn ground_stop(&self, inp: &Inputs, lifted: bool, now: u64) -> Option<Stop> {
        if lifted {
            return None;
        }
        if self.rest.is_some() && self.tilt_deg > ABORT_DEG {
            return Some(Stop::Tipping);
        }
        if self.rest.is_some() && inp.motors.is_some_and(|m| m.max > SATURATION) {
            return Some(Stop::Saturated);
        }
        (now.saturating_sub(self.began_ms) > GROUND_TIMEOUT_MS).then_some(Stop::Timeout)
    }

    /// A tip began on the skids: back down, and load a better trim.
    fn probe(&mut self, inp: &Inputs, tilt: (f32, f32), r0: f32, p0: f32) -> Cmd {
        let Some(i) = inp.i else {
            self.phase = Phase::Stopped(Stop::NoRateData);
            return self.idle_at(r0, p0);
        };
        if self.probes >= PROBE_MAX {
            self.phase = Phase::Stopped(Stop::NoTrim);
            return self.idle_at(r0, p0);
        }
        self.probes += 1;
        let frac = self.thrust / self.hover;
        for (ax, d) in [tilt.0, tilt.1].into_iter().enumerate() {
            if d.abs().to_degrees() > 0.5 * PROBE_DEG {
                self.target[ax] = self.bracket[ax].next(i[ax], d, frac);
                self.lean[ax] = (self.target[ax] - i[ax]).signum();
                self.loading[ax] = true;
            }
        }
        self.thrust = LOAD_FRAC * self.hover;
        self.phase = Phase::Settle { since: None };
        Cmd::Attitude {
            roll: r0,
            pitch: p0,
            yaw: self.yaw,
            thrust: self.thrust,
        }
    }

    /// Off the skids: level, easing toward hover, then the position target.
    fn step_climb(&mut self, inp: &Inputs, rise: f32, dt: f32) -> Option<Cmd> {
        if let Phase::Climb { since } = self.phase {
            self.thrust = (self.thrust - dt * 0.1 * self.hover / 0.5).max(CLIMB_K * self.hover);
            let clear = rise > HANDOVER_RISE_M
                || inp.now_ms.saturating_sub(since) > CLIMB_MAX_MS
                || self.tilt_deg > CLIMB_TILT_DEG;
            match (clear, inp.local) {
                (true, Some(l)) => {
                    self.phase = Phase::Hold {
                        ne: [l.n, l.e],
                        zt: l.d,
                    };
                }
                _ => {
                    return Some(Cmd::Attitude {
                        roll: 0.0,
                        pitch: 0.0,
                        yaw: self.yaw,
                        thrust: self.thrust,
                    })
                }
            }
        }
        self.hold_or_none().map(|c| self.climb_target(c, dt))
    }

    /// The climbing position target, carried up from where it handed over
    /// at [`CLIMB_MPS`], slowing into the launch height.
    fn climb_target(&mut self, c: Cmd, dt: f32) -> Cmd {
        let Phase::Hold { ne, zt } = self.phase else {
            return c;
        };
        let goal = self.start.map_or(0.0, |s| s.d) - self.alt_m;
        let left = (zt - goal).max(0.0);
        let v = CLIMB_MPS.min((2.0 * CLIMB_DECEL_MPS2 * left).sqrt());
        let zt = (zt - v * dt).max(goal);
        self.phase = Phase::Hold { ne, zt };
        Cmd::Position {
            pos: [ne[0], ne[1], zt],
            vel: [0.0, 0.0, if zt > goal { -v } else { 0.0 }],
            yaw: self.yaw,
        }
    }

    fn hold_or_none(&self) -> Option<Cmd> {
        match self.phase {
            Phase::Hold { ne, zt } => Some(Cmd::Position {
                pos: [ne[0], ne[1], zt],
                vel: [0.0; 3],
                yaw: self.yaw,
            }),
            _ => None,
        }
    }

    /// Settled (or spooled): load the trim if there is one to load.
    fn after_settle(&self, now: u64) -> Phase {
        if self.loading.iter().any(|&l| l) {
            Phase::Level { since: now }
        } else {
            Phase::Ramp
        }
    }

    fn idle_at(&self, r0: f32, p0: f32) -> Cmd {
        Cmd::Attitude {
            roll: r0,
            pitch: p0,
            yaw: self.yaw,
            thrust: 0.0,
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

    /// The surfaces' view.
    pub fn view(&self) -> ados_protocol::pilot::Liftoff {
        let probing = !self.learned || self.probes > 0;
        let (phase, text) = match self.phase {
            Phase::Spool { .. } => ("spool", "Spinning up the motors"),
            Phase::Level { .. } | Phase::Settle { .. } if probing => (
                "probe",
                "Finding the balance on the ground (first flight on this drone)",
            ),
            Phase::Level { .. } | Phase::Settle { .. } => ("level", "Levelling on the ground"),
            Phase::Ramp => ("lift", "Lifting off"),
            Phase::Climb { .. } => ("climb", "Climbing out level"),
            Phase::Hold { .. } => ("done", "Lifted off level"),
            Phase::Stopped(s) => ("stopped", s.text()),
        };
        ados_protocol::pilot::Liftoff {
            phase: phase.into(),
            text: text.into(),
            trim: if self.learned { "learned" } else { "probe" }.into(),
            tilt_deg: f64::from((self.tilt_deg * 10.0).round() / 10.0),
        }
    }
}
