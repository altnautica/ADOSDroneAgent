//! The pilot's own failsafes in the control tick: a lost controller, link
//! loss and the low-battery RTH countdown.

use ados_protocol::pilot::Owner;
use serde_json::json;

use super::super::Pilot;
use crate::flows::banner;

/// Low-battery RTH countdown.
const LOW_BATT_COUNTDOWN_MS: u64 = 10_000;

impl Pilot {
    pub(super) fn failsafes(&mut self, now_ms: u64) {
        // The FC's own landed state: a touchdown never holds a failsafe back.
        let airborne = self.fc.armed && !self.fc.landed();
        // Controller lost: the owning session stopped streaming.
        if self.owner() == Owner::Pilot
            && self.sessions.owner.is_some()
            && self.failsafe.input_stale(now_ms)
            && !self.controller_lost_shown
        {
            self.controller_lost_shown = true;
            self.sessions.axes = [0.0; 4];
            self.brake_hold(now_ms, Some("controller-lost"));
            self.banner = Some(banner("pilot.banner.controller_lost", &[], now_ms));
        }
        let flying = matches!(
            self.owner(),
            Owner::Pilot | Owner::Skill | Owner::Goto | Owner::Swarm
        );
        let window = u64::from(self.settings.pilot.link_loss_s) * 1000;
        if airborne && flying && self.failsafe.link_lost(now_ms, window) {
            let action = self.settings.pilot.link_loss_action;
            self.banner = Some(banner(
                "pilot.banner.link_loss",
                &[("action", json!(action.as_str())), ("in_s", json!(0))],
                now_ms,
            ));
            let _ = match action {
                crate::config::LinkLossAction::Rth => self.do_rth(now_ms),
                crate::config::LinkLossAction::Land => self.do_land(now_ms),
                crate::config::LinkLossAction::Hold => {
                    self.revoke("link-loss");
                    self.brake_hold(now_ms, Some("link-loss"));
                    Ok(())
                }
            };
        }
    }

    pub(super) fn low_battery(&mut self, now_ms: u64) {
        let Some(pct) = self.fc.battery_pct else {
            return;
        };
        let p = &self.fc.params;
        let low_pct = match (p.get("BATT_LOW_MAH"), p.get("BATT_CAPACITY")) {
            (Some(&l), Some(&c)) if l > 0.0 && c > 0.0 => l / c * 100.0,
            _ => 20.0,
        };
        let airborne = self.fc.armed && !self.fc.landed();
        if !airborne || pct > low_pct + 5.0 {
            self.rth_deadline_ms = None;
            if pct > low_pct + 5.0 {
                self.low_batt_cancelled = false;
            }
            return;
        }
        if pct > low_pct || self.low_batt_cancelled || matches!(self.owner(), Owner::FcMode) {
            return;
        }
        let deadline = *self
            .rth_deadline_ms
            .get_or_insert(now_ms + LOW_BATT_COUNTDOWN_MS);
        if now_ms >= deadline {
            self.rth_deadline_ms = None;
            self.low_batt_cancelled = true;
            let _ = self.do_rth(now_ms);
        }
    }
}
