//! The pre-flight checks the status pill lists, as `web-shared/src/hud/preflight.ts`
//! `preflightChecks` / `preflightFailing` word them: on the ground only, and
//! never a check whose fault a base alert already reports (the pill never
//! lists the same fault twice). EKF health is not on the telemetry wire, so
//! it is not listed.

use super::base_alerts::{battery_low_pct, battery_pct, BaseAlert};
use super::vehicle::{Batteries, Telemetry};

/// EKF_STATUS_FLAGS `EKF_POS_HORIZ_ABS`: an absolute horizontal position.
const POS_H_ABS: u32 = 16;

/// Vehicle telemetry is live while its last sample is younger than this
/// (`vehicle-live.ts` `LIVE_FRESH_MS`).
pub const LIVE_FRESH_MS: u64 = 4000;

/// One check: its id, and the failure worded as the fault.
struct Check {
    id: &'static str,
    fail: &'static str,
    ok: bool,
}

/// The base alerts that already report a check's failure (`CHECK_COVERED_BY`).
fn covered_by(id: &str) -> &'static [&'static str] {
    match id {
        "link" => &["link_lost", "link_stale"],
        "gps" => &["gps_no_3d"],
        "battery" => &["batt_low", "batt_crit"],
        _ => &[],
    }
}

/// The flight controller has a home on the ground: a HOME_POSITION with a
/// position, or an EKF with an absolute horizontal position (`fcHomeSet`).
fn home_set(tel: Option<&Telemetry>) -> bool {
    tel.is_some_and(|t| t.home_reported || t.ekf.is_some_and(|(flags, _)| flags & POS_H_ABS != 0))
}

/// The checks, in checklist order (`preflightChecks`). `live`: vehicle
/// telemetry is live.
fn checks(tel: Option<&Telemetry>, battery: Option<&Batteries>, live: bool) -> [Check; 4] {
    let fix = tel.and_then(|t| t.fix_type);
    [
        Check {
            id: "link",
            fail: "Vehicle link not fresh",
            ok: live,
        },
        Check {
            id: "gps",
            fail: "GPS fix below 3D",
            ok: live && fix.is_some_and(|f| f >= 3),
        },
        Check {
            id: "battery",
            fail: "Battery in the low band",
            ok: battery_pct(battery, tel).is_some_and(|p| p > battery_low_pct(battery)),
        },
        Check {
            id: "home",
            fail: "Home position not set",
            ok: home_set(tel),
        },
    ]
}

/// The failing checks the pill lists, as (id, fault) (`preflightFailing`):
/// none unless the vehicle reports itself disarmed, and none a listed base
/// alert already covers.
pub fn preflight_failing(
    tel: Option<&Telemetry>,
    battery: Option<&Batteries>,
    live: bool,
    alerts: &[BaseAlert],
    armed: Option<bool>,
) -> Vec<(String, String)> {
    if armed != Some(false) {
        return Vec::new();
    }
    checks(tel, battery, live)
        .into_iter()
        .filter(|c| {
            !c.ok
                && !covered_by(c.id)
                    .iter()
                    .any(|id| alerts.iter().any(|a| a.id == *id))
        })
        .map(|c| (c.id.to_string(), c.fail.to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::base_alerts::Level;

    fn ground(fix: i64) -> Telemetry {
        Telemetry {
            vehicle: true,
            armed: Some(false),
            fix_type: Some(fix),
            voltage: Some(15.4),
            remaining: Some(99.0),
            ..Telemetry::default()
        }
    }

    fn ids(v: &[(String, String)]) -> Vec<&str> {
        v.iter().map(|(id, _)| id.as_str()).collect()
    }

    /// A disarmed vehicle indoors: no 3D fix and no home, as the cockpit
    /// lists them ("GPS fix below 3D", "Home position not set").
    #[test]
    fn a_disarmed_vehicle_without_a_fix_lists_gps_and_home() {
        let tel = ground(1);
        let failing = preflight_failing(Some(&tel), None, true, &[], Some(false));
        assert_eq!(ids(&failing), ["gps", "home"]);
        assert_eq!(failing[1].1, "Home position not set");
    }

    #[test]
    fn nothing_is_listed_unless_known_disarmed() {
        let tel = ground(1);
        assert!(preflight_failing(Some(&tel), None, true, &[], None).is_empty());
        assert!(preflight_failing(Some(&tel), None, true, &[], Some(true)).is_empty());
    }

    #[test]
    fn home_reads_from_home_position_or_an_absolute_ekf_position() {
        let mut tel = ground(3);
        tel.home_reported = true;
        assert!(preflight_failing(Some(&tel), None, true, &[], Some(false)).is_empty());
        let mut tel = ground(3);
        tel.ekf = Some((POS_H_ABS, 100));
        assert!(preflight_failing(Some(&tel), None, true, &[], Some(false)).is_empty());
    }

    #[test]
    fn a_check_a_base_alert_covers_is_left_out() {
        let tel = ground(3);
        let stale = BaseAlert {
            id: "link_stale".into(),
            level: Level::Warning,
            title: String::new(),
        };
        let failing = preflight_failing(Some(&tel), None, false, &[stale], Some(false));
        // The link check is the stale alert's; the fix check fails too (not live).
        assert_eq!(ids(&failing), ["gps", "home"]);
    }
}
