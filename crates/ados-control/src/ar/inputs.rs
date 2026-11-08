//! Scene inputs from the state snapshot, the camera roster and the view
//! demand.

use super::*;

pub(super) fn f(v: &Value, k: &str) -> Option<f64> {
    v.get(k).and_then(Value::as_f64)
}

pub(super) fn arr(v: &Value) -> Option<Vec<f64>> {
    v.as_array()?.iter().map(Value::as_f64).collect()
}

/// The gimbal `(pitch_deg, yaw_deg, earth_frame)` while fresh.
pub(super) fn gimbal_of(state: &Value) -> Option<(f64, f64, bool)> {
    let g = state.get("gimbal")?;
    if g.get("age_ms").and_then(Value::as_u64)? > FRESH_MS {
        return None;
    }
    Some((
        f(g, "pitch_deg")?,
        f(g, "yaw_deg")?,
        g.get("earth_frame")?.as_bool()?,
    ))
}

/// The scene inputs from a state snapshot and the `pilot`/`follow` kinds.
pub fn scene_from(
    state: &Value,
    ts_ms: i64,
    pilot: Option<&Value>,
    follow: Option<&Value>,
) -> SceneIn {
    let att = state.get("attitude");
    let lp = state.get("local_position").filter(|l| {
        l.get("age_ms")
            .and_then(Value::as_u64)
            .is_some_and(|a| a <= FRESH_MS)
    });
    let pose = match (att, lp) {
        (Some(a), Some(l)) => Some(Pose {
            ned: [
                f(l, "x").unwrap_or(0.0),
                f(l, "y").unwrap_or(0.0),
                f(l, "z").unwrap_or(0.0),
            ],
            roll: f(a, "roll").unwrap_or(0.0),
            pitch: f(a, "pitch").unwrap_or(0.0),
            yaw: f(a, "yaw").unwrap_or(0.0),
        }),
        _ => None,
    };
    let vel = lp.map_or([0.0; 3], |l| {
        [
            f(l, "vx").unwrap_or(0.0),
            f(l, "vy").unwrap_or(0.0),
            f(l, "vz").unwrap_or(0.0),
        ]
    });
    let home = state.get("home").and_then(|h| {
        let ned = arr(h.get("local_ned")?)?;
        Some(Home {
            lat_e7: h.get("lat_e7")?.as_i64()? as i32,
            lon_e7: h.get("lon_e7")?.as_i64()? as i32,
            alt_msl_m: f(h, "alt_msl_m")?,
            ned: [*ned.first()?, *ned.get(1)?, *ned.get(2)?],
        })
    });
    let mission = state
        .get("mission")
        .filter(|m| m.is_object())
        .map(|m| MissionIn {
            state: m
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or("none")
                .to_owned(),
            seq: m.get("seq").and_then(Value::as_u64).map(|s| s as u16),
            items: m
                .get("items")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|i| {
                            Some(MissionItem {
                                seq: i.get("seq")?.as_u64()? as u16,
                                frame: i.get("frame")?.as_u64()? as u8,
                                lat_e7: i.get("lat_e7")?.as_i64()? as i32,
                                lon_e7: i.get("lon_e7")?.as_i64()? as i32,
                                alt_m: f(i, "alt_m")?,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default(),
            wire: {
                // The frame carries the mission-state object, not the mirror's item list.
                let mut w = m.clone();
                if let Value::Object(o) = &mut w {
                    o.remove("items");
                    o.remove("rev");
                    o.remove("uploads_seen");
                }
                w
            },
        });
    let owner = pilot
        .and_then(|p| p.get("owner"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    SceneIn {
        ts_ms,
        pose,
        vel,
        groundspeed: state
            .get("velocity")
            .and_then(|v| f(v, "groundspeed"))
            .unwrap_or(0.0),
        armed: state.get("armed").and_then(Value::as_bool).unwrap_or(false),
        follow: follow_from(owner.as_deref(), pilot, follow),
        owner,
        home,
        mission,
    }
}

/// The follow geometry while a follow skill flies: the path and goal from the
/// pilot (or, without a pilot, from follow-me's own state), the orbit from
/// follow-me's mode, radius and target.
pub(super) fn follow_from(
    owner: Option<&str>,
    pilot: Option<&Value>,
    follow: Option<&Value>,
) -> Option<FollowIn> {
    let follow_active = follow
        .and_then(|f| f.get("active"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let source = match (owner, pilot) {
        (Some("skill"), Some(p)) => p,
        (None, _) if follow_active => follow?,
        _ => return None,
    };
    let plan_ne = source
        .get("plan_ned")
        .and_then(Value::as_array)
        .map(|pts| {
            pts.iter()
                .filter_map(arr)
                .filter(|p| p.len() >= 2)
                .map(|p| [p[0], p[1]])
                .collect()
        })
        .unwrap_or_default();
    let goal = source
        .get("goal_ned")
        .and_then(arr)
        .filter(|g| g.len() >= 2)
        .map(|g| [g[0], g[1], g.get(2).copied().unwrap_or(0.0)]);
    let setpoint_d = source
        .get("setpoint_ned")
        .and_then(arr)
        .and_then(|s| s.get(2).copied());
    let orbit = follow.and_then(|fs| {
        if fs.get("follow_mode").and_then(Value::as_str) != Some("orbit") {
            return None;
        }
        let t = arr(fs.get("target_ned")?)?;
        Some(Orbit {
            center_ne: [*t.first()?, *t.get(1)?],
            radius_m: f(fs, "orbit_radius_m")?,
            cw: fs.get("orbit_direction").and_then(Value::as_str) != Some("ccw"),
        })
    });
    Some(FollowIn {
        plan_ne,
        goal,
        setpoint_d,
        avoid_state: source
            .get("avoid_state")
            .and_then(Value::as_str)
            .unwrap_or("clear")
            .to_owned(),
        orbit,
    })
}

/// Cameras for the scene plus the models published for the pilot.
pub fn cameras_from(
    roster: &[RosterCam],
    gimbal: Option<(f64, f64, bool)>,
    vehicle_yaw_deg: f64,
) -> (Vec<CameraIn>, BTreeMap<String, CameraModel>) {
    let mut cams = Vec::new();
    let mut models = BTreeMap::new();
    for r in roster {
        let g = if r.primary { gimbal } else { None };
        let camera = scene::camera_model(r.width, r.height, r.fov_deg, r.mount_pitch_deg, g);
        if let Some(c) = camera {
            let (tilt, yaw) = match c.aim {
                Aim::Body { tilt_down, yaw } => (tilt_down, yaw.to_degrees()),
                Aim::Gimbal {
                    tilt_down,
                    yaw,
                    earth,
                } => {
                    let y = yaw.to_degrees();
                    (tilt_down, if earth { y - vehicle_yaw_deg } else { y })
                }
            };
            let hfov = c.hfov;
            let vfov = 2.0 * ((hfov / 2.0).tan() * c.h / c.w).atan();
            models.insert(
                r.served_id.clone(),
                CameraModel {
                    frame_w: r.width,
                    frame_h: r.height,
                    hfov_deg: hfov.to_degrees(),
                    vfov_deg: vfov.to_degrees(),
                    tilt_down_deg: tilt.to_degrees(),
                    yaw_deg: yaw,
                },
            );
        }
        cams.push(CameraIn {
            id: r.served_id.clone(),
            frame_w: r.width,
            frame_h: r.height,
            camera,
        });
    }
    (cams, models)
}

/// The demanded AR layers per camera; cameras without an AR layer dropped.
pub(super) fn ar_demand(demand: &ViewDemand) -> BTreeMap<String, BTreeSet<String>> {
    demand
        .cameras
        .iter()
        .filter(|(_, l)| l.contains("ar_route") || l.contains("ar_motion"))
        .map(|(c, l)| (c.clone(), l.clone()))
        .collect()
}
