//! The pilot's answer to an FC position reset ([`crate::fc::pos_reset`]):
//! the estimate stepped while the vehicle stayed put, and ArduCopter's GUIDED
//! position submodes then fly the step to their target's old place in the new
//! frame. The pilot always moves what it keeps in the FC's local frame by the
//! step (its shaped reference, the hold model's point, a goal's hold, the
//! obstacle store), so its own setpoints stay where the vehicle is. Armed and
//! hovering in GUIDED (a multirotor, or a VTOL out of forward flight) it also
//!
//! * sends one zero-velocity setpoint: GUIDED leaves its position submode and
//!   starts its position controller again at the vehicle
//!   (`ModeGuided::velaccel_control_start`), dropping the moved target and
//!   the offset ArduPilot slews it in by; a velocity submode never moves on a
//!   reset, so with nobody streaming (owner `idle` or `fc_mode`) the FC holds
//!   there, and through any further reset;
//! * holds: a skill, a goto or the swarm gives way to the pilot's hold, and
//!   the pilot's own hold holds at the vehicle, with `hold_reason`
//!   [`HOLD_POSITION_RESET`]. The operator's sticks keep flying, from the
//!   moved reference;
//! * says so in the banner `pilot.banner.position_reset`.
//!
//! During a launch's arm and take-off steps the FC flies its own take-off
//! (or the pilot its lift-off), neither of which a reset moves: only the
//! pilot's frame moves, and a goto waiting after the take-off holds instead.

use ados_protocol::pilot::{Owner, VehicleClass};
use serde_json::json;

use super::Pilot;
use crate::fc::pos_reset::PosReset;
use crate::fc::MASK_VELOCITY;
use crate::flows::{banner, LaunchStep};
use crate::geo;

/// `hold_reason` while the pilot holds after an FC position reset.
pub const HOLD_POSITION_RESET: &str = "position-reset";

impl Pilot {
    /// Take a reset the FC state saw, if any.
    pub(super) fn check_pos_reset(&mut self, now_ms: u64) {
        if let Some(r) = self.fc.pos_reset.take() {
            self.on_pos_reset(r, now_ms);
        }
    }

    fn on_pos_reset(&mut self, r: PosReset, now_ms: u64) {
        let d = r.shift;
        let shift_m = geo::norm(d);
        self.rebase(d, now_ms);
        let owner = self.owner();
        let airborne = self.fc.armed && !self.fc.landed();
        // A plane's GUIDED flies a global loiter point no reset moves.
        let hovering = match self.fc.vehicle_class() {
            VehicleClass::Multirotor => true,
            VehicleClass::Vtol => !self.fc.vtol_forward(),
            VehicleClass::Fixedwing => false,
        };
        let acts = airborne && hovering && self.in_guided() && owner != Owner::Tuning;
        tracing::warn!(
            target: "ados_pilot",
            cause = r.cause.code(),
            shift_n = d[0],
            shift_e = d[1],
            shift_m,
            owner = ?owner,
            acts,
            "pilot_position_reset"
        );
        if !acts {
            return;
        }
        let taking_off = self.launch.as_mut().is_some_and(|l| {
            if l.after == "goto" {
                l.after = "hold".into();
                l.goto = None;
            }
            matches!(
                l.step,
                LaunchStep::Checks | LaunchStep::Arm | LaunchStep::Takeoff
            )
        });
        if !taking_off {
            let m = self
                .tx()
                .setpoint(MASK_VELOCITY, [0.0; 3], [0.0; 3], 0.0, 0.0);
            self.mav(m);
            match owner {
                Owner::Skill | Owner::Goto | Owner::Swarm => {
                    self.brake_hold(Some(HOLD_POSITION_RESET));
                }
                Owner::Pilot if self.sessions.owner.is_none() && !self.landing => {
                    if let Some(pos) = self.pos(now_ms) {
                        let yaw = self.yaw(now_ms);
                        self.mr.hold_here(pos, yaw);
                    }
                    self.hold_reason = Some(HOLD_POSITION_RESET.into());
                }
                _ => {}
            }
        }
        let shown = (shift_m * 10.0).round() / 10.0;
        self.banner = Some(banner(
            "pilot.banner.position_reset",
            &[("shift_m", json!(shown)), ("cause", json!(r.cause.code()))],
            now_ms,
        ));
    }

    /// Move what the pilot keeps in the FC's local frame by `d`, the step
    /// of the estimate. The planner plans again from the moved frame at once.
    fn rebase(&mut self, d: geo::V2, now_ms: u64) {
        let add3 = |p: &mut [f32; 3]| {
            p[0] += d[0];
            p[1] += d[1];
        };
        if let Some(r) = self.motion_ref.as_mut() {
            r.p = geo::add(r.p, d);
        }
        if let Some(h) = self.mr.hold_ne.as_mut() {
            *h = geo::add(*h, d);
        }
        for p in [
            self.plan_hold.as_mut(),
            self.stale_hold.as_mut(),
            self.setpoint.as_mut(),
        ]
        .into_iter()
        .flatten()
        {
            add3(p);
        }
        // The FC no longer holds the target given outside `ados`: give it
        // again.
        self.fc_hold = None;
        self.held_route.clear();
        self.obstacles.shift(d);
        if let Some(p) = self.pos_ne(now_ms) {
            self.obstacles.rebuild(p, now_ms);
        }
        self.plan_ms = 0;
    }
}
