//! Typed vehicle readings from `GET /api/telemetry`, the dashboard snapshot's
//! FC block, and the battery packs from `GET /api/v1/battery`.
//!
//! Every field is optional: the route answers `{}` when the state hub is
//! silent and drops the vehicle keys while the FC is down, so nothing here is
//! filled in with a default.

use serde_json::Value;

use super::ring::Ring;

/// Chart history length: 120 samples, one per second.
pub const VEHICLE_HISTORY: usize = 120;

fn f(v: &Value, path: &[&str]) -> Option<f64> {
    walk(v, path)
        .and_then(Value::as_f64)
        .filter(|x| x.is_finite())
}

fn i(v: &Value, path: &[&str]) -> Option<i64> {
    walk(v, path).and_then(Value::as_i64)
}

fn s(v: &Value, path: &[&str]) -> Option<String> {
    walk(v, path)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn walk<'a>(v: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter().try_fold(v, |cur, key| cur.get(key))
}

/// One `/api/telemetry` reading.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Telemetry {
    /// The vehicle block is present (FC alive, or a fresh relayed vehicle).
    pub vehicle: bool,
    pub relayed: bool,
    /// Radians.
    pub roll: Option<f64>,
    pub pitch: Option<f64>,
    /// Degrees 0–360, from the position block: `None` before a position fix
    /// (the route then reports a 0.0 placeholder) and for the FC's "unknown"
    /// sentinel (655.35).
    pub heading: Option<f64>,
    pub voltage: Option<f64>,
    pub current: Option<f64>,
    pub remaining: Option<f64>,
    pub temperature: Option<f64>,
    pub fix_type: Option<i64>,
    pub satellites: Option<i64>,
    /// GPS horizontal dilution (`gps.eph`).
    pub eph: Option<f64>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub alt_rel: Option<f64>,
    pub alt_msl: Option<f64>,
    pub groundspeed: Option<f64>,
    pub climb: Option<f64>,
    pub mode: Option<String>,
    pub armed: Option<bool>,
}

impl Telemetry {
    pub fn from_value(v: &Value) -> Self {
        // The vehicle keys travel together; `attitude` is the marker.
        let vehicle = v.get("attitude").is_some_and(Value::is_object);
        let relayed = v.get("telemetry_source").and_then(Value::as_str) == Some("relayed");
        if !vehicle {
            return Self {
                relayed,
                ..Self::default()
            };
        }
        // Position is real only once a GLOBAL_POSITION_INT arrived.
        let has_position = v.get("position_age_ms").is_some_and(|a| !a.is_null());
        let pos = |k: &str| {
            if has_position {
                f(v, &["position", k])
            } else {
                None
            }
        };
        Self {
            vehicle,
            relayed,
            roll: f(v, &["attitude", "roll"]),
            pitch: f(v, &["attitude", "pitch"]),
            heading: pos("heading").filter(|h| (0.0..=360.0).contains(h)),
            voltage: f(v, &["battery", "voltage"]),
            current: f(v, &["battery", "current"]),
            remaining: f(v, &["battery", "remaining"]),
            temperature: f(v, &["battery", "temperature"]),
            fix_type: i(v, &["gps", "fix_type"]),
            satellites: i(v, &["gps", "satellites"]),
            eph: f(v, &["gps", "eph"]),
            lat: pos("lat"),
            lon: pos("lon"),
            alt_rel: pos("alt_rel"),
            alt_msl: pos("alt_msl"),
            groundspeed: f(v, &["velocity", "groundspeed"]),
            climb: f(v, &["velocity", "climb"]),
            mode: s(v, &["mode"]),
            armed: v.get("armed").and_then(Value::as_bool),
        }
    }
}

/// The FC identity from `/api/v1/dashboard/snapshot` `fc`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FcIdent {
    pub vehicle: Option<String>,
    pub firmware: Option<String>,
    pub hdop: Option<f64>,
}

impl FcIdent {
    pub fn from_snapshot(v: &Value) -> Self {
        Self {
            vehicle: s(v, &["fc", "vehicle"]),
            firmware: s(v, &["fc", "firmware"]),
            hdop: f(v, &["fc", "gps", "hdop"]),
        }
    }
}

