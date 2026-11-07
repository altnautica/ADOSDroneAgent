//! The autonomy readings from `GET /api/v1/nav/status`: who flies the vehicle
//! (the `pilot` kind), the follow state, the navigation source, the lidar and
//! the live map session; and the detector's per-camera state from
//! `GET /api/vision/status`.
//!
//! The nav route answers each kind as `{ts_ms, age_ms, data}`, or `null` when
//! no plugin has published it, so every part here is optional and nothing is
//! filled with a default.

use serde_json::Value;

/// The path the Vehicle tab polls.
pub const NAV_STATUS_PATH: &str = "/api/v1/nav/status";

/// A kind older than this is shown as stale rather than as current.
pub const STALE_AFTER_MS: u64 = 3_000;

/// The vision engine's status read (its `detection` block names the cameras
/// the detector is running on).
pub const VISION_STATUS_PATH: &str = "/api/vision/status";

fn f(v: &Value, key: &str) -> Option<f64> {
    v.get(key).and_then(Value::as_f64).filter(|x| x.is_finite())
}

fn i(v: &Value, key: &str) -> Option<i64> {
    v.get(key).and_then(Value::as_i64)
}

fn s(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// One kind's payload and how old it is.
fn kind<'a>(v: &'a Value, name: &str) -> Option<(&'a Value, bool)> {
    let entry = v.get(name).filter(|e| !e.is_null())?;
    let data = entry.get("data")?;
    let stale = entry
        .get("age_ms")
        .and_then(Value::as_u64)
        .is_some_and(|age| age > STALE_AFTER_MS);
    Some((data, stale))
}

/// The follow-me read-back (`follow.state`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Follow {
    pub phase: Option<String>,
    pub track_id: Option<i64>,
    pub range_m: Option<f64>,
    pub range_sigma_m: Option<f64>,
    pub reacquire_mode: Option<String>,
    pub coast_remaining_s: Option<f64>,
    pub stale: bool,
}

/// The navigation source (vision-nav `nav.state`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NavSource {
    pub mode: Option<String>,
    pub active_set: Option<i64>,
    pub extnav_health: Option<String>,
    pub odom_rate_hz: Option<f64>,
    pub obstacle_rate_hz: Option<f64>,
    pub min_obstacle_m: Option<f64>,
    pub gps_fix: Option<i64>,
    pub stale: bool,
}

/// The latest lidar scan's nearest return.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Lidar {
    pub min_range_m: Option<f64>,
    pub stale: bool,
}

/// The live mapping session (`map_status`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MapSession {
    pub placement: Option<String>,
    pub sweep_points: Option<i64>,
    pub area_m2: Option<f64>,
    pub voxel_m: Option<f64>,
    pub memory_used_mb: Option<f64>,
    pub memory_budget_mb: Option<f64>,
    pub keyframes_dropped: Option<i64>,
    pub stale: bool,
}

/// The mission progress inside the pilot state.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PilotMission {
    pub state: Option<String>,
    pub seq: Option<i64>,
    pub total: Option<i64>,
}

/// Who flies the vehicle and how (the `pilot` kind).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Pilot {
    pub owner: Option<String>,
    pub owner_label: Option<String>,
    /// The owning skill's label, when a skill owns.
    pub skill_label: Option<String>,
    pub control_mode: Option<String>,
    pub profile: Option<String>,
    pub avoid: Option<String>,
    pub avoid_state: Option<String>,
    pub hold_reason: Option<String>,
    pub min_clearance_m: Option<f64>,
    pub fc_mode_name: Option<String>,
    pub armed: Option<bool>,
    pub mission: Option<PilotMission>,
    pub stale: bool,
}

/// One `/api/v1/nav/status` reading.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Autonomy {
    pub pilot: Option<Pilot>,
    pub follow: Option<Follow>,
    pub nav: Option<NavSource>,
    pub lidar: Option<Lidar>,
    pub map: Option<MapSession>,
}

