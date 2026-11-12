//! The alert rules every surface shares, as `web-shared/src/hud/status-pill.ts`
//! `baseAlerts` words them: the vehicle link going quiet, the battery low or
//! critical (`battery.ts` `batteryReading`), pack anomalies, and a GPS fix
//! below 3D while armed. The terminal mounts no video and its pill drops
//! advisories, so the frozen-video and uplink rules have nothing to read here.

use std::sync::LazyLock;

use serde_json::Value;

use super::spec_text::spec_text;
use super::vehicle::{Batteries, Telemetry};

/// The spec's layout table (`layout.json`): the pill and battery bands.
pub const LAYOUT_JSON: &str = include_str!("../../../../web-shared/src/cockpit-spec/layout.json");

static LAYOUT: LazyLock<Value> =
    LazyLock::new(|| serde_json::from_str(LAYOUT_JSON).expect("layout.json is the spec layout"));

fn layout_f64(path: &[&str]) -> f64 {
    path.iter()
        .try_fold(&*LAYOUT, |v, k| v.get(k))
        .and_then(Value::as_f64)
        .unwrap_or_else(|| panic!("layout.json has {}", path.join(".")))
}

/// Storage use that warns, and that is critical (percent).
pub fn storage_warn_pct() -> f64 {
    layout_f64(&["status_pill", "storage_warn_pct"])
}
pub fn storage_crit_pct() -> f64 {
    layout_f64(&["status_pill", "storage_crit_pct"])
}

/// The low band when the battery engine reports no reserve.
fn battery_low_fallback_pct() -> f64 {
    layout_f64(&["battery", "low_fallback_pct"])
}
const BATTERY_CRITICAL_PCT: f64 = 10.0;

pub const LINK_STALE_MS: u64 = 3000;
pub const LINK_LOST_MS: u64 = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Critical,
    Warning,
    /// Never raised here (the terminal has no uplink rule); the shared
    /// golden cases carry it, and the pill drops it.
    #[cfg_attr(not(test), allow(dead_code))]
    Advisory,
}

impl Level {
    /// A shared case file's level name.
    #[cfg(test)]
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "critical" => Level::Critical,
            "warning" => Level::Warning,
            "advisory" => Level::Advisory,
            _ => return None,
        })
    }
}

/// One alert: `id` is the spec key suffix, the same on every surface.
#[derive(Debug, Clone, PartialEq)]
pub struct BaseAlert {
    pub id: String,
    pub level: Level,
    pub title: String,
}

/// What the rules read.
#[derive(Default)]
pub struct BaseInputs<'a> {
    pub tel: Option<&'a Telemetry>,
    pub battery: Option<&'a Batteries>,
    /// Milliseconds since vehicle telemetry was last live; `None` when it
    /// has never been live (nothing to have lost).
    pub ms_since_live: Option<u64>,
    /// The arm state at the last live sample.
    pub armed_at_last_live: Option<bool>,
}

/// Whether a pack voltage is a real measurement: MAVLink sends 65535 mV
/// with no monitor and 0 with nothing attached.
fn measured(voltage: Option<f64>) -> bool {
    voltage.is_some_and(|v| {
        let mv = (v * 1000.0).round();
        mv > 0.0 && mv != 65535.0
    })
}

/// The battery percent a surface trusts: the engine's freshest pack, else
/// the FC's own percentage with a measured voltage; `None` is unknown.
pub fn battery_pct(battery: Option<&Batteries>, tel: Option<&Telemetry>) -> Option<f64> {
    let engine = battery
        .filter(|b| b.enabled && !b.stale)
        .and_then(|b| {
            b.packs
                .iter()
                .find(|p| !p.stale && p.remaining.is_some_and(|r| r >= 0.0))
        })
        .and_then(|p| p.remaining);
    engine.or_else(|| {
        tel.filter(|t| measured(t.voltage))
            .and_then(|t| t.remaining)
            .filter(|r| *r >= 0.0)
    })
}

/// The low band: the engine's reserve when it is higher than the fallback.
fn battery_low_pct(battery: Option<&Batteries>) -> f64 {
    let reserve = battery.and_then(|b| b.reserve_percent).unwrap_or(0) as f64;
    battery_low_fallback_pct().max(reserve)
}

fn alert(id: impl Into<String>, level: Level, title: String) -> BaseAlert {
    BaseAlert {
        id: id.into(),
        level,
        title,
    }
}

