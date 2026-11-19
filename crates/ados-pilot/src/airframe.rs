//! Airframe health: the trim an unbalanced airframe needs, learned from its
//! own flights and kept per flight controller, and whether it has the motor
//! headroom to fly at all.
//!
//! The supervised lift-off loads the learned trim into the FC's rate loop on
//! the ground ([`Airframe::trim`]); without one it finds it on the ground
//! (probe) and reports what it found ([`Airframe::note_liftoff`]). In steady
//! hover the roll and pitch rate-loop integrators, the hover throttle and the
//! busiest motor's thrust demand are averaged ([`Airframe::tick`]). The
//! record is filed under the FC's identity in the setup state
//! ([`Airframe::load`], [`Airframe::save`]) and becomes the
//! `airframe_balance` readiness row ([`Airframe::readiness`]).

use ados_protocol::pilot::{Readiness, Severity};
use serde::{Deserialize, Serialize};

use crate::fc::FcState;

#[cfg(test)]
mod tests;

/// The FC parameters airframe health reads: the frame layout and the motor
/// output range and thrust curve.
pub const PARAMS: &[&str] = &[
    "FRAME_CLASS",
    "FRAME_TYPE",
    "MOT_PWM_MIN",
    "MOT_PWM_MAX",
    "MOT_SPIN_MIN",
    "MOT_SPIN_MAX",
    "MOT_THST_EXPO",
];

/// Steady hover: airborne this long (ms)…
const AIRBORNE_MS: u64 = 3000;
/// …climbing or sinking slower than this (m/s)…
const STEADY_VZ_MPS: f32 = 0.2;
/// …tilted less than this…
const STEADY_TILT_DEG: f32 = 5.0;
/// …not accelerating horizontally (m/s², low-passed over
/// [`ACCEL_TAU_S`]): no position-target acceleration…
const STEADY_ACCEL_MPS2: f32 = 0.3;
const ACCEL_TAU_S: f32 = 0.5;
/// …and the rate loop asked for no rotation faster than this (deg/s).
const STEADY_RATE_DPS: f32 = 10.0;
/// Rate-loop, motor and throttle data older than this is not used (ms).
const FRESH_MS: u64 = 500;
/// The averages weigh each new sample at least 1 / this: a plain mean over
/// the first samples, then about the last 500 (20 s at 25 Hz).
const WINDOW: u32 = 500;
/// The headroom counts once this many steady-hover samples are behind it.
const HEADROOM_MIN_SAMPLES: u32 = 25;
/// Warn from this estimated centre-of-gravity offset (mm)…
const WARN_MM: f32 = 15.0;
/// …or, without the rotor geometry, from this share of the rate loop's
/// authority held as trim.
const WARN_AUTHORITY: f32 = 0.25;
/// Block below this motor headroom in steady hover: too little left to climb
/// and steer.
pub const BLOCK_HEADROOM: f32 = 0.10;
/// A lift-off that found an integrator this far from the learned one: the
/// airframe changed (one probe step), so the learning restarts from it.
const CHANGED_I: f32 = 0.08;
/// A lift-off on a learned airframe moves the learned trim this far
/// toward what it found.
const LIFTOFF_WEIGHT: f32 = 0.2;
/// Motor radius (centre to motor axis) per propeller diameter when the
/// airframe's outer radius is not set: the usual quad spacing, motors about
/// 1.4 propeller diameters apart (0.25 m arms on 10" props).
const ARM_PER_PROP: f32 = 0.98;
const M_PER_IN: f32 = 0.0254;

/// ArduPilot's motor defaults until the parameters are read.
const SPIN_MIN: f32 = 0.15;
const SPIN_MAX: f32 = 0.95;
const THST_EXPO: f32 = 0.65;
const PWM_MIN: f32 = 1000.0;
const PWM_MAX: f32 = 2000.0;

/// The rate-loop integrators an airframe holds in steady hover, and its
/// hover throttle.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Trim {
    /// Roll and pitch rate-loop I (PID_TUNING), in the FC's output units.
    pub i: [f32; 2],
    /// Hover throttle, 0..1.
    pub hover_thr: f32,
    /// Steady-hover samples behind the values (0: a lift-off probe only).
    pub samples: u32,
}

