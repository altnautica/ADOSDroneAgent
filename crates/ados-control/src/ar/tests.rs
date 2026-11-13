use std::time::{Duration, Instant};

use serde_json::json;

use super::*;
use crate::routes::nav::ViewLease;

fn roster() -> Vec<RosterCam> {
    roster_cams(&[
        json!({"id": "eo", "role": "primary", "state": "assigned", "width": 1280, "height": 720,
               "fov_deg": 90.0, "mount_pitch_deg": -10.0}),
        json!({"id": "down", "role": "down", "state": "assigned", "width": 640, "height": 480,
               "fov_deg": 70.0, "mount_pitch_deg": -90.0}),
        json!({"id": "x", "state": "discovered_unassigned"}),
    ])
}

/// A hovering vehicle 10 m over home with a loaded 2-waypoint mission.
fn snapshot() -> Value {
    json!({
        "armed": false,
        "attitude": {"roll": 0.0, "pitch": 0.0, "yaw": 0.0, "time_boot_ms": 1},
        "local_position": {"x": 0.0, "y": 0.0, "z": -10.0, "vx": 0.0, "vy": 0.0, "vz": 0.0, "age_ms": 20},
        "velocity": {"groundspeed": 0.0},
        "home": {"lat_e7": 470_000_000, "lon_e7": 85_000_000, "alt_msl_m": 400.0, "local_ned": [0.0, 0.0, 0.0]},
        "gimbal": null,
        "mission": {"state": "loaded", "seq": 1, "total": 2, "item_reached": null, "id": null,
                    "source": "mission_state", "age_ms": 100, "landing_seq": false, "rev": 3,
                    "uploads_seen": 0,
                    "items": [
                        {"seq": 1, "cmd": 16, "frame": 3, "lat_e7": 470_003_593, "lon_e7": 85_000_000, "alt_m": 10.0},
                        {"seq": 2, "cmd": 16, "frame": 3, "lat_e7": 470_007_186, "lon_e7": 85_000_000, "alt_m": 10.0}]},
    })
}

fn demand(views: &[&str]) -> ViewDemand {
    let mut d = ViewDemand::default();
    for v in views {
        let l = ViewLease::from_query(v).unwrap();
        d.cameras
            .entry(l.camera_id.clone())
            .or_default()
            .extend(l.layers.iter().cloned());
    }
    d
}

#[test]
fn the_primary_is_served_as_main_and_unassigned_devices_are_skipped() {
    let r = roster();
    assert_eq!(r.len(), 2);
    assert_eq!(r[0].served_id, "main");
    assert!(r[0].primary);
    assert_eq!(r[1].served_id, "down");
}

#[test]
fn camera_models_follow_the_roster_and_the_gimbal() {
    let (cams, models) = cameras_from(&roster(), None, 0.0);
    assert_eq!(cams.len(), 2);
    let main = models["main"];
    assert!((main.tilt_down_deg - 10.0).abs() < 1e-9);
    assert!((main.hfov_deg - 90.0).abs() < 1e-9);
    assert!((main.vfov_deg - 2.0 * (720.0f64 / 1280.0).atan().to_degrees()).abs() < 1e-6);
    assert!((models["down"].tilt_down_deg - 90.0).abs() < 1e-9);
    // A fresh gimbal drives the primary: pitch -30 (down), earth yaw 90 with
    // the vehicle heading 60 is 30 from the nose.
    let (_, models) = cameras_from(&roster(), Some((-30.0, 90.0, true)), 60.0);
    assert!((models["main"].tilt_down_deg - 30.0).abs() < 1e-9);
    assert!((models["main"].yaw_deg - 30.0).abs() < 1e-9);
    assert!(
        (models["down"].tilt_down_deg - 90.0).abs() < 1e-9,
        "gimbal is the primary's only"
    );
    // The gimbal-driven primary holds against the horizon; the body-fixed
    // belly camera turns with the airframe.
    assert!(models["main"].stabilised);
    assert!(!models["down"].stabilised);
    let (_, models) = cameras_from(&roster(), None, 0.0);
    assert!(!models["main"].stabilised);
}