impl Autonomy {
    pub fn from_value(v: &Value) -> Self {
        let pilot = kind(v, "pilot").map(|(d, stale)| Pilot {
            owner: s(d, "owner"),
            owner_label: s(d, "owner_label"),
            skill_label: d.get("skill").and_then(|k| s(k, "label")),
            control_mode: s(d, "control_mode"),
            profile: s(d, "profile"),
            avoid: s(d, "avoid"),
            avoid_state: s(d, "avoid_state"),
            hold_reason: s(d, "hold_reason"),
            min_clearance_m: f(d, "min_clearance_m"),
            fc_mode_name: s(d, "fc_mode_name"),
            armed: d.get("armed").and_then(Value::as_bool),
            mission: d
                .get("mission")
                .filter(|m| m.is_object())
                .map(|m| PilotMission {
                    state: s(m, "state"),
                    seq: i(m, "seq"),
                    total: i(m, "total"),
                }),
            stale,
        });
        let follow = kind(v, "follow").map(|(d, stale)| Follow {
            phase: s(d, "phase"),
            track_id: i(d, "track_id"),
            range_m: f(d, "range_m"),
            range_sigma_m: f(d, "range_sigma_m"),
            reacquire_mode: s(d, "reacquire_mode"),
            coast_remaining_s: f(d, "coast_remaining_s"),
            stale,
        });
        let nav = kind(v, "nav").map(|(d, stale)| NavSource {
            mode: s(d, "mode"),
            active_set: i(d, "active_set"),
            extnav_health: s(d, "extnav_health"),
            odom_rate_hz: f(d, "odom_rate_hz"),
            obstacle_rate_hz: f(d, "obstacle_rate_hz"),
            min_obstacle_m: f(d, "min_obstacle_m"),
            gps_fix: i(d, "gps_fix"),
            stale,
        });
        let lidar = kind(v, "scan").map(|(d, stale)| Lidar {
            // `0.0` is "no return" on the scan topic, so it is not a range.
            min_range_m: d
                .get("ranges_m")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_f64)
                .filter(|r| r.is_finite() && *r > 0.0)
                .reduce(f64::min),
            stale,
        });
        let map = kind(v, "map").map(|(d, stale)| MapSession {
            placement: s(d, "placement"),
            sweep_points: i(d, "sweep_points"),
            area_m2: f(d, "area_m2"),
            voxel_m: f(d, "voxel_m"),
            memory_used_mb: f(d, "memory_used_mb"),
            memory_budget_mb: f(d, "memory_budget_mb"),
            keyframes_dropped: i(d, "keyframes_dropped"),
            stale,
        });
        Self {
            pilot,
            follow,
            nav,
            lidar,
            map,
        }
    }

    /// Whether any kind has ever been published.
    pub fn any(&self) -> bool {
        self.pilot.is_some()
            || self.follow.is_some()
            || self.nav.is_some()
            || self.lidar.is_some()
            || self.map.is_some()
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
        })
    }

    /// `idle`, or `detecting: main, belly`.
    pub fn word(&self) -> String {
        if self.detecting.is_empty() {
            "idle".to_string()
        } else {
            format!("detecting: {}", self.detecting.join(", "))
        }
    }
}

/// How a state word reads: good, a caution, a fault, or neutral. The panel
/// pairs each with a symbol and a colour, and always prints the word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Good,
    Caution,
    Fault,
    Neutral,
}

/// The tone of a follow phase.
pub fn phase_tone(phase: &str) -> Tone {
    match phase {
        "following" => Tone::Good,
        "acquiring" | "coasting" | "searching" | "holding" => Tone::Caution,
        "lost" => Tone::Fault,
        _ => Tone::Neutral,
    }
}

/// The tone of an external-nav health word.
pub fn health_tone(health: &str) -> Tone {
    match health {
        "good" => Tone::Good,
        "degraded" => Tone::Caution,
        "lost" => Tone::Fault,
        _ => Tone::Neutral,
    }
}

