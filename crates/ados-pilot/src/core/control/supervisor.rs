//! The supervisor: while the FC flies a mission (AUTO) or a return (SMART_RTL,
//! RTL) under `ados`, the pilot still brakes for obstacles and watches for a
//! mission or return that stopped making progress ([`super::stall`]). A
//! stopped FC flight holds in GUIDED as `pilot` and resumes in the FC mode it
//! left once the way ahead is clear. Every return the pilot commands is a
//! SmartRTL along the flown path, and it lands in place when the FC cannot fly
//! one.

use ados_protocol::pilot::{AvoidMode, ControlMode, Owner};
use serde_json::json;

use super::super::Pilot;
use super::{
    brake_limit, max_abs, progress, HOLD_OBSTACLE_CLOSE, HOLD_OBSTACLE_STALE, PREEMPT_AXIS,
};
use crate::flows::banner;
use crate::geo::V2;
use crate::modes::{copter, Family};

/// The brake acts when the speed the obstacles allow along the track falls
/// this far below the speed flown, m/s.
const BRAKE_EXCESS_MPS: f32 = 0.3;
/// BRAKE gives way to the GUIDED hold once the vehicle is slower than this,
/// or `BRAKE_SETTLE_MS` after it was commanded.
const BRAKE_STOPPED_MPS: f32 = 0.3;
const BRAKE_SETTLE_MS: u64 = 1500;
/// A held FC flight resumes once the way ahead keeps this much more than the
/// margin for `RESUME_CLEAR_MS`.
const RESUME_EXTRA_M: f32 = 0.5;
const RESUME_CLEAR_MS: u64 = 3000;
/// The speed the clear-ahead check plans for, m/s.
const RESUME_PROBE_MPS: f32 = 2.0;
/// A commanded SmartRTL the FC does not fly within this long lands instead.
pub const RETURN_CONFIRM_MS: u64 = 1000;

/// The FC flight a supervisor hold resumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resume {
    Mission,
    Return,
}

#[derive(Debug, Clone)]
pub struct SupHold {
    pub resume: Resume,
    /// BRAKE was commanded at this time; the GUIDED hold follows.
    pub braking_ms: Option<u64>,
    /// The way ahead has been clear since.
    pub clear_since: Option<u64>,
    /// The FC flew its own failsafe return when the hold began: the resumed
    /// return is still that failsafe.
    pub failsafe_cause: Option<&'static str>,
    /// The route held the mission (an infeasible leg ahead): the route from
    /// here, not the way ahead, resumes it, even once a stall was raised.
    pub route: bool,
}

#[derive(Debug, Default)]
pub struct Supervisor {
    pub hold: Option<SupHold>,
    pub stall: super::stall::Stall,
    /// SmartRTL commanded (time, the FC mode it was asked from), not yet
    /// flying.
    pub return_req: Option<(u64, u32)>,
    /// The pending return is the low-battery action at this charge: its
    /// outcome is announced as such.
    pub return_battery_pct: Option<f32>,
    /// A resumed failsafe return re-asserts its cause on the mode change.
    pub restore_failsafe: Option<&'static str>,
}

impl Pilot {
    /// The FC flight the supervisor watches now, if any.
    pub(in crate::core) fn supervised(&self) -> Option<Resume> {
        if self.control_mode() != ControlMode::Ados
            || !self.fc.armed
            || self.fc.landed()
            || self.family() != Family::Copter
        {
            return None;
        }
        if let Some(h) = &self.sup.hold {
            return (self.owner() == Owner::Pilot).then_some(h.resume);
        }
        let mode = self.fc.custom_mode;
        match self.owner() {
            Owner::MissionFc if mode == copter::AUTO && !self.mission_paused => {
                Some(Resume::Mission)
            }
            Owner::FcMode if matches!(mode, copter::RTL | copter::SMART_RTL) => {
                Some(Resume::Return)
            }
            _ => None,
        }
    }

    /// Every tick after the replan.
    pub(super) fn supervise(&mut self, now_ms: u64) {
        self.check_return(now_ms);
        if self.sup.hold.is_some() && self.owner() != Owner::Pilot {
            // An action, a failsafe or the operator took the vehicle.
            self.end_supervision();
        }
        let Some(watch) = self.supervised() else {
            self.sup.stall.clear();
            return;
        };
        if self.sup.hold.is_some() && max_abs(self.raw_axes()) > PREEMPT_AXIS {
            // A stick takeover: the operator flies from here, and a held
            // mission waits for `resume`.
            if self.end_supervision() == Some(Resume::Mission) {
                self.mission_paused = true;
            }
            self.banner = Some(banner("pilot.banner.pilot_took_control", &[], now_ms));
            tracing::info!(target: "ados_pilot", "pilot_supervisor_takeover");
            return;
        }
        self.settle_brake(now_ms);
        self.watch_progress(watch, now_ms);
        if self.sup.hold.is_none() {
            self.route_hold(now_ms);
        }
        if self.sup.hold.is_none() {
            if let Some(w) = self.supervised() {
                self.auto_brake(w, now_ms);
            }
        } else {
            self.try_resume(now_ms);
        }
    }

