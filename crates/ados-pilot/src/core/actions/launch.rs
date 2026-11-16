//! The `launch` sequence (C10): arm, take off, then the after-takeoff and
//! "when done" actions.

use ados_protocol::mavlink::ardupilotmega::MavCmd;
use ados_protocol::pilot::{Owner, PlaceClass, Severity, VehicleClass};
use serde_json::{json, Value};

use super::{ActionRes, SKILL_START_MS, STEP_TIMEOUT_MS};
use crate::core::Pilot;
use crate::flows::{banner, Launch, LaunchStep};
use crate::modes::{plane, Family};

impl Pilot {
    pub(super) fn launch_from_args(&mut self, args: Option<&Value>, now_ms: u64) -> ActionRes {
        let a = args.ok_or("bad_args")?;
        let after = a
            .get("after")
            .and_then(Value::as_str)
            .unwrap_or("hold")
            .to_owned();
        if !matches!(after.as_str(), "hold" | "skill" | "mission" | "goto") {
            return Err("bad_args".into());
        }
        let skill = a.get("skill").and_then(|s| {
            Some((
                s.get("plugin_id")?.as_str()?.to_owned(),
                s.get("skill_id")?.as_str()?.to_owned(),
            ))
        });
        let goto = a.get("goto").and_then(|g| {
            Some([
                g.get("n")?.as_f64()? as f32,
                g.get("e")?.as_f64()? as f32,
                g.get("d")?.as_f64()? as f32,
            ])
        });
        if after == "skill" && skill.is_none() || after == "goto" && goto.is_none() {
            return Err("bad_args".into());
        }
        // A launch into the mission flies all of it: an earlier flight's skips
        // are forgotten before the route is checked (an airborne launch is
        // refused below and forgets nothing).
        if after == "mission" && (!self.fc.armed || self.fc.landed()) {
            self.sup.stall.new_flight();
        }
        // A confined place flies a margin from the vehicle's outer extent:
        // without the radius there is none; a mission is flown only along a
        // route that keeps it.
        if self.settings.pilot.place_class == PlaceClass::Confined {
            if self.settings.pilot.prop_tip_radius_m.is_none() {
                tracing::warn!(target: "ados_pilot", reason = "no_prop_radius", "pilot_launch_refused");
                return Err("no_prop_radius".into());
            }
            // The route is checked only on a launch nothing more basic
            // refuses: without a position the router places no item, and
            // the refusal names the missing position, not the route.
            if after == "mission" {
                self.launch_refusal(true, now_ms)?;
                self.route_launch_check(now_ms)?;
            }
        }
        // With no RC transmitter the FC's mission must keep inside the
        // pilot's caps, after the more basic refusals.
        if after == "mission" && !self.rc_receiver().is_present() {
            self.launch_refusal(true, now_ms)?;
            self.no_rc_route_check()?;
        }
        let default_alt = match self.fc.vehicle_class() {
            VehicleClass::Fixedwing => self.fc.params.get("TKOFF_ALT").copied().unwrap_or(50.0),
            _ => 3.0,
        };
        let alt_m = a
            .get("alt_m")
            .and_then(Value::as_f64)
            .map(|v| v as f32)
            .unwrap_or(default_alt);
        // With no RC transmitter the climb stops at the height cap.
        let alt_m = if self.rc_receiver().is_present() {
            alt_m
        } else {
            alt_m.min(self.settings.pilot.max_alt())
        };
        let when_done = a
            .get("when_done")
            .and_then(Value::as_str)
            .map(str::to_owned);
        self.start_launch(
            Launch {
                step: LaunchStep::Checks,
                after,
                skill,
                goto,
                alt_m,
                when_done,
                error: None,
                step_ms: now_ms,
                cmd: None,
                supervised: None,
            },
            now_ms,
        )
    }

    pub(super) fn start_launch(&mut self, l: Launch, now_ms: u64) -> ActionRes {
        self.launch_refusal(l.step == LaunchStep::Checks || !self.fc.armed, now_ms)?;
        let mut l = Launch {
            step: LaunchStep::Arm,
            step_ms: now_ms,
            cmd: None,
            supervised: self
                .supervised_liftoff_applies()
                .then(crate::liftoff::Supervision::default),
            ..l
        };
        // A deliberate take-off ends a touchdown.
        self.touchdown = Default::default();
        self.arm_for_launch(&mut l, now_ms);
        self.launch = Some(l);
        Ok(())
    }

