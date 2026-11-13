//! Named actions (C1 `action`) and the `launch` sequence (C10).

use ados_protocol::mavlink::ardupilotmega::MavCmd;
use ados_protocol::pilot::{AvoidMode, ControlMode, Mission, Owner, Profile, VehicleClass};
use serde_json::{json, Value};

use super::{Out, Pilot};
use crate::flows::{banner, Launch, LaunchStep};
use crate::modes::plane;
use crate::readiness;

/// Default take-off height for the `takeoff` primitive.
pub const TAKEOFF_ALT_M: f32 = 1.5;
/// Arming and mode changes must land within this long.
const STEP_TIMEOUT_MS: u64 = 5000;
/// A skill chosen as the after-takeoff action must take the lease this fast.
const SKILL_START_MS: u64 = 10_000;

type ActionRes = Result<(), String>;

mod goto;
mod launch;

/// Pilot actions a ground surface may send beyond the safety set.
pub const SURFACE_OPS: &[&str] = &["goto"];

impl Pilot {
    pub(super) fn action(&mut self, name: &str, args: Option<&Value>, now_ms: u64) -> ActionRes {
        if !self.fc.linked(now_ms)
            && !matches!(
                name.split(':').next(),
                Some("profile" | "control_mode" | "avoid")
            )
        {
            return Err("no_fc".into());
        }
        // An operator safety action always ends a tuning flight first.
        if matches!(name, "pause" | "rth" | "land" | "kill" | "take_control")
            && self.owner() == Owner::Tuning
        {
            self.exit_tuning(now_ms);
        }
        self.touchdown_refusal(name)?;
        if let Some((kind, value)) = name.split_once(':') {
            return self.set_option(kind, value, now_ms);
        }
        match name {
            "arm" => self.do_arm(now_ms),
            "disarm" => {
                if !self.fc.armed {
                    return Err("disarmed".into());
                }
                if !self.landed() {
                    return Err("not_landed".into());
                }
                let m = self.tx().arm(false, false);
                self.mav(m);
                Ok(())
            }
            "kill" => {
                let m = self.tx().arm(false, true);
                self.mav(m);
                self.launch = None;
                Ok(())
            }
            "takeoff" => self.start_launch(
                Launch {
                    step: LaunchStep::Arm,
                    after: "hold".into(),
                    skill: None,
                    goto: None,
                    alt_m: args
                        .and_then(|a| a.get("alt_m"))
                        .and_then(Value::as_f64)
                        .map_or(TAKEOFF_ALT_M, |v| (v as f32).clamp(1.0, 120.0)),
                    when_done: None,
                    error: None,
                    step_ms: now_ms,
                    cmd: None,
                },
                now_ms,
            ),
            "launch" => self.launch_from_args(args, now_ms),
            "land" => self.do_land(now_ms),
            "rth" => self.do_rth(now_ms),
            "goto" => self.surface_goto(args, now_ms),
            "pause" => {
                self.rth_deadline_ms = None;
                self.low_batt_cancelled = self.fc.armed;
                self.do_pause(now_ms)
            }
            "rth_cancel" => {
                self.rth_deadline_ms = None;
                self.low_batt_cancelled = true;
                Ok(())
            }
            "resume" => self.do_resume(now_ms),
            "take_control" => self.take_control(now_ms),
            "hand_to_fc" => {
                let mode = args
                    .and_then(|a| a.get("mode"))
                    .and_then(Value::as_str)
                    .unwrap_or("loiter");
                let fam = self.family();
                let m = fam.hand_mode(mode);
                self.set_mode(m, now_ms);
                self.set_owner(Owner::FcMode, "hand_to_fc");
                let to = args
                    .and_then(|a| a.get("control_mode"))
                    .and_then(Value::as_str)
                    .unwrap_or("fc_manual");
                if let Some(cm) = ControlMode::parse(to).filter(|c| *c != ControlMode::Ados) {
                    self.persist_mode(cm);
                }
                self.banner = Some(banner(
                    "pilot.banner.fc_control",
                    &[("mode", json!(fam.mode_name(m)))],
                    now_ms,
                ));
                Ok(())
            }
            _ => Err("unknown_action".into()),
        }
    }

