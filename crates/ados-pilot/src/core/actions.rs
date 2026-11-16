//! Named actions (C1 `action`) and the `launch` sequence (C10).

use ados_protocol::pilot::{AvoidMode, ControlMode, Owner, PlaceClass, Profile, VehicleClass};
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
mod liftoff;
mod mission;

/// Pilot actions a ground surface may send beyond the safety set.
pub const SURFACE_OPS: &[&str] = &["goto"];

impl Pilot {
    pub(super) fn action(&mut self, name: &str, args: Option<&Value>, now_ms: u64) -> ActionRes {
        if !self.fc.linked(now_ms)
            && !matches!(
                name.split(':').next(),
                Some("profile" | "control_mode" | "avoid" | "place_class" | "rc_receiver")
            )
        {
            return Err("no_fc".into());
        }
        self.take_superseded(name);
        // An operator's motion action replaces a low-battery SmartRTL
        // request still waiting on the FC: its Land fallback never runs over it.
        if matches!(
            name.split(':').next(),
            Some(
                "pause"
                    | "rth_cancel"
                    | "land"
                    | "rth"
                    | "kill"
                    | "resume"
                    | "take_control"
                    | "hand_to_fc"
                    | "goto"
                    | "launch"
                    | "control_mode"
                    | "vtol"
            )
        ) {
            self.drop_return();
        }
        // An operator safety action always ends a tuning flight first.
        if matches!(name, "pause" | "rth" | "land" | "kill" | "take_control")
            && self.owner() == Owner::Tuning
        {
            self.exit_tuning(now_ms);
        }
        // An operator stop while a lift-off is still on the ground spins it
        // down and disarms: there is nothing to hold or land yet.
        if matches!(name, "pause" | "land" | "rth" | "take_control") && self.liftoff_operator_stop()
        {
            return Ok(());
        }
        self.touchdown_refusal(name)?;
        // An operator answer ends a supervisor hold or stall; a held mission
        // then waits for `resume`.
        if matches!(name, "pause" | "take_control" | "hand_to_fc" | "kill")
            && self.end_supervision() == Some(super::control::Resume::Mission)
            && matches!(name, "pause" | "take_control")
        {
            self.mission_paused = true;
        }
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
                    supervised: None,
                },
                now_ms,
            ),
            "launch" => self.launch_from_args(args, now_ms),
            "land" => self.do_land(now_ms),
            "rth" => self.do_rth("operator", now_ms),
            "mission_skip" => self.mission_skip(true, now_ms),
            "goto" => self.surface_goto(args, now_ms),
            "pause" => {
                self.cancel_low_battery("pause");
                self.low_batt_cancelled = self.fc.armed;
                self.do_pause(now_ms)
            }
            "rth_cancel" => {
                self.cancel_low_battery("operator");
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
                // With no RC transmitter the control mode stays `ados`: the FC
                // flies its mode under the pilot's link-loss watch.
                if let Some(cm) = ControlMode::parse(to).filter(|c| *c != ControlMode::Ados) {
                    if self.settings.pilot.rc_receiver.is_present() {
                        self.persist_mode(cm);
                    }
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
        if let Some(err) = self.no_rc_refusal(kind, value) {
            return Err(err.into());
        }
        match kind {
            "rc_receiver" => self.set_rc_receiver(value),
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
            "place_class" => {
                let c: PlaceClass =
                    serde_json::from_value(json!(value)).map_err(|_| "bad_value")?;
                // The margin, route check and cap change with the class: never
                // while the vehicle is armed.
                if self.fc.armed {
                    return Err("armed".into());
                }
                self.settings.pilot.place_class = c;
                self.out.push(Out::Persist {
                    section: "pilot",
                    key: "place_class",
                    value: c.as_str().into(),
                });
                tracing::info!(target: "ados_pilot", place_class = c.as_str(), "pilot_place_class");
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
                    self.brake_hold(None);
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
        self.end_supervision();
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
                self.brake_hold(None);
                self.landing = true;
            }
        }
        Ok(())
    }

    /// The return: SmartRTL along the flown path, landing in place when the
    /// FC cannot fly it. Never a straight RTL on a copter.
    pub(super) fn do_rth(&mut self, reason: &str, now_ms: u64) -> ActionRes {
        if !self.fc.armed {
            return Err("disarmed".into());
        }
        self.revoke("rth");
        self.command_return(reason, now_ms);
        self.set_owner(Owner::FcMode, "rth");
        self.rth_deadline_ms = None;
        Ok(())
    }

    /// Take the vehicle as `pilot`. Outside GUIDED the FC is asked for it and
    /// the request waits on its answer ([`super::take`]): nothing changes
    /// hands until the FC confirms the mode.
    ///
    /// Refused during an FC failsafe RTL or land (`fc_failsafe`): the pilot
    /// never flies over the FC's own failsafe, and a GUIDED request there
    /// would cancel it. The operator changes the FC's mode first (the Mode
    /// sheet, or Hand to FC), which ends the failsafe state.
    fn take_control(&mut self, now_ms: u64) -> ActionRes {
        if self.take.pending() {
            return Err("pending".into());
        }
        if !self.fc.armed {
            return Err("disarmed".into());
        }
        if self.fc_failsafe {
            return Err("failsafe".into());
        }
        if self.owner() == Owner::Tuning {
            return Err("busy".into());
        }
        if self.control_mode() == ControlMode::Ados {
            self.ready_to_take(now_ms)?;
        }
        if self.in_guided() {
            self.took_control(now_ms);
        } else {
            let g = self.family().guided();
            self.set_mode(g, now_ms);
            self.take.start(now_ms, self.owner());
        }
        Ok(())
    }

    /// The FC flies GUIDED for a Take control: the operator flies now, and
    /// whatever the vehicle does next is theirs.
    pub(super) fn took_control(&mut self, now_ms: u64) {
        if self.owner() == Owner::MissionFc {
            self.mission_paused = true;
        }
        self.clear_touchdown();
        self.set_owner(Owner::Pilot, "take_control");
        self.engage_pilot(now_ms);
        self.sessions.arm_gate();
        let profile = format!("{:?}", self.settings.pilot.profile).to_uppercase();
        self.banner = Some(banner(
            "pilot.banner.ados_control",
            &[("profile", json!(profile))],
            now_ms,
        ));
    }

    /// Take control refuses while any of these items blocks, whatever else
    /// blocks before it in the list, and names the most basic one: a battery
    /// failsafe item never hides a missing position estimate.
    fn ready_to_take(&self, now_ms: u64) -> ActionRes {
        const REFUSES: [&str; 4] = ["firmware", "position", "fc_profile", "battery_failsafe"];
        let items = self.readiness(now_ms);
        let blocks = |id: &str| {
            items
                .iter()
                .any(|r| r.id == id && !r.ok && r.severity == ados_protocol::pilot::Severity::Block)
        };
        match REFUSES.iter().find(|id| blocks(id)) {
            Some(id) => Err(format!("not_ready:{id}")),
            None => Ok(()),
        }
    }

    /// Start flying as `pilot` from the current state: the reference flown
    /// just before, or one starting at the vehicle's position and velocity,
    /// brakes to a hold.
    pub(super) fn engage_pilot(&mut self, now_ms: u64) {
        let yaw = self.yaw(now_ms);
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