/// The shared rules, worst first; equal levels keep their order.
pub fn base_alerts(i: &BaseInputs) -> Vec<BaseAlert> {
    let mut out = Vec::new();
    let armed = i.tel.and_then(|t| t.armed) == Some(true);
    let armed_at_loss = i.armed_at_last_live.unwrap_or(armed);
    match i.ms_since_live {
        Some(ms) if ms > LINK_LOST_MS && armed_at_loss => out.push(alert(
            "link_lost",
            Level::Critical,
            spec_text("alert.link_lost", &[]),
        )),
        Some(ms) if ms > LINK_STALE_MS => out.push(alert(
            "link_stale",
            Level::Warning,
            spec_text("alert.link_stale", &[]),
        )),
        _ => {}
    }
    if let Some(pct) = battery_pct(i.battery, i.tel) {
        if pct <= BATTERY_CRITICAL_PCT {
            out.push(alert(
                "batt_crit",
                Level::Critical,
                spec_text("alert.batt_crit", &[]),
            ));
        } else if pct <= battery_low_pct(i.battery) {
            out.push(alert(
                "batt_low",
                Level::Warning,
                spec_text("alert.batt_low", &[]),
            ));
        }
    }
    for pack in i
        .battery
        .filter(|b| !b.stale)
        .into_iter()
        .flat_map(|b| &b.packs)
    {
        if pack.stale {
            continue;
        }
        for a in pack.anomalies.iter().filter(|a| a.alerts && !a.cleared) {
            let n = (pack.id + 1).to_string();
            let rule = a.rule.replace('_', " ");
            out.push(alert(
                format!("battery_pack_{}_{}", pack.id, a.rule),
                if a.critical {
                    Level::Critical
                } else {
                    Level::Warning
                },
                spec_text("alert.battery_pack", &[("pack", &n), ("rule", &rule)]),
            ));
        }
    }
    if armed && i.tel.and_then(|t| t.fix_type).is_some_and(|f| f < 3) {
        out.push(alert(
            "gps_no_3d",
            Level::Warning,
            spec_text("alert.gps_no_3d", &[]),
        ));
    }
    // A stable sort: equal levels keep their source order.
    out.sort_by_key(|a| a.level as u8);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::vehicle::{Anomaly, Pack};

    fn tel(armed: bool, fix: i64, remaining: f64) -> Telemetry {
        Telemetry {
            vehicle: true,
            armed: Some(armed),
            fix_type: Some(fix),
            remaining: Some(remaining),
            voltage: Some(15.8),
            ..Telemetry::default()
        }
    }

    fn ids(i: &BaseInputs) -> Vec<String> {
        base_alerts(i).into_iter().map(|a| a.id).collect()
    }

    #[test]
    fn the_fc_battery_alerts_at_the_low_and_critical_bands() {
        let t = tel(false, 3, 18.0);
        let i = BaseInputs {
            tel: Some(&t),
            ..BaseInputs::default()
        };
        assert_eq!(ids(&i), ["batt_low"]);
        let t = tel(false, 3, 9.0);
        let i = BaseInputs {
            tel: Some(&t),
            ..BaseInputs::default()
        };
        assert_eq!(base_alerts(&i)[0].title, "Battery critical");
        // No measured voltage: an unknown battery never alerts.
        let t = Telemetry {
            voltage: Some(65.535),
            ..tel(false, 3, 5.0)
        };
        let i = BaseInputs {
            tel: Some(&t),
            ..BaseInputs::default()
        };
        assert!(ids(&i).is_empty());
    }

    #[test]
    fn pack_anomalies_and_a_2d_fix_while_armed_alert() {
        let pack = Pack {
            id: 0,
            source: String::new(),
            stale: false,
            cells_plausible: true,
            cells: vec![],
            weakest: None,
            divergence_mv: None,
            voltage: None,
            current: None,
            remaining: Some(60.0),
            temperature: None,
            prediction_state: None,
            eta_s: None,
            anomalies: vec![Anomaly {
                rule: "cell_divergence".into(),
                critical: false,
                alerts: true,
                cleared: false,
                value: None,
                threshold: None,
            }],
        };
        let b = Batteries {
            enabled: true,
            stale: false,
            reserve_percent: Some(25),
            packs: vec![pack],
        };
        let t = tel(true, 2, 60.0);
        let alerts = base_alerts(&BaseInputs {
            tel: Some(&t),
            battery: Some(&b),
            ..BaseInputs::default()
        });
        let got: Vec<&str> = alerts.iter().map(|a| a.title.as_str()).collect();
        assert_eq!(got, ["Battery 1: cell divergence", "GPS fix below 3D"]);
    }

    #[test]
    fn a_quiet_link_is_stale_then_lost_while_armed() {
        let t = tel(true, 3, 80.0);
        let at = |ms| {
            ids(&BaseInputs {
                tel: Some(&t),
                ms_since_live: Some(ms),
                ..BaseInputs::default()
            })
        };
        assert!(at(LINK_STALE_MS).is_empty());
        assert_eq!(at(LINK_STALE_MS + 1), ["link_stale"]);
        assert_eq!(at(LINK_LOST_MS + 1), ["link_lost"]);
    }
}