#[test]
fn scene_from_reads_the_v5_snapshot() {
    let s = scene_from(&snapshot(), 7, None, None);
    assert_eq!(s.pose.unwrap().ned, [0.0, 0.0, -10.0]);
    assert_eq!(s.home.unwrap().alt_msl_m, 400.0);
    let m = s.mission.unwrap();
    assert_eq!(m.items.len(), 2);
    assert_eq!(m.state, "loaded");
    assert!(m.wire.get("items").is_none() && m.wire.get("rev").is_none());
    assert_eq!(m.wire["source"], "mission_state");
    // A stale local position is no pose.
    let mut stale = snapshot();
    stale["local_position"]["age_ms"] = json!(5000);
    assert!(scene_from(&stale, 7, None, None).pose.is_none());
}

#[test]
fn follow_geometry_comes_from_the_pilot_while_a_skill_flies() {
    let pilot = json!({"owner": "skill", "plan_ned": [[5.0, 1.0]], "goal_ned": [10.0, 0.0, -10.0],
                       "setpoint_ned": [1.0, 0.0, -9.0], "avoid_state": "detour"});
    let follow = json!({"follow_mode": "orbit", "target_ned": [20.0, 0.0, 0.0],
                        "orbit_radius_m": 8.0, "orbit_direction": "ccw"});
    let s = scene_from(&snapshot(), 7, Some(&pilot), Some(&follow));
    let fi = s.motion.unwrap();
    assert_eq!(fi.plan_ne, vec![[5.0, 1.0]]);
    assert_eq!(fi.setpoint_d, Some(-9.0));
    assert_eq!(fi.avoid_state, "detour");
    let o = fi.orbit.unwrap();
    assert!(!o.cw && o.radius_m == 8.0);
    // The pilot flying sticks: no motion items.
    let idle = json!({"owner": "pilot"});
    assert!(scene_from(&snapshot(), 7, Some(&idle), Some(&follow))
        .motion
        .is_none());
}

#[test]
fn a_goto_draws_its_planned_path_and_goal() {
    let pilot = json!({"owner": "goto", "plan_ned": [[5.0, 1.0], [9.0, 2.0]], "goal_ned": [12.0, 3.0, -10.0],
                       "setpoint_ned": [1.0, 0.0, -10.0], "avoid_state": "clear"});
    let s = scene_from(&snapshot(), 7, Some(&pilot), None);
    let fi = s.motion.expect("goto geometry");
    assert_eq!(fi.plan_ne, vec![[5.0, 1.0], [9.0, 2.0]]);
    assert_eq!(fi.goal, Some([12.0, 3.0, -10.0]));
    assert!(fi.orbit.is_none());
}

#[test]
fn empty_demand_builds_nothing() {
    let mut b = Builder::new(10.0);
    let (cams, _) = cameras_from(&roster(), None, 0.0);
    let scene = scene_from(&snapshot(), 1, None, None);
    let t0 = Instant::now();
    for i in 0..50 {
        let out = b.step(
            t0 + Duration::from_millis(50 * i),
            &ViewDemand::default(),
            &scene,
            &cams,
            None,
        );
        assert!(out.is_none());
    }
    // A demand without an AR layer is no demand for this builder either.
    assert!(b
        .step(
            t0 + Duration::from_secs(5),
            &demand(&["main:obstacles"]),
            &scene,
            &cams,
            None
        )
        .is_none());
    assert_eq!(b.frames_built, 0);
}

#[test]
fn two_clients_on_one_camera_cost_one_computation() {
    let mut b = Builder::new(10.0);
    let (cams, _) = cameras_from(&roster(), None, 0.0);
    let t0 = Instant::now();
    let both = demand(&["main:ar_route", "main:ar_route,ar_motion"]);
    for i in 0..10u32 {
        let mut scene = scene_from(&snapshot(), i64::from(i), None, None);
        // Move a little each tick so every frame differs.
        scene.pose.as_mut().unwrap().yaw = f64::from(i) * 0.01;
        b.step(
            t0 + Duration::from_millis(100 * u64::from(i)),
            &both,
            &scene,
            &cams,
            None,
        );
    }
    assert_eq!(b.frames_built, 10, "one frame per tick, not one per client");
    let frame = b.prev.clone().unwrap();
    assert!(frame["cameras"].get("main").is_some());
    assert!(
        frame["cameras"].get("down").is_none(),
        "an unwatched camera is not computed"
    );
}

