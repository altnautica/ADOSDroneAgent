//! Flight-controller parameter metadata: the MAVLink catalogs the web dashboard
//! ships, embedded in the binary and decoded on a background thread.
//!
//! A catalog gives each parameter its human name, description, range, units,
//! enum values, bitmask bits, default, and the reboot/read-only flags. A
//! parameter the catalog does not know still lists, with no metadata.

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver};
use std::thread;

use flate2::read::GzDecoder;
use serde::Deserialize;

/// Every embedded catalog key.
pub const CATALOG_KEYS: [&str; 5] = [
    "ardupilot-copter",
    "ardupilot-plane",
    "ardupilot-rover",
    "ardupilot-sub",
    "px4",
];

/// The gzip bytes of one catalog.
fn bytes(key: &str) -> Option<&'static [u8]> {
    Some(match key {
        "ardupilot-copter" => &include_bytes!("../../../../dashboard/public/param-metadata/ardupilot-copter.json.gz")[..],
        "ardupilot-plane" => &include_bytes!("../../../../dashboard/public/param-metadata/ardupilot-plane.json.gz")[..],
        "ardupilot-rover" => &include_bytes!("../../../../dashboard/public/param-metadata/ardupilot-rover.json.gz")[..],
        "ardupilot-sub" => &include_bytes!("../../../../dashboard/public/param-metadata/ardupilot-sub.json.gz")[..],
        "px4" => &include_bytes!("../../../../dashboard/public/param-metadata/px4.json.gz")[..],
        _ => return None,
    })
}

/// Where a catalog came from.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Provenance {
    pub firmware: String,
    pub version: String,
    pub param_count: u64,
}

/// A value range.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
pub struct Range {
    pub min: f64,
    pub max: f64,
}

/// One parameter's metadata.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ParamMeta {
    pub name: String,
    pub human_name: String,
    pub description: String,
    pub range: Option<Range>,
    pub units: Option<String>,
    /// Enum code → label. Codes may be fractional.
    pub values: Option<Vec<(f64, String)>>,
    /// Bit index → label.
    pub bitmask: Option<Vec<(u32, String)>>,
    pub increment: Option<f64>,
    pub default_value: Option<f64>,
    pub reboot_required: bool,
    pub read_only: bool,
    pub group: Option<String>,
    pub decimal_places: Option<u32>,
}

impl ParamMeta {
    /// The enum entries, when there are any.
    pub fn enum_values(&self) -> Option<&[(f64, String)]> {
        self.values.as_deref().filter(|v| !v.is_empty())
    }

    /// The bitmask entries, when there are any.
    pub fn bits(&self) -> Option<&[(u32, String)]> {
        self.bitmask.as_deref().filter(|b| !b.is_empty())
    }

    /// The enum label for `value`.
    pub fn label_for(&self, value: f64) -> Option<&str> {
        self.enum_values()?
            .iter()
            .find(|(code, _)| crate::params::same(*code, value))
            .map(|(_, label)| label.as_str())
    }

    /// Whether `value` is outside the declared range.
    pub fn out_of_range(&self, value: f64) -> bool {
        self.range.is_some_and(|r| value < r.min || value > r.max)
    }
}

#[derive(Deserialize)]
struct Snapshot {
    provenance: Provenance,
    params: Vec<ParamMeta>,
}

/// A decoded catalog, indexed by parameter name.
#[derive(Debug, Default)]
pub struct Catalog {
    pub key: &'static str,
    pub provenance: Provenance,
    pub params: HashMap<String, ParamMeta>,
}

impl Catalog {
    pub fn get(&self, name: &str) -> Option<&ParamMeta> {
        self.params.get(name)
    }
}

/// Decode one embedded catalog.
pub fn decode(key: &str) -> Result<Catalog, String> {
    let key = CATALOG_KEYS
        .iter()
        .copied()
        .find(|k| *k == key)
        .ok_or_else(|| format!("no embedded catalog {key}"))?;
    let gz = bytes(key).ok_or_else(|| format!("no embedded catalog {key}"))?;
    let snap: Snapshot = serde_json::from_reader(GzDecoder::new(gz)).map_err(|e| format!("{key}: {e}"))?;
    Ok(Catalog {
        key,
        provenance: snap.provenance,
        params: snap.params.into_iter().map(|p| (p.name.clone(), p)).collect(),
    })
}

/// Decode a catalog on a background thread; the receiver yields exactly once.
pub fn spawn_decode(key: &'static str) -> Receiver<Result<Catalog, String>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(decode(key));
    });
    rx
}