/// The 1 Hz chart buffers.
#[derive(Debug, Clone)]
pub struct VehicleHistory {
    pub alt: Ring,
    pub volts: Ring,
    pub amps: Ring,
    pub pct: Ring,
}

impl Default for VehicleHistory {
    fn default() -> Self {
        Self {
            alt: Ring::new(VEHICLE_HISTORY),
            volts: Ring::new(VEHICLE_HISTORY),
            amps: Ring::new(VEHICLE_HISTORY),
            pct: Ring::new(VEHICLE_HISTORY),
        }
    }
}

impl VehicleHistory {
    /// Record the values present in one reading.
    pub fn record(&mut self, t: &Telemetry) {
        self.alt.push(t.alt_rel);
        self.volts.push(t.voltage);
        self.amps.push(t.current);
        self.pct.push(t.remaining);
    }
}

/// One live battery anomaly.
#[derive(Debug, Clone, PartialEq)]
pub struct Anomaly {
    pub rule: String,
    pub critical: bool,
    pub value: Option<f64>,
    pub threshold: Option<f64>,
}

/// One pack from `/api/v1/battery`.
#[derive(Debug, Clone, PartialEq)]
pub struct Pack {
    pub id: i64,
    pub source: String,
    pub stale: bool,
    pub cells_plausible: bool,
    /// Volts per cell.
    pub cells: Vec<f64>,
    pub weakest: Option<usize>,
    pub divergence_mv: Option<f64>,
    pub voltage: Option<f64>,
    pub current: Option<f64>,
    pub remaining: Option<f64>,
    pub temperature: Option<f64>,
    pub prediction_state: Option<String>,
    pub eta_s: Option<i64>,
    pub anomalies: Vec<Anomaly>,
}

/// The battery route as a whole.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Batteries {
    pub enabled: bool,
    pub stale: bool,
    pub reserve_percent: Option<i64>,
    pub packs: Vec<Pack>,
}

impl Batteries {
    pub fn from_value(v: &Value) -> Self {
        let packs = v
            .get("packs")
            .and_then(Value::as_array)
            .map(|a| a.iter().map(pack).collect())
            .unwrap_or_default();
        Self {
            enabled: v.get("enabled").and_then(Value::as_bool).unwrap_or(false),
            stale: v.get("stale").and_then(Value::as_bool).unwrap_or(true),
            reserve_percent: i(v, &["thresholds", "reserve_percent"]),
            packs,
        }
    }
}

fn pack(p: &Value) -> Pack {
    Pack {
        id: i(p, &["id"]).unwrap_or(0),
        source: s(p, &["source"]).unwrap_or_default(),
        stale: p.get("stale").and_then(Value::as_bool).unwrap_or(false),
        cells_plausible: p
            .get("cells_plausible")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        cells: p
            .get("cell_voltages_v")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_f64).collect())
            .unwrap_or_default(),
        weakest: p
            .get("weakest_cell_index")
            .and_then(Value::as_u64)
            .and_then(|n| usize::try_from(n).ok()),
        divergence_mv: f(p, &["divergence_mv"]),
        voltage: f(p, &["voltage_v"]),
        current: f(p, &["current_a"]),
        remaining: f(p, &["remaining_pct"]),
        temperature: f(p, &["temperature_c"]),
        prediction_state: s(p, &["prediction", "state"]),
        eta_s: i(p, &["prediction", "eta_s"]),
        anomalies: p
            .get("anomalies")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .map(|x| Anomaly {
                        rule: s(x, &["rule"]).unwrap_or_default(),
                        critical: x.get("severity").and_then(Value::as_str) == Some("critical"),
                        value: f(x, &["value"]),
                        threshold: f(x, &["threshold"]),
                    })
                    .collect()
            })
            .unwrap_or_default(),
    }
}

/// `7:05` for seconds.
pub fn eta_text(secs: i64) -> String {
    let s = secs.max(0);
    format!("{}:{:02}", s / 60, s % 60)
}

