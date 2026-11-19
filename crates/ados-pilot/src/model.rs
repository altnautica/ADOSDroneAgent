//! The pilot's flight models: the profile limits every autonomous path
//! flies at ([`Limits`], [`GoalLimits`]), the operator's per-airframe stick
//! limits held to the flight controller's guidance limits
//! ([`StickLimits`]), expo, and the jerk-limited shapers.

use ados_protocol::pilot::{Profile, VehicleClass};

use crate::geo::{self, V2};

/// One profile's limits.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Limits {
    pub v_h: f32,
    pub v_up: f32,
    pub v_down: f32,
    pub yaw_dps: f32,
    pub a_h: f32,
    pub jerk: f32,
    pub expo: f32,
}

pub fn limits(p: Profile) -> Limits {
    match p {
        Profile::Cine => Limits {
            v_h: 3.0,
            v_up: 1.5,
            v_down: 1.0,
            yaw_dps: 30.0,
            a_h: 1.5,
            jerk: 3.0,
            expo: 0.3,
        },
        Profile::Normal => Limits {
            v_h: 8.0,
            v_up: 3.0,
            v_down: 2.0,
            yaw_dps: 90.0,
            a_h: 3.0,
            jerk: 6.0,
            expo: 0.2,
        },
        Profile::Sport => Limits {
            v_h: 14.0,
            v_up: 5.0,
            v_down: 3.0,
            yaw_dps: 150.0,
            a_h: 6.0,
            jerk: 12.0,
            expo: 0.1,
        },
    }
}

/// One profile's limits on the setpoints the pilot flies for a skill goal or
/// a goto. Softer than the stick limits: the camera is fixed to the body, and
/// a multirotor tilts by atan(a/g) to accelerate at `a` and changes that tilt
/// at about j/g rad/s for a jerk `j`. On Normal, 1.0 m/s² is a 5.8° tilt and
/// 2.0 m/s³ an 11.7°/s tilt rate; ArduCopter's own guided shaping defaults
/// are `WP_ACC` 2.5 m/s² and `PSC_NE_JERK` 5 m/s³, which turned stepped
/// follow commands into 15-26°/s tilt rates. The obstacle brake keeps the
/// profile's stick limits ([`Limits::a_h`]): a stop is never held to these.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GoalLimits {
    /// Horizontal acceleration, m/s².
    pub a_h: f32,
    /// Horizontal jerk, m/s³.
    pub jerk: f32,
    /// Heading rate, °/s.
    pub yaw_dps: f32,
    /// Heading acceleration, °/s².
    pub yaw_dps2: f32,
}

pub fn goal_limits(p: Profile) -> GoalLimits {
    match p {
        Profile::Cine => GoalLimits {
            a_h: 0.6,
            jerk: 1.2,
            yaw_dps: 20.0,
            yaw_dps2: 40.0,
        },
        Profile::Normal => GoalLimits {
            a_h: 1.0,
            jerk: 2.0,
            yaw_dps: 30.0,
            yaw_dps2: 60.0,
        },
        Profile::Sport => GoalLimits {
            a_h: 2.5,
            jerk: 5.0,
            yaw_dps: 60.0,
            yaw_dps2: 120.0,
        },
    }
}

impl GoalLimits {
    /// The shaper's limits for a goal: one acceleration both ways, settling
    /// on jerk over acceleration (2/s on Normal), softer than the sticks'.
    /// A lower acceleration coming on (a speed cap's) holds at once, and a
    /// stop settles on without coming to rest at a speed.
    pub fn shape(&self) -> ShapeLimits {
        ShapeLimits {
            a_acc: self.a_h,
            a_brk: self.a_h,
            jerk: self.jerk,
            settle: self.jerk / self.a_h,
            unwind: f32::INFINITY,
            rest_mps: 0.0,
        }
    }
}

/// One profile's stick limits: what the operator's sticks fly at, and only
/// they. Every autonomous path (goals, gotos, holds, returns, landing, the
/// obstacle brake on them) keeps [`Limits`] and [`GoalLimits`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StickLimits {
    /// Horizontal speed at full stick, m/s.
    pub v_h: f32,
    /// Climb and descent speed at full stick, m/s.
    pub v_up: f32,
    pub v_down: f32,
    /// Yaw rate at full stick, °/s.
    pub yaw_dps: f32,
    /// Horizontal acceleration while speeding up, m/s².
    pub a_acc: f32,
    /// Horizontal deceleration while slowing, braking to a hold or braking
    /// for an obstacle, m/s².
    pub a_brk: f32,
    /// Horizontal jerk, m/s³.
    pub jerk: f32,
    /// Vertical acceleration, m/s², and jerk, m/s³.
    pub a_v: f32,
    pub jerk_v: f32,
    /// Yaw acceleration, °/s².
    pub yaw_dps2: f32,
    /// Stick expo per axis ([`expo`]).
    pub expo_h: f32,
    pub expo_v: f32,
    pub expo_yaw: f32,
}