    fn set_option(&mut self, kind: &str, value: &str, now_ms: u64) -> ActionRes {
        match kind {
            "profile" => {
                let p: Profile = serde_json::from_value(json!(value)).map_err(|_| "bad_value")?;
                self.settings.pilot.profile = p;
                self.out.push(Out::Persist {
                    section: "pilot",
                    key: "profile",
                    value: value.into(),
                });
                Ok(())
            }
            "avoid" => {
                let a: AvoidMode = serde_json::from_value(json!(value)).map_err(|_| "bad_value")?;
                self.settings.pilot.avoid = a;
                self.out.push(Out::Persist {
                    section: "pilot",
                    key: "avoid",
                    value: value.into(),
                });
                Ok(())
            }
            "control_mode" => {
                let cm = ControlMode::parse(value).ok_or("bad_value")?;
                self.revoke("control-mode");
                if cm == ControlMode::Ados && self.fc.armed && self.owner() == Owner::Pilot {
                    self.sessions.arm_gate();
                }
                self.persist_mode(cm);
                if cm != ControlMode::Ados && self.owner() == Owner::Pilot {
                    self.brake_hold(now_ms, None);
                }
                Ok(())
            }
            "vtol" => {
                if self.fc.vehicle_class() != VehicleClass::Vtol {
                    return Err("vehicle_class".into());
                }
                // ArduPlane only accepts DO_VTOL_TRANSITION in AUTO, so the
                // transition is a mode change: CRUISE flies forward, QLOITER hovers.
                let m = if value == "fw" {
                    plane::CRUISE
                } else {
                    plane::QLOITER
                };
                self.set_mode(m, now_ms);
                self.set_owner(Owner::FcMode, "vtol-transition");
                Ok(())
            }
            _ => Err("unknown_action".into()),
        }
    }

    fn persist_mode(&mut self, cm: ControlMode) {
        self.settings.control.mode = cm;
        self.out.push(Out::Persist {
            section: "control",
            key: "mode",
            value: cm.as_str().into(),
        });
    }

    fn ready_to_arm(&self, now_ms: u64) -> ActionRes {
        if self.control_mode() != ControlMode::Ados {
            return Ok(());
        }
        let items = self.readiness(now_ms);
        match readiness::first_block(&items) {
            Some(r) => Err(format!("not_ready:{}", r.id)),
            None => Ok(()),
        }
    }

    fn do_arm(&mut self, now_ms: u64) -> ActionRes {
        if self.fc.armed {
            return Err("armed".into());
        }
        self.ready_to_arm(now_ms)?;
        let m = self.tx().arm(true, false);
        self.mav(m);
        Ok(())
    }