/// What a supervised lift-off found, for airframe health.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct LiftoffOutcome {
    /// The roll and pitch I at lift-off (`None`: it never lifted).
    pub i: Option<[f32; 2]>,
    /// The thrust it lifted at, 0..1.
    pub thrust: f32,
    /// The busiest motor's share of its output range before lift-off, 0..1.
    pub max_motor: f32,
    /// It stopped because a motor reached the top of its range.
    pub saturated: bool,
}

/// What airframe health keeps per flight controller (the setup state's
/// `data.airframe`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Record {
    pub trim: Option<Trim>,
    /// The busiest motor's thrust demand in steady hover, 0..1.
    pub busiest: Option<f32>,
    /// Steady-hover samples behind `busiest`.
    pub busiest_samples: u32,
    /// The last supervised lift-off on this FC stopped because a motor ran
    /// out of range.
    pub saturated: bool,
    /// The busiest motor's share of its output range at that stop, 0..1.
    pub stop_motor: f32,
    /// Counts the saves (disarm after learning, a lift-off outcome, a
    /// reset).
    pub rev: u64,
}

impl Record {
    /// The motor headroom in steady hover, 0..1, once enough samples are
    /// behind it.
    pub fn headroom(&self) -> Option<f32> {
        (self.busiest_samples >= HEADROOM_MIN_SAMPLES)
            .then_some(self.busiest?)
            .map(|b| 1.0 - b)
    }
}

/// A record to file under the FC identity `uid`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Save {
    pub uid: String,
    pub record: Record,
}

/// Airframe health for the connected FC.
#[derive(Debug, Clone, Default)]
pub struct Airframe {
    rec: Record,
    /// The FC identity the record is filed under, once known.
    uid: Option<String>,
    /// The propeller size from the setup wizard (inches).
    prop_in: Option<f32>,
    /// `pilot.prop_tip_radius_m`.
    tip_radius_m: Option<f32>,
    /// Steady-hover samples taken since the last save point.
    unsaved: bool,
    was_armed: bool,
    airborne_since: Option<u64>,
    /// The last horizontal velocity and when, and the low-passed
    /// acceleration magnitude.
    vel: Option<([f32; 2], u64)>,
    accel: f32,
    /// The PID_TUNING pitch sample last learned from.
    last_pid_ms: u64,
    /// A mutation stub: the Balance check reads healthy whatever the
    /// record holds, to prove a test's refusal is the check's own.
    #[cfg(test)]
    pub(crate) force_healthy: bool,
}

impl Airframe {
    /// The learned trim for the connected FC; `None` until one is known (the
    /// lift-off then probes).
    pub fn trim(&self) -> Option<Trim> {
        self.rec.trim
    }

    pub fn record(&self) -> &Record {
        &self.rec
    }

    /// The FC identity is known: its stored `record` (`None`: nothing stored
    /// yet), the wizard's propeller size and `pilot.prop_tip_radius_m`. What
    /// was learned before the identity was known is kept and filed under it.
    /// A repeat for the same FC updates the geometry only.
    pub fn load(
        &mut self,
        uid: &str,
        record: Option<Record>,
        prop_in: Option<f32>,
        tip_radius_m: Option<f32>,
    ) {
        self.prop_in = prop_in.filter(|p| *p > 0.0);
        self.tip_radius_m = tip_radius_m.filter(|r| *r > 0.0);
        if self.uid.as_deref() == Some(uid) {
            return;
        }
        let learned_before = self.uid.is_none() && (self.unsaved || self.rec.rev > 0);
        self.uid = Some(uid.to_owned());
        if learned_before {
            // Filed over the stored record: a revision past it.
            self.rec.rev = self.rec.rev.max(record.map_or(0, |r| r.rev));
            self.commit();
        } else if let Some(r) = record {
            self.rec = r;
            log_record("pilot_airframe_loaded", uid, &self.rec);
        } else {
            self.rec = Record::default();
        }
    }

