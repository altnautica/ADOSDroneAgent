//! The shaped reference a skill goal or a goto is flown on. The pilot keeps
//! its own reference point, velocity and acceleration, moves the velocity
//! toward what the goal asks at the profile's goal limits
//! ([`crate::model::GoalLimits`]) and sends all three every tick, so the
//! FC's position loop tracks a smooth path instead of a stepping target.
//! Only a safety cut (the obstacle brake, the hard stop, stale obstacle
//! data, a speed cap or the geofence) may step it.

use super::FC_POS_GAIN;
use crate::core::{Pilot, TICK_MS};
use crate::geo::{self, V2};
use crate::model::{goal_limits, sqrt_rate, GoalLimits, Shaper};

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

/// The reference a goal is flown on, in the FC's local NED.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct GoalRef {
    /// The reference point sent as the setpoint's position.
    pub p: V2,
    /// Its velocity and acceleration.
    pub sh: Shaper,
    /// The heading sent, radians, and its rate, rad/s.
    pub yaw: f32,
    pub yaw_rate: f32,
    /// The tick it was last flown.
    pub at_ms: u64,
    /// The asked velocity, smoothed on [`ASK_SMOOTH_S`].
    pub ask: V2,
}

impl GoalRef {
    /// A safety cut: the reference jumps to point `p` flying `v`, with no
    /// acceleration.
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
        self.yaw_rate += (rate - self.yaw_rate).clamp(-acc * dt, acc * dt);
        self.yaw = geo::wrap_pi(self.yaw + self.yaw_rate * dt);
        self.yaw
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
    let d = geo::sub(p, pos);
    let n = geo::norm(d);
    if n <= REF_LEASH_M {
        p
    } else {
        geo::add(pos, geo::scale(d, REF_LEASH_M / n))
    }
}

impl Pilot {
    /// The reference to fly this tick: last tick's, or one starting at the
    /// vehicle with its velocity and heading when no goal flew just before.
    pub(super) fn goal_ref(&self, pos: V2, yaw: f32, now_ms: u64) -> GoalRef {
        match self.goal_ref {
            Some(r) if now_ms.saturating_sub(r.at_ms) <= REF_STALE_MS => r,
            _ => {
                let mut sh = Shaper::default();
                sh.reset(
                    self.fc
                        .fresh_local(now_ms)
                        .map_or([0.0; 2], |l| [l.vn, l.ve]),
                );
                GoalRef {
                    p: pos,
                    sh,
                    yaw,
                    yaw_rate: 0.0,
                    ask: sh.v,
                    at_ms: now_ms,
                }
            }
        }
    }

    /// The profile's goal limits, the acceleration held to a speed cap's.
    pub(super) fn goal_limits_now(&self, now_ms: u64) -> GoalLimits {
        let mut g = goal_limits(self.settings.pilot.profile);
        g.a_h = g.a_h.min(self.limits_now(now_ms).a_h);
        g
    }
}
