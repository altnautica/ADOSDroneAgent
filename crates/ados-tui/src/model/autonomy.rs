//! The autonomy readings from `GET /api/v1/nav/status`: who flies the vehicle
//! (the `pilot` kind), the follow state, the navigation source, the lidar and
//! the live map session; and, in [`detection`], the detector's per-camera
//! state from `GET /api/vision/status`.
//!
//! The nav route answers each kind as `{ts_ms, age_ms, data}`, or `null` when
//! no plugin has published it, so every part here is optional and nothing is
//! filled with a default.

use serde_json::Value;

use super::flight_state::{stale_after_ms, AR_ETA_MAX_AGE_MS, PILOT_OWNERS};

pub mod detection;

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

/// One kind's payload, its age, and whether it is stale: at least the spec's
/// `stale_after_ms` old. A kind without a whole-millisecond `age_ms` is of
/// unknown age, so it is stale too.
fn kind<'a>(v: &'a Value, name: &str) -> Option<(&'a Value, Option<u64>, bool)> {
    let entry = v.get(name).filter(|e| !e.is_null())?;
    let data = entry.get("data")?;
    let age = entry.get("age_ms").and_then(Value::as_u64);
    let stale = age.is_none_or(|a| a >= stale_after_ms());
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
    /// The kind's age at the agent when read; `None` when it reports none.
    pub age_ms: Option<u64>,
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
    /// How old vision-nav's pose was when it reported (ms).
    pub pose_age_ms: Option<f64>,
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
    /// The owning skill's plugin and skill ids, for its echoed parameters.
    pub skill_ref: Option<(String, String)>,
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
    /// What tripped that failsafe (`fc`, `rc_or_failsafe`), when the pilot said.
    pub failsafe_cause: Option<String>,
    /// The pilot operations a surface may offer now (`goto`, ...).
    pub surface_ops: Vec<String>,
    /// The arm and landed state; `None` while unknown (the FC is unheard),
    /// which may be airborne, never disarmed.
    pub armed: Option<bool>,
    pub landed: Option<bool>,
    /// Who holds the sticks: the input session's label, else its source.
    pub input_label: Option<String>,
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
        let follow = kind(v, "follow").map(|(d, age_ms, stale)| Follow {
            phase: s(d, "phase"),
            track_id: i(d, "track_id"),
            range_m: f(d, "range_m"),
            range_sigma_m: f(d, "range_sigma_m"),
            reacquire_mode: s(d, "reacquire_mode"),
            coast_remaining_s: f(d, "coast_remaining_s"),
            agl_m: f(d, "agl_m"),
            agl_source: one_of(d, "agl_source", &["rng", "baro"]),
            age_ms,
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
            pose_age_ms: f(d, "pose_age_ms"),
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
        skill_ref: d.get("skill").and_then(|k| {
            Some((
                k.get("plugin_id")?.as_str()?.to_string(),
                k.get("skill_id")?.as_str()?.to_string(),
            ))
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
        failsafe_cause: one_of(d, "failsafe_cause", &["fc", "rc_or_failsafe"]),
        surface_ops: d
            .get("surface_ops")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        armed: d.get("armed").and_then(Value::as_bool),
        landed: d.get("landed").and_then(Value::as_bool),
        input_label: d
            .get("input_session")
            .filter(|s| s.get("src").and_then(Value::as_str).is_some())
            .and_then(|s| {
                s.get("label")
                    .and_then(Value::as_str)
                    .or(s.get("src")?.as_str())
            })
            .or_else(|| d.get("input_src").and_then(Value::as_str))
            .filter(|l| !l.is_empty())
            .map(str::to_string),
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

/// How a state word reads: good, a caution, a fault, or neutral. The panel
/// pairs each with a symbol and a colour, and always prints the word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Good,
    Caution,
    Fault,
    Neutral,
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
mod tests;
