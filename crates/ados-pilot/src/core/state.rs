//! The published `pilot` and `obstacles` kinds.

use ados_protocol::pilot::{
    wire, AvoidMode, AvoidState, FcStatusText, ObstaclesFrame, Owner, PilotState, Profile,
    Sequence, SkillRef,
};

use super::Pilot;
use crate::readiness;

fn profile_name(p: Profile) -> &'static str {
    match p {
        Profile::Cine => "CINE",
        Profile::Normal => "NORMAL",
        Profile::Sport => "SPORT",
    }
}

impl Pilot {
    /// The current `pilot` kind body. `mission` is the router's mission state,
    /// read from its state socket.
    pub fn state(&self, now_ms: u64) -> PilotState {
        let fam = self.family();
        let owner = self.owner();
        let fc_mode_name = fam.mode_name(self.fc.custom_mode);
        let readiness = self.readiness(now_ms);
        let skill = self.auth.lease().map(|l| SkillRef {
            plugin_id: l.holder.plugin_id.clone(),
            skill_id: l.holder.skill_id.clone(),
            label: l.holder.skill_id.clone(),
        });
        let owner_label = match owner {
            Owner::Idle => match readiness::first_block(&readiness) {
                Some(r) => format!("NOT READY · {}", r.msg),
                None => "READY".to_owned(),
            },
            Owner::Pilot => format!("PILOT · {}", profile_name(self.settings.pilot.profile)),
            Owner::Skill => skill
                .as_ref()
                .map(|s| s.label.to_uppercase())
                .unwrap_or_else(|| "SKILL".into()),
            Owner::Goto => match (self.goto, self.pos_ne(now_ms)) {
                (Some(g), Some(p)) => format!("GOTO · {:.0} m", crate::geo::dist(p, [g[0], g[1]])),
                _ => "GOTO".into(),
            },
            Owner::Swarm => "SWARM".into(),
            Owner::MissionFc => "MISSION".into(),
            Owner::FcMode if fam.is_rtl(self.fc.custom_mode) => {
                let d = self
                    .pos_ne(now_ms)
                    .map(|p| crate::geo::dist(p, self.fc.home_ne()));
                d.map_or("RTH".into(), |d| format!("RTH · {d:.0} m"))
            }
            Owner::FcMode if fam.is_land(self.fc.custom_mode) => "LANDING".into(),
            Owner::FcMode => format!("FC · {fc_mode_name}"),
            Owner::Tuning => "TUNING".into(),
        };
        let avoid_state = if self.settings.pilot.avoid == AvoidMode::Off {
            AvoidState::Off
        } else if matches!(owner, Owner::Skill | Owner::Goto) {
            // The worse of the planner's verdict and the goal brake's.
            match (self.plan.state, self.brake_avoid) {
                (AvoidState::Clear, b) => b,
                (AvoidState::Detour, AvoidState::Blocked) => AvoidState::Blocked,
                (p, _) => p,
            }
        } else if owner == Owner::Pilot {
            self.brake_avoid
        } else {
            AvoidState::Clear
        };
        let min_clearance_m = self
            .pos_ne(now_ms)
            .and_then(|p| self.obstacles.grid.clearance(p))
            .filter(|c| c.is_finite())
            .or(self.plan.min_clearance)
            .map(wire);
        let planning = matches!(owner, Owner::Skill | Owner::Goto);
        let goal_ned = if planning {
            self.plan.goal.map(|g| {
                let d = self
                    .auth
                    .lease()
                    .and_then(|l| l.goal.as_ref())
                    .map(|g| g.d)
                    .or(self.goto.map(|t| t[2]));
                [wire(g[0]), wire(g[1]), wire(d.unwrap_or(0.0))]
            })
        } else {
            None
        };
        let window = u64::from(self.settings.pilot.link_loss_s) * 1000;
        let link_loss_in_s = if self.fc.armed && !self.landed() {
            self.failsafe.link_countdown_s(now_ms, window).map(wire)
        } else {
            None
        };
        let agl = self
            .fc
            .rangefinder_agl(now_ms)
            .map(|m| (m, "rangefinder"))
            .or_else(|| self.fc.ekf_height(now_ms).map(|m| (m, "home")));
        let fc_linked = self.fc.linked(now_ms);
        PilotState {
            owner,
            owner_label,
            skill,
            vehicle_class: self.fc.vehicle_class(),
            control_mode: self.control_mode(),
            profile: self.settings.pilot.profile,
            avoid: self.settings.pilot.avoid,
            fc_mode: self.fc.custom_mode,
            fc_mode_name,
            armed: fc_linked.then_some(self.fc.armed),
            landed: fc_linked.then(|| self.fc.landed()),
            vtol_transitioning: fc_linked && self.fc.vtol_transitioning(),
            input_src: self.sessions.owner.as_ref().map(|o| o.src.clone()),
            input_age_ms: self
                .sessions
                .owner
                .as_ref()
                .and(self.failsafe.input_age_ms(now_ms)),
            hold_reason: self.hold_reason.clone(),
            avoid_state,
            min_clearance_m,
            goal_ned,
            plan_ned: if planning {
                self.plan
                    .path
                    .iter()
                    .map(|p| [wire(p[0]), wire(p[1])])
                    .collect()
            } else {
                Vec::new()
            },
            setpoint_ned: self.setpoint.map(|p| p.map(wire)),
            readiness,
            link_loss_in_s,
            link_loss_action: self.settings.pilot.link_loss_action.as_str().to_owned(),
            // A mission the pilot paused (a stick takeover, Pause) reads
            // `paused`; the router sees GUIDED and reports it `loaded`.
            mission: self.mission.clone().map(|mut m| {
                if self.mission_paused && !matches!(m.state.as_str(), "complete" | "none") {
                    m.state = "paused".to_owned();
                }
                m
            }),
            input_session: self.sessions.owner.clone(),
            gate: self.sessions.gate(now_ms),
            sequence: self.launch.as_ref().map(|l| Sequence {
                step: l.step.as_str().to_owned(),
                after: l.after.clone(),
                error: l.error.clone(),
            }),
            banner: self.banner.clone().map(|b| ados_protocol::pilot::Banner {
                until_ms: b.until_ms + self.epoch_offset_ms,
                ..b
            }),
            rth_countdown_s: self
                .rth_deadline_ms
                .map(|d| d.saturating_sub(now_ms) as f64 / 1000.0),
            // The pilot flies a surface's point only while it owns motion; in
            // `fc_manual` / `elrs` a surface uses the FC's reposition instead.
            surface_ops: if self.control_mode() == ados_protocol::pilot::ControlMode::Ados {
                super::actions::SURFACE_OPS
                    .iter()
                    .map(|s| (*s).to_owned())
                    .collect()
            } else {
                Vec::new()
            },
            agl_m: agl.map(|(m, _)| wire(m)),
            agl_source: agl.map(|(_, s)| s.to_owned()),
            statustext: self
                .fc
                .statustext
                .iter()
                .rev()
                .map(|s| (s, now_ms.saturating_sub(s.at_ms)))
                .take_while(|(_, age)| *age <= crate::fc::STATUSTEXT_MAX_AGE_MS)
                .map(|(s, age_ms)| FcStatusText {
                    severity: s.v.0,
                    text: s.v.1.clone(),
                    age_ms,
                })
                .collect(),
            fc_failsafe: self.fc_failsafe,
            failsafe_cause: self.failsafe_cause.map(str::to_owned),
            setup_step: if owner == Owner::Tuning {
                self.setup_step.map(str::to_owned)
            } else {
                None
            },
        }
    }

