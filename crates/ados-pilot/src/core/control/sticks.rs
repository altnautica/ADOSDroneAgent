//! Stick flight: the operator's sticks fly the pilot's one shaped reference
//! ([`super::MotionRef`]) on the profile's stick limits.

use ados_protocol::pilot::{AvoidMode, AvoidState, ControlMode, Owner, VehicleClass};

use super::motion_ref::{leash_within, REF_LEASH_M, SHAPER_REPORT_MPS};
use super::{
    brake_limit, geofence, progress, FC_POS_GAIN, FLOOR_AGL_M, HOLD_OBSTACLE_CLOSE,
    HOLD_OBSTACLE_STALE, LAND_SWITCH_AGL_M, STOPPED_MPS,
};
use crate::core::position_check::HOLD_POSITION_UNVERIFIED;
use crate::core::{limit, trace, Pilot};
use crate::fc::{MASK_POS_VEL_ACC_YAW, MASK_VELOCITY};
use crate::geo;
use crate::model::StickAsk;

impl Pilot {
    /// The operator's sticks fly the pilot's one reference
    /// ([`super::MotionRef`]) on the stick limits: what they ask after the centre
    /// threshold and the expo is reached at the accelerate limit, and a
    /// released stick brakes to a hold at the brake limit, holding where the
    /// reference stops. The vertical speed and the yaw rate ramp and brake
    /// the same way on their own accelerations. Only a safety cut steps it:
    /// the obstacle brake, the geofence, a speed cap coming on, a height
    /// bound.
    pub(super) fn fly_sticks(
        &mut self,
        axes: [f32; 4],
        pos: [f32; 3],
        yaw: f32,
        dt: f32,
        now_ms: u64,
    ) {
        let mut lim = self.stick_limits(now_ms);
        // An unverified position: the sticks fly slowly, and a hold is a
        // velocity brake, never a position hold on the estimate.
        let unverified = self.position_unverified(now_ms).is_some();
        let capped = unverified || self.speed_capped(now_ms);
        let agl = self.agl(now_ms);
        // Unknown obstacles: the sticks ask nothing (a landing still
        // descends), and with no scans there is nothing to push away from.
        // Scans that arrive empty where a surface should be leave the
        // vertical stick live: climbing or descending out of the scan plane
        // is how that resolves, and the planar lidar never braked it.
        let stale = self.obstacle_data_stale(now_ms);
        let mut axes = if !stale {
            axes
        } else if self.stale_on_empty_scans(now_ms) {
            [0.0, 0.0, axes[2], 0.0]
        } else {
            [0.0; 4]
        };
        if self.landing {
            // The pilot's landing is no stick flight: it descends at the
            // profile's own descent speed, as every autonomous path flies.
            lim.v_down = self.limits_now(now_ms).v_down;
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
        let pos_ne = [pos[0], pos[1]];
        let unavailable = self.obstacle_data_unavailable(now_ms);
        let brake = self.settings.pilot.avoid != AvoidMode::Off && !stale && !unavailable;
        let ask = StickAsk::of(axes, yaw, &lim);
        let mut r = self.motion_ref(pos, yaw, now_ms);
        let mut limits = 0;
        // Horizontal. A speed cap that comes on mid-flight holds at once.
        if capped && geo::norm(r.sh.v) > lim.v_h + 1e-3 {
            r.sh.reset(geo::clamp_len(r.sh.v, lim.v_h));
            limits |= limit::NAV_CAP;
        }
        let shaped = r.sh.track_ab(ask.ne, lim.a_acc, lim.a_brk, lim.jerk, dt);
        if geo::dist(shaped, ask.ne) > SHAPER_REPORT_MPS {
            limits |= limit::SHAPER;
        }
        let braked = brake.then(|| {
            let geom = self.brake_geom(self.settings.pilot.effective_margin_m());
            let v2 = [vel[0], vel[1]];
            brake_limit(&self.obstacles, pos_ne, v2, shaped, lim.a_brk, geom, now_ms)
        });
        let mut v = braked.map_or(shaped, |b| b.v);
        if let Some(b) = braked {
            limits |= b.by;
        }
        // The speed the sticks may fly now: the seen-space brake holds them
        // below the profile when the space the lidar has seen free is short.
        let v_cap = braked
            .filter(|b| b.by & limit::SEEN_SPACE != 0)
            .map_or(lim.v_h, |b| lim.v_h.min(geo::norm(b.v)));
        let fenced = geofence(pos_ne, self.fc.home_ne(), self.settings.pilot.max_dist(), v);
        if geo::dist(fenced, v) > 1e-3 {
            limits |= limit::GEOFENCE;
            v = fenced;
        }
        if unverified || geo::norm(v) < geo::norm(shaped) - 1e-3 {
            // A cut that slows flies at once from the vehicle's own
            // position, and hands its velocity back to the reference:
            // leaving the obstacle is shaped again from what flew.
            r.cut(pos_ne, v);
        } else {
            if geo::dist(v, shaped) > 1e-3 {
                // The push back out of the margin (or a fence's slide)
                // moves the reference with it, so a hold walks out.
                r.sh.reset(v);
            }
            // The reference point moves on what flies, held near the vehicle
            // while it moves; standing still it is the hold point and stays,
            // wherever the vehicle drifts (or a ground station repositions
            // it outside `ados`). Capped, the FC's pull toward the reference
            // may add only what the cap leaves.
            if v != [0.0, 0.0] {
                let reach = if capped {
                    (lim.v_h - geo::norm(v)).clamp(0.0, REF_LEASH_M)
                } else {
                    REF_LEASH_M
                };
                r.p = leash_within(geo::add(r.p, geo::scale(v, dt)), pos_ne, reach);
            }
        }
        // Vertical (NED: down positive). A floor, a ceiling or the landing's
        // descent cap the reference is past cuts it at once, and its height
        // never lies beyond the vehicle's toward that bound. Faster than the
        // profile's own speed (a profile change, a skill's descent handed
        // over), it brakes on the vertical acceleration instead.
        if r.vert.v > vz.1 && vz.1 < lim.v_down {
            r.vert.reset(vz.1);
        } else if r.vert.v < vz.0 && vz.0 > -lim.v_up {
            r.vert.reset(vz.0);
        }
        let vd = if self.landing {
            // The landing's descent steps to its speed and its cap, as
            // before the sticks were shaped.
            let vd = ask.vd.clamp(vz.0, vz.1);
            r.vert.reset(vd);
            vd
        } else {
            r.vert
                .track(ask.vd.clamp(vz.0, vz.1), lim.a_v, lim.jerk_v, dt)
        };
        let mut d = r.d + vd * dt;
        if vz.1 <= 0.0 {
            d = d.min(pos[2]);
        }
        if vz.0 >= 0.0 {
            d = d.max(pos[2]);
        }
        if vd != 0.0 {
            r.d = d.clamp(pos[2] - REF_LEASH_M, pos[2] + REF_LEASH_M);
        } else {
            r.d = d;
        }
        // Heading: the yaw rate ramps and brakes on the yaw acceleration,
        // and the heading holds where the turn stops.
        let yaw_cmd = r.slew_rate(ask.yaw_rate, lim.yaw_dps2.to_radians(), dt);
        // Unverified, or landing: the height is flown as velocity only, never
        // a reference leading the vehicle.
        if unverified || self.landing {
            r.d = pos[2];
        }
        // A goal taking over goes on from the velocity flown.
        r.ask = r.sh.v;
        r.at_ms = now_ms;
        self.motion_ref = Some(r);

        // Judged against what the sticks ask, not the shaped velocity: the
        // reference follows what flew, push included, so it may point back.
        self.brake_avoid = match braked {
            _ if stale => AvoidState::Stale,
            _ if unavailable => AvoidState::Unavailable,
            Some(b) if b.close || (b.cut && progress(ask.ne, b.v) < STOPPED_MPS) => {
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
            Some(r) => {
                self.hold_reason = Some(r.into());
                limits |= limit::HOLD;
            }
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
        self.stick = Some(trace::StickTick {
            at_ms: now_ms,
            ask,
            v,
            a: r.sh.a,
            vd,
            yaw_rate: r.yaw_rate,
            cmd: geo::add(v, geo::scale(geo::sub(r.p, pos_ne), FC_POS_GAIN)),
            braking: ask.is_release() && !r.at_rest(),
            input_age_ms: self
                .failsafe
                .input_age_ms(now_ms)
                .map_or(0, |a| u32::try_from(a).unwrap_or(u32::MAX)),
            input_seq: self.sessions.last_seq().unwrap_or(0),
            limits,
            lim,
            v_cap,
        });

        let tx = self.tx();
        if unverified {
            self.fc_hold = None;
            let m = tx.setpoint(MASK_VELOCITY, [0.0; 3], [v[0], v[1], vd], 0.0, r.yaw_rate);
            self.mav(m);
            return;
        }
        let sp = [r.p[0], r.p[1], r.d];
        self.setpoint = Some(sp);
        // Outside `ados` no operator steers this hold (the axes are zeroed):
        // the FC's GUIDED keeps the target once given, so a reference at
        // rest is sent once, and a ground station's reposition is not
        // overwritten on the next tick.
        if self.control_mode() != ControlMode::Ados && r.at_rest() {
            let same = self
                .fc_hold
                .is_some_and(|h| h.iter().zip(sp).all(|(a, b)| (a - b).abs() < 0.05));
            if same {
                return;
            }
            self.fc_hold = Some(sp);
        } else {
            self.fc_hold = None;
        }
        let m = tx.setpoint_acc(
            MASK_POS_VEL_ACC_YAW,
            sp,
            [v[0], v[1], vd],
            [r.sh.a[0], r.sh.a[1], r.vert.a],
            yaw_cmd,
            0.0,
        );
        self.mav(m);
    }
}