    /// End a supervisor hold and any stall; returns what the hold would have
    /// resumed.
    pub(in crate::core) fn end_supervision(&mut self) -> Option<Resume> {
        let held = self.sup.hold.take();
        self.sup.stall.clear();
        if held.is_some()
            && matches!(
                self.hold_reason.as_deref(),
                Some(
                    HOLD_OBSTACLE_CLOSE
                        | HOLD_OBSTACLE_STALE
                        | super::stall::HOLD_MISSION_STALLED
                        | super::stall::HOLD_RETURN_BLOCKED
                        | super::route::HOLD_ROUTE_INFEASIBLE
                )
            )
        {
            self.hold_reason = None;
        }
        held.map(|h| h.resume)
    }

    /// Stop the FC flight: BRAKE, then hold in GUIDED as `pilot` with
    /// `reason`. A hold already running only takes the new reason.
    pub(super) fn supervise_hold(&mut self, resume: Resume, reason: &str, now_ms: u64) {
        if self.sup.hold.is_none() {
            let failsafe_cause = self.failsafe_cause.filter(|_| self.fc_failsafe);
            self.set_mode(copter::BRAKE, now_ms);
            self.set_owner(Owner::Pilot, reason);
            self.sessions.arm_gate();
            self.sup.hold = Some(SupHold {
                resume,
                braking_ms: Some(now_ms),
                clear_since: None,
                failsafe_cause,
                route: false,
            });
        }
        self.hold_reason = Some(reason.to_owned());
    }

    /// BRAKE until stopped (or for `BRAKE_SETTLE_MS`), then the GUIDED hold.
    fn settle_brake(&mut self, now_ms: u64) {
        let Some(t) = self.sup.hold.as_ref().and_then(|h| h.braking_ms) else {
            return;
        };
        let speed = self
            .fc
            .fresh_local(now_ms)
            .map_or(0.0, |l| l.vn.hypot(l.ve));
        let stopped = self.fc.custom_mode == copter::BRAKE && speed < BRAKE_STOPPED_MPS;
        if !stopped && now_ms.saturating_sub(t) < BRAKE_SETTLE_MS {
            return;
        }
        if let Some(h) = &mut self.sup.hold {
            h.braking_ms = None;
        }
        let reason = self.hold_reason.clone();
        self.set_mode(copter::GUIDED, now_ms);
        self.engage_pilot(now_ms);
        self.hold_reason = reason;
    }

    /// The nearest obstacle seen: distance and world bearing (deg).
    fn nearest_obstacle(&self, pos: V2, now_ms: u64) -> Option<(f32, f32)> {
        let yaw = self.yaw(now_ms);
        let sectors = self.obstacles.sectors(pos, yaw, now_ms);
        let step = 360.0 / sectors.len().max(1) as f32;
        sectors
            .iter()
            .enumerate()
            .filter_map(|(i, d)| d.map(|d| (d, i)))
            .min_by(|a, b| a.0.total_cmp(&b.0))
            .map(|(d, i)| {
                let b = (yaw.to_degrees() + (i as f32 + 0.5) * step).rem_euclid(360.0);
                (d, b)
            })
    }

    /// The proximity brake on the FC's own flight: the speed flown against
    /// what the obstacles allow (the stick brake's rule).
    fn auto_brake(&mut self, watch: Resume, now_ms: u64) {
        if self.settings.pilot.avoid == AvoidMode::Off || self.obstacle_data_state(now_ms).is_some()
        {
            return;
        }
        let Some(l) = self.fc.fresh_local(now_ms) else {
            return;
        };
        let pos = [l.n, l.e];
        let vel = [l.vn, l.ve];
        let a_h = self.limits_now(now_ms).a_h;
        let geom = self.brake_geom(self.settings.pilot.effective_margin_m());
        let b = brake_limit(&self.obstacles, pos, vel, vel, a_h, geom, now_ms);
        let speed = vel[0].hypot(vel[1]);
        if !(b.close || speed - progress(vel, b.v) > BRAKE_EXCESS_MPS) {
            return;
        }
        let (clearance_m, sector_deg) = self.nearest_obstacle(pos, now_ms).unzip();
        tracing::info!(
            target: "ados_pilot",
            fc_mode = %self.family().mode_name(self.fc.custom_mode),
            sector_deg,
            clearance_m,
            speed_ms = speed,
            "pilot_auto_brake"
        );
        self.supervise_hold(watch, HOLD_OBSTACLE_CLOSE, now_ms);
        self.banner = Some(banner("pilot.banner.obstacle_close", &[], now_ms));
    }

