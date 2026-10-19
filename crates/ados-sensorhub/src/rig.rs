//! Sensor-to-body rotations from the stored `rig.json`.
//!
//! The hub reports IMU and magnetometer vectors in the chip frame; the bus
//! carries body FLU. The rotation for sensor `imu<k>` / `mag<k>` is the upper
//! 3×3 block of that sensor's `T_body_sensor` (row-major 4×4).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

/// Row-major 3×3 rotation, sensor → body.
pub type Mat3 = [[f64; 3]; 3];

pub const IDENTITY: Mat3 = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];

/// `rig.json` in the agent config directory (`ADOS_ETC_DIR` aware).
pub fn rig_path() -> PathBuf {
    std::env::var_os("ADOS_ETC_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/etc/ados"))
        .join("rig.json")
}

/// The rotations of every rig sensor, by id.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Extrinsics {
    rotations: HashMap<String, Mat3>,
}

impl Extrinsics {
    /// Load the rig. An absent file is an empty rig; an invalid one is
    /// reported and treated as empty.
    pub fn load(path: &Path) -> Self {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "rig unreadable; sensor frames unrotated");
                return Self::default();
            }
        };
        let parsed = serde_json::from_str::<Value>(&text)
            .map_err(|e| e.to_string())
            .and_then(|doc| Self::from_rig(&doc));
        parsed.unwrap_or_else(|e| {
            tracing::warn!(path = %path.display(), error = %e, "rig invalid; sensor frames unrotated");
            Self::default()
        })
    }

    /// Extract the rotations of a contract rig document (validated first).
    pub fn from_rig(doc: &Value) -> Result<Self, String> {
        ados_sensorbus::rig::validate(doc)?;
        let mut rotations = HashMap::new();
        for s in doc["sensors"].as_array().into_iter().flatten() {
            let (Some(id), Some(t)) = (s["id"].as_str(), s["T_body_sensor"].as_array()) else {
                continue;
            };
            let m: Vec<f64> = t.iter().filter_map(Value::as_f64).collect();
            if m.len() != 16 {
                return Err(format!("sensor {id}: T_body_sensor is not 16 numbers"));
            }
            let r = [[m[0], m[1], m[2]], [m[4], m[5], m[6]], [m[8], m[9], m[10]]];
            rotations.insert(id.to_string(), r);
        }
        Ok(Self { rotations })
    }

    pub fn rotation(&self, id: &str) -> Option<&Mat3> {
        self.rotations.get(id)
    }
}

/// `r · v`.
pub fn rotate(r: &Mat3, v: [f64; 3]) -> [f64; 3] {
    [
        r[0][0] * v[0] + r[0][1] * v[1] + r[0][2] * v[2],
        r[1][0] * v[0] + r[1][1] * v[1] + r[1][2] * v[2],
        r[2][0] * v[0] + r[2][1] * v[1] + r[2][2] * v[2],
    ]
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    /// A minimal contract rig with one IMU mounted chip-x → body-left,
    /// chip-y → body-back, chip-z → body-up (a 90° yaw).
    pub(crate) fn rig_with_imu0() -> Value {
        let yaw90 = [
            0.0, -1.0, 0.0, 0.01, //
            1.0, 0.0, 0.0, 0.0, //
            0.0, 0.0, 1.0, 0.0, //
            0.0, 0.0, 0.0, 1.0,
        ];
        json!({
            "contract": "ados.capture/1",
            "rig_id": "test-rig",
            "calib_version": "2026-10-19.1",
            "sensors": [
                {"id": "imu0", "kind": "imu", "role": "nav", "T_body_sensor": yaw90, "time_offset_ns": 0}
            ]
        })
    }

    #[test]
    fn rotations_come_from_the_transform_block() {
        let ext = Extrinsics::from_rig(&rig_with_imu0()).unwrap();
        let r = ext.rotation("imu0").unwrap();
        assert_eq!(rotate(r, [1.0, 0.0, 0.0]), [0.0, 1.0, 0.0]);
        assert_eq!(rotate(r, [0.0, 1.0, 0.0]), [-1.0, 0.0, 0.0]);
        assert_eq!(rotate(&IDENTITY, [1.0, 2.0, 3.0]), [1.0, 2.0, 3.0]);
        assert!(ext.rotation("mag0").is_none());

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rig.json");
        std::fs::write(&path, rig_with_imu0().to_string()).unwrap();
        assert_eq!(Extrinsics::load(&path), ext);
    }

    #[test]
    fn an_absent_or_invalid_rig_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            Extrinsics::load(&dir.path().join("rig.json")),
            Extrinsics::default()
        );
        std::fs::write(dir.path().join("bad.json"), "{not json").unwrap();
        assert_eq!(
            Extrinsics::load(&dir.path().join("bad.json")),
            Extrinsics::default()
        );
    }
}
