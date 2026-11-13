//! The `launch` sequence (C10): arm, take off, then the after-takeoff and
//! "when done" actions.

use ados_protocol::mavlink::ardupilotmega::MavCmd;
use ados_protocol::pilot::{Owner, VehicleClass};
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
        let default_alt = match self.fc.vehicle_class() {
            VehicleClass::Fixedwing => self.fc.params.get("TKOFF_ALT").copied().unwrap_or(50.0),
            _ => 3.0,
        };
        let alt_m = a
            .get("alt_m")
            .and_then(Value::as_f64)
            .map(|v| v as f32)
            .unwrap_or(default_alt);
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
            },
            now_ms,
        )
    }

    pub(super) fn start_launch(&mut self, l: Launch, now_ms: u64) -> ActionRes {
        if self.fc.armed && !self.fc.landed() {
            return Err("airborne".into());
        }
        if l.step == LaunchStep::Checks || !self.fc.armed {
            self.ready_to_arm(now_ms)?;
        }
        let mut l = Launch {
            step: LaunchStep::Arm,
            step_ms: now_ms,
            cmd: None,
            ..l
        };
        // A deliberate take-off ends a touchdown.
        self.touchdown = Default::default();
        self.arm_for_launch(&mut l, now_ms);
        self.launch = Some(l);
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

    /// While disarmed: select the launch mode, then arm once the FC reports
    /// it; each command re-sent at most once a second.
    fn arm_for_launch(&mut self, l: &mut Launch, now_ms: u64) {
        if self.fc.armed {
            return;
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
            self.banner = Some(banner(
                "pilot.banner.launch_failed",
                &[("reason", json!(e))],
                now_ms,
            ));
            if self.fc.armed && !self.fc.landed() {
                self.brake_hold(now_ms, None);
            }
            if l.step == LaunchStep::Done || elapsed > 5000 {
                return;
            }
        }
        self.launch = Some(l);
    }

    fn launch_after(&mut self, l: &mut Launch, now_ms: u64) {
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
            self.do_rth(now_ms)
        };
    }
}
