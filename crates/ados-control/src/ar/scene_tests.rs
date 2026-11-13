use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::*;
use crate::ar::geo::Home;
use crate::ar::project::Pose;

const HOME: Home = Home {
    lat_e7: 470_000_000,
    lon_e7: 85_000_000,
    alt_msl_m: 400.0,
    ned: [0.0, 0.0, 0.0],
};

/// `n` metres north of home, `alt` above home.
fn wp(seq: u16, n: f64, alt: f64, frame: u8) -> MissionItem {
    MissionItem {
        seq,
        frame,
        lat_e7: HOME.lat_e7 + (n / 111_319.49 * 1e7).round() as i32,
        lon_e7: HOME.lon_e7,
        alt_m: alt,
    }
}

fn mission(state: &str, seq: u16, items: Vec<MissionItem>) -> MissionIn {
    MissionIn {
        state: state.into(),
        seq: Some(seq),
        items,
        wire: json!({"state": state, "seq": seq}),
    }
}

/// Hovering 10 m up at home, nose north.
fn scene(m: MissionIn) -> SceneIn {
    SceneIn {
        ts_ms: 1,
        pose: Some(Pose {
            ned: [0.0, 0.0, -10.0],
            roll: 0.0,
            pitch: 0.0,
            yaw: 0.0,
        }),
        armed: true,
        owner: Some("mission_fc".into()),
        home: Some(HOME),
        mission: Some(m),
        ..SceneIn::default()
    }
}

fn main_cam() -> Vec<CameraIn> {
    vec![CameraIn {
        id: "main".into(),
        frame_w: 1280,
        frame_h: 720,
        camera: camera_model(1280, 720, Some(90.0), Some(-10.0), None),
    }]
}

fn demand(layers: &[&str]) -> BTreeMap<String, BTreeSet<String>> {
    BTreeMap::from([(
        "main".to_owned(),
        layers.iter().map(|s| s.to_string()).collect(),
    )])
}