    /// Why a launch is refused before anything is sent: airborne, in an FC
    /// failsafe, with the position unverified, or (`ready`) while a
    /// blocking readiness item fails.
    fn launch_refusal(&self, ready: bool, now_ms: u64) -> ActionRes {
        if self.fc.armed && !self.fc.landed() {
            return Err("airborne".into());
        }
        // Armed on the ground in an FC failsafe land (before its disarm): a
        // launch would switch the FC to GUIDED and climb out of it.
        if self.fc_failsafe {
            return Err("failsafe".into());
        }
        if self.position_unverified(now_ms).is_some() {
            return Err("position_unverified".into());
        }
        if ready {
            self.ready_to_arm(now_ms)?;
        }
        // An airframe without the motor headroom to lift off level is
        // refused in every control mode, not only where readiness gates.
        if self.supervised_liftoff_applies() {
            if let Some(r) = self
                .airframe
                .readiness(&self.fc)
                .filter(|r| !r.ok && r.severity == Severity::Block)
            {
                return Err(format!("not_ready:{}", r.id));
            }
        }
        Ok(())
    }

    /// The mode a launch arms and climbs in: Plane TAKEOFF for fixed-wing,
    /// GUIDED otherwise. The FC refuses to arm in some modes (RTL, where a
    /// plane boots), so this mode is selected before arming.
    fn launch_mode(&self) -> u32 {
        if self.fc.vehicle_class() == VehicleClass::Fixedwing {
            plane::TAKEOFF
        } else {
            self.family().guided()
        }
    }

    /// While disarmed: set the FC up for a supervised lift-off, select the
    /// launch mode, then arm once the FC reports it; each command re-sent at
    /// most once a second.
    fn arm_for_launch(&mut self, l: &mut Launch, now_ms: u64) {
        if self.fc.armed {
            return;
        }
        if l.error.is_some() {
            return;
        }
        match self.liftoff_prepared(l, now_ms) {
            Ok(true) => {}
            Ok(false) => return,
            Err(e) => {
                l.error = Some(e);
                l.step_ms = now_ms;
                return;
            }
        }
        let arm = self.fc.custom_mode == self.launch_mode();
        if l.cmd
            .is_some_and(|(a, t)| a == arm && now_ms.saturating_sub(t) < 1000)
        {
            return;
        }
        l.cmd = Some((arm, now_ms));
        if arm {
            let m = self.tx().arm(true, false);
            self.mav(m);
        } else {
            self.set_mode(self.launch_mode(), now_ms);
        }
    }

