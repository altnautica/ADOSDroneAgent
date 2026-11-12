//! The detector's per-camera state and the depth layer, from
//! `GET /api/vision/status`.

use serde_json::Value;

use super::s;

/// The vision engine's status read (its `detection` block names the cameras
/// the detector is running on).
pub const VISION_STATUS_PATH: &str = "/api/vision/status";

/// The engine's newest detection batch, read as the dashboard reads it.
pub const DETECTIONS_LATEST_PATH: &str = "/api/vision/detections/latest";

/// What the newest batch found: the object count and its camera.
#[derive(Debug, Clone, PartialEq)]
pub struct Seen {
    pub count: usize,
    pub camera: Option<String>,
}

impl Seen {
    /// The batch a `/latest` reply carries; `None` when it carries none
    /// (vision off, or a stale batch).
    pub fn from_latest(v: &Value) -> Option<Self> {
        if v.get("available").and_then(Value::as_bool) != Some(true)
            || v.get("stale").and_then(Value::as_bool) == Some(true)
        {
            return None;
        }
        Some(Self {
            count: v.get("detections")?.as_array()?.len(),
            camera: s(v, "camera_id"),
        })
    }
}

/// Where the detector runs: the engine detects on a camera only while a
/// consumer holds it (`on_demand`), or on every captured camera (`always`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Detection {
    pub mode: Option<String>,
    /// The cameras detection is running on, sorted.
    pub detecting: Vec<String>,
    /// The depth layer, when the engine reports it.
    pub depth: Option<DepthLayer>,
    /// The newest batch's count, when the engine reports a fresh one.
    pub seen: Option<Seen>,
}

/// The depth layer: whether its model is installed, and where the depth grid
/// and the landing zones run.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DepthLayer {
    /// `ready`, `no_model` or `unavailable`.
    pub state: String,
    /// The cameras the grid runs on, sorted, each with its last scale source.
    pub grids: Vec<(String, Option<String>)>,
    /// The cameras the landing zones run on, sorted.
    pub surfaces: Vec<String>,
}

impl DepthLayer {
    fn from_block(d: &Value) -> Option<Self> {
        let d = d.as_object()?;
        let mut grids = Vec::new();
        let mut surfaces = Vec::new();
        for (id, cam) in d
            .get("cameras")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
        {
            if cam.get("depth").and_then(Value::as_bool) == Some(true) {
                let scale = cam
                    .get("scale_source")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                grids.push((id.clone(), scale));
            }
            if cam.get("surfaces").and_then(Value::as_bool) == Some(true) {
                surfaces.push(id.clone());
            }
        }
        grids.sort();
        surfaces.sort();
        Some(Self {
            state: d
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            grids,
            surfaces,
        })
    }

    /// `no model`, `unavailable`, `idle`, or where it runs:
    /// `main (lidar) · zones: belly`.
    pub fn word(&self) -> String {
        match self.state.as_str() {
            "no_model" => return "no model".into(),
            "unavailable" => return "unavailable".into(),
            _ => {}
        }
        let mut parts = Vec::new();
        if !self.grids.is_empty() {
            let grids: Vec<String> = self
                .grids
                .iter()
                .map(|(id, scale)| match scale.as_deref() {
                    Some("lidar") => format!("{id} (lidar)"),
                    Some(_) => format!("{id} (unscaled)"),
                    None => id.clone(),
                })
                .collect();
            parts.push(grids.join(", "));
        }
        if !self.surfaces.is_empty() {
            parts.push(format!("zones: {}", self.surfaces.join(", ")));
        }
        if parts.is_empty() {
            "idle".into()
        } else {
            parts.join(" · ")
        }
    }
}

impl Detection {
    /// The `detection` block of a `/api/vision/status` body (with its `depth`
    /// block, when present); `None` when the engine does not report one.
    pub fn from_status(v: &Value) -> Option<Self> {
        let d = v.get("detection").filter(|d| d.is_object())?;
        let mut detecting: Vec<String> = d
            .get("cameras")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
            .filter(|(_, cam)| cam.get("detecting").and_then(Value::as_bool) == Some(true))
            .map(|(id, _)| id.clone())
            .collect();
        detecting.sort();
        Some(Self {
            mode: s(d, "mode"),
            detecting,
            depth: v.get("depth").and_then(DepthLayer::from_block),
            seen: None,
        })
    }

    /// `idle`, or `detecting: main, belly`, then what the newest batch found
    /// (`· 3 objects on main`) while the detector runs and the batch is fresh.
    pub fn word(&self) -> String {
        if self.detecting.is_empty() {
            return "idle".to_string();
        }
        let mut word = format!("detecting: {}", self.detecting.join(", "));
        if let Some(seen) = &self.seen {
            let noun = if seen.count == 1 { "object" } else { "objects" };
            word.push_str(&format!(" · {} {noun}", seen.count));
            if let Some(cam) = &seen.camera {
                word.push_str(&format!(" on {cam}"));
            }
        }
        word
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn detection_names_the_cameras_the_detector_runs_on() {
        let d = Detection::from_status(&json!({"detection": {
            "mode": "on_demand",
            "cameras": {
                "main": {"detecting": true, "leases": [{"owner": "cockpit", "remaining_ms": 4100}]},
                "belly": {"detecting": false, "leases": []},
                "aft": {"detecting": true, "leases": []},
            },
        }}))
        .unwrap();
        assert_eq!(d.mode.as_deref(), Some("on_demand"));
        assert_eq!(d.detecting, vec!["aft", "main"]);
        assert_eq!(d.word(), "detecting: aft, main");
    }

    #[test]
    fn detection_with_no_camera_held_reads_idle() {
        let d = Detection::from_status(&json!({"detection": {
            "mode": "on_demand",
            "cameras": {"main": {"detecting": false, "leases": []}},
        }}))
        .unwrap();
        assert_eq!(d.word(), "idle");
    }

    #[test]
    fn a_status_without_a_detection_block_has_no_detection() {
        assert_eq!(Detection::from_status(&json!({"models": []})), None);
        assert_eq!(Detection::from_status(&json!({"detection": null})), None);
    }

    #[test]
    fn the_depth_layer_names_its_cameras_and_scale_or_says_why_it_cannot_run() {
        let status = |depth: Value| {
            Detection::from_status(&json!({
                "detection": {"mode": "on_demand", "cameras": {}},
                "depth": depth,
            }))
            .unwrap()
            .depth
        };
        let d = status(json!({
            "state": "ready", "interval_ms": 500,
            "cameras": {
                "main": {"depth": true, "surfaces": false, "leases": [], "scale_source": "lidar", "runs": 3},
                "belly": {"depth": true, "surfaces": true, "leases": [], "scale_source": "none", "runs": 1},
                "aft": {"depth": false, "surfaces": false, "leases": [], "runs": 0},
            },
        }))
        .unwrap();
        assert_eq!(d.word(), "belly (unscaled), main (lidar) · zones: belly");
        let idle = status(json!({"state": "ready", "interval_ms": 500, "cameras": {}})).unwrap();
        assert_eq!(idle.word(), "idle");
        let none = status(json!({"state": "no_model", "interval_ms": 0, "cameras": {}})).unwrap();
        assert_eq!(none.word(), "no model");
        assert_eq!(status(Value::Null), None);
    }
}
