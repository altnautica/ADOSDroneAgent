//! The pilot's own failsafes in the control tick: a lost controller, link
//! loss and the low-battery countdown (the return, or Land on a multirotor
//! that lands on a low battery: [`crate::batt_failsafe::lands_on_low`]),
//! announced and logged.

use ados_protocol::pilot::{Owner, VehicleClass};
use serde_json::json;

use super::super::Pilot;
use super::energy::Reserve;
use crate::flows::banner;

/// MAVLink severity of the low-battery announcement: CRITICAL.
const LOW_BATT_SEVERITY: u8 = 2;

/// The banners a low-battery countdown shows.
const COUNTDOWN_BANNERS: [&str; 3] = [
    "pilot.banner.low_battery_land_in",
    "pilot.banner.low_battery_rth_energy",
    "pilot.banner.low_battery_rth",
];

impl Pilot {
    pub(super) fn failsafes(&mut self, now_ms: u64) {
        self.watch_rc(now_ms);
        // The FC's own landed state: a touchdown never holds a failsafe back.
        let airborne = self.fc.armed && !self.fc.landed();
        // Controller lost: the owning session stopped streaming. The sticks
        // ask nothing, so the reference brakes to a hold on the profile's
        // brake limits, and resumed frames fly only once centred.
        if self.owner() == Owner::Pilot
            && self.sessions.owner.is_some()
            && self.failsafe.input_stale(now_ms)
            && !self.controller_lost_shown
        {
            self.controller_lost_shown = true;
            self.sessions.axes = [0.0; 4];
            self.sessions.arm_gate();
            self.brake_hold(Some("controller-lost"));
            self.banner = Some(banner("pilot.banner.controller_lost", &[], now_ms));
        }
        let window = u64::from(self.settings.pilot.link_loss_s) * 1000;
        let lost = if self.settings.pilot.rc_receiver.is_present() {
            let flying = matches!(
                self.owner(),
                Owner::Pilot | Owner::Skill | Owner::Goto | Owner::Swarm
            );
            airborne
                && flying
                && self.sup.routed.is_none()
                && self.failsafe.link_lost(now_ms, window)
        } else {
            // No RC transmitter: every airborne owner, each operator
            // instance on its own, except while the FC returns or lands or
            // the pilot lands.
            airborne
                && !self.landing
                && !self.fc_returns_or_lands()
                && self.failsafe.instances.lost(now_ms, window).is_some()
        };
        if lost {
            self.link_loss_action(now_ms);
        }
    }

    /// The FC flies a failsafe of its own, or a return or a landing, or the
    /// pilot routes a blocked return home: with no RC transmitter, link loss
    /// leaves it alone (a critical-battery Land is never turned into a
    /// return), and no skill takes the vehicle back from it.
    pub(in crate::core) fn fc_returns_or_lands(&self) -> bool {
        let fam = self.family();
        let mode = self.fc.custom_mode;
        self.fc_failsafe || fam.is_rtl(mode) || fam.is_land(mode) || self.sup.routed.is_some()
    }

    fn link_loss_action(&mut self, now_ms: u64) {
        let action = self.settings.pilot.link_loss_action;
        self.banner = Some(banner(
            "pilot.banner.link_loss",
            &[("action", json!(action.as_str())), ("in_s", json!(0))],
            now_ms,
        ));
        // A tuning flight ends first, as for the operator's own safety
        // actions.
        if self.owner() == Owner::Tuning {
            self.exit_tuning(now_ms);
        }
        let _ = match action {
            crate::config::LinkLossAction::Rth => self.do_rth("link_loss", now_ms),
            crate::config::LinkLossAction::Land => self.do_land(now_ms),
            crate::config::LinkLossAction::Hold => {
                self.revoke("link-loss");
                self.brake_hold(Some("link-loss"));
                Ok(())
            }
        };
    }