    /// Advance a running launch sequence (called every tick).
    pub(in crate::core) fn drive_launch(&mut self, now_ms: u64) {
        let Some(mut l) = self.launch.take() else {
            return;
        };
        let class = self.fc.vehicle_class();
        let elapsed = now_ms.saturating_sub(l.step_ms);
        match l.step {
            LaunchStep::Checks => {
                l.step = LaunchStep::Arm;
                l.step_ms = now_ms;
            }
            LaunchStep::Arm => {
                if self.fc.armed {
                    let mode = self.launch_mode();
                    if self.fc.custom_mode != mode {
                        self.set_mode(mode, now_ms);
                    }
                    l.step = LaunchStep::Takeoff;
                    l.step_ms = now_ms;
                } else if elapsed > 2 * STEP_TIMEOUT_MS {
                    l.error = Some(if self.fc.custom_mode == self.launch_mode() {
                        "arm_refused".into()
                    } else {
                        "mode_refused".into()
                    });
                } else {
                    self.arm_for_launch(&mut l, now_ms);
                }
            }
            LaunchStep::Takeoff if l.supervised.is_some() => {
                self.drive_supervised_takeoff(&mut l, elapsed, now_ms);
            }
            LaunchStep::Takeoff => {
                let height = self.agl(now_ms).unwrap_or(0.0);
                if class != VehicleClass::Fixedwing
                    && self.in_guided()
                    && elapsed < STEP_TIMEOUT_MS * 4
                {
                    // NAV_TAKEOFF once GUIDED is confirmed; re-sent each second until climbing.
                    if height < 0.3 && elapsed % 1000 < crate::core::TICK_MS {
                        let m = self.tx().command(
                            MavCmd::MAV_CMD_NAV_TAKEOFF,
                            [0.0, 0.0, 0.0, f32::NAN, 0.0, 0.0, l.alt_m],
                        );
                        self.mav(m);
                    }
                }
                if height >= 0.9 * l.alt_m {
                    l.step = LaunchStep::After;
                    l.step_ms = now_ms;
                    self.launch_after(&mut l, now_ms);
                } else if !self.fc.armed {
                    l.error = Some("disarmed".into());
                } else if elapsed > 60_000 {
                    l.error = Some("takeoff_timeout".into());
                }
            }
            LaunchStep::After => {
                if l.after == "skill" {
                    let started = self.auth.lease().is_some_and(|ls| {
                        l.skill.as_ref().is_some_and(|(p, s)| {
                            &ls.holder.plugin_id == p && &ls.holder.skill_id == s
                        })
                    });
                    if started {
                        l.step = LaunchStep::Done;
                        l.step_ms = now_ms;
                    } else if elapsed > SKILL_START_MS {
                        l.error = Some("skill_not_started".into());
                    }
                }
            }
            LaunchStep::Done => {
                if elapsed > 5000 && l.when_done.is_none() {
                    return;
                }
            }
        }
        if let Some(e) = &l.error {
            // A lift-off stopped on the ground has spun down and disarmed
            // (or handed the FC its Land): no hold is flown over it.
            let stopped = l
                .supervised
                .as_ref()
                .and_then(|s| s.liftoff.as_ref())
                .is_some_and(|lo| lo.stopped().is_some());
            let reason = Self::liftoff_view(&l)
                .filter(|v| v.phase == "stopped")
                .map(|v| format!("{}. {}", v.text, v.detail))
                .or_else(|| crate::liftoff::error_text(e).map(str::to_owned))
                .unwrap_or_else(|| e.clone());
            self.banner = Some(banner(
                "pilot.banner.launch_failed",
                &[("reason", json!(reason))],
                now_ms,
            ));
            if self.fc.armed && !self.fc.landed() && !stopped {
                self.brake_hold(None);
            }
            // From the step's start, or from the error where it restarts it.
            if l.step == LaunchStep::Done || now_ms.saturating_sub(l.step_ms) > 5000 {
                return;
            }
        }
        self.launch = Some(l);
    }

    pub(super) fn launch_after(&mut self, l: &mut Launch, now_ms: u64) {
        let fam = self.family();
        match l.after.as_str() {
            "mission" => {
                self.set_mode(fam.auto(), now_ms);
                self.set_owner(Owner::MissionFc, "mission");
                l.step = LaunchStep::Done;
            }
            "goto" => {
                if let Some(g) = l.goto {
                    self.start_goto(g, now_ms);
                }
                l.step = LaunchStep::Done;
            }
            "skill" => {
                self.set_owner(Owner::Pilot, "launch");
                self.engage_pilot(now_ms);
            }
            _ => {
                if fam == Family::Plane && self.fc.vehicle_class() == VehicleClass::Fixedwing {
                    self.set_mode(fam.guided(), now_ms);
                }
                self.set_owner(Owner::Pilot, "launch");
                self.engage_pilot(now_ms);
                l.step = LaunchStep::Done;
            }
        }
        l.step_ms = now_ms;
    }

    /// The "when done" action after a mission completes or a one-shot skill
    /// ends. Only in the air: a mission that ended on the ground (a LAND item)
    /// needs nothing more.
    pub(in crate::core) fn when_done(&mut self, now_ms: u64) {
        let Some(l) = &self.launch else { return };
        let Some(w) = l.when_done.clone() else { return };
        self.launch = None;
        if !self.fc.armed || self.fc.landed() {
            return;
        }
        let _ = if w == "land" {
            self.do_land(now_ms)
        } else {
            self.do_rth("mission_done", now_ms)
        };
    }
}
