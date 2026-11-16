//! The shaped reference every pilot-flown motion is flown on: the operator's
//! sticks, a skill goal, a goto and every brake to a hold. The pilot keeps
//! its own reference point, velocity and acceleration on three axes
//! (horizontal, vertical and heading), moves them toward what is asked at
//! the profile's limits (the stick limits, [`crate::model::Limits`], or the
//! goal limits, [`crate::model::GoalLimits`]) and sends them together every
//! tick, so the FC's position loop tracks a smooth path instead of a
//! stepping target. A hold is the reference standing still. Handing motion
//! from one owner to another keeps the reference's velocity and
//! acceleration. Only a safety cut (the obstacle brake, the hard stop, stale
//! obstacle data, a speed cap, a height bound or the geofence) may step it.

use super::FC_POS_GAIN;
use crate::core::{Pilot, TICK_MS};
use crate::geo::{self, V2};
use crate::model::{goal_limits, sqrt_rate, GoalLimits, Shaper, Shaper1};

/// A reference not flown for longer than this starts again from the vehicle.
const REF_STALE_MS: u64 = 3 * TICK_MS;
/// Farthest the reference point may be from the vehicle, metres: the FC's
/// position loop then pulls at most about this many m/s toward it.
pub const REF_LEASH_M: f32 = 1.0;
/// Share of the goal acceleration a stop at a point is planned on; the rest
/// is room for the jerk ramp, so the shaped stop does not overshoot.
pub const APPROACH_DECEL_SHARE: f32 = 0.7;
/// The shaper is named among the limits when the velocity it flies is this
/// far from the one asked, m/s.
pub const SHAPER_REPORT_MPS: f32 = 0.05;
/// Gain of the heading slew's linear zone near its target, 1/s.
const YAW_GAIN: f32 = 2.0;
/// Time constant the asked velocity is smoothed on before the shaper, s.
/// Goals arrive at about 10 Hz and a noisy one reverses at that rate; a
/// jerk-limited shaper cannot follow a reversal within one update, and its
/// acceleration then cycles about an offset that it has no force to
/// recentre, so the vehicle wanders at up to the noise's speed. Smoothed
/// over 1.5 updates, the reversals shrink to what the shaper can follow,
/// and it tracks their mean.
pub const ASK_SMOOTH_S: f32 = 0.15;

/// The reference flown, in the FC's local NED.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct MotionRef {
    /// The reference point sent as the setpoint's horizontal position.
    pub p: V2,
    /// Its velocity and acceleration.
    pub sh: Shaper,
    /// The reference height sent (NED down, metres), and its down velocity
    /// and acceleration.
    pub d: f32,
    pub vert: Shaper1,
    /// The heading sent, radians, and its rate, rad/s.
    pub yaw: f32,
    pub yaw_rate: f32,
    /// The tick it was last flown.
    pub at_ms: u64,
    /// A goal's asked velocity, smoothed on [`ASK_SMOOTH_S`].
    pub ask: V2,
}

impl MotionRef {
    /// A reference standing at the vehicle's position `pos` and heading,
    /// moving at its velocity `vel` with no acceleration.
    pub fn at(pos: [f32; 3], vel: [f32; 3], yaw: f32, now_ms: u64) -> Self {
        let mut sh = Shaper::default();
        sh.reset([vel[0], vel[1]]);
        let mut vert = Shaper1::default();
        vert.reset(vel[2]);
        Self {
            p: [pos[0], pos[1]],
            sh,
            d: pos[2],
            vert,
            yaw,
            yaw_rate: 0.0,
            at_ms: now_ms,
            ask: sh.v,
        }
    }

    /// A safety cut: the reference jumps to point `p` flying `v`, with no
    /// acceleration; what flies next is shaped again from there.
    pub fn cut(&mut self, p: V2, v: V2) {
        self.p = p;
        self.sh.reset(v);
        self.ask = v;
    }

    /// Slew the heading toward `want` at the goal limits' rate and
    /// acceleration; with no heading wanted the turn slows to a stop.
    pub fn slew_yaw(&mut self, want: Option<f32>, lim: &GoalLimits, dt: f32) -> f32 {
        let acc = lim.yaw_dps2.to_radians();
        let rate = match want {
            Some(w) => {
                let err = geo::wrap_pi(w - self.yaw);
                let max = lim.yaw_dps.to_radians();
                sqrt_rate(err.abs(), YAW_GAIN, acc, max, dt).copysign(err)
            }
            None => 0.0,
        };
        self.slew_rate(rate, acc, dt)
    }

    /// Slew the heading's rate toward `rate` (rad/s) at no more than `acc`
    /// (rad/s²) and turn on it; a rate of zero brakes the turn, and the
    /// heading holds where it stops.
    pub fn slew_rate(&mut self, rate: f32, acc: f32, dt: f32) -> f32 {
        self.yaw_rate += (rate - self.yaw_rate).clamp(-acc * dt, acc * dt);
        self.yaw = geo::wrap_pi(self.yaw + self.yaw_rate * dt);
        self.yaw
    }

    /// Standing still on every axis: a hold.
    pub fn at_rest(&self) -> bool {
        self.sh.v == [0.0, 0.0]
            && self.sh.a == [0.0, 0.0]
            && self.vert.v == 0.0
            && self.vert.a == 0.0
            && self.yaw_rate == 0.0
    }
}

/// The velocity that flies to a point `to` away and slows into it at no
/// more than `a_dec`, at most `v_max`: the pilot's own pull toward a goal's
/// point, linear at [`FC_POS_GAIN`] near it, never past it in one `dt`.
pub fn approach(to: V2, a_dec: f32, v_max: f32, dt: f32) -> V2 {
    let e = geo::norm(to);
    if e < 1e-4 {
        return [0.0, 0.0];
    }
    geo::scale(to, sqrt_rate(e, FC_POS_GAIN, a_dec, v_max, dt) / e)
}

/// `p` held within [`REF_LEASH_M`] of the vehicle at `pos`.
pub fn leash(p: V2, pos: V2) -> V2 {
    leash_within(p, pos, REF_LEASH_M)
}

/// `p` held within `r` of `pos`.
pub fn leash_within(p: V2, pos: V2, r: f32) -> V2 {
    let d = geo::sub(p, pos);
    let n = geo::norm(d);
    if n <= r {
        p
    } else {
        geo::add(pos, geo::scale(d, r / n))
    }
}

impl Pilot {
    /// The reference to fly this tick: last tick's, whoever flew it, or one
    /// standing at the vehicle with its velocity and heading when nothing
    /// flew one just before.
    pub(super) fn motion_ref(&self, pos: [f32; 3], yaw: f32, now_ms: u64) -> MotionRef {
        match self.motion_ref {
            Some(r) if now_ms.saturating_sub(r.at_ms) <= REF_STALE_MS => r,
            _ => {
                let vel = self
                    .fc
                    .fresh_local(now_ms)
                    .map_or([0.0; 3], |l| [l.vn, l.ve, l.vd]);
                MotionRef::at(pos, vel, yaw, now_ms)
            }
        }
    }

    /// The profile's goal limits, the acceleration held to the stick
    /// limits in force (a speed cap's, the FC's).
    pub(super) fn goal_limits_now(&self, now_ms: u64) -> GoalLimits {
        let mut g = goal_limits(self.settings.pilot.profile);
        let l = self.limits_now(now_ms);
        g.a_h = g.a_h.min(l.a_acc);
        g.jerk = g.jerk.min(l.jerk);
        g
    }
}
