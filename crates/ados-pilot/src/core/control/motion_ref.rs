//! The shaped reference the operator's sticks are flown on, and every brake
//! to a hold the pilot flies as their owner. The pilot keeps its own
//! reference point, velocity and acceleration on three axes (horizontal,
//! vertical and heading), moves them toward what the sticks ask at the
//! profile's stick limits ([`crate::model::StickLimits`]) and sends them
//! together every tick, so the FC's position loop tracks a smooth path
//! instead of a stepping target. A hold is the reference standing still.
//! Sticks taking over from a skill goal or a goto start from that goal's
//! reference ([`super::GoalRef`]), so its velocity and acceleration carry
//! over. Goals and gotos keep their own reference and limits. Only a safety
//! cut (the obstacle brake, a speed cap, a height bound or the geofence) may
//! step it.

use crate::core::{Pilot, TICK_MS};
use crate::geo::{self, V2};
use crate::model::{Shaper, Shaper1};

/// A reference not flown for longer than this starts again from the vehicle.
const REF_STALE_MS: u64 = 3 * TICK_MS;
/// Farthest the reference point may be from the vehicle, metres: the FC's
/// position loop then pulls at most about this many m/s toward it.
pub const REF_LEASH_M: f32 = 1.0;
/// The shaper is named among the limits when the velocity it flies is this
/// far from the one asked, m/s.
pub const SHAPER_REPORT_MPS: f32 = 0.05;

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
    /// The asked velocity the reference last took up.
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
    /// The reference to fly this tick: last tick's, a skill goal's or a
    /// goto's flown just before (its velocity, acceleration and heading
    /// carry over), or one standing at the vehicle with its velocity and
    /// heading when nothing flew one just before.
    pub(super) fn motion_ref(&self, pos: [f32; 3], yaw: f32, now_ms: u64) -> MotionRef {
        let fresh = |at_ms: u64| now_ms.saturating_sub(at_ms) <= REF_STALE_MS;
        let vel = self
            .fc
            .fresh_local(now_ms)
            .map_or([0.0; 3], |l| [l.vn, l.ve, l.vd]);
        match (self.motion_ref, self.goal_ref) {
            (Some(r), g) if fresh(r.at_ms) && g.is_none_or(|g| g.at_ms <= r.at_ms) => r,
            (_, Some(g)) if fresh(g.at_ms) => {
                let mut r = MotionRef::at(pos, vel, g.yaw, now_ms);
                r.p = g.p;
                r.sh = g.sh;
                r.ask = g.sh.v;
                r.yaw_rate = g.yaw_rate;
                r
            }
            _ => MotionRef::at(pos, vel, yaw, now_ms),
        }
    }
}
