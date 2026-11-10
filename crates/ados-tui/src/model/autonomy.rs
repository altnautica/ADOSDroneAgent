//! The autonomy readings from `GET /api/v1/nav/status`: who flies the vehicle
//! (the `pilot` kind), the follow state, the navigation source, the lidar and
//! the live map session; and the detector's per-camera state from
//! `GET /api/vision/status`.
//!
//! The nav route answers each kind as `{ts_ms, age_ms, data}`, or `null` when
//! no plugin has published it, so every part here is optional and nothing is
//! filled with a default.

use serde_json::Value;

use super::flight_state::{stale_after_ms, AR_ETA_MAX_AGE_MS, PILOT_OWNERS};

/// The path the Vehicle tab polls.
pub const NAV_STATUS_PATH: &str = "/api/v1/nav/status";

/// The steps of the pilot's launch sequence.
const LAUNCH_STEPS: [&str; 5] = ["checks", "arm", "takeoff", "after", "done"];

/// The mission states the pilot kind reports.
const MISSION_STATES: [&str; 6] = [
    "none",
    "loaded",
    "active",
    "paused",
    "complete",
    "returning",
];

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

/// A string field holding one of `values`, else `None`.
fn one_of(v: &Value, key: &str, values: &[&str]) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|s| values.contains(s))
        .map(str::to_string)
}

/// One kind's payload, its age, and whether it is stale. A kind without a
/// whole-millisecond `age_ms` is of unknown age, so it is stale too.
fn kind<'a>(v: &'a Value, name: &str) -> Option<(&'a Value, Option<u64>, bool)> {
    let entry = v.get(name).filter(|e| !e.is_null())?;
    let data = entry.get("data")?;
    let age = entry.get("age_ms").and_then(Value::as_u64);
    let stale = age.is_none_or(|a| a > stale_after_ms());
    Some((data, age, stale))
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
    /// Height above ground the follow skill flies on (`rng` or `baro`).
    pub agl_m: Option<f64>,
    pub agl_source: Option<String>,
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
    pub seq: Option<f64>,
    pub total: Option<f64>,
    /// The mission is on its landing sequence.
    pub landing_seq: bool,
}

/// Who flies the vehicle and how (the `pilot` kind). Enumerated fields hold
/// only the values the shared parser accepts; anything else reads as absent.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Pilot {
    /// One of [`PILOT_OWNERS`]; `None` for an owner the chip does not know.
    pub owner: Option<String>,
    /// The owning skill's label, when a skill owns.
    pub skill_label: Option<String>,
    pub control_mode: Option<String>,
    pub profile: Option<String>,
    pub avoid: Option<String>,
    pub avoid_state: Option<String>,
    pub hold_reason: Option<String>,
    pub min_clearance_m: Option<f64>,
    pub fc_mode: Option<f64>,
    pub fc_mode_name: Option<String>,
    pub vehicle_class: Option<String>,
    pub vtol_transitioning: bool,
    /// The goal, NED metres about the local origin.
    pub goal_ned: Option<[f64; 3]>,
    pub readiness: Vec<ReadinessItem>,
    /// Seconds left before the link-loss action, while the link is lost.
    pub link_loss_in_s: Option<f64>,
    pub link_loss_action: Option<String>,
    pub mission: Option<PilotMission>,
    /// Height above ground: a fresh downward rangefinder, else the EKF
    /// height above home (`agl_source`).
    pub agl_m: Option<f64>,
    pub agl_source: Option<String>,
    /// FC STATUSTEXT at WARNING or worse from the last 30 s, newest first.
    pub statustext: Vec<FcStatusText>,
    /// The setup step a tuning flight is on.
    pub setup_step: Option<String>,
    /// The step of a running launch sequence: a known step other than
    /// `done`, with no error.
    pub launch_step: Option<String>,
    /// The FC flies an RTL or land of its own (a failsafe, not an operator's
    /// mode change); the agent omits `fc_failsafe` while it is false.
    pub fc_failsafe: bool,
    /// The kind's age when read; `None` when the route did not say.
    pub age_ms: Option<u64>,
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
    /// The next waypoint's ETA from a fresh `ar` kind.
    pub ar_eta_s: Option<f64>,
}