fn items(frame: &Value) -> Vec<Value> {
    frame["cameras"]["main"]["items"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

fn route() -> Vec<MissionItem> {
    vec![
        wp(1, 40.0, 10.0, 3),
        wp(2, 80.0, 10.0, 3),
        wp(3, 120.0, 10.0, 3),
        wp(4, 160.0, 10.0, 3),
    ]
}

#[test]
fn next_and_after_roles_follow_the_current_seq() {
    let f = build(
        &scene(mission("active", 2, route())),
        &main_cam(),
        &demand(&["ar_route"]),
    );
    let wps: Vec<_> = items(&f).into_iter().filter(|i| i["t"] == "wp").collect();
    let role = |seq: u64| {
        wps.iter()
            .find(|i| i["seq"] == seq)
            .map(|i| i["role"].clone())
    };
    assert_eq!(role(2), Some(json!("next")));
    assert_eq!(role(3), Some(json!("after")));
    assert_eq!(role(4), Some(json!("other")));
    assert_eq!(wps.iter().find(|i| i["seq"] == 2).unwrap()["k"], "wp:2");
    assert_eq!(f["mission"]["state"], "active");
}

#[test]
fn active_mission_draws_the_active_leg_others_are_future() {
    let f = build(
        &scene(mission("active", 2, route())),
        &main_cam(),
        &demand(&["ar_route"]),
    );
    let legs: Vec<_> = items(&f).into_iter().filter(|i| i["t"] == "leg").collect();
    assert!(legs
        .iter()
        .any(|l| l["role"] == "active" && l["k"] == "leg:active"));
    assert!(legs.iter().any(|l| l["role"] == "next"));

    let f = build(
        &scene(mission("paused", 2, route())),
        &main_cam(),
        &demand(&["ar_route"]),
    );
    let legs: Vec<_> = items(&f).into_iter().filter(|i| i["t"] == "leg").collect();
    assert!(!legs.is_empty());
    assert!(legs.iter().all(|l| l["role"] == "future"), "{legs:?}");
}

#[test]
fn returning_and_complete_hide_the_route_loaded_previews_on_the_ground() {
    for state in ["returning", "complete", "none"] {
        let f = build(
            &scene(mission(state, 2, route())),
            &main_cam(),
            &demand(&["ar_route"]),
        );
        assert!(
            items(&f).iter().all(|i| i["t"] != "wp" && i["t"] != "leg"),
            "{state}"
        );
    }
    let mut s = scene(mission("loaded", 1, route()));
    s.armed = false;
    let f = build(&s, &main_cam(), &demand(&["ar_route"]));
    assert!(items(&f).iter().any(|i| i["t"] == "wp"));
    s.armed = true;
    s.owner = Some("skill".into());
    let f = build(&s, &main_cam(), &demand(&["ar_route"]));
    assert!(items(&f).iter().all(|i| i["t"] != "wp"));
}

#[test]
fn a_next_waypoint_behind_the_camera_is_an_edge_and_home_too() {
    // Next waypoint 40 m south, behind a north-facing camera; home is below.
    let f = build(
        &scene(mission(
            "active",
            1,
            vec![wp(1, -40.0, 10.0, 3), wp(2, 80.0, 10.0, 3)],
        )),
        &main_cam(),
        &demand(&["ar_route"]),
    );
    let it = items(&f);
    let edge = it
        .iter()
        .find(|i| i["t"] == "edge" && i["of"] == "wp")
        .expect("edge for next");
    assert_eq!(edge["seq"], 1);
    assert_eq!(edge["k"], "edge:wp:1");
    assert!(it.iter().all(|i| !(i["t"] == "wp" && i["seq"] == 1)));
    let home = it
        .iter()
        .find(|i| i["k"] == "edge:home")
        .expect("home is under the drone");
    assert_eq!(home["of"], "home");
}

#[test]
fn terrain_frame_marks_the_frame_approximate() {
    let f = build(
        &scene(mission("active", 1, route())),
        &main_cam(),
        &demand(&["ar_route"]),
    );
    assert_eq!(f["approx"], false);
    let f = build(
        &scene(mission(
            "active",
            1,
            vec![wp(1, 40.0, 10.0, 10), wp(2, 80.0, 10.0, 3)],
        )),
        &main_cam(),
        &demand(&["ar_route"]),
    );
    assert_eq!(f["approx"], true);
}

#[test]
fn only_demanded_cameras_and_layers_are_built() {
    let s = scene(mission("active", 1, route()));
    let f = build(&s, &main_cam(), &BTreeMap::new());
    assert_eq!(f["cameras"], json!({}));
    let f = build(&s, &main_cam(), &demand(&["ar_motion"]));
    assert!(items(&f).iter().all(|i| i["t"] != "wp" && i["t"] != "home"));
    let mut no_fov = main_cam();
    no_fov[0].camera = None;
    let f = build(&s, &no_fov, &demand(&["ar_route"]));
    assert_eq!(f["cameras"]["main"]["reason"], "no_fov");
}

#[test]
fn motion_items_fpv_fpath_fgoal_orbit() {
    let mut s = scene(mission("none", 0, Vec::new()));
    s.vel = [5.0, 0.0, 0.0];
    s.groundspeed = 5.0;
    s.motion = Some(MotionIn {
        plan_ne: vec![[10.0, 2.0]],
        goal: Some([20.0, 0.0, -10.0]),
        setpoint_d: Some(-10.0),
        avoid_state: "detour".into(),
        orbit: Some(Orbit {
            center_ne: [30.0, 0.0],
            radius_m: 8.0,
            cw: true,
        }),
    });
    let f = build(&s, &main_cam(), &demand(&["ar_motion"]));
    let it = items(&f);
    for t in ["fpv", "fpath", "fgoal", "orbit"] {
        assert!(it.iter().any(|i| i["t"] == t && i["k"] == t), "{t}: {it:?}");
    }
    assert_eq!(
        it.iter().find(|i| i["t"] == "fpath").unwrap()["state"],
        "detour"
    );
    assert_eq!(it.iter().find(|i| i["t"] == "orbit").unwrap()["dir"], "cw");
}

#[test]
fn no_pose_is_reported() {
    let mut s = scene(mission("active", 1, route()));
    s.pose = None;
    let f = build(&s, &main_cam(), &demand(&["ar_route"]));
    assert_eq!(f["reason"], "no_pose");
    assert_eq!(f["cameras"]["main"]["items"], json!([]));
}

#[test]
fn same_scene_tolerates_jitter_not_changes() {
    let s = scene(mission("active", 2, route()));
    let a = build(&s, &main_cam(), &demand(&["ar_route"]));
    let mut s2 = s.clone();
    s2.ts_ms = 99;
    s2.pose = Some(Pose {
        ned: [0.001, 0.0, -10.0],
        ..s.pose.unwrap()
    });
    let b = build(&s2, &main_cam(), &demand(&["ar_route"]));
    assert!(same_scene(&a, &b, 0.002));
    s2.pose = Some(Pose {
        yaw: 0.2,
        ..s.pose.unwrap()
    });
    let c = build(&s2, &main_cam(), &demand(&["ar_route"]));
    assert!(!same_scene(&a, &c, 0.002));
}

// -- golden fixture ---------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArFrame {
    v: u32,
    ts_ms: i64,
    approx: bool,
    reason: Option<String>,
    mission: Value,
    cameras: BTreeMap<String, ArCamera>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArCamera {
    frame_w: u32,
    frame_h: u32,
    reason: Option<String>,
    items: Vec<ArItem>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "lowercase", deny_unknown_fields)]
enum ArItem {
    Wp {
        k: String,
        seq: u16,
        role: String,
        x: f64,
        y: f64,
        gx: Option<f64>,
        gy: Option<f64>,
        dist_m: f64,
        eta_s: Option<f64>,
        size: f64,
    },
    Leg {
        k: String,
        role: String,
        pts: Vec<[f64; 2]>,
    },
    Home {
        k: String,
        x: f64,
        y: f64,
        gx: Option<f64>,
        gy: Option<f64>,
        dist_m: f64,
    },
    Fpv {
        k: String,
        x: f64,
        y: f64,
    },
    Fpath {
        k: String,
        state: String,
        pts: Vec<[f64; 2]>,
    },
    Fgoal {
        k: String,
        x: f64,
        y: f64,
    },
    Orbit {
        k: String,
        dir: String,
        cx: Option<f64>,
        cy: Option<f64>,
        pts: Vec<[f64; 2]>,
    },
    Edge {
        k: String,
        of: String,
        seq: Option<u16>,
        ang: f64,
        dist_m: f64,
    },
}

const GOLDEN: &str = include_str!("../../tests/fixtures/ar-frame.json");

#[test]
fn the_golden_frame_round_trips() {
    let raw: Value = serde_json::from_str(GOLDEN).unwrap();
    let typed: ArFrame = serde_json::from_value(raw.clone()).unwrap();
    assert_eq!(serde_json::to_value(&typed).unwrap(), raw);
    let kinds: BTreeSet<_> = typed.cameras["main"]
        .items
        .iter()
        .map(|i| {
            serde_json::to_value(i).unwrap()["t"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    for t in [
        "wp", "leg", "home", "fpv", "fpath", "fgoal", "orbit", "edge",
    ] {
        assert!(kinds.contains(t), "golden frame lacks {t}");
    }
}

#[test]
fn a_built_frame_parses_as_the_golden_schema() {
    let mut s = scene(mission("active", 2, route()));
    s.groundspeed = 3.0;
    s.vel = [3.0, 0.0, 0.0];
    s.motion = Some(MotionIn {
        plan_ne: vec![[10.0, 2.0]],
        goal: Some([20.0, 0.0, -10.0]),
        setpoint_d: None,
        avoid_state: "clear".into(),
        orbit: Some(Orbit {
            center_ne: [30.0, 0.0],
            radius_m: 8.0,
            cw: false,
        }),
    });
    let f = build(&s, &main_cam(), &demand(&["ar_route", "ar_motion"]));
    let typed: ArFrame = serde_json::from_value(f).unwrap();
    assert!(!typed.cameras["main"].items.is_empty());
}