impl StickLimits {
    /// Every limit the lower of `self` and `o`.
    pub fn min(self, o: StickLimits) -> StickLimits {
        StickLimits {
            v_h: self.v_h.min(o.v_h),
            v_up: self.v_up.min(o.v_up),
            v_down: self.v_down.min(o.v_down),
            yaw_dps: self.yaw_dps.min(o.yaw_dps),
            a_acc: self.a_acc.min(o.a_acc),
            a_brk: self.a_brk.min(o.a_brk),
            jerk: self.jerk.min(o.jerk),
            a_v: self.a_v.min(o.a_v),
            jerk_v: self.jerk_v.min(o.jerk_v),
            yaw_dps2: self.yaw_dps2.min(o.yaw_dps2),
            // Expo only softens the centre; the profile's own stays.
            expo_h: self.expo_h,
            expo_v: self.expo_v,
            expo_yaw: self.expo_yaw,
        }
    }

    /// The shaper's limits for the sticks: speeding up and braking on their
    /// own accelerations, settling on [`SETTLE_GAIN`], a brake ending in a
    /// new push unwinding at the jerk limit, and a hold coming to rest
    /// ([`REST_MPS`]).
    pub fn shape(&self) -> ShapeLimits {
        ShapeLimits {
            a_acc: self.a_acc,
            a_brk: self.a_brk,
            jerk: self.jerk,
            settle: SETTLE_GAIN,
            unwind: self.jerk,
            rest_mps: REST_MPS,
        }
    }
}

/// The open-place multirotor stick rows: at or under ArduCopter's default
/// guided limits (`WP_ACC` 2.5 m/s², `PSC_NE_JERK` 5 m/s³, `WP_ACC_Z`
/// 1 m/s², `PSC_D_JERK` 5 m/s³, `ATC_ACC_Y_MAX` 270 °/s²), so the flight
/// controller tracks the sticks' reference rather than shaping it again.
/// Braking is firmer than speeding up, and Sport has the most expo, so its
/// extra speed stays fine near the centre.
fn multirotor_sticks(p: Profile) -> StickLimits {
    match p {
        Profile::Cine => StickLimits {
            v_h: 3.0,
            v_up: 1.5,
            v_down: 1.0,
            yaw_dps: 30.0,
            a_acc: 1.0,
            a_brk: 2.0,
            jerk: 4.0,
            a_v: 0.6,
            jerk_v: 4.0,
            yaw_dps2: 60.0,
            expo_h: 0.25,
            expo_v: 0.3,
            expo_yaw: 0.2,
        },
        Profile::Normal => StickLimits {
            v_h: 7.0,
            v_up: 2.5,
            v_down: 1.5,
            yaw_dps: 90.0,
            a_acc: 2.0,
            a_brk: 2.5,
            jerk: 5.0,
            a_v: 1.0,
            jerk_v: 5.0,
            yaw_dps2: 180.0,
            expo_h: 0.25,
            expo_v: 0.3,
            expo_yaw: 0.2,
        },
        Profile::Sport => StickLimits {
            v_h: 10.0,
            v_up: 3.0,
            v_down: 2.0,
            yaw_dps: 150.0,
            a_acc: 2.5,
            a_brk: 2.5,
            jerk: 5.0,
            a_v: 1.0,
            jerk_v: 5.0,
            yaw_dps2: 270.0,
            expo_h: 0.4,
            expo_v: 0.4,
            expo_yaw: 0.3,
        },
    }
}

/// A profile's stick limits for an airframe class. The table is the open
/// multirotor's; a class without a row of its own (a VTOL in hover) flies
/// that row held to Cine's.
pub fn stick_limits(class: VehicleClass, p: Profile) -> StickLimits {
    match class {
        VehicleClass::Multirotor => multirotor_sticks(p),
        _ => multirotor_sticks(p).min(multirotor_sticks(Profile::Cine)),
    }
}