#[test]
fn an_unchanged_scene_sends_only_keepalives() {
    let mut b = Builder::new(10.0);
    let (cams, _) = cameras_from(&roster(), None, 0.0);
    let scene = scene_from(&snapshot(), 42, None, None);
    let d = demand(&["main:ar_route"]);
    let t0 = Instant::now();
    let mut published = Vec::new();
    // 3 s at 10 Hz.
    for i in 0..30u64 {
        if let Some(f) = b.step(t0 + Duration::from_millis(100 * i), &d, &scene, &cams, None) {
            published.push((i, f));
        }
    }
    assert_eq!(b.frames_built, 30);
    let ticks: Vec<u64> = published.iter().map(|(i, _)| *i).collect();
    assert_eq!(
        ticks,
        vec![0, 10, 20],
        "first frame then one keep-alive a second"
    );
    assert!(
        published.iter().all(|(_, f)| f["ts_ms"] == 42),
        "keep-alives keep the ts_ms"
    );
}

#[test]
fn fast_motion_raises_the_rate_to_20_hz() {
    let mut b = Builder::new(10.0);
    let (cams, _) = cameras_from(&roster(), None, 0.0);
    let d = demand(&["main:ar_route"]);
    let t0 = Instant::now();
    let mut built_slow = 0;
    for i in 0..20u64 {
        let scene = scene_from(&snapshot(), i as i64, None, None);
        b.step(t0 + Duration::from_millis(50 * i), &d, &scene, &cams, None);
        built_slow = b.frames_built;
    }
    assert_eq!(built_slow, 10, "10 Hz while still");
    let mut b = Builder::new(10.0);
    for i in 0..20u64 {
        let mut scene = scene_from(&snapshot(), i as i64, None, None);
        // 60 deg/s of yaw.
        scene.pose.as_mut().unwrap().yaw = (3.0 * i as f64).to_radians();
        b.step(t0 + Duration::from_millis(50 * i), &d, &scene, &cams, None);
    }
    assert_eq!(b.frames_built, 20, "20 Hz while yawing");
}

/// The 50 ms tick as it really wakes: a few milliseconds early or late.
fn jittered(t0: Instant, i: u64) -> Instant {
    const JITTER_MS: [i64; 10] = [0, 2, -3, 1, -2, 3, 1, -1, -3, 2];
    let ms = 50 * i as i64 + JITTER_MS[(i % 10) as usize];
    t0 + Duration::from_millis(ms as u64)
}

#[test]
fn the_rate_holds_under_tick_jitter() {
    let (cams, _) = cameras_from(&roster(), None, 0.0);
    let d = demand(&["main:ar_route"]);
    let t0 = Instant::now();
    let mut b = Builder::new(10.0);
    for i in 0..40u64 {
        let scene = scene_from(&snapshot(), i as i64, None, None);
        b.step(jittered(t0, i), &d, &scene, &cams, None);
    }
    assert_eq!(b.frames_built, 20, "10 Hz over 2 s while still");
    let mut b = Builder::new(10.0);
    for i in 0..40u64 {
        let mut scene = scene_from(&snapshot(), i as i64, None, None);
        scene.pose.as_mut().unwrap().yaw = (3.0 * i as f64).to_radians();
        b.step(jittered(t0, i), &d, &scene, &cams, None);
    }
    assert_eq!(b.frames_built, 40, "20 Hz over 2 s while yawing");
}

#[tokio::test]
async fn the_build_counter_is_logged_every_10_s_without_vehicle_state() {
    let t0 = Instant::now();
    let mut r = Runner::new(
        &ArConfig::default(),
        StateIpcClient::disconnected(),
        NavHub::new(),
        t0,
    );
    assert_eq!(r.tick(t0).await, None);
    let log = r
        .tick(t0 + Duration::from_secs(10))
        .await
        .expect("no counter record without vehicle state");
    assert_eq!((log.built, log.built_last_10s), (0, 0));
    assert_eq!(r.tick(t0 + Duration::from_secs(15)).await, None);
    assert!(r.tick(t0 + Duration::from_secs(20)).await.is_some());
}

#[test]
fn config_defaults_and_bounds() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("config.yaml");
    assert_eq!(ArConfig::load_from(&p).rate_hz, 10.0);
    std::fs::write(&p, "ar:\n  enabled: false\n  rate_hz: 15\n").unwrap();
    let c = ArConfig::load_from(&p);
    assert!(!c.enabled);
    assert_eq!(c.rate_hz, 15.0);
    std::fs::write(&p, "ar:\n  rate_hz: 500\n").unwrap();
    assert_eq!(ArConfig::load_from(&p).rate_hz, 10.0);
}
