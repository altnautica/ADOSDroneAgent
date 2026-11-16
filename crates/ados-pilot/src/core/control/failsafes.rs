//! The pilot's own failsafes in the control tick: a lost controller, link
//! loss and the low-battery countdown (the return, or Land on a multirotor
//! that lands on a low battery: [`crate::batt_failsafe::lands_on_low`]),
//! announced and logged.

use ados_protocol::pilot::{Owner, VehicleClass};
use serde_json::json;

use super::super::Pilot;
use crate::flows::banner;

/// MAVLink severity of the low-battery announcement: CRITICAL.
const LOW_BATT_SEVERITY: u8 = 2;

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
                crate::config::LinkLossAction::Rth => self.do_rth("link_loss", now_ms),
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
            self.low_batt_action = None;
            if pct > low_pct + 5.0 {
                self.low_batt_cancelled = false;
            }
            return;
        }
        if pct > low_pct || self.low_batt_cancelled || matches!(self.owner(), Owner::FcMode) {
            return;
        }
        // A multirotor that may fly without GNSS lands where it is: a return
        // climbs to the RTL altitude, into the roof indoors. In a confined
        // place on external nav it returns along the flown path instead (the
        // `battery_failsafe` readiness item's rule).
        let land = self.fc.vehicle_class() == VehicleClass::Multirotor
            && crate::batt_failsafe::lands_on_low(self.settings.pilot.place_class, |n| {
                self.fc.params.get(n).copied()
            });
        let action = if land { "land" } else { "rth" };
        self.low_batt_action = Some(action);
        let Some(deadline) = self.rth_deadline_ms else {
            let countdown_s = self.settings.pilot.low_battery_countdown_s;
            self.rth_deadline_ms = Some(now_ms + u64::from(countdown_s) * 1000);
            // The countdown names the action it ends in.
            self.banner = Some(banner(
                if land {
                    "pilot.banner.low_battery_land_in"
                } else {
                    "pilot.banner.low_battery_rth"
                },
                &[
                    ("pct", json!(whole_pct(pct))),
                    ("need_pct", json!(whole_pct(low_pct))),
                    ("in_s", json!(countdown_s)),
                ],
                now_ms,
            ));
            tracing::warn!(
                target: "ados_pilot",
                pct,
                need_pct = low_pct,
                action,
                event = "countdown",
                in_s = countdown_s,
                "pilot_battery_reserve"
            );
            return;
        };
        if now_ms < deadline {
            return;
        }
        self.rth_deadline_ms = None;
        self.low_batt_cancelled = true;
        let res = if land {
            self.do_land(now_ms)
        } else {
            self.do_rth("low_battery", now_ms)
        };
        tracing::warn!(
            target: "ados_pilot",
            pct,
            need_pct = low_pct,
            action,
            event = "fired",
            refused = res.as_ref().err(),
            "pilot_battery_reserve"
        );
        if res.is_ok() && !land && self.family() == crate::modes::Family::Copter {
            // The shared return asked the FC for SmartRTL: its outcome (the
            // FC flying it, or the landing when it is refused) is announced
            // once known ([`Self::return_outcome`]).
            self.sup.return_battery_pct = Some(pct);
            return;
        }
        let banner_key = if land {
            "pilot.banner.low_battery_land"
        } else {
            "pilot.banner.low_battery_returning"
        };
        self.announce_low_battery(action, banner_key, pct, now_ms);
    }

    /// The low-battery action's announcement: the published action, a
    /// CRITICAL status line every surface lists, and the banner naming why.
    pub(in crate::core) fn announce_low_battery(
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
        self.banner = Some(banner(
            banner_key,
            &[("pct", json!(whole_pct(pct)))],
            now_ms,
        ));
    }

    /// The operator stopped the low-battery countdown (Cancel, Pause).
    pub(in crate::core) fn cancel_low_battery(&mut self, by: &str) {
        self.low_batt_action = None;
        if self.rth_deadline_ms.take().is_some() {
            tracing::warn!(
                target: "ados_pilot",
                pct = self.fc.battery_pct,
                event = "cancelled",
                by,
                "pilot_battery_reserve"
            );
        }
    }
}

/// A battery percentage as the whole number every surface prints.
fn whole_pct(pct: f32) -> i64 {
    pct.round() as i64
}
