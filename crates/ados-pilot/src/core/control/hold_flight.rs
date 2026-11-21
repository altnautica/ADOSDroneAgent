//! The pilot's own holds and its landing while no operator stick session owns
//! the vehicle: a pause, a lease revoked, a link-loss hold, stale obstacle
//! data, an unverified position, a goto's arrival and the pilot's landing.
//! They fly the hold model ([`crate::model::Multirotor`]) on the profile
//! limits ([`crate::model::Limits`]), exactly as in 0.117; only the
//! operator's sticks fly the stick limits ([`super::sticks`]).

use ados_protocol::pilot::{AvoidMode, AvoidState, ControlMode, Owner, VehicleClass};

use super::{
    brake_limit, geofence, progress, Braked, FLOOR_AGL_M, HOLD_OBSTACLE_CLOSE, HOLD_OBSTACLE_STALE,
    LAND_SWITCH_AGL_M, STOPPED_MPS,
};
use crate::core::position_check::HOLD_POSITION_UNVERIFIED;
use crate::core::Pilot;
use crate::fc::{MASK_POS_VEL_YAW, MASK_VELOCITY};
use crate::geo::{self, V2};
use crate::model::{Command, StickIn};

impl Pilot {
    pub(super) fn fly_hold(
        &mut self,
        axes: [f32; 4],
        pos: [f32; 3],
        yaw: f32,
        dt: f32,
        now_ms: u64,
    ) {
        let mut lim = self.hold_limits(axes, now_ms);
        // An unverified position: a hold is a zero-velocity brake, never a
        // position hold on the estimate.
        let unverified = self.position_unverified(now_ms).is_some();
        if unverified {
            use crate::core::position_check::UNVERIFIED_MAX_MPS;
            lim.v_h = lim.v_h.min(UNVERIFIED_MAX_MPS);
            self.mr.shaper.v = geo::clamp_len(self.mr.shaper.v, lim.v_h);
        }
        let agl = self.agl(now_ms);
        // Unknown obstacles: nothing is asked (a landing still descends), and
        // with no scans there is nothing to push away from.
        let stale = self.obstacle_data_stale(now_ms);
        let mut axes = if !stale {
            axes
        } else if self.stale_on_empty_scans(now_ms) {
            [0.0, 0.0, axes[2], 0.0]
        } else {
            [0.0; 4]
        };
        if self.landing {
            axes = [axes[0], axes[1], -1.0, axes[3]];
            if agl.is_some_and(|a| a < LAND_SWITCH_AGL_M) {
                let m = self
                    .family()
                    .land(self.fc.vehicle_class() == VehicleClass::Vtol);
                self.set_mode(m, now_ms);
                self.set_owner(Owner::FcMode, "land");
                self.landing = false;
                self.land_handed_over();
                return;
            }
        }
        // Landing protection: full down held at the floor lands.
        let at_floor = agl.is_some_and(|a| a < LAND_SWITCH_AGL_M);
        if self
            .land_protect
            .update(axes[2] < -0.9 && at_floor, now_ms, 1500)
        {
            self.landing = true;
        }
        let mut vz = (-lim.v_up, lim.v_down);
        if let Some(a) = agl {
            if a < FLOOR_AGL_M && !self.landing {
                vz.1 = 0.0;
            }
            if self.landing {
                let vd = self.fc.fresh_local(now_ms).map_or(0.0, |l| l.vd);
                vz.1 = vz.1.min(self.land_descent_cap(a, vd));
            }
            if a >= self.settings.pilot.max_alt() {
                vz.0 = 0.0;
            }
        }
        let vel = self
            .fc
            .fresh_local(now_ms)
            .map_or([0.0; 3], |l| [l.vn, l.ve, l.vd]);
        let inp = StickIn {
            axes,
            pos,
            vel,
            yaw,
            dt,
        };
        let pos_ne = [pos[0], pos[1]];
        let unavailable = self.obstacle_data_unavailable(now_ms);
        let brake = self.settings.pilot.avoid != AvoidMode::Off && !stale && !unavailable;
        let home = self.fc.home_ne();
        let max_dist = self.settings.pilot.max_dist();
        let obstacles = &self.obstacles;
        let a_h = lim.a_h;
        let geom = self.brake_geom(self.settings.pilot.effective_margin_m());
        let mut braked: Option<Braked> = None;
        let limit = |v: V2| {
            let v = if brake {
                let b = brake_limit(obstacles, pos_ne, [vel[0], vel[1]], v, a_h, geom, now_ms);
                braked = Some(b);
                b.v
            } else {
                v
            };
            geofence(pos_ne, home, max_dist, v)
        };
        let cmd = self.mr.step(&inp, &lim, limit, vz);
        // Judged against what is asked, not the shaped velocity: the shaper
        // follows what flew, push included, so it may point back.
        let ask = if axes[0] == 0.0 && axes[1] == 0.0 {
            [0.0, 0.0]
        } else {
            crate::model::Multirotor::stick_velocity(axes, yaw, &lim)
        };
        self.brake_avoid = match braked {
            _ if stale => AvoidState::Stale,
            _ if unavailable => AvoidState::Unavailable,
            Some(b) if b.close || (b.cut && progress(ask, b.v) < STOPPED_MPS) => {
                AvoidState::Blocked
            }
            Some(b) if b.cut => AvoidState::Detour,
            _ => AvoidState::Clear,
        };
        let reason = if stale && !self.landing {
            Some(HOLD_OBSTACLE_STALE)
        } else if braked.is_some_and(|b| b.close) {
            Some(HOLD_OBSTACLE_CLOSE)
        } else if unverified {
            Some(HOLD_POSITION_UNVERIFIED)
        } else {
            None
        };
        let passing = |r: &str| {
            matches!(
                r,
                HOLD_OBSTACLE_CLOSE | HOLD_OBSTACLE_STALE | HOLD_POSITION_UNVERIFIED
            )
        };
        match reason {
            Some(r) => self.hold_reason = Some(r.into()),
            // Cleared: back to what a supervisor hold is about (a stall
            // waiting for the operator still holds), else no reason.
            None if self.hold_reason.as_deref().is_some_and(passing) => {
                self.hold_reason = self
                    .sup
                    .hold
                    .as_ref()
                    .filter(|_| self.supervised().is_some())
                    .map(|h| h.reason)
                    .filter(|r| !passing(r))
                    .map(Into::into);
            }
            None => {}
        }
        let tx = self.tx();
        let m = match cmd {
            Command::Hold { .. } if unverified => {
                self.fc_hold = None;
                // A tripped flow check measured the EKF's velocity off by
                // the flow's (an external position that stopped pins it
                // while the vehicle flies on), so zero on it is not zero
                // over the ground: ask minus that gap while it is tripped.
                let v = self
                    .flow_brake_ne(now_ms)
                    .map_or([0.0; 3], |v| [v[0], v[1], 0.0]);
                tx.setpoint(MASK_VELOCITY, [0.0; 3], v, 0.0, 0.0)
            }
            Command::Hold { pos, yaw } => {
                self.setpoint = Some(pos);
                // Outside `ados` no operator steers this hold (the axes are
                // zeroed): the FC's GUIDED keeps the target once given, so it
                // is sent once, and a ground station's reposition is not
                // overwritten on the next tick.
                if self.control_mode() != ControlMode::Ados {
                    let same = self
                        .fc_hold
                        .is_some_and(|h| h.iter().zip(pos).all(|(a, b)| (a - b).abs() < 0.05));
                    if same {
                        return;
                    }
                    self.fc_hold = Some(pos);
                }
                tx.setpoint(MASK_POS_VEL_YAW, pos, [0.0; 3], yaw, 0.0)
            }
            Command::Velocity { vel, yaw_rate } => {
                self.fc_hold = None;
                tx.setpoint(MASK_VELOCITY, [0.0; 3], vel, 0.0, yaw_rate)
            }
        };
        self.mav(m);
    }
}