/// Whether the FC speaks MSP (Betaflight / iNav): the agent does not decode
/// its telemetry.
pub fn is_msp(fc_firmware: Option<&str>, fc_variant: Option<&str>) -> bool {
    matches!(fc_firmware, Some("betaflight" | "inav")) || fc_variant.is_some()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn telemetry_extraction_tolerates_nulls_and_missing_blocks() {
        assert!(!Telemetry::from_value(&json!({})).vehicle);
        let down = Telemetry::from_value(&json!({"param_generation": 4}));
        assert!(!down.vehicle);
        assert_eq!(down.voltage, None);

        let t = Telemetry::from_value(&json!({
            "armed": false, "mode": "",
            "attitude": {"roll": 0.1, "pitch": null, "yaw": 1.5},
            "position": {"lat": 0.0, "lon": 0.0, "alt_rel": 0.0, "heading": 655.35},
            "position_age_ms": null,
            "battery": {"voltage": null, "current": 3.5, "remaining": null, "cell_voltages": []},
            "gps": {"fix_type": 3, "satellites": 9, "eph": null}
        }));
        assert!(t.vehicle);
        assert_eq!(t.roll, Some(0.1));
        assert_eq!(t.pitch, None);
        assert_eq!(t.heading, None, "no heading until GLOBAL_POSITION_INT");
        assert_eq!(t.lat, None, "no position until GLOBAL_POSITION_INT");
        assert_eq!(t.alt_rel, None);
        assert_eq!(t.voltage, None);
        assert_eq!(t.current, Some(3.5));
        assert_eq!(t.mode, None);
        assert_eq!(t.armed, Some(false));
        assert_eq!(t.eph, None);
        assert_eq!(t.fix_type, Some(3));
    }

    #[test]
    fn heading_needs_a_position_fix() {
        let sample = |age: Value, heading: f64| {
            Telemetry::from_value(&json!({
                "attitude": {"roll": 0.0, "pitch": 0.0, "yaw": 0.0},
                "position": {"lat": 0.0, "lon": 0.0, "alt_rel": 0.0, "heading": heading},
                "position_age_ms": age,
            }))
        };
        assert_eq!(sample(Value::Null, 0.0).heading, None, "0.0 placeholder before a fix");
        assert_eq!(sample(json!(120), 0.0).heading, Some(0.0));
        assert_eq!(sample(json!(120), 271.5).heading, Some(271.5));
        assert_eq!(sample(json!(120), 655.35).heading, None, "unknown sentinel");
    }

    #[test]
    fn history_records_only_present_values() {
        let mut h = VehicleHistory::default();
        h.record(&Telemetry {
            voltage: Some(16.4),
            ..Telemetry::default()
        });
        h.record(&Telemetry::default());
        assert_eq!(h.volts.len(), 1);
        assert!(h.alt.is_empty());
        assert!(h.amps.is_empty());
    }

    #[test]
    fn battery_packs_parse_with_nullable_fields() {
        let b = Batteries::from_value(&json!({
            "enabled": true, "stale": false, "thresholds": {"reserve_percent": 25},
            "packs": [{
                "id": 0, "source": "mavlink", "stale": false, "cells_plausible": true,
                "cell_voltages_v": [3.9, 3.45], "weakest_cell_index": 1,
                "divergence_mv": 450.0, "voltage_v": 7.35, "current_a": null,
                "remaining_pct": 60, "prediction": {"state": "normal", "eta_s": 425},
                "anomalies": [{"rule": "cell_low", "severity": "warning", "value": 3.45, "threshold": 3.5}]
            }]
        }));
        assert_eq!(b.packs.len(), 1);
        let p = &b.packs[0];
        assert_eq!(p.weakest, Some(1));
        assert_eq!(p.current, None);
        assert_eq!(p.eta_s.map(eta_text).as_deref(), Some("7:05"));
        assert_eq!(p.anomalies[0].rule, "cell_low");
        assert!(!p.anomalies[0].critical);
        assert!(Batteries::from_value(&json!({})).packs.is_empty());
    }

    #[test]
    fn msp_is_recognised_from_either_field() {
        assert!(is_msp(Some("betaflight"), None));
        assert!(is_msp(None, Some("inav")));
        assert!(!is_msp(Some("ardupilot"), None));
    }
}
