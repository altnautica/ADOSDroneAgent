//! The AR scene: what to draw over each camera, projected into its image.
//!
//! Items (normalised 0..1, y down), each with a stable key `k` so a renderer
//! interpolates the same item between frames:
//!
//! * layer `ar_route`: `wp` (mission waypoints: `next`, `after`, `other`),
//!   `leg` (`active` vehicle→next while the mission is active, `next`
//!   next→after, `future` up to five more), `home`, and `edge` arrows for a
//!   next waypoint or home that is off-frame or behind the camera;
//! * layer `ar_motion`: `fpv` (where the vehicle will be in 3 s), and while a
//!   follow skill runs `fpath` (vehicle → plan → goal), `fgoal` and `orbit`.
//!
//! The route is drawn while the mission is `active` or `paused`, and as a
//! preview while it is `loaded` and the vehicle is disarmed or flown by the
//! pilot or the FC directly; never while `returning`, `complete` or `none`.
//! Only cameras and layers named in the view demand are computed.

use std::collections::BTreeSet;

use serde_json::{json, Value};

use super::geo::{waypoint_ned, Home};
use super::project::{on_frame, Aim, Camera, Pose, View};

/// How far ahead the flight-path vector looks, seconds.
const FPV_HORIZON_S: f64 = 3.0;
/// Groundspeed above which the vehicle counts as moving.
const MOVING_MPS: f64 = 0.5;
/// Future legs drawn after the next one.
const FUTURE_LEGS: usize = 5;
/// Points on the orbit ring.
const ORBIT_POINTS: usize = 36;
/// A marker this much above the ground gets a ground point.
const AIRBORNE_M: f64 = 1.0;

/// One mission item as the state snapshot carries it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MissionItem {
    pub seq: u16,
    pub frame: u8,
    pub lat_e7: i32,
    pub lon_e7: i32,
    pub alt_m: f64,
}

/// The mission facts the scene needs.
#[derive(Debug, Clone, PartialEq)]
pub struct MissionIn {
    /// `none|loaded|active|paused|complete|returning`.
    pub state: String,
    pub seq: Option<u16>,
    pub items: Vec<MissionItem>,
    /// The whole mission-state object, copied into the frame.
    pub wire: Value,
}

/// The follow geometry, from the pilot (`plan_ned`, `goal_ned`,
/// `setpoint_ned`, `avoid_state`) and follow-me (orbit around `target_ned`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct FollowIn {
    pub plan_ne: Vec<[f64; 2]>,
    pub goal: Option<[f64; 3]>,
    pub setpoint_d: Option<f64>,
    pub avoid_state: String,
    pub orbit: Option<Orbit>,
}

/// An orbit to draw.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Orbit {
    pub center_ne: [f64; 2],
    pub radius_m: f64,
    pub cw: bool,
}

/// Everything one frame is built from.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SceneIn {
    pub ts_ms: i64,
    pub pose: Option<Pose>,
    /// Local NED velocity, m/s.
    pub vel: [f64; 3],
    pub groundspeed: f64,
    pub armed: bool,
    /// The pilot owner (`idle|pilot|skill|…`), when the pilot reports.
    pub owner: Option<String>,
    pub home: Option<Home>,
    pub mission: Option<MissionIn>,
    pub follow: Option<FollowIn>,
}

/// One camera to draw over.
#[derive(Debug, Clone, PartialEq)]
pub struct CameraIn {
    /// The served id (`main` for the primary leg).
    pub id: String,
    pub frame_w: u32,
    pub frame_h: u32,
    /// `None` when the roster has no field of view for it.
    pub camera: Option<Camera>,
}