    /// The record to file, once the FC identity is known.
    pub fn save(&self) -> Option<Save> {
        Some(Save {
            uid: self.uid.clone()?,
            record: self.rec.clone(),
        })
    }

    /// The operator rebalanced the airframe: forget what was learned.
    pub fn reset(&mut self) {
        self.rec = Record {
            rev: self.rec.rev,
            ..Record::default()
        };
        self.commit();
    }

    fn commit(&mut self) {
        self.rec.rev += 1;
        self.unsaved = false;
    }

    /// Fold one tick of FC state in (steady-hover learning); a disarm after
    /// learning is a save point.
    pub fn tick(&mut self, fc: &FcState, now_ms: u64) {
        if self.was_armed && !fc.armed && self.unsaved {
            self.commit();
        }
        self.was_armed = fc.armed;
        if !fc.armed || fc.landed() {
            self.airborne_since = None;
            self.vel = None;
            self.accel = 0.0;
            return;
        }
        let since = *self.airborne_since.get_or_insert(now_ms);
        self.track_accel(fc);
        if now_ms.saturating_sub(since) < AIRBORNE_MS || !self.steady(fc, now_ms) {
            return;
        }
        let (Some(roll), Some(pitch)) = (fc.rates.roll, fc.rates.pitch) else {
            return;
        };
        if pitch.at_ms == self.last_pid_ms
            || now_ms.saturating_sub(roll.at_ms) >= FRESH_MS
            || now_ms.saturating_sub(pitch.at_ms) >= FRESH_MS
        {
            return;
        }
        self.last_pid_ms = pitch.at_ms;
        let hover = fc
            .rates
            .fresh_throttle(now_ms, FRESH_MS)
            .or_else(|| fc.params.get("MOT_THST_HOVER").copied());
        if let Some(hover) = hover {
            let t = self.rec.trim.filter(|t| t.samples > 0);
            let n = t.map_or(0, |t| t.samples);
            let a = weight(n);
            let (i0, h0) = t.map_or(([0.0; 2], 0.0), |t| (t.i, t.hover_thr));
            self.rec.trim = Some(Trim {
                i: [
                    i0[0] + a * (roll.v.i - i0[0]),
                    i0[1] + a * (pitch.v.i - i0[1]),
                ],
                hover_thr: h0 + a * (hover - h0),
                samples: n.saturating_add(1),
            });
        }
        if let Some(busiest) = busiest_demand(fc, now_ms) {
            let n = self.rec.busiest_samples;
            let b0 = self.rec.busiest.unwrap_or(busiest);
            self.rec.busiest = Some(b0 + weight(n) * (busiest - b0));
            self.rec.busiest_samples = n.saturating_add(1);
        }
        self.unsaved = true;
    }

    /// The horizontal acceleration from successive local velocities,
    /// low-passed.
    fn track_accel(&mut self, fc: &FcState) {
        let Some(l) = fc.local else { return };
        let v = [l.v.vn, l.v.ve];
        match self.vel {
            Some((_, t)) if t == l.at_ms => {}
            Some((v0, t)) => {
                let dt = l.at_ms.saturating_sub(t) as f32 / 1000.0;
                if dt > 0.0 {
                    let raw = (v[0] - v0[0]).hypot(v[1] - v0[1]) / dt;
                    self.accel += (raw - self.accel) * (dt / ACCEL_TAU_S).min(1.0);
                }
                self.vel = Some((v, l.at_ms));
            }
            None => self.vel = Some((v, l.at_ms)),
        }
    }

    fn steady(&self, fc: &FcState, now_ms: u64) -> bool {
        let (Some(l), Some(a)) = (fc.fresh_local(now_ms), fc.fresh_attitude(now_ms)) else {
            return false;
        };
        let tilt = crate::geo::acos((a.roll.cos() * a.pitch.cos()).clamp(-1.0, 1.0));
        let calm = |s: Option<crate::fc::Stamped<crate::fc::rates::PidAxis>>| {
            s.is_none_or(|s| s.v.desired.abs() < STEADY_RATE_DPS)
        };
        l.vd.abs() < STEADY_VZ_MPS
            && tilt < STEADY_TILT_DEG.to_radians()
            && self.accel < STEADY_ACCEL_MPS2
            && calm(fc.rates.roll)
            && calm(fc.rates.pitch)
    }

