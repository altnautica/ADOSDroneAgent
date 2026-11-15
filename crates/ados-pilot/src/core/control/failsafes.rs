//! The pilot's own failsafes in the control tick: a lost controller, link
//! loss and the low-battery countdown (RTH, or Land on a multirotor that may
//! fly without GNSS).

use crate::modes::copter;
use ados_protocol::pilot::{Owner, VehicleClass};
use serde_json::json;

use super::super::Pilot;
use crate::flows::banner;

/// Low-battery countdown.
const LOW_BATT_COUNTDOWN_MS: u64 = 10_000;
/// MAVLink severity of the low-battery announcement: CRITICAL.
const LOW_BATT_SEVERITY: u8 = 2;
/// How long the FC has to enter SmartRTL before the low-battery rule lands.
const LOW_BATT_SRTL_ANSWER_MS: u64 = 2000;

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
        self.low_battery_smart_rtl(now_ms);
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
            self.low_batt_action = None;
            if pct > low_pct + 5.0 {
                self.low_batt_cancelled = false;
            }
            return;
        }
        if pct > low_pct || self.low_batt_cancelled || matches!(self.owner(), Owner::FcMode) {
            return;
        }
        // A multirotor that may fly without GNSS lands where it is: an RTL
        // climbs to the RTL altitude, into the roof indoors (the
        // `battery_failsafe` readiness item's rule).
        let land = self.fc.vehicle_class() == VehicleClass::Multirotor
            && crate::batt_failsafe::gnss_free(|n| self.fc.params.get(n).copied());
        let action = if land { "land" } else { "rth" };
        if self.rth_deadline_ms.is_none() {
            tracing::warn!(pct, action, "pilot_low_battery_countdown");
        }
        self.low_batt_action = Some(action);
        let deadline = *self
            .rth_deadline_ms
            .get_or_insert(now_ms + LOW_BATT_COUNTDOWN_MS);
        if now_ms < deadline {
            return;
        }
        self.rth_deadline_ms = None;
        self.low_batt_cancelled = true;
        if !land && self.fc.vehicle_class() == VehicleClass::Multirotor {
            // SmartRTL retraces the flown path and never climbs to the RTL
            // altitude; the FC refuses it without a recorded path, and the
            // vehicle lands instead ([`Self::low_battery_smart_rtl`]). Local
            // to this rule until `do_rth` flies SmartRTL-or-Land for every
            // return; this then calls `do_rth`.
            self.revoke("rth");
            self.set_mode(copter::SMART_RTL, now_ms);
            self.set_owner(Owner::FcMode, "rth");
            self.low_batt_srtl = Some((now_ms, pct, self.fc.custom_mode));
            tracing::warn!(
                pct,
                action,
                taken = "smart_rtl_requested",
                "pilot_low_battery_action"
            );
            return;
        }
        let res = if land {
            self.do_land(now_ms)
        } else {
            self.do_rth(now_ms)
        };
        let (reason, banner_key) = if land {
            ("gnss_free", "pilot.banner.low_battery_land")
        } else {
            ("rtl", "pilot.banner.low_battery_returning")
        };
        match &res {
            Ok(()) => tracing::warn!(pct, action, taken = reason, "pilot_low_battery_action"),
            Err(e) => {
                tracing::warn!(pct, action, taken = reason, error = %e, "pilot_low_battery_action_refused")
            }
        }
        self.announce_low_battery(action, banner_key, pct, now_ms);
    }

    /// The low-battery SmartRTL request's outcome: the FC in SmartRTL
    /// announces the return. The FC still in the mode it flew when asked,
    /// [`LOW_BATT_SRTL_ANSWER_MS`] later, refused (no recorded path,
    /// `SRTL_POINTS` 0): the vehicle lands. Any other mode is someone
    /// else's (an operator's mode change, an FC failsafe): the request is
    /// dropped and nothing overrides it.
    fn low_battery_smart_rtl(&mut self, now_ms: u64) {
        let Some((sent_ms, pct, asked_in)) = self.low_batt_srtl else {
            return;
        };
        if !self.fc.armed || self.fc.landed() {
            self.low_batt_srtl = None;
            return;
        }
        let mode = self.fc.custom_mode;
        if mode == copter::SMART_RTL {
            self.low_batt_srtl = None;
            tracing::warn!(
                pct,
                action = "rth",
                taken = "smart_rtl",
                "pilot_low_battery_action"
            );
            self.announce_low_battery("rth", "pilot.banner.low_battery_smart_rtl", pct, now_ms);
        } else if mode != asked_in {
            self.low_batt_srtl = None;
            tracing::warn!(pct, mode, taken = "superseded", "pilot_low_battery_action");
        } else if now_ms.saturating_sub(sent_ms) >= LOW_BATT_SRTL_ANSWER_MS {
            self.low_batt_srtl = None;
            self.set_mode(copter::LAND, now_ms);
            self.set_owner(Owner::FcMode, "land");
            tracing::warn!(
                pct,
                action = "land",
                taken = "land_no_smart_rtl",
                "pilot_low_battery_action"
            );
            self.announce_low_battery("land", "pilot.banner.low_battery_land_no_srtl", pct, now_ms);
        }
    }

    /// The low-battery action's announcement: the published action, a
    /// CRITICAL status line every surface lists, and the banner naming why.
    fn announce_low_battery(
        &mut self,
        action: &'static str,
        banner_key: &str,
        pct: f32,
        now_ms: u64,
    ) {
        self.low_batt_action = Some(action);
        let alert = if action == "land" {
            "alert.low_battery_land"
        } else {
            "alert.low_battery_rth"
        };
        self.fc.push_statustext(
            LOW_BATT_SEVERITY,
            crate::batt_failsafe::spec(alert).to_owned(),
            now_ms,
        );
        self.banner = Some(banner(banner_key, &[("pct", json!(pct))], now_ms));
    }
}
