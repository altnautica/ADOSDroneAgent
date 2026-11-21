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

use ados_protocol::mavlink::ardupilotmega::MavCmd;
use ados_protocol::pilot::{Owner, VehicleClass};
use serde_json::json;

use super::Pilot;
use crate::fc::pos_reset::PosReset;
use crate::fc::MASK_VELOCITY;
use crate::flows::{banner, LaunchStep};
use crate::geo;

/// HOME_POSITION's message id, asked for again after a reset.
const MSG_HOME_POSITION: f32 = 242.0;

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
        let shift_m = d.iter().map(|v| v * v).sum::<f32>().sqrt();
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
            shift_d = d[2],
            shift_m,
            owner = ?owner,
            acts,
            "pilot_position_reset"
        );
        if !acts {
            return;
        }
        // The FC's own take-off (GUIDED's take-off submode) and the
        // supervised lift-off's attitude targets fly no position target a
        // reset moves; the lift-off's climb after its hand-over does.
        let fc_takes_off = self.launch.as_mut().is_some_and(|l| {
            if l.after == "goto" {
                l.after = "hold".into();
                l.goto = None;
            }
            let positioned = l
                .supervised
                .as_ref()
                .and_then(|s| s.liftoff.as_ref())
                .is_some_and(|lo| lo.handed_over());
            matches!(
                l.step,
                LaunchStep::Checks | LaunchStep::Arm | LaunchStep::Takeoff
            ) && !positioned
        });
        if !fc_takes_off {
            let m = self
                .tx()
                .setpoint(MASK_VELOCITY, [0.0; 3], [0.0; 3], 0.0, 0.0);
            self.mav(m);
            // A height-only step (a rangefinder or barometer switch near the
            // ground) leaves every horizontal point where it was: the moved
            // heights are enough, and what flies goes on.
            let horizontal = r.shift_ne() != [0.0, 0.0];
            match owner {
                Owner::Skill | Owner::Goto | Owner::Swarm if horizontal => {
                    self.brake_hold(Some(HOLD_POSITION_RESET));
                }
                Owner::Pilot if horizontal && self.sessions.owner.is_none() && !self.landing => {
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

    /// Move what the pilot keeps in the FC's local frame by `d` (north,
    /// east, down), the step of the estimate, and ask the FC for its home
    /// again. The planner plans again from the moved frame at once.
    fn rebase(&mut self, d: [f32; 3], now_ms: u64) {
        let ne = [d[0], d[1]];
        let add3 = |p: &mut [f32; 3]| {
            for (v, s) in p.iter_mut().zip(d) {
                *v += s;
            }
        };
        if let Some(r) = self.motion_ref.as_mut() {
            r.p = geo::add(r.p, ne);
            r.d += d[2];
        }
        if let Some(h) = self.mr.hold_ne.as_mut() {
            *h = geo::add(*h, ne);
        }
        if let Some(h) = self.mr.hold_d.as_mut() {
            *h += d[2];
        }
        for p in [
            self.plan_hold.as_mut(),
            self.stale_hold.as_mut(),
            self.setpoint.as_mut(),
            // A goto flying on through a height-only step keeps its height
            // against the vehicle.
            self.goto.as_mut(),
            // Home keeps its place against the vehicle (the height above
            // it too) until the FC answers with its own.
            self.fc.home_local.as_mut(),
        ]
        .into_iter()
        .flatten()
        {
            add3(p);
        }
        if let Some(lo) = self
            .launch
            .as_mut()
            .and_then(|l| l.supervised.as_mut())
            .and_then(|s| s.liftoff.as_mut())
        {
            lo.rebase(d);
        }
        // The touchdown's floor, so a step on the ground is no climb, and the
        // ground under a subject, so the floor over them keeps its height.
        if let Some(f) = self.touchdown.floor_d.as_mut() {
            *f += d[2];
        }
        self.subject_track.rebase(d);
        self.sup.trail.shift(ne);
        if self.fc.home_local.is_some() {
            let m = self.tx().command(
                MavCmd::MAV_CMD_REQUEST_MESSAGE,
                [MSG_HOME_POSITION, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            );
            self.mav(m);
        }
        // The FC no longer holds the target given outside `ados`: give it
        // again.
        self.fc_hold = None;
        self.held_route.clear();
        self.obstacles.shift(ne);
        if let Some(p) = self.pos_ne(now_ms) {
            self.obstacles.rebuild(p, now_ms);
        }
        self.plan_ms = 0;
    }
}