    /// Record a supervised lift-off's outcome (a save point). A lift-off
    /// that stopped for a saturated motor blocks the next one; one that
    /// lifted seeds the trim, or moves a learned one toward what it found
    /// (and restarts the learning from it when the airframe changed).
    pub fn note_liftoff(&mut self, o: &LiftoffOutcome, _now_ms: u64) {
        self.rec.saturated = o.saturated;
        self.rec.stop_motor = if o.saturated { o.max_motor } else { 0.0 };
        if let Some(i) = o.i {
            self.rec.trim = Some(match self.rec.trim.filter(|t| t.samples > 0) {
                Some(t)
                    if (i[0] - t.i[0]).abs() <= CHANGED_I && (i[1] - t.i[1]).abs() <= CHANGED_I =>
                {
                    Trim {
                        i: [
                            t.i[0] + LIFTOFF_WEIGHT * (i[0] - t.i[0]),
                            t.i[1] + LIFTOFF_WEIGHT * (i[1] - t.i[1]),
                        ],
                        ..t
                    }
                }
                _ => Trim {
                    i,
                    hover_thr: o.thrust,
                    samples: 0,
                },
            });
        }
        self.commit();
    }

    /// The motor headroom in steady hover, 0..1, once enough samples are
    /// behind it.
    pub fn headroom(&self) -> Option<f32> {
        self.rec.headroom()
    }

    /// The imbalance as the balance row reads it ("Balance: nose-heavy. Off
    /// by about 35 mm. Move the battery back."), `None` while balanced or
    /// nothing is learned.
    pub fn imbalance_text(&self, fc: &FcState) -> Option<String> {
        let (row, detail) = self.learned_imbalance(fc)?;
        Some(format!("Balance: {row}. {detail}"))
    }

    fn learned_imbalance(&self, fc: &FcState) -> Option<(String, String)> {
        let t = self.rec.trim?;
        let hover = Some(t.hover_thr)
            .filter(|h| *h > 0.0)
            .or_else(|| fc.params.get("MOT_THST_HOVER").copied())?;
        self.imbalance(fc, t.i, hover)
    }

    /// The `airframe_balance` readiness row on an ArduPilot multirotor
    /// (`None` on anything else). Block while the last lift-off on this FC
    /// stopped for a saturated motor or the steady-hover headroom is under
    /// [`BLOCK_HEADROOM`]; a failing Warn while it is out of balance; a
    /// passing row while it is learning or balanced.
    pub fn readiness(&self, fc: &FcState) -> Option<Readiness> {
        if fc.firmware != Some(crate::fc_profile::Firmware::Ardupilot)
            || fc.vehicle_class() != ados_protocol::pilot::VehicleClass::Multirotor
        {
            return None;
        }
        let row = |ok, severity, msg: String| Readiness {
            id: "airframe_balance".into(),
            ok,
            severity,
            msg,
        };
        #[cfg(test)]
        if self.force_healthy {
            return Some(row(true, Severity::Warn, BALANCE_OK.into()));
        }
        if self.rec.saturated {
            return Some(row(
                false,
                Severity::Block,
                no_margin_text(self.rec.stop_motor, false),
            ));
        }
        if let Some(h) = self.headroom().filter(|h| *h < BLOCK_HEADROOM) {
            return Some(row(false, Severity::Block, no_margin_text(1.0 - h, true)));
        }
        if let Some(t) = self.imbalance_text(fc) {
            return Some(row(false, Severity::Warn, t));
        }
        let learned = self.rec.trim.is_some_and(|t| t.samples > 0);
        Some(row(
            true,
            Severity::Warn,
            if learned {
                BALANCE_OK
            } else {
                BALANCE_LEARNING
            }
            .into(),
        ))
    }