/// The camera model for a roster row: `fov_deg` horizontal, tilt from the
/// gimbal while `gimbal` is given (primary only), otherwise from
/// `mount_pitch_deg` (negative looks down).
pub fn camera_model(
    frame_w: u32,
    frame_h: u32,
    fov_deg: Option<f64>,
    mount_pitch_deg: Option<f64>,
    gimbal: Option<(f64, f64, bool)>,
) -> Option<Camera> {
    let hfov = fov_deg.filter(|f| *f > 1.0 && *f < 179.0)?.to_radians();
    let aim = match gimbal {
        Some((pitch_deg, yaw_deg, earth)) => Aim::Gimbal {
            tilt_down: (-pitch_deg).to_radians(),
            yaw: yaw_deg.to_radians(),
            earth,
        },
        None => Aim::Body {
            tilt_down: (-mount_pitch_deg.unwrap_or(0.0)).to_radians(),
            yaw: 0.0,
        },
    };
    Some(Camera {
        w: f64::from(frame_w.max(1)),
        h: f64::from(frame_h.max(1)),
        hfov,
        aim,
    })
}

fn r3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

fn r1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

fn pt(p: [f64; 2]) -> Value {
    json!([r3(p[0]), r3(p[1])])
}

fn dist(a: [f64; 3], b: [f64; 3]) -> f64 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

/// Whether the route is drawn for this mission state and vehicle.
fn route_visible(scene: &SceneIn, state: &str) -> bool {
    match state {
        "active" | "paused" => true,
        "loaded" => {
            !scene.armed
                || scene
                    .owner
                    .as_deref()
                    .is_none_or(|o| o == "pilot" || o == "fc_mode")
        }
        _ => false,
    }
}

/// A marker (`wp` or `home`) or, when it is off-frame or behind the camera
/// and `edge_of` is set, an edge arrow.
struct Marker<'a> {
    view: &'a View,
    from: [f64; 3],
    groundspeed: f64,
    ground_d: f64,
}

impl Marker<'_> {
    fn place(&self, p: [f64; 3]) -> (Option<[f64; 2]>, Option<[f64; 2]>, f64) {
        let image = self.view.project(p).filter(|q| on_frame(*q));
        let ground = (self.ground_d - p[2] > AIRBORNE_M)
            .then(|| self.view.project([p[0], p[1], self.ground_d]))
            .flatten();
        (image, ground, dist(self.from, p))
    }

    fn eta(&self, d: f64) -> Value {
        if self.groundspeed > MOVING_MPS {
            json!(r1(d / self.groundspeed))
        } else {
            Value::Null
        }
    }

    fn edge(&self, p: [f64; 3], of: &str, seq: Option<u16>, d: f64) -> Value {
        let ang = View::screen_angle(self.view.to_cam(p));
        let k = match seq {
            Some(s) => format!("edge:wp:{s}"),
            None => "edge:home".to_owned(),
        };
        json!({"t": "edge", "k": k, "of": of, "seq": seq, "ang": r3(ang), "dist_m": r1(d)})
    }
}

/// The items for one camera.
fn camera_items(
    scene: &SceneIn,
    pose: &Pose,
    cam: &Camera,
    layers: &BTreeSet<String>,
    approx: &mut bool,
) -> Vec<Value> {
    let view = View::new(cam, pose);
    let mut items = Vec::new();
    let ground_d = scene.home.map_or(pose.ned[2] + 10.0, |h| h.ned[2]);
    let marker = Marker {
        view: &view,
        from: pose.ned,
        groundspeed: scene.groundspeed,
        ground_d,
    };

    if layers.contains("ar_route") {
        if let (Some(home), Some(mission)) = (scene.home, scene.mission.as_ref()) {
            if route_visible(scene, &mission.state) && !mission.items.is_empty() {
                route_items(pose, &home, mission, &marker, approx, &mut items);
            }
        }
        if let Some(home) = scene.home {
            let (image, ground, d) = marker.place(home.ned);
            match image {
                Some(q) => items.push(json!({
                    "t": "home", "k": "home", "x": r3(q[0]), "y": r3(q[1]),
                    "gx": ground.map(|g| r3(g[0])), "gy": ground.map(|g| r3(g[1])),
                    "dist_m": r1(d),
                })),
                None => items.push(marker.edge(home.ned, "home", None, d)),
            }
        }
    }

    if layers.contains("ar_motion") {
        if scene.groundspeed > MOVING_MPS {
            let ahead = [
                pose.ned[0] + scene.vel[0] * FPV_HORIZON_S,
                pose.ned[1] + scene.vel[1] * FPV_HORIZON_S,
                pose.ned[2] + scene.vel[2] * FPV_HORIZON_S,
            ];
            if let Some(q) = view.project(ahead) {
                items.push(json!({"t": "fpv", "k": "fpv", "x": r3(q[0]), "y": r3(q[1])}));
            }
        }
        if let Some(follow) = scene.follow.as_ref() {
            follow_items(pose, follow, &view, &mut items);
        }
    }
    items
}