/// The catalog for an FC: the firmware family from `/api/status` plus the
/// vehicle type label from the dashboard snapshot (`fc.vehicle`, a
/// `MAV_TYPE` name such as `Fixed Wing`, `VTOL Quadrotor`, `Ground Rover`,
/// `Surface Boat`, `Submarine`). ArduPilot splits per vehicle: fixed wing and
/// every VTOL frame run Plane, rovers and boats run Rover, submarines run Sub,
/// and everything else (multirotors, helicopters, unknown) defaults to copter.
/// `None` for an MSP or unidentified FC.
pub fn catalog_key(fc_firmware: &str, vehicle: Option<&str>) -> Option<&'static str> {
    match fc_firmware.to_ascii_lowercase().as_str() {
        "px4" => Some("px4"),
        "ardupilot" => {
            let v = vehicle.unwrap_or("").trim().to_ascii_lowercase();
            Some(
                if v.contains("fixed wing") || v.contains("plane") || v.starts_with("vtol") {
                    "ardupilot-plane"
                } else if v.contains("rover") || v.contains("boat") {
                    "ardupilot-rover"
                } else if v.contains("submarine") || v == "sub" {
                    "ardupilot-sub"
                } else {
                    "ardupilot-copter"
                },
            )
        }
        _ => None,
    }
}

/// A value for display: fixed to `decimalPlaces` when the catalog gives it,
/// else the shortest text that reads back as the same single-precision float
/// (the wire type of every MAVLink parameter), integers without a fraction.
pub fn format_value(value: f64, meta: Option<&ParamMeta>) -> String {
    if let Some(dp) = meta.and_then(|m| m.decimal_places) {
        return format!("{value:.*}", dp as usize);
    }
    plain(value)
}

/// A value as plain text, no metadata.
pub fn plain(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        format!("{}", value as f32)
    }
}

/// A value with its enum label, e.g. `1 (Enabled)`.
pub fn format_labeled(value: f64, meta: Option<&ParamMeta>) -> String {
    let text = format_value(value, meta);
    match meta.and_then(|m| m.label_for(value)) {
        Some(label) => format!("{text} ({label})"),
        None => text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_embedded_catalog_decodes() {
        for key in CATALOG_KEYS {
            let cat = decode(key).unwrap_or_else(|e| panic!("{e}"));
            assert!(cat.provenance.param_count > 0, "{key}");
            assert!(!cat.params.is_empty(), "{key}");
        }
    }

    #[test]
    fn catalog_key_follows_firmware_and_vehicle() {
        for plane in [
            "Fixed Wing",
            "VTOL Quadrotor",
            "VTOL Tailsitter",
            "VTOL Tailsitter Duo",
            "VTOL Tiltrotor",
            "VTOL Tiltwing",
            "VTOL Reserved",
        ] {
            assert_eq!(catalog_key("ardupilot", Some(plane)), Some("ardupilot-plane"), "{plane}");
        }
        assert_eq!(catalog_key("ardupilot", Some("Ground Rover")), Some("ardupilot-rover"));
        assert_eq!(catalog_key("ardupilot", Some("Helicopter")), Some("ardupilot-copter"));
        assert_eq!(catalog_key("ardupilot", Some("Hexarotor")), Some("ardupilot-copter"));
        assert_eq!(catalog_key("ardupilot", Some("Vehicle 99")), Some("ardupilot-copter"));
        assert_eq!(catalog_key("ardupilot", Some("Surface Boat")), Some("ardupilot-rover"));
        assert_eq!(catalog_key("ardupilot", Some("Submarine")), Some("ardupilot-sub"));
        assert_eq!(catalog_key("ardupilot", Some("Quadrotor")), Some("ardupilot-copter"));
        assert_eq!(catalog_key("ardupilot", None), Some("ardupilot-copter"));
        assert_eq!(catalog_key("px4", Some("Quadrotor")), Some("px4"));
        assert_eq!(catalog_key("px4", None), Some("px4"));
        assert_eq!(catalog_key("betaflight", None), None);
        assert_eq!(catalog_key("unknown", None), None);
    }

    #[test]
    fn values_format_with_precision_and_labels() {
        let meta = ParamMeta {
            values: Some(vec![(0.0, "Disabled".into()), (1.0, "Enabled".into())]),
            ..Default::default()
        };
        assert_eq!(format_labeled(1.0, Some(&meta)), "1 (Enabled)");
        assert_eq!(plain(0.135_000_005_364_418_03), "0.135");
        let dp = ParamMeta {
            decimal_places: Some(2),
            ..Default::default()
        };
        assert_eq!(format_value(1.5, Some(&dp)), "1.50");
    }
}