/// A GPS fix type as a short word (MAVLink `GPS_FIX_TYPE`).
pub fn gps_fix_word(fix: i64) -> &'static str {
    match fix {
        0 | 1 => "no fix",
        2 => "2D",
        3 => "3D",
        4 => "DGPS",
        5 => "RTK float",
        6 => "RTK fixed",
        7 => "static",
        8 => "PPP",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn every_kind_null_reads_as_nothing_published() {
        let a = Autonomy::from_value(
            &json!({"pose": null, "scan": null, "nav": null, "follow": null, "map": null}),
        );
        assert_eq!(a, Autonomy::default());
        assert!(!a.any());
    }

    #[test]
    fn kinds_are_read_with_their_age() {
        let a = Autonomy::from_value(&json!({
            "follow": {"ts_ms": 1, "age_ms": 40, "data": {
                "phase": "coasting", "track_id": 12, "range_m": 10.2, "range_sigma_m": 0.6,
                "reacquire_mode": "smart", "coast_remaining_s": 1.4}},
            "nav": {"ts_ms": 1, "age_ms": 9000, "data": {
                "mode": "external_nav", "active_set": 1, "extnav_health": "good",
                "odom_rate_hz": 30.0, "obstacle_rate_hz": 10.0, "min_obstacle_m": 3.2, "gps_fix": 0}},
            "scan": {"ts_ms": 1, "age_ms": 50, "data": {"ranges_m": [0.0, 4.5, 2.25, 0.0]}},
            "map": {"ts_ms": 1, "age_ms": 500, "data": {
                "placement": "hybrid", "sweep_points": 120000, "area_m2": 850.5, "voxel_m": 0.1,
                "memory_used_mb": 210, "memory_budget_mb": 614, "keyframes_dropped": 3}},
        }));
        let follow = a.follow.unwrap();
        assert_eq!(follow.phase.as_deref(), Some("coasting"));
        assert_eq!(follow.track_id, Some(12));
        assert!(!follow.stale);
        let nav = a.nav.unwrap();
        assert_eq!(nav.active_set, Some(1));
        assert!(nav.stale, "9 s old is stale");
        assert_eq!(a.lidar.unwrap().min_range_m, Some(2.25));
        let map = a.map.unwrap();
        assert_eq!(map.sweep_points, Some(120000));
        assert_eq!(map.memory_budget_mb, Some(614.0));
    }

    #[test]
    fn a_scan_with_no_return_has_no_nearest_range() {
        let a = Autonomy::from_value(
            &json!({"scan": {"ts_ms": 1, "age_ms": 1, "data": {"ranges_m": [0.0, 0.0]}}}),
        );
        assert_eq!(a.lidar.unwrap().min_range_m, None);
    }

    /// The shared golden pilot frame, read from the control crate's fixture.
    pub(crate) fn pilot_frame() -> Value {
        serde_json::from_str(include_str!(
            "../../../ados-control/tests/fixtures/pilot-frame.json"
        ))
        .expect("pilot frame fixture is JSON")
    }

    #[test]
    fn the_pilot_kind_reads_the_golden_frame() {
        let a = Autonomy::from_value(
            &json!({"pilot": {"ts_ms": 1, "age_ms": 100, "data": pilot_frame()}}),
        );
        assert!(a.any());
        let p = a.pilot.unwrap();
        assert_eq!(p.owner.as_deref(), Some("skill"));
        assert_eq!(p.owner_label.as_deref(), Some("FOLLOW · ORBIT 8 m"));
        assert_eq!(p.skill_label.as_deref(), Some("Follow"));
        assert_eq!(p.control_mode.as_deref(), Some("ados"));
        assert_eq!(p.profile.as_deref(), Some("normal"));
        assert_eq!(p.avoid.as_deref(), Some("brake"));
        assert_eq!(p.avoid_state.as_deref(), Some("detour"));
        assert_eq!(p.hold_reason, None);
        assert_eq!(p.min_clearance_m, Some(2.4));
        assert_eq!(p.fc_mode_name.as_deref(), Some("GUIDED"));
        assert_eq!(p.armed, Some(true));
        assert_eq!(
            p.mission,
            Some(PilotMission {
                state: Some("none".into()),
                seq: Some(0),
                total: Some(0)
            })
        );
        assert!(!p.stale);
    }

    #[test]
    fn an_old_pilot_kind_is_stale_and_a_null_one_is_absent() {
        let old = Autonomy::from_value(
            &json!({"pilot": {"ts_ms": 1, "age_ms": 4000, "data": pilot_frame()}}),
        );
        assert!(old.pilot.unwrap().stale);
        assert_eq!(Autonomy::from_value(&json!({"pilot": null})).pilot, None);
    }

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

    #[test]
    fn tones_cover_the_phase_and_health_words() {
        assert_eq!(phase_tone("following"), Tone::Good);
        assert_eq!(phase_tone("searching"), Tone::Caution);
        assert_eq!(phase_tone("lost"), Tone::Fault);
        assert_eq!(phase_tone("idle"), Tone::Neutral);
        assert_eq!(health_tone("degraded"), Tone::Caution);
        assert_eq!(gps_fix_word(3), "3D");
    }
}
