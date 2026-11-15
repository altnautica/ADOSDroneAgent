//! The FC mission's actions: the router's mission state, pause and resume.

use ados_protocol::mavlink::ardupilotmega::MavCmd;
use ados_protocol::pilot::{Mission, Owner, VehicleClass};

use super::super::Pilot;
use super::ActionRes;
use crate::flows::LaunchStep;

impl Pilot {
    /// The router's mission state, or `None` while it is unknown. A
    /// launch whose after-takeoff action started the mission runs its "when
    /// done" on the transition to `complete`; a finished or removed mission
    /// leaves nothing to resume.
    pub(in crate::core) fn on_mission(&mut self, m: Option<Mission>, now_ms: u64) {
        let is = |m: &Option<Mission>, s: &str| m.as_ref().is_some_and(|m| m.state == s);
        let completed = is(&m, "complete") && !is(&self.mission, "complete");
        if is(&m, "complete") || is(&m, "none") {
            self.mission_paused = false;
        }
        self.sup.stall.on_mission(m.as_ref());
        self.mission = m;
        if completed
            && self
                .launch
                .as_ref()
                .is_some_and(|l| l.after == "mission" && l.step == LaunchStep::Done)
        {
            self.when_done(now_ms);
        }
    }

    /// A mission is waiting for `resume`: the router reports it paused (a
    /// pause in AUTO, or a hold mode mid-mission), or the pilot paused it
    /// (Pause, a stick takeover) and the router does not report it finished.
    fn mission_resumable(&self) -> bool {
        match self.mission.as_ref().map(|m| m.state.as_str()) {
            Some("paused") => true,
            Some("complete" | "none") => false,
            _ => self.mission_paused,
        }
    }

    pub(super) fn do_pause(&mut self, now_ms: u64) -> ActionRes {
        if let Some(l) = &self.launch {
            if l.step != LaunchStep::Done {
                self.launch = None;
                if self.fc.armed && self.fc.landed() {
                    let m = self.tx().arm(false, false);
                    self.mav(m);
                    return Ok(());
                }
            }
        }
        if !self.fc.armed {
            return Err("disarmed".into());
        }
        self.revoke("pause");
        match self.owner() {
            Owner::MissionFc => {
                let m = self
                    .tx()
                    .command(MavCmd::MAV_CMD_DO_PAUSE_CONTINUE, [0.0; 7]);
                self.mav(m);
                self.mission_paused = true;
                return Ok(());
            }
            Owner::Tuning => return Err("tuning".into()),
            // The FC flies (a ground station's reposition, a mode of its
            // own): the pilot takes the vehicle and brakes it to a hold here,
            // as it does for its own flight.
            Owner::FcMode | Owner::Idle => self.take_for_pause(now_ms)?,
            _ => {}
        }
        self.landing = false;
        self.brake_hold(now_ms, None);
        if self.fc.vehicle_class() == VehicleClass::Fixedwing || self.fc.vtol_forward() {
            self.fw = Default::default();
        }
        Ok(())
    }

    /// Pause while the FC flies: GUIDED, owner `pilot`. Refused (nothing
    /// sent) on the ground, without a position for the hold, and during an
    /// FC failsafe RTL or land, which the pilot never leaves on its own.
    fn take_for_pause(&mut self, now_ms: u64) -> ActionRes {
        if self.landed() {
            return Err("landed".into());
        }
        if self.fc_failsafe {
            return Err("failsafe".into());
        }
        if self.pos(now_ms).is_none() {
            return Err("no_position".into());
        }
        if !self.in_guided() {
            let g = self.family().guided();
            self.set_mode(g, now_ms);
        }
        self.set_owner(Owner::Pilot, "pause");
        self.sessions.arm_gate();
        Ok(())
    }

    /// Continue a paused mission: in AUTO (a pause there) with
    /// DO_PAUSE_CONTINUE, otherwise by putting the FC back in AUTO, where it
    /// carries on from the item it was on. Either way the FC flies it
    /// (`mission_fc`). Refused during an FC failsafe RTL or land: AUTO would
    /// pull the FC out of it; and in a confined place while the route from
    /// here is too tight (`route_infeasible`).
    pub(super) fn do_resume(&mut self, now_ms: u64) -> ActionRes {
        if !self.fc.armed {
            return Err("disarmed".into());
        }
        if self.fc_failsafe {
            return Err("failsafe".into());
        }
        // The FC's mission flies on the same unverified estimate; in a
        // confined place the leg from here needs a position to be checked.
        if self.auto_position_blocked(now_ms) {
            return Err("position_unverified".into());
        }
        if !self.mission_resumable() {
            return Err("nothing_paused".into());
        }
        if let Some(s) = self.mission.as_ref().and_then(|m| m.seq) {
            if !self.confined_auto_clear(s, now_ms) {
                tracing::warn!(target: "ados_pilot", seq = s, "pilot_resume_refused_route");
                return Err("route_infeasible".into());
            }
        }
        let fam = self.family();
        if fam.is_auto(self.fc.custom_mode) {
            let m = self.tx().command(
                MavCmd::MAV_CMD_DO_PAUSE_CONTINUE,
                [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            );
            self.mav(m);
        } else {
            self.set_mode(fam.auto(), now_ms);
        }
        if self.owner() != Owner::MissionFc {
            self.set_owner(Owner::MissionFc, "resume");
        }
        self.mission_paused = false;
        Ok(())
    }
}