    /// The router's mission state, or `None` while it is unknown. A
    /// launch whose after-takeoff action started the mission runs its "when
    /// done" on the transition to `complete`; a finished or removed mission
    /// leaves nothing to resume.
    pub(super) fn on_mission(&mut self, m: Option<Mission>, now_ms: u64) {
        let is = |m: &Option<Mission>, s: &str| m.as_ref().is_some_and(|m| m.state == s);
        let completed = is(&m, "complete") && !is(&self.mission, "complete");
        if is(&m, "complete") || is(&m, "none") {
            self.mission_paused = false;
        }
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

    pub(super) fn skill_ended(&mut self, now_ms: u64) {
        if self
            .launch
            .as_ref()
            .is_some_and(|l| l.after == "skill" && l.step == LaunchStep::Done)
        {
            self.when_done(now_ms);
        }
    }

    pub(super) fn on_skill_granted(&mut self, now_ms: u64) {
        let _ = now_ms;
        self.hold_reason = None;
    }

    pub(super) fn do_land(&mut self, now_ms: u64) -> ActionRes {
        if !self.fc.armed {
            return Err("disarmed".into());
        }
        self.revoke("land");
        let fam = self.family();
        match self.fc.vehicle_class() {
            VehicleClass::Fixedwing => {
                self.set_mode(fam.rtl(), now_ms);
                self.set_owner(Owner::FcMode, "land");
            }
            VehicleClass::Vtol if self.fc.vtol_forward() => {
                self.set_mode(plane::QRTL, now_ms);
                self.set_owner(Owner::FcMode, "land");
            }
            // The FC is flying (a mission, or a mode of its own): it lands.
            class if matches!(self.owner(), Owner::MissionFc | Owner::FcMode) => {
                let m = fam.land(class == VehicleClass::Vtol);
                self.set_mode(m, now_ms);
                self.set_owner(Owner::FcMode, "land");
                self.land_handed_over();
            }
            _ => {
                self.brake_hold(now_ms, None);
                self.landing = true;
            }
        }
        Ok(())
    }

    pub(super) fn do_rth(&mut self, now_ms: u64) -> ActionRes {
        if !self.fc.armed {
            return Err("disarmed".into());
        }
        self.revoke("rth");
        let m = self.family().rtl();
        self.set_mode(m, now_ms);
        self.set_owner(Owner::FcMode, "rth");
        self.rth_deadline_ms = None;
        Ok(())
    }

    fn do_pause(&mut self, now_ms: u64) -> ActionRes {
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
    /// (`mission_fc`).
    fn do_resume(&mut self, now_ms: u64) -> ActionRes {
        if !self.fc.armed {
            return Err("disarmed".into());
        }
        if !self.mission_resumable() {
            return Err("nothing_paused".into());
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

    fn take_control(&mut self, now_ms: u64) -> ActionRes {
        if !self.fc.armed {
            return Err("disarmed".into());
        }
        if self.owner() == Owner::Tuning {
            return Err("busy".into());
        }
        if self.control_mode() == ControlMode::Ados {
            self.ready_to_take(now_ms)?;
        }
        if self.owner() == Owner::MissionFc {
            self.mission_paused = true;
        }
        let g = self.family().guided();
        if !self.in_guided() {
            self.set_mode(g, now_ms);
        }
        self.set_owner(Owner::Pilot, "take_control");
        self.engage_pilot(now_ms);
        self.sessions.arm_gate();
        let profile = format!("{:?}", self.settings.pilot.profile).to_uppercase();
        self.banner = Some(banner(
            "pilot.banner.ados_control",
            &[("profile", json!(profile))],
            now_ms,
        ));
        Ok(())
    }

    fn ready_to_take(&self, now_ms: u64) -> ActionRes {
        let items = self.readiness(now_ms);
        match items.iter().find(|r| {
            !r.ok && r.severity == ados_protocol::pilot::Severity::Block && r.id != "fc_link"
        }) {
            Some(r) if r.id == "fc_profile" || r.id == "firmware" || r.id == "position" => {
                Err(format!("not_ready:{}", r.id))
            }
            _ => Ok(()),
        }
    }

    /// Start flying as `pilot` from the current state (hold here).
    pub(super) fn engage_pilot(&mut self, now_ms: u64) {
        let p = self.fc.fresh_local(now_ms);
        let yaw = self.yaw(now_ms);
        self.mr
            .engage(p.map(|p| [p.vn, p.ve]).unwrap_or([0.0, 0.0]));
        if let Some(p) = p {
            self.mr.hold_here([p.n, p.e, p.d], yaw);
        }
        let lim = crate::fixedwing::PlaneLimits::from_params(|n| self.fc.params.get(n).copied());
        let alt = self
            .fc
            .ekf_height(now_ms)
            .unwrap_or(self.settings.pilot.min_agl_fw_m);
        self.fw.engage(yaw.to_degrees(), alt, lim.airspeed_cruise);
        self.hold_reason = None;
        self.landing = false;
        self.fc_hold = None;
    }
}