    /// The `obstacles` kind: sectors always; camera grids only for cameras a
    /// viewer leased with the `obstacles` layer.
    pub fn obstacles_frame(&mut self, now_ms: u64) -> Option<ObstaclesFrame> {
        let pos = self.pos_ne(now_ms)?;
        let yaw = self.yaw(now_ms);
        // A stale attitude levels the camera, as the yaw above falls to 0.
        let att = self
            .fc
            .fresh_attitude(now_ms)
            .map(|a| crate::obstacles::Attitude {
                roll: a.roll,
                pitch: a.pitch,
                yaw: a.yaw,
            })
            .unwrap_or_default();
        let agl = self.agl(now_ms).unwrap_or(0.0);
        let sectors_m = self
            .obstacles
            .sectors(pos, yaw, now_ms)
            .into_iter()
            .map(|d| d.map(wire))
            .collect();
        let mut cameras = std::collections::BTreeMap::new();
        for (id, layers) in &self.view.cameras {
            if !layers.iter().any(|l| l == "obstacles") {
                continue;
            }
            let Some(model) = self.view.models.get(id) else {
                continue;
            };
            let scratch = self.scratch.entry(id.clone()).or_default();
            let g = crate::obstacles::project_camera(
                &self.obstacles,
                model,
                pos,
                att,
                agl,
                now_ms,
                scratch,
            );
            cameras.insert(id.clone(), g);
        }
        self.scratch
            .retain(|id, _| self.view.cameras.contains_key(id));
        self.obstacles_ms = now_ms;
        Some(ObstaclesFrame {
            v: 1,
            ts_ms: now_ms,
            sectors_m,
            cameras,
        })
    }

    /// Obstacles publish at 10 Hz, 20 Hz while yawing fast or moving.
    pub fn obstacles_due(&self, now_ms: u64) -> bool {
        let fast = self
            .fc
            .fresh_attitude(now_ms)
            .is_some_and(|a| a.yaw_rate.abs() > 20f32.to_radians())
            || self
                .fc
                .fresh_local(now_ms)
                .is_some_and(|p| p.vn.hypot(p.ve) > 2.0);
        let period = if fast { 50 } else { 100 };
        now_ms.saturating_sub(self.obstacles_ms) >= period
    }
}