    pub(super) fn low_battery(&mut self, now_ms: u64) {
        let Some(pct) = self.fc.battery_pct else {
            self.sup.energy.reserve = None;
            return;
        };
        let p = &self.fc.params;
        // No charge level on the FC: the wizard's floor (`CAPACITY_RESERVE`).
        let low_pct = match (p.get("BATT_LOW_MAH"), p.get("BATT_CAPACITY")) {
            (Some(&l), Some(&c)) if l > 0.0 && c > 0.0 => l / c * 100.0,
            _ => crate::setup::adapters::ardupilot_copter::CAPACITY_RESERVE[0].1 * 100.0,
        };
        let airborne = self.fc.armed && !self.fc.landed();
        // A multirotor that may fly without GNSS lands where it is: a return
        // climbs to the RTL altitude, into the roof indoors. In a confined
        // place on external nav it returns along the flown path instead (the
        // `battery_failsafe` readiness item's rule).
        let land = self.fc.vehicle_class() == VehicleClass::Multirotor
            && crate::batt_failsafe::lands_on_low(self.settings.pilot.place_class, |n| {
                self.fc.params.get(n).copied()
            });
        let reserve = if airborne {
            let r = self.battery_reserve(pct, low_pct, now_ms);
            // Landing where it is needs no charge for the way home.
            Some(if land {
                Reserve {
                    need_pct: low_pct,
                    energy: false,
                    short: false,
                    ..r
                }
            } else {
                r
            })
        } else {
            self.sup.energy.clear();
            None
        };
        self.sup.energy.reserve = reserve;
        let short = reserve.is_some_and(|r| r.short);
        // Re-armed 5 % above the level it fired at (the need shrinks on the
        // way home).
        let need_pct = reserve.map_or(low_pct, |r| r.need_pct);
        let rearm_pct = self.sup.energy.latch_pct.unwrap_or(need_pct).max(need_pct) + 5.0;
        if !airborne || (pct > rearm_pct && !short) {
            self.rth_deadline_ms = None;
            self.low_batt_action = None;
            self.sup.energy.latch_pct = None;
            if pct > rearm_pct {
                self.low_batt_cancelled = false;
            }
            return;
        }
        // A running countdown either ends in the return or is withdrawn: once
        // the charge is 5 % above the need again (the path home was pruned
        // as a survey crossed its own lanes, or the vehicle came closer), it
        // stops, and a later shortfall starts a fresh one. It used to freeze
        // with its deadline set, still publishing the countdown, and the
        // next shortfall fired the return at once with no countdown.
        let counting = self.rth_deadline_ms.is_some();
        if counting && !self.low_batt_cancelled && !short && pct > need_pct + 5.0 {
            self.withdraw_low_battery(pct, need_pct, now_ms);
            return;
        }
        if (!counting && pct > low_pct && !short)
            || self.low_batt_cancelled
            || matches!(self.owner(), Owner::FcMode)
            || self.sup.routed.is_some()
        {
            return;
        }
        // Above the floor only the energy estimate starts a countdown.
        let basis = if pct > low_pct { "energy" } else { "floor" };
        let return_s = reserve.map_or(0.0, |r| r.return_s);
        let action = if land { "land" } else { "rth" };
        self.low_batt_action = Some(action);
        let Some(deadline) = self.rth_deadline_ms else {
            self.sup.energy.latch_pct = Some(need_pct);
            let countdown_s = self.settings.pilot.low_battery_countdown_s;
            self.rth_deadline_ms = Some(now_ms + u64::from(countdown_s) * 1000);
            // The countdown names the action it ends in, and a return the
            // energy estimate asked for names the charge the way home needs.
            let key = match (land, basis) {
                (true, _) => "pilot.banner.low_battery_land_in",
                (false, "energy") => "pilot.banner.low_battery_rth_energy",
                (false, _) => "pilot.banner.low_battery_rth",
            };
            self.banner = Some(banner(
                key,
                &[
                    ("pct", json!(whole_pct(pct))),
                    ("need_pct", json!(whole_pct(need_pct))),
                    ("in_s", json!(countdown_s)),
                ],
                now_ms,
            ));
            tracing::warn!(
                target: "ados_pilot",
                pct,
                need_pct,
                basis,
                return_s,
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
            need_pct,
            basis,
            return_s,
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

    /// The charge covers the way home again: the countdown stops, its
    /// banner goes, and the next shortfall starts a fresh one (the operator's
    /// cancel is the one that waits for the charge to recover 5 %).
    fn withdraw_low_battery(&mut self, pct: f32, need_pct: f32, now_ms: u64) {
        self.rth_deadline_ms = None;
        self.low_batt_action = None;
        self.sup.energy.latch_pct = None;
        if self
            .banner
            .as_ref()
            .is_some_and(|b| COUNTDOWN_BANNERS.contains(&b.text_key.as_str()))
        {
            self.banner = None;
        }
        tracing::warn!(
            target: "ados_pilot",
            pct,
            need_pct,
            event = "withdrawn",
            at_ms = now_ms,
            "pilot_battery_reserve"
        );
    }
}

/// A battery percentage as the whole number every surface prints.
fn whole_pct(pct: f32) -> i64 {
    pct.round() as i64
}