/// `lim` held to the flight controller's own guidance limits where it
/// reports them (`param` reads one): `WP_ACC` and `WP_ACC_Z` (m/s²; the
/// older `WPNAV_ACCEL` and `WPNAV_ACCEL_Z` in cm/s²), `PSC_NE_JERK` and
/// `PSC_D_JERK` (m/s³; the older `PSC_JERK_XY` and `PSC_JERK_Z`), and
/// `ATC_ACC_Y_MAX` (°/s², or the older `ATC_ACCEL_Y_MAX` in cdeg/s²; 0 is
/// no limit), and the yaw rate: GUIDED turns the heading target the pilot
/// sends no faster than `ATC_RATE_WPY_MAX` (°/s; the older `ATC_SLEW_YAW` in
/// cdeg/s), nor than `ATC_RATE_Y_MAX` (°/s, 0 none). Asking more than these,
/// the FC would shape the reference again.
pub fn fc_capped(mut lim: StickLimits, param: impl Fn(&str) -> Option<f32>) -> StickLimits {
    let pos = |v: Option<f32>| v.filter(|v| v.is_finite() && *v > 0.0);
    let either = |new: &str, old: &str, old_scale: f32| {
        pos(param(new)).or_else(|| pos(param(old)).map(|v| v * old_scale))
    };
    if let Some(a) = either("WP_ACC", "WPNAV_ACCEL", 0.01) {
        lim.a_acc = lim.a_acc.min(a);
        lim.a_brk = lim.a_brk.min(a);
    }
    if let Some(j) = either("PSC_NE_JERK", "PSC_JERK_XY", 1.0) {
        lim.jerk = lim.jerk.min(j);
    }
    if let Some(a) = either("WP_ACC_Z", "WPNAV_ACCEL_Z", 0.01) {
        lim.a_v = lim.a_v.min(a);
    }
    if let Some(j) = either("PSC_D_JERK", "PSC_JERK_Z", 1.0) {
        lim.jerk_v = lim.jerk_v.min(j);
    }
    // A value past 1000 is centidegrees (firmware before the unit change).
    let yaw = pos(param("ATC_ACC_Y_MAX"))
        .map(|v| if v > 1000.0 { v / 100.0 } else { v })
        .or_else(|| pos(param("ATC_ACCEL_Y_MAX")).map(|v| v / 100.0));
    if let Some(y) = yaw {
        lim.yaw_dps2 = lim.yaw_dps2.min(y);
    }
    let slew = either("ATC_RATE_WPY_MAX", "ATC_SLEW_YAW", 0.01);
    for y in [slew, pos(param("ATC_RATE_Y_MAX"))].into_iter().flatten() {
        lim.yaw_dps = lim.yaw_dps.min(y);
    }
    lim
}

/// The rate that closes `err` without overshoot at `rate_max`, slowing at no
/// more than `acc` (a square-root profile, linear within `acc`/`k`² of zero
/// so it settles without chatter), and never more than `err` in one `dt`.
/// `err` is a magnitude ≥ 0; so is the result.
pub fn sqrt_rate(err: f32, k: f32, acc: f32, rate_max: f32, dt: f32) -> f32 {
    let linear = acc / (k * k);
    let r = if err > linear {
        (2.0 * acc * (err - linear / 2.0)).sqrt()
    } else {
        k * err
    };
    r.min(rate_max).min(err / dt)
}

/// `(1 - e)·x + e·x³`.
pub fn expo(x: f32, e: f32) -> f32 {
    let x = x.clamp(-1.0, 1.0);
    (1.0 - e) * x + e * x * x * x
}

/// Below this speed a goal's or a hold's reference counts as stopped.
pub const HOLD_CAPTURE_MPS: f32 = 0.2;

/// A stick axis smaller than this is centred: every surface's own deadzone
/// (at least 0.04) is larger, so a tiny residual never creeps.
pub const CENTRED_AXIS: f32 = 0.02;

/// `x`, or 0 when it is centred ([`CENTRED_AXIS`]).
pub fn centred(x: f32) -> f32 {
    if x.abs() < CENTRED_AXIS {
        0.0
    } else {
        x
    }
}

/// Gain the stick reference settles on near its target, 1/s: a 0.25 s
/// time constant below `jerk`/16 m/s of the target (0.3 m/s at 5 m/s³).
/// A goal's own gain (jerk over acceleration, 2/s on Normal) would leave a
/// stick stop crawling for over a second below 0.5 m/s.
pub const SETTLE_GAIN: f32 = 4.0;

/// A stick reference asked to stop comes to rest from below this speed,
/// m/s, so a hold stands still.
pub const REST_MPS: f32 = 1e-3;

/// The limits the one shaper ([`Shaper::track`]) moves a reference's
/// horizontal velocity on: the sticks' ([`StickLimits::shape`]) or a goal's
/// ([`GoalLimits::shape`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShapeLimits {
    /// Acceleration while speeding up, and while slowing (a smaller or
    /// reversed target), m/s².
    pub a_acc: f32,
    pub a_brk: f32,
    /// Jerk, m/s³.
    pub jerk: f32,
    /// Gain the velocity settles on near its target, 1/s.
    pub settle: f32,
    /// How fast an acceleration above the limit now in force falls, m/s³
    /// (infinite: held to the limit at once).
    pub unwind: f32,
    /// Asked to stop, the reference comes to rest once slower than this,
    /// m/s, its acceleration within one jerk step of none (0: never).
    pub rest_mps: f32,
}

