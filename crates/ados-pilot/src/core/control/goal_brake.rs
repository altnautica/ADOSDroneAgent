//! The stick brake applied to the pilot's own state, and its use on a goal
//! setpoint (skill or goto).

use ados_protocol::pilot::{AvoidMode, AvoidState};

use super::{brake_limit, progress, STOPPED_MPS};
use crate::core::Pilot;
use crate::geo::{self, V2};

impl Pilot {
    pub(super) fn brake_limit(&self, pos: [f32; 3], v: V2, now_ms: u64) -> V2 {
        if self.settings.pilot.avoid == AvoidMode::Off || self.obstacle_data_unavailable(now_ms) {
            return v;
        }
        let lim = self.limits_now(now_ms);
        let vel = self
            .fc
            .fresh_local(now_ms)
            .map_or([0.0; 2], |l| [l.vn, l.ve]);
        brake_limit(
            &self.obstacles,
            [pos[0], pos[1]],
            vel,
            v,
            lim.a_h,
            self.margin(),
            now_ms,
        )
        .v
    }

    /// The brake for a goal setpoint (skill or goto). With a position
    /// setpoint the FC flies the feed-forward plus its position loop's pull
    /// toward the target (`ask`), which the planner's `clear` never bounds by
    /// the stopping distance. So `ask`, and the velocity the vehicle flies
    /// now, are held to the stick brake's caps ([`brake_limit`]: seen space,
    /// obstacles, FC lag). When either breaks one, the braked `ask` within
    /// `v_cap` is returned, to fly from where the vehicle is; `None` leaves
    /// the planned setpoint.
    pub(super) fn goal_brake(&mut self, pos: V2, ask: V2, v_cap: f32, now_ms: u64) -> Option<V2> {
        if self.settings.pilot.avoid == AvoidMode::Off || self.obstacle_data_unavailable(now_ms) {
            return None;
        }
        let a_h = self.limits_now(now_ms).a_h;
        let margin = self.margin();
        let vel = self
            .fc
            .fresh_local(now_ms)
            .map_or([0.0; 2], |l| [l.vn, l.ve]);
        let asked = brake_limit(&self.obstacles, pos, vel, ask, a_h, margin, now_ms);
        let flying = brake_limit(&self.obstacles, pos, vel, vel, a_h, margin, now_ms);
        if !asked.cut && !flying.cut {
            return None;
        }
        let v = geo::clamp_len(asked.v, v_cap);
        self.brake_avoid = if progress(ask, v) < STOPPED_MPS {
            AvoidState::Blocked
        } else {
            AvoidState::Detour
        };
        Some(v)
    }
}