    /// The way the held flight goes next keeps the margin plus
    /// `RESUME_EXTRA_M`: toward the mission item, or home for a return.
    fn ahead_clear(&self, resume: Resume, now_ms: u64) -> bool {
        if self.obstacle_data_state(now_ms).is_some() {
            return false;
        }
        let Some(l) = self.fc.fresh_local(now_ms) else {
            return false;
        };
        let pos = [l.n, l.e];
        let margin = self.settings.pilot.effective_margin_m() + RESUME_EXTRA_M;
        let toward = match resume {
            Resume::Mission => self.mission_target_ned().map(|t| [t[0], t[1]]),
            Resume::Return => Some(self.fc.home_ne()),
        };
        let dir = toward
            .map(|t| [t[0] - pos[0], t[1] - pos[1]])
            .filter(|d| d[0].hypot(d[1]) > 0.5);
        match dir {
            Some(d) => {
                let k = RESUME_PROBE_MPS / d[0].hypot(d[1]);
                let ask = [d[0] * k, d[1] * k];
                let a_h = self.limits_now(now_ms).a_h;
                let geom = self.brake_geom(margin);
                let b = brake_limit(&self.obstacles, pos, [l.vn, l.ve], ask, a_h, geom, now_ms);
                !b.cut && !b.close
            }
            None => self
                .nearest_obstacle(pos, now_ms)
                .is_none_or(|(d, _)| d > margin),
        }
    }

    /// A hold resumes the FC flight it stopped: an obstacle hold (no stall)
    /// once the way ahead has kept clear for `RESUME_CLEAR_MS`; a route hold,
    /// stalled or not, once the route from here has read anything but
    /// infeasible for [`super::route::ROUTE_RESUME_DWELL_MS`]. Resuming ends
    /// the stall, its countdown and the hold reason.
    fn try_resume(&mut self, now_ms: u64) {
        let Some(h) = self.sup.hold.clone() else {
            return;
        };
        if h.braking_ms.is_some() || !self.in_guided() {
            return;
        }
        if h.route {
            // The route reads clear on stale scans too (no hits), and AUTO
            // brakes on none: resume only on fresh obstacle data.
            if self.obstacle_data_state(now_ms).is_some() || !self.route_resume_due() {
                return;
            }
        } else {
            if self.sup.stall.active.is_some() {
                return;
            }
            let clear_since = self
                .ahead_clear(h.resume, now_ms)
                .then(|| h.clear_since.unwrap_or(now_ms));
            if let Some(hh) = &mut self.sup.hold {
                hh.clear_since = clear_since;
            }
            if !clear_since.is_some_and(|t| now_ms.saturating_sub(t) >= RESUME_CLEAR_MS) {
                return;
            }
        }
        if h.resume == Resume::Mission {
            // The mission is not flown on an estimate nobody vouches for,
            // nor (confined) without a position or along a route from here
            // that is too tight.
            if self.auto_position_blocked(now_ms) {
                return;
            }
            let seq = self.mission.as_ref().and_then(|m| m.seq);
            if let Some(s) = seq {
                if !self.confined_auto_clear(s, now_ms) {
                    self.hold_route_infeasible(seq, now_ms);
                    return;
                }
            }
        }
        self.end_supervision();
        match h.resume {
            Resume::Mission => {
                self.set_mode(copter::AUTO, now_ms);
                self.set_owner(Owner::MissionFc, "supervisor-resume");
            }
            Resume::Return => {
                self.sup.restore_failsafe = h.failsafe_cause;
                self.set_mode(copter::SMART_RTL, now_ms);
                self.set_owner(Owner::FcMode, "supervisor-resume");
                self.sup.return_req = Some((now_ms, copter::GUIDED));
            }
        }
        tracing::info!(target: "ados_pilot", resume = ?h.resume, route = h.route, "pilot_supervisor_resume");
    }

    /// The mission item the FC flies, in local NED, when the router knows it.
    pub(super) fn mission_target_ned(&self) -> Option<[f32; 3]> {
        let m = self.mission.as_ref()?;
        let t = m.target.as_ref().filter(|t| Some(t.seq) == m.seq)?;
        t.ned.map(|n| [n[0] as f32, n[1] as f32, n[2] as f32])
    }

