//! `rig.json` validation against the vendored contract schema, and the
//! subset of the rig the agent reads (sensor id → kind/role).

use std::sync::LazyLock;

use jsonschema::{Draft, JSONSchema};
use serde::Deserialize;
use serde_json::Value;

const BASE: &str = "https://altnautica.com/schemas/ados-capture/1/";
const RIG: &str = include_str!("../contract/schema/rig.schema.json");
const CAMERA_MODEL: &str = include_str!("../contract/schema/camera_model.schema.json");
const MANIFEST: &str = include_str!("../contract/schema/manifest.schema.json");
const GEO_ANCHOR: &str = include_str!("../contract/schema/geo_anchor.schema.json");
static SCHEMA: LazyLock<JSONSchema> = LazyLock::new(|| {
    let parse = |s: &str| serde_json::from_str::<Value>(s).expect("vendored schema is JSON");
    JSONSchema::options()
        .with_draft(Draft::Draft202012)
        .with_document(
            format!("{BASE}camera_model.schema.json"),
            parse(CAMERA_MODEL),
        )
        .with_document(format!("{BASE}manifest.schema.json"), parse(MANIFEST))
        .with_document(format!("{BASE}geo_anchor.schema.json"), parse(GEO_ANCHOR))
        .compile(&parse(RIG))
        .expect("vendored rig schema compiles")
});

/// The fields of a rig the agent itself uses.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RigSummary {
    pub rig_id: String,
    pub calib_version: String,
    pub sensors: Vec<RigSensor>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RigSensor {
    pub id: String,
    pub kind: String,
    pub role: String,
}

impl RigSummary {
    /// The rig sensor with this id.
    pub fn sensor(&self, id: &str) -> Option<&RigSensor> {
        self.sensors.iter().find(|s| s.id == id)
    }
}

/// Validate a `rig.json` document against the contract Rig schema.
/// `Err` carries a one-line description of the first few violations.
pub fn validate(doc: &Value) -> Result<RigSummary, String> {
    if let Err(errors) = SCHEMA.validate(doc) {
        let msgs: Vec<String> = errors
            .take(3)
            .map(|e| format!("{}: {}", e.instance_path, e))
            .collect();
        return Err(msgs.join("; "));
    }
    serde_json::from_value(doc.clone()).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rig() -> Value {
        json!({
            "contract": "ados.capture/1",
            "rig_id": "visor-pro-a",
            "calib_version": "2026-10-19.1",
            "sensors": [{
                "id": "front_left", "kind": "camera", "role": "front_left",
                "T_body_sensor": [1.0,0.0,0.0,0.0, 0.0,1.0,0.0,0.0, 0.0,0.0,1.0,0.0, 0.0,0.0,0.0,1.0],
                "camera": {"model": "pinhole", "width": 640, "height": 480,
                           "fx": 500.0, "fy": 500.0, "cx": 320.0, "cy": 240.0,
                           "dist": [], "calibrated": false, "readout_ns": 0},
                "time_offset_ns": 0
            }, {
                "id": "imu0", "kind": "imu", "role": "nav",
                "T_body_sensor": [1.0,0.0,0.0,0.0, 0.0,1.0,0.0,0.0, 0.0,0.0,1.0,0.0, 0.0,0.0,0.0,1.0],
                "time_offset_ns": 0
            }]
        })
    }

    #[test]
    fn a_valid_rig_passes_and_summarises() {
        let s = validate(&rig()).unwrap();
        assert_eq!(s.rig_id, "visor-pro-a");
        assert_eq!(s.sensor("imu0").unwrap().role, "nav");
    }

    #[test]
    fn schema_violations_are_reported() {
        let mut bad = rig();
        bad["sensors"][0]["kind"] = json!("sonar");
        assert!(validate(&bad).is_err());
        let mut short = rig();
        short["sensors"][1]["T_body_sensor"] = json!([1.0, 0.0]);
        assert!(validate(&short).is_err());
        let mut contract = rig();
        contract["contract"] = json!("other/9");
        assert!(validate(&contract).is_err());
        assert!(validate(&json!({"rig_id": "x"})).is_err());
    }
}