impl Autonomy {
    pub fn from_value(v: &Value) -> Self {
        let pilot = kind(v, "pilot").map(|(d, age, stale)| pilot(d, age, stale));
        let follow = kind(v, "follow").map(|(d, _, stale)| Follow {
            phase: s(d, "phase"),
            track_id: i(d, "track_id"),
            range_m: f(d, "range_m"),
            range_sigma_m: f(d, "range_sigma_m"),
            reacquire_mode: s(d, "reacquire_mode"),
            coast_remaining_s: f(d, "coast_remaining_s"),
            agl_m: f(d, "agl_m"),
            agl_source: one_of(d, "agl_source", &["rng", "baro"]),
            stale,
        });
        let nav = kind(v, "nav").map(|(d, _, stale)| NavSource {
            mode: s(d, "mode"),
            active_set: i(d, "active_set"),
            extnav_health: s(d, "extnav_health"),
            odom_rate_hz: f(d, "odom_rate_hz"),
            obstacle_rate_hz: f(d, "obstacle_rate_hz"),
            min_obstacle_m: f(d, "min_obstacle_m"),
            gps_fix: i(d, "gps_fix"),
            stale,
        });
        let lidar = kind(v, "scan").map(|(d, _, stale)| Lidar {
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
        let map = kind(v, "map").map(|(d, _, stale)| MapSession {
            placement: s(d, "placement"),
            sweep_points: i(d, "sweep_points"),
            area_m2: f(d, "area_m2"),
            voxel_m: f(d, "voxel_m"),
            memory_used_mb: f(d, "memory_used_mb"),
            memory_budget_mb: f(d, "memory_budget_mb"),
            keyframes_dropped: i(d, "keyframes_dropped"),
            stale,
        });
        let ar_eta_s = kind(v, "ar").and_then(|(d, age, _)| next_waypoint_eta_s(d, age));
        Self {
            pilot,
            follow,
            nav,
            lidar,
            map,
            ar_eta_s,
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

    /// Mark every kind stale: the last good reading stays on screen after a
    /// failed read, but no row of it reads as current.
    pub fn mark_stale(&mut self) {
        if let Some(p) = &mut self.pilot {
            p.stale = true;
        }
        if let Some(f) = &mut self.follow {
            f.stale = true;
        }
        if let Some(n) = &mut self.nav {
            n.stale = true;
        }
        if let Some(l) = &mut self.lidar {
            l.stale = true;
        }
        if let Some(m) = &mut self.map {
            m.stale = true;
        }
    }
}

fn pilot(d: &Value, age_ms: Option<u64>, stale: bool) -> Pilot {
    let readiness = d
        .get("readiness")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|r| {
            Some(ReadinessItem {
                id: r.get("id")?.as_str()?.to_string(),
                ok: r.get("ok").and_then(Value::as_bool) == Some(true),
                block: r.get("severity").and_then(Value::as_str) != Some("warn"),
                msg: r
                    .get("msg")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            })
        })
        .collect();
    let statustext = d
        .get("statustext")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .take(8)
        .filter_map(|s| {
            Some(FcStatusText {
                severity: f(s, "severity")?,
                text: s.get("text")?.as_str()?.to_string(),
            })
        })
        .collect();
    let goal_ned = d
        .get("goal_ned")
        .and_then(Value::as_array)
        .filter(|a| a.len() >= 3)
        .and_then(|a| {
            let n = |i: usize| a[i].as_f64().filter(|x| x.is_finite());
            Some([n(0)?, n(1)?, n(2)?])
        });
    Pilot {
        owner: one_of(d, "owner", &PILOT_OWNERS),
        skill_label: d.get("skill").and_then(|k| {
            // A skill names its plugin and skill id; the label defaults to the id.
            k.get("plugin_id")?.as_str()?;
            let id = k.get("skill_id")?.as_str()?;
            Some(
                k.get("label")
                    .and_then(Value::as_str)
                    .unwrap_or(id)
                    .to_string(),
            )
        }),
        control_mode: one_of(d, "control_mode", &["ados", "fc_manual", "elrs"]),
        profile: one_of(d, "profile", &["cine", "normal", "sport"]),
        avoid: one_of(d, "avoid", &["brake", "bypass", "off"]),
        avoid_state: one_of(d, "avoid_state", &["clear", "detour", "blocked", "off"]),
        hold_reason: s(d, "hold_reason"),
        min_clearance_m: f(d, "min_clearance_m"),
        fc_mode: f(d, "fc_mode"),
        fc_mode_name: d
            .get("fc_mode_name")
            .and_then(Value::as_str)
            .map(str::to_string),
        vehicle_class: one_of(d, "vehicle_class", &["multirotor", "fixedwing", "vtol"]),
        vtol_transitioning: d.get("vtol_transitioning").and_then(Value::as_bool) == Some(true),
        goal_ned,
        readiness,
        link_loss_in_s: f(d, "link_loss_in_s"),
        link_loss_action: one_of(
            d,
            "link_loss_action",
            &["rth", "rtl", "hold", "loiter", "land"],
        ),
        mission: d
            .get("mission")
            .filter(|m| m.is_object())
            .map(|m| PilotMission {
                state: one_of(m, "state", &MISSION_STATES),
                seq: f(m, "seq").or_else(|| f(m, "current_seq")),
                total: f(m, "total"),
                landing_seq: m.get("landing_seq").and_then(Value::as_bool) == Some(true),
            }),
        agl_m: f(d, "agl_m"),
        agl_source: one_of(d, "agl_source", &["rangefinder", "home"]),
        statustext,
        setup_step: d
            .get("setup_step")
            .and_then(Value::as_str)
            .map(str::to_string),
        launch_step: d.get("sequence").filter(|q| q.is_object()).and_then(|q| {
            let step = q.get("step")?.as_str()?;
            let running = LAUNCH_STEPS.contains(&step)
                && step != "done"
                && q.get("error").and_then(Value::as_str).is_none();
            running.then(|| step.to_string())
        }),
        fc_failsafe: d.get("fc_failsafe").and_then(Value::as_bool) == Some(true),
        age_ms,
        stale,
    }
}

/// The ETA of the next waypoint in an `ar` frame `age_ms` old, from whichever
/// camera carries it; `None` once the frame is older than
/// [`AR_ETA_MAX_AGE_MS`] or names no next waypoint.
fn next_waypoint_eta_s(data: &Value, age_ms: Option<u64>) -> Option<f64> {
    if age_ms.is_none_or(|a| a > AR_ETA_MAX_AGE_MS) {
        return None;
    }
    data.get("cameras")?
        .as_object()?
        .values()
        .filter_map(|c| c.get("items").and_then(Value::as_array))
        .flatten()
        .find(|it| {
            it.get("t").and_then(Value::as_str) == Some("wp")
                && it.get("role").and_then(Value::as_str) == Some("next")
                && f(it, "eta_s").is_some()
        })
        .and_then(|it| f(it, "eta_s"))
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

    /// `idle`, or `detecting: main, belly`. The engine's status reports, per
    /// camera, only whether detection runs there (and the leases holding it),
    /// not how many objects it found, so no count is shown.
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

/// One FC STATUSTEXT line from the pilot kind; `severity` is MAVLink's
/// (0 emergency … 4 warning).
#[derive(Debug, Clone, PartialEq)]
pub struct FcStatusText {
    pub severity: f64,
    pub text: String,
}

/// One pre-flight readiness item from the pilot kind.
#[derive(Debug, Clone, PartialEq)]
pub struct ReadinessItem {
    pub id: String,
    pub ok: bool,
    /// A failing blocking item refuses take-off; otherwise it only warns.
    pub block: bool,
    pub msg: String,
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
        assert_eq!(p.skill_label.as_deref(), Some("Follow"));
        assert_eq!(p.control_mode.as_deref(), Some("ados"));
        assert_eq!(p.profile.as_deref(), Some("normal"));
        assert_eq!(p.avoid.as_deref(), Some("brake"));
        assert_eq!(p.avoid_state.as_deref(), Some("detour"));
        assert_eq!(p.hold_reason, None);
        assert_eq!(p.min_clearance_m, Some(2.4));
        assert_eq!(p.fc_mode, Some(4.0));
        assert_eq!(p.fc_mode_name.as_deref(), Some("GUIDED"));
        assert_eq!(p.vehicle_class.as_deref(), Some("multirotor"));
        assert_eq!(p.goal_ned, Some([12.0, -3.5, -4.0]));
        assert_eq!(p.readiness.len(), 3);
        assert!(!p.readiness[2].ok && !p.readiness[2].block);
        assert_eq!(p.link_loss_in_s, None);
        assert_eq!(p.link_loss_action.as_deref(), Some("rth"));
        assert_eq!(p.agl_m, Some(4.2));
        assert_eq!(p.agl_source.as_deref(), Some("rangefinder"));
        assert_eq!(
            p.statustext,
            vec![FcStatusText {
                severity: 4.0,
                text: "PreArm: Compass not calibrated".into()
            }]
        );
        assert_eq!(p.setup_step, None);
        assert_eq!(
            p.mission,
            Some(PilotMission {
                state: Some("none".into()),
                seq: Some(0.0),
                total: Some(0.0),
                landing_seq: false,
            })
        );
        assert_eq!(p.age_ms, Some(100));
        assert!(!p.stale);
    }

    #[test]
    fn an_unknown_owner_or_enum_value_reads_as_absent() {
        let mut frame = pilot_frame();
        frame["owner"] = json!("warp");
        frame["profile"] = json!("ludicrous");
        frame["link_loss_action"] = json!("teleport");
        let p = Autonomy::from_value(&json!({"pilot": {"ts_ms": 1, "age_ms": 1, "data": frame}}))
            .pilot
            .unwrap();
        assert_eq!(p.owner, None);
        assert_eq!(p.profile, None);
        assert_eq!(p.link_loss_action, None);
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
    fn a_kind_past_the_spec_threshold_is_stale() {
        let at = |age: u64| {
            Autonomy::from_value(
                &json!({"pilot": {"ts_ms": 1, "age_ms": age, "data": pilot_frame()}}),
            )
            .pilot
            .unwrap()
            .stale
        };
        assert!(!at(stale_after_ms()));
        assert!(at(stale_after_ms() + 1));
        assert!(at(2500), "2.5 s is past the spec's 2 s");
    }

    #[test]
    fn a_kind_without_a_whole_millisecond_age_is_stale() {
        for entry in [
            json!({"ts_ms": 1, "data": {"phase": "following"}}),
            json!({"ts_ms": 1, "age_ms": null, "data": {"phase": "following"}}),
            json!({"ts_ms": 1, "age_ms": "40", "data": {"phase": "following"}}),
            json!({"ts_ms": 1, "age_ms": -5, "data": {"phase": "following"}}),
            json!({"ts_ms": 1, "age_ms": 40.5, "data": {"phase": "following"}}),
        ] {
            let a = Autonomy::from_value(&json!({ "follow": entry }));
            assert!(a.follow.unwrap().stale, "{entry}");
        }
    }

    #[test]
    fn mark_stale_marks_every_kind() {
        let mut a = Autonomy::from_value(&json!({
            "pilot": {"ts_ms": 1, "age_ms": 10, "data": pilot_frame()},
            "follow": {"ts_ms": 1, "age_ms": 10, "data": {}},
            "nav": {"ts_ms": 1, "age_ms": 10, "data": {}},
            "scan": {"ts_ms": 1, "age_ms": 10, "data": {}},
            "map": {"ts_ms": 1, "age_ms": 10, "data": {}},
        }));
        a.mark_stale();
        assert!(a.pilot.unwrap().stale);
        assert!(a.follow.unwrap().stale);
        assert!(a.nav.unwrap().stale);
        assert!(a.lidar.unwrap().stale);
        assert!(a.map.unwrap().stale);
    }

    #[test]
    fn the_ar_kind_carries_the_next_waypoints_eta_while_fresh() {
        let ar: Value = serde_json::from_str(include_str!(
            "../../../ados-control/tests/fixtures/ar-frame.json"
        ))
        .unwrap();
        let eta = |age: u64| {
            Autonomy::from_value(&json!({"ar": {"ts_ms": 1, "age_ms": age, "data": ar}})).ar_eta_s
        };
        assert_eq!(eta(100), Some(42.0));
        assert_eq!(eta(AR_ETA_MAX_AGE_MS + 1), None);
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
    }
}
