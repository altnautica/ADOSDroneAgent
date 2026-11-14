//! Operator-facing queries and settings: readiness, the RC ADOS slot, leaving
//! the tuning owner, link loss and the control mode in force.

use ados_protocol::pilot::{ControlMode, Owner};

use super::{Out, Pilot};

impl Pilot {
    /// Every readiness item, including the setup engine's.
    pub fn readiness(&self, now_ms: u64) -> Vec<ados_protocol::pilot::Readiness> {
        let mut items = crate::readiness::items(
            &self.fc,
            self.control_mode(),
            self.sessions.owner.is_some(),
            now_ms,
        );
        if let Some(v) = &self.setup_needed {
            items.push(ados_protocol::pilot::Readiness {
                id: "setup_needed".into(),
                ok: v.is_none(),
                severity: ados_protocol::pilot::Severity::Block,
                msg: v
                    .clone()
                    .unwrap_or_else(|| "Aircraft setup complete".into()),
            });
        }
        if let Some(item) = self.mission_inferred() {
            items.push(item);
        }
        // Avoid is on. No obstacle sensor expected: a pre-arm warning that
        // nothing will be avoided and Fly Here is refused. One expected but
        // stale: a block, so a lidar lost on the ground refuses the arm and
        // the launch instead of holding after take-off.
        if self.settings.pilot.avoid != ados_protocol::pilot::AvoidMode::Off {
            use ados_protocol::pilot::Severity;
            let (ok, severity, msg) = if !self.lidar_expected() {
                (
                    false,
                    Severity::Warn,
                    "No lidar: obstacle avoidance unavailable",
                )
            } else if self.obstacles.scan_fresh(now_ms) {
                (true, Severity::Block, "Lidar reporting")
            } else {
                (
                    false,
                    Severity::Block,
                    "Obstacle data stale: lidar not reporting",
                )
            };
            items.push(ados_protocol::pilot::Readiness {
                id: "lidar".into(),
                ok,
                severity,
                msg: msg.into(),
            });
        }
        items
    }

    /// The warning that the mission state is inferred: an ArduPilot older
    /// than 4.5 sends no mission state in MISSION_CURRENT, so the router
    /// works it out from the sequence alone (`source: "fallback"`).
    fn mission_inferred(&self) -> Option<ados_protocol::pilot::Readiness> {
        let m = self.mission.as_ref()?;
        (m.source == "fallback"
            && m.state != "none"
            && self.fc.firmware == Some(crate::fc_profile::Firmware::Ardupilot))
        .then(|| ados_protocol::pilot::Readiness {
            id: "mission_inferred".into(),
            ok: false,
            severity: ados_protocol::pilot::Severity::Warn,
            msg: "FC firmware < 4.5: mission state is inferred".into(),
        })
    }

    pub fn set_rc_ados_position(&mut self, slot: u8) {
        self.settings.control.rc_ados_position = slot;
        self.out.push(Out::Persist {
            section: "control",
            key: "rc_ados_position",
            value: slot.to_string(),
        });
    }

    /// The setup engine asks the FC for `mode` on its own link: its change
    /// to it is the operator's, never an FC failsafe.
    pub fn note_setup_mode(&mut self, mode: u32, now_ms: u64) {
        self.own_mode = Some((mode, now_ms));
    }

    /// Leave the setup engine's `tuning` owner. Airborne: fly as `pilot` from a
    /// hold here (back in GUIDED); on the ground: idle or whatever came before.
    /// During an FC failsafe the FC keeps flying it (`fc_mode`), and so it does
    /// after the wizard's own return or land (its abort RTL), even before the
    /// FC's heartbeat shows that mode.
    pub fn exit_tuning(&mut self, now_ms: u64) {
        if self.owner() != Owner::Tuning {
            return;
        }
        self.auth.exit_tuning();
        let returning = self.is_returning(self.fc.custom_mode)
            || self.own_mode.is_some_and(|(m, t)| {
                now_ms.saturating_sub(t) < super::OWN_MODE_WINDOW_MS && self.is_returning(m)
            });
        if self.fc.armed && !self.fc.landed() && (returning || self.fc_failsafe) {
            self.set_owner(Owner::FcMode, "tuning-end");
        } else if self.fc.armed && !self.fc.landed() {
            if !self.in_guided() {
                let g = self.family().guided();
                self.set_mode(g, now_ms);
            }
            self.set_owner(Owner::Pilot, "tuning-end");
            self.engage_pilot(now_ms);
        } else if !self.fc.armed {
            self.set_owner(Owner::Idle, "tuning-end");
        }
    }

    /// A mode in which the FC returns or lands on its own.
    fn is_returning(&self, mode: u32) -> bool {
        let fam = self.family();
        fam.is_rtl(mode)
            || fam.is_land(mode)
            || mode == crate::modes::copter::LAND && fam == crate::modes::Family::Copter
            || fam == crate::modes::Family::Plane
                && matches!(
                    mode,
                    crate::modes::plane::LOITER | crate::modes::plane::QLAND
                )
    }

    /// No operator surface heard from for `pilot.link_loss_s`.
    pub fn link_lost(&self, now_ms: u64) -> bool {
        let window = u64::from(self.settings.pilot.link_loss_s) * 1000;
        self.failsafe
            .surface_silence_ms(now_ms)
            .is_some_and(|s| s >= window)
    }

    /// The control mode now in force.
    pub fn control_mode(&self) -> ControlMode {
        self.settings.control.mode
    }
}