    /// The imbalance the roll and pitch integrators `i` hold at thrust
    /// `thrust`, as sentences for a take-off stop's detail ("Nose-heavy.
    /// Off by about 35 mm. Move the battery back."), `None` while within
    /// the balance tolerance. A quad X or + with a known propeller size
    /// reads the centre-of-gravity offset in mm; any other airframe the
    /// share of the rate loop's authority ("Uneven. Using 30 % of the
    /// control margin to stay level.").
    pub fn imbalance_words(&self, fc: &FcState, i: [f32; 2], thrust: f32) -> Option<String> {
        let (row, detail) = self.imbalance(fc, i, thrust)?;
        let mut c = row.chars();
        let first = c.next()?.to_uppercase();
        Some(format!("{first}{}. {detail}", c.as_str()))
    }

    /// The balance row's state ("nose-heavy", "uneven") and its detail.
    fn imbalance(&self, fc: &FcState, i: [f32; 2], thrust: f32) -> Option<(String, String)> {
        match self.half_spacing_m(fc) {
            Some(r) if thrust > 0.0 => {
                let (x, y) = cg_offset_m(i, thrust, r);
                words_mm(x, y)
            }
            _ => words_authority(i),
        }
    }

    /// The rotor half-spacing the pitch and roll integrators act through:
    /// the motor radius over √2 on a quad X, over 2 on a quad + (two motors
    /// per axis at the full radius, against four at the half-spacing).
    fn half_spacing_m(&self, fc: &FcState) -> Option<f32> {
        let layout = quad_layout(fc)?;
        let d = self.prop_in? * M_PER_IN;
        let arm = match self.tip_radius_m {
            Some(t) if t > d => t - d / 2.0,
            _ => ARM_PER_PROP * d,
        };
        Some(match layout {
            Quad::X => arm * std::f32::consts::FRAC_1_SQRT_2,
            Quad::Plus => arm / 2.0,
        })
    }
}

/// The structured event for a record filed (`pilot_airframe_saved`) or
/// read back (`pilot_airframe_loaded`) under the FC identity `uid`: the same
/// fields in both, so a saved record can be matched to its reload.
pub fn log_record(event: &'static str, uid: &str, r: &Record) {
    let t = r.trim.unwrap_or_default();
    let headroom = r.headroom();
    let (i_roll, i_pitch, hover_thr, samples) = (t.i[0], t.i[1], t.hover_thr, t.samples);
    let (saturated, rev) = (r.saturated, r.rev);
    match event {
        "pilot_airframe_loaded" => tracing::info!(
            target: "ados_pilot", uid, i_roll, i_pitch, hover_thr, samples, headroom, saturated, rev,
            "pilot_airframe_loaded"
        ),
        _ => tracing::info!(
            target: "ados_pilot", uid, i_roll, i_pitch, hover_thr, samples, headroom, saturated, rev,
            "pilot_airframe_saved"
        ),
    }
}

/// The weight of a new sample after `n`.
fn weight(n: u32) -> f32 {
    1.0 / (n.min(WINDOW - 1) + 1) as f32
}

/// The balance row while nothing is learned yet, and once balanced.
pub const BALANCE_LEARNING: &str = "Balance: learning. Learned after the first steady hover.";
pub const BALANCE_OK: &str = "Balance: OK.";