fn route_items(
    pose: &Pose,
    home: &Home,
    mission: &MissionIn,
    marker: &Marker<'_>,
    approx: &mut bool,
    items: &mut Vec<Value>,
) {
    let view = marker.view;
    let pts: Vec<(u16, [f64; 3])> = mission
        .items
        .iter()
        .map(|i| {
            let (p, terrain) = waypoint_ned(home, i.lat_e7, i.lon_e7, i.alt_m, i.frame);
            *approx |= terrain;
            (i.seq, p)
        })
        .collect();
    let cur = mission.seq.unwrap_or(0);
    let next_idx = pts.iter().position(|(s, _)| *s >= cur).unwrap_or(0);
    let active = mission.state == "active";
    for (idx, (seq, p)) in pts.iter().enumerate() {
        let role = match idx {
            i if i == next_idx => "next",
            i if i == next_idx + 1 => "after",
            _ => "other",
        };
        let (image, ground, d) = marker.place(*p);
        match image {
            Some(q) => items.push(json!({
                "t": "wp", "k": format!("wp:{seq}"), "seq": seq, "role": role,
                "x": r3(q[0]), "y": r3(q[1]),
                "gx": ground.map(|g| r3(g[0])), "gy": ground.map(|g| r3(g[1])),
                "dist_m": r1(d),
                "eta_s": if role == "next" { marker.eta(d) } else { Value::Null },
                "size": r3((20.0 / d.max(1e-3)).clamp(0.25, 1.0)),
            })),
            None if role == "next" => items.push(marker.edge(*p, "wp", Some(*seq), d)),
            None => {}
        }
    }
    let mut leg = |k: String, role: &str, a: [f64; 3], b: [f64; 3]| {
        if let Some(s) = view.segment(a, b) {
            items.push(json!({"t": "leg", "k": k, "role": role, "pts": [pt(s[0]), pt(s[1])]}));
        }
    };
    let next = pts[next_idx].1;
    if active {
        leg("leg:active".into(), "active", pose.ned, next);
    }
    if let Some((_, after)) = pts.get(next_idx + 1) {
        let role = if active { "next" } else { "future" };
        leg("leg:next".into(), role, next, *after);
    }
    for w in pts.windows(2).skip(next_idx + 1).take(FUTURE_LEGS) {
        leg(format!("leg:future:{}", w[0].0), "future", w[0].1, w[1].1);
    }
}

fn follow_items(pose: &Pose, follow: &FollowIn, view: &View, items: &mut Vec<Value>) {
    let d = follow.setpoint_d.unwrap_or(pose.ned[2]);
    let mut path: Vec<[f64; 3]> = vec![pose.ned];
    path.extend(follow.plan_ne.iter().map(|p| [p[0], p[1], d]));
    if let Some(g) = follow.goal {
        path.push([g[0], g[1], d]);
    }
    if path.len() >= 2 {
        let mut pts: Vec<Value> = Vec::new();
        for w in path.windows(2) {
            if let Some(s) = view.segment(w[0], w[1]) {
                if pts.is_empty() {
                    pts.push(pt(s[0]));
                }
                pts.push(pt(s[1]));
            }
        }
        if pts.len() >= 2 {
            let state = if follow.avoid_state.is_empty() {
                "clear"
            } else {
                follow.avoid_state.as_str()
            };
            items.push(json!({"t": "fpath", "k": "fpath", "state": state, "pts": pts}));
        }
    }
    if let Some(q) = follow.goal.and_then(|g| view.project([g[0], g[1], d])) {
        items.push(json!({"t": "fgoal", "k": "fgoal", "x": r3(q[0]), "y": r3(q[1])}));
    }
    if let Some(o) = follow.orbit {
        let ring: Vec<Value> = (0..=ORBIT_POINTS)
            .filter_map(|i| {
                let a = std::f64::consts::TAU * i as f64 / ORBIT_POINTS as f64;
                view.project([
                    o.center_ne[0] + o.radius_m * a.cos(),
                    o.center_ne[1] + o.radius_m * a.sin(),
                    d,
                ])
            })
            .map(pt)
            .collect();
        if ring.len() >= 2 {
            let c = view.project([o.center_ne[0], o.center_ne[1], d]);
            items.push(json!({
                "t": "orbit", "k": "orbit", "dir": if o.cw { "cw" } else { "ccw" },
                "cx": c.map(|c| r3(c[0])), "cy": c.map(|c| r3(c[1])), "pts": ring,
            }));
        }
    }
}