/// A 2-D velocity shaper: acceleration magnitude within the limits, change
/// of acceleration ≤ `jerk`·dt.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Shaper {
    pub v: V2,
    pub a: V2,
}

impl Shaper {
    pub fn reset(&mut self, v: V2) {
        self.v = v;
        self.a = [0.0, 0.0];
    }

    /// Advance one step toward `target` on `lim`: speeding up at `a_acc`
    /// and slowing at `a_brk`, the change of acceleration held to
    /// `jerk`·`dt`, never to zero at once. Near the target the acceleration
    /// falls off linearly on the `settle` gain (within `jerk`/`settle`² of
    /// it), so it settles without snapping. An acceleration above the limit
    /// now in force (a brake that ends in a new stick push) falls at
    /// `unwind`. A target of zero comes to rest below `rest_mps`.
    pub fn track(&mut self, target: V2, lim: &ShapeLimits, dt: f32) -> V2 {
        let slowing = geo::norm(target) < geo::norm(self.v) || geo::dot(target, self.v) < 0.0;
        let a_max = if slowing { lim.a_brk } else { lim.a_acc };
        let a_max = a_max.max(geo::norm(self.a) - lim.unwind * dt);
        let err = geo::sub(target, self.v);
        let e = geo::norm(err);
        let a_mag = sqrt_rate(e, lim.settle, lim.jerk, a_max, dt);
        let a_des = if e > 1e-6 {
            geo::scale(err, a_mag / e)
        } else {
            [0.0, 0.0]
        };
        let da = geo::clamp_len(geo::sub(a_des, self.a), lim.jerk * dt);
        self.a = geo::clamp_len(geo::add(self.a, da), a_max);
        self.v = geo::add(self.v, geo::scale(self.a, dt));
        if geo::norm(target) < 1e-6
            && geo::norm(self.v) < lim.rest_mps
            && geo::norm(self.a) <= lim.jerk * dt
        {
            self.reset([0.0, 0.0]);
        }
        self.v
    }
}

/// A 1-D [`Shaper`] (the stick reference's vertical speed).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Shaper1 {
    pub v: f32,
    pub a: f32,
}

impl Shaper1 {
    pub fn reset(&mut self, v: f32) {
        self.v = v;
        self.a = 0.0;
    }

    /// [`Shaper::track`] on one axis on the sticks' terms, at `a_max` both
    /// ways.
    pub fn track(&mut self, target: f32, a_max: f32, jerk: f32, dt: f32) -> f32 {
        let mut s = Shaper {
            v: [self.v, 0.0],
            a: [self.a, 0.0],
        };
        let lim = ShapeLimits {
            a_acc: a_max,
            a_brk: a_max,
            jerk,
            settle: SETTLE_GAIN,
            unwind: jerk,
            rest_mps: REST_MPS,
        };
        s.track([target, 0.0], &lim, dt);
        self.v = s.v[0];
        self.a = s.a[0];
        self.v
    }
}

/// What the sticks ask, after the centre threshold and the expo, before
/// shaping.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct StickAsk {
    /// Horizontal velocity, NED m/s.
    pub ne: V2,
    /// Down velocity, m/s (NED: positive descends).
    pub vd: f32,
    /// Yaw rate, rad/s (positive clockwise).
    pub yaw_rate: f32,
}

impl StickAsk {
    /// `axes` (`[roll, pitch, thr, yaw]` in [-1, 1]) at heading `yaw`.
    pub fn of(axes: [f32; 4], yaw: f32, lim: &StickLimits) -> Self {
        let [roll, pitch, thr, yaw_in] = axes.map(centred);
        let fwd = expo(pitch, lim.expo_h) * lim.v_h;
        let right = expo(roll, lim.expo_h) * lim.v_h;
        let t = expo(thr, lim.expo_v);
        let vd = if t > 0.0 {
            -t * lim.v_up
        } else {
            -t * lim.v_down
        };
        Self {
            // A diagonal full stick flies the horizontal limit, not √2 of it.
            ne: geo::clamp_len(geo::body_to_ned(fwd, right, yaw), lim.v_h),
            vd,
            yaw_rate: expo(yaw_in, lim.expo_yaw) * lim.yaw_dps.to_radians(),
        }
    }

    /// Nothing asked on any axis: the reference brakes to a hold.
    pub fn is_release(&self) -> bool {
        self.ne == [0.0, 0.0] && self.vd == 0.0 && self.yaw_rate == 0.0
    }
}

mod hold;
pub use hold::*;

#[cfg(test)]
mod tests;