    /// The return: SmartRTL along the flown path on a copter (the FC lands
    /// when it has no path), the FC's RTL on a plane.
    pub(in crate::core) fn command_return(&mut self, reason: &str, now_ms: u64) {
        self.end_supervision();
        self.sup.return_battery_pct = None;
        let fam = self.family();
        if fam == Family::Copter {
            let asked_in = self.fc.custom_mode;
            self.set_mode(copter::SMART_RTL, now_ms);
            self.sup.return_req = Some((now_ms, asked_in));
            tracing::info!(target: "ados_pilot", mode = "smart_rtl", reason, "pilot_return");
        } else {
            self.set_mode(fam.rtl(), now_ms);
            tracing::info!(target: "ados_pilot", mode = "rtl", reason, "pilot_return");
        }
    }

    /// Drop a pending return request (an operator action took over).
    pub(in crate::core) fn drop_return(&mut self) {
        self.sup.return_req = None;
        self.sup.return_battery_pct = None;
    }

    /// The FC refused a mode command (a failed COMMAND_ACK).
    pub(in crate::core) fn mode_refused(&mut self, now_ms: u64) {
        if self.sup.return_req.is_some() && self.fc.custom_mode != copter::SMART_RTL {
            self.return_fallback("refused", now_ms);
        }
    }

    /// A commanded SmartRTL: the FC flying it ends the request (a
    /// low-battery return is announced then); the FC still in the mode it
    /// was asked from `RETURN_CONFIRM_MS` later lands in place; any other
    /// mode is someone else's (an operator, an FC failsafe) and is never
    /// overridden.
    fn check_return(&mut self, now_ms: u64) {
        let Some((t, asked_in)) = self.sup.return_req else {
            return;
        };
        if !self.fc.armed || self.fc.landed() {
            self.drop_return();
            return;
        }
        let mode = self.fc.custom_mode;
        if mode == copter::SMART_RTL {
            self.sup.return_req = None;
            if let Some(pct) = self.sup.return_battery_pct.take() {
                tracing::warn!(target: "ados_pilot", pct, action = "rth", taken = "smart_rtl", "pilot_low_battery_action");
                self.announce_low_battery("rth", "pilot.banner.low_battery_smart_rtl", pct, now_ms);
            }
        } else if mode != asked_in {
            tracing::info!(target: "ados_pilot", mode, reason = "superseded", "pilot_return");
            self.drop_return();
        } else if now_ms.saturating_sub(t) >= RETURN_CONFIRM_MS {
            self.return_fallback("no_mode_change", now_ms);
        }
    }

    fn return_fallback(&mut self, why: &str, now_ms: u64) {
        let battery = self.sup.return_battery_pct.take();
        self.sup.return_req = None;
        self.land_in_place(now_ms);
        tracing::warn!(target: "ados_pilot", mode = "land", reason = why, "pilot_return");
        match battery {
            Some(pct) => {
                tracing::warn!(target: "ados_pilot", pct, action = "land", taken = "land_no_smart_rtl", "pilot_low_battery_action");
                self.announce_low_battery(
                    "land",
                    "pilot.banner.low_battery_land_no_srtl",
                    pct,
                    now_ms,
                );
            }
            None => {
                self.banner = Some(banner("pilot.banner.return_land_fallback", &[], now_ms));
            }
        }
    }

    /// The FC lands where the vehicle is.
    pub(super) fn land_in_place(&mut self, now_ms: u64) {
        self.end_supervision();
        let m = self.family().land(false);
        self.set_mode(m, now_ms);
        self.set_owner(Owner::FcMode, "land");
        self.land_handed_over();
    }

    /// The `mission_stall` of the published state.
    pub(in crate::core) fn mission_stall_wire(
        &self,
        now_ms: u64,
    ) -> Option<ados_protocol::pilot::MissionStall> {
        let a = self.sup.stall.active.as_ref()?;
        Some(ados_protocol::pilot::MissionStall {
            kind: a.kind.as_str().to_owned(),
            seq: a.seq,
            wp_label: a.seq.map(|s| s.to_string()),
            since_ms: a.since_ms + self.epoch_offset_ms,
            skip_in_s: a
                .deadline_ms
                .map(|d| d.saturating_sub(now_ms) as f64 / 1000.0),
            window_s: f64::from(self.settings.pilot.stall_window_s),
        })
    }
}

/// Banner args helper for the waypoint label.
pub(super) fn wp_arg(seq: Option<u32>) -> serde_json::Value {
    json!(seq.map(|s| s.to_string()).unwrap_or_default())
}