/// Build the `ar` frame for the demanded cameras and layers. `demand` maps a
/// served camera id to its requested layers; cameras absent from it are not
/// computed.
pub fn build(
    scene: &SceneIn,
    cameras: &[CameraIn],
    demand: &std::collections::BTreeMap<String, BTreeSet<String>>,
) -> Value {
    let mut approx = false;
    let mut reason: Option<&str> = None;
    if scene.pose.is_none() {
        reason = Some("no_pose");
    } else if scene.home.is_none() && scene.mission.as_ref().is_some_and(|m| !m.items.is_empty()) {
        reason = Some("no_home");
    }
    let mut out = serde_json::Map::new();
    for cam in cameras {
        let Some(layers) = demand.get(&cam.id) else {
            continue;
        };
        if !layers.contains("ar_route") && !layers.contains("ar_motion") {
            continue;
        }
        let (cam_reason, items) = match (cam.camera.as_ref(), scene.pose.as_ref()) {
            (None, _) => (Some("no_fov"), Vec::new()),
            (_, None) => (Some("no_pose"), Vec::new()),
            (Some(c), Some(pose)) => (None, camera_items(scene, pose, c, layers, &mut approx)),
        };
        out.insert(
            cam.id.clone(),
            json!({"frame_w": cam.frame_w, "frame_h": cam.frame_h, "reason": cam_reason, "items": items}),
        );
    }
    json!({
        "v": 1,
        "ts_ms": scene.ts_ms,
        "approx": approx,
        "reason": reason,
        "mission": scene.mission.as_ref().map_or(Value::Null, |m| m.wire.clone()),
        "cameras": Value::Object(out),
    })
}

/// Whether two frames draw the same thing: same cameras, keys and strings,
/// every image coordinate within `tol` (normalised units), distances and
/// times within half a unit; the time stamp and ages are ignored.
pub fn same_scene(a: &Value, b: &Value, tol: f64) -> bool {
    fn tol_for(key: &str, tol: f64) -> Option<f64> {
        match key {
            "x" | "y" | "gx" | "gy" | "cx" | "cy" | "pts" => Some(tol),
            "dist_m" | "eta_s" => Some(0.5),
            "size" | "ang" => Some(0.01),
            "ts_ms" | "age_ms" => None,
            _ => Some(0.0),
        }
    }
    fn eq(a: &Value, b: &Value, tol: f64, key: &str) -> bool {
        let Some(t) = tol_for(key, tol) else {
            return true;
        };
        match (a, b) {
            (Value::Number(x), Value::Number(y)) => {
                (x.as_f64().unwrap_or(f64::NAN) - y.as_f64().unwrap_or(f64::NAN)).abs() <= t
            }
            // Array elements inherit the key (a `pts` polyline).
            (Value::Array(x), Value::Array(y)) => {
                x.len() == y.len() && x.iter().zip(y).all(|(p, q)| eq(p, q, tol, key))
            }
            (Value::Object(x), Value::Object(y)) => {
                x.len() == y.len()
                    && x.iter()
                        .all(|(k, v)| y.get(k).is_some_and(|w| eq(v, w, tol, k)))
            }
            _ => a == b,
        }
    }
    eq(a, b, tol, "")
}

#[cfg(test)]
#[path = "scene_tests.rs"]
mod tests;