/// The Block text: the busiest motor at `motor` (0..1) in steady hover
/// (`hover`) or at the take-off that stopped.
pub fn no_margin_text(motor: f32, hover: bool) -> String {
    format!(
        "Can't take off: no climb margin. Motors at {} % {}. Lighten the drone or rebalance it.",
        (motor.clamp(0.0, 1.0) * 100.0).round(),
        if hover { "in hover" } else { "at take-off" }
    )
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Quad {
    X,
    Plus,
}

/// A quad X or + (`FRAME_CLASS` 1; `FRAME_TYPE` 0 plus, 1/12/13/14/18 the
/// X layouts with their motor orders), `None` for anything else or while
/// the parameters are unread.
fn quad_layout(fc: &FcState) -> Option<Quad> {
    let p = |n: &str| fc.params.get(n).map(|v| *v as i32);
    if p("FRAME_CLASS")? != 1 {
        return None;
    }
    match p("FRAME_TYPE")? {
        0 => Some(Quad::Plus),
        1 | 12 | 13 | 14 | 18 => Some(Quad::X),
        _ => None,
    }
}

/// The centre-of-gravity offset `(x forward, y right)` in metres from the
/// hover integrators: the rate loop holds the torque of the offset weight,
/// x = I_pitch·r / (2·T_hover), y = −I_roll·r / (2·T_hover).
fn cg_offset_m(i: [f32; 2], thrust: f32, r: f32) -> (f32, f32) {
    (i[1] * r / (2.0 * thrust), -i[0] * r / (2.0 * thrust))
}

/// The heavy sides and where the battery moves, for an offset `(x forward,
/// y right)`: each axis carrying at least 40 % of the larger one.
fn sides(x: f32, y: f32) -> (Vec<&'static str>, Vec<&'static str>) {
    let big = x.abs().max(y.abs());
    let mut heavy = Vec::new();
    let mut fix = Vec::new();
    if x.abs() >= 0.4 * big {
        heavy.push(if x > 0.0 { "nose-heavy" } else { "tail-heavy" });
        fix.push(if x > 0.0 { "back" } else { "forward" });
    }
    if y.abs() >= 0.4 * big {
        heavy.push(if y > 0.0 { "right-heavy" } else { "left-heavy" });
        fix.push(if y > 0.0 { "left" } else { "right" });
    }
    (heavy, fix)
}

/// Rounded to the nearest 5.
fn round5(v: f32) -> u32 {
    ((v / 5.0).round() * 5.0) as u32
}

fn words_mm(x: f32, y: f32) -> Option<(String, String)> {
    let mm = x.hypot(y) * 1000.0;
    if mm < WARN_MM {
        return None;
    }
    let (heavy, fix) = sides(x, y);
    Some((
        heavy.join(" and "),
        format!(
            "Off by about {} mm. Move the battery {}.",
            round5(mm),
            fix.join(" and ")
        ),
    ))
}

fn words_authority(i: [f32; 2]) -> Option<(String, String)> {
    let big = i[0].abs().max(i[1].abs());
    if big <= WARN_AUTHORITY {
        return None;
    }
    Some((
        "uneven".into(),
        format!(
            "Using {} % of the control margin to stay level.",
            round5(big * 100.0)
        ),
    ))
}

/// The motor outputs of the airframe's frame class.
pub fn motor_count(fc: &FcState) -> usize {
    match fc.params.get("FRAME_CLASS").map(|v| *v as u32) {
        Some(2 | 5) => 6,
        Some(3 | 4 | 12 | 14) => 8,
        Some(7) => 3,
        _ => 4,
    }
}

/// A motor output's thrust demand, 0..1: its share of the PWM range,
/// linearised over the spin range and through the thrust curve,
/// t = (1 − E)·a + E·a² (`MOT_THST_EXPO` E).
pub fn thrust_demand(fc: &FcState, pwm: u16) -> f32 {
    let p = |n: &str, d: f32| fc.params.get(n).copied().unwrap_or(d);
    let lo = Some(p("MOT_PWM_MIN", 0.0))
        .filter(|v| *v > 0.0)
        .unwrap_or(PWM_MIN);
    let hi = Some(p("MOT_PWM_MAX", 0.0))
        .filter(|v| *v > lo)
        .unwrap_or(PWM_MAX.max(lo + 1.0));
    let (s0, s1) = (p("MOT_SPIN_MIN", SPIN_MIN), p("MOT_SPIN_MAX", SPIN_MAX));
    let e = p("MOT_THST_EXPO", THST_EXPO);
    let a = (f32::from(pwm) - lo) / (hi - lo);
    let lin = if s1 > s0 {
        ((a - s0) / (s1 - s0)).clamp(0.0, 1.0)
    } else {
        a.clamp(0.0, 1.0)
    };
    (1.0 - e) * lin + e * lin * lin
}

/// The busiest motor's thrust demand from fresh SERVO_OUTPUT_RAW.
fn busiest_demand(fc: &FcState, now_ms: u64) -> Option<f32> {
    let o = fc.rates.fresh_motors(motor_count(fc), now_ms, FRESH_MS)?;
    let max = *o.iter().max()?;
    (max > 0).then(|| thrust_demand(fc, max))
}
