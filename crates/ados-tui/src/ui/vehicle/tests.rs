use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::app::{App, Slot, Tab, TabId};
use crate::model::ProfileRoutes;
use crate::poll::ResourceId;
use crate::ui::tests::{draw, feed, test_app};

/// Put a resource body in the app and hand it to the Vehicle tab.
fn land(app: &mut App, id: ResourceId, value: Value) {
    app.core.res.insert(
        id.clone(),
        Slot {
            value: Some(value),
            error: None,
            code: None,
            error_body: None,
            at: Instant::now(),
        },
    );
    app.tabs.vehicle.on_resource(&mut app.core, &id);
}

fn drone() -> Value {
    json!({
        "version": "0.99.108", "device_name": "ados-x", "profile": "drone", "paired": true,
        "mavlink": {"connected": true},
        "telemetry": {"mode": "LOITER", "armed": false}
    })
}

fn telemetry() -> Value {
    json!({
        "armed": false, "mode": "LOITER",
        "attitude": {"roll": 0.05, "pitch": -0.02, "yaw": 1.57},
        "position": {"lat": 12.97, "lon": 77.59, "alt_rel": 42.5, "alt_msl": 920.0, "heading": 90.0},
        "position_age_ms": 120,
        "velocity": {"groundspeed": 2.2, "climb": 0.5},
        "battery": {"voltage": 16.4, "current": 12.1, "remaining": 87, "temperature": null, "cell_voltages": []},
        "gps": {"fix_type": 3, "satellites": 14, "eph": 0.8}
    })
}

fn battery() -> Value {
    json!({
        "enabled": true, "stale": false, "thresholds": {"reserve_percent": 25},
        "packs": [{
            "id": 0, "source": "mavlink", "stale": false, "cells_plausible": true,
            "cell_voltages_v": [3.9, 3.9, 3.9, 3.45], "weakest_cell_index": 3,
            "divergence_mv": 450.0, "voltage_v": 15.15, "current_a": 12.0,
            "remaining_pct": 60, "prediction": {"state": "normal", "eta_s": 420},
            "anomalies": [{"rule": "cell_divergence", "severity": "warning", "value": 450.0, "threshold": 50.0}]
        }]
    })
}

fn live_app() -> App {
    let mut app = test_app();
    feed(&mut app, drone(), ProfileRoutes::default(), Duration::ZERO);
    app.activate_tab(TabId::Vehicle);
    land(&mut app, ResourceId::Telemetry, telemetry());
    land(
        &mut app,
        ResourceId::Snapshot,
        json!({"fc": {"vehicle": "Quadrotor", "firmware": "ArduPilot", "connected": true, "gps": {"hdop": 0.8}}}),
    );
    land(&mut app, ResourceId::Battery, battery());
    app
}

#[test]
fn vehicle_renders_attitude_gps_charts_and_packs() {
    let mut app = live_app();
    let text = draw(&mut app, 120, 40);
    for want in [
        "Attitude",
        "roll +2.9°",
        "Quadrotor · ArduPilot",
        "LOITER",
        "DISARMED",
        "090° E",
        "3D",
        "12.970000, 77.590000",
        "Altitude 42.5 m",
        "Voltage 16.4 V",
        "Pack 0",
        "div 450 mV",
        "ETA 7:00 to 25 %",
        "cell divergence",
        "c4▼",
    ] {
        assert!(text.contains(want), "missing {want:?}\n{text}");
    }
}

#[test]
fn a_stale_snapshot_hides_the_flight_readings() {
    let mut app = test_app();
    feed(
        &mut app,
        drone(),
        ProfileRoutes::default(),
        Duration::from_secs(30),
    );
    app.activate_tab(TabId::Vehicle);
    land(&mut app, ResourceId::Telemetry, telemetry());
    let text = draw(&mut app, 120, 40);
    assert!(text.contains("flight readings hidden"), "{text}");
    assert!(!text.contains("LOITER  "), "{text}");
    assert!(!text.contains("12.970000"), "{text}");
}

#[test]
fn an_msp_flight_controller_shows_the_explanation() {
    let mut app = test_app();
    feed(&mut app, drone(), ProfileRoutes::default(), Duration::ZERO);
    if let Some(d) = app.core.view.dash.as_mut() {
        d.fc_firmware = Some("betaflight".into());
    }
    app.activate_tab(TabId::Vehicle);
    let text = draw(&mut app, 120, 40);
    assert!(
        text.contains("MSP flight controller: live telemetry is decoded"),
        "{text}"
    );
}

#[test]
fn a_dead_fc_body_reads_as_not_connected() {
    let mut app = test_app();
    feed(&mut app, drone(), ProfileRoutes::default(), Duration::ZERO);
    app.activate_tab(TabId::Vehicle);
    land(
        &mut app,
        ResourceId::Telemetry,
        json!({"param_generation": 3}),
    );
    let text = draw(&mut app, 120, 40);
    assert!(text.contains("No live vehicle telemetry"), "{text}");
    assert!(!text.contains("roll "), "{text}");
}

fn nav_status() -> Value {
    json!({
        "pose": {"ts_ms": 1, "age_ms": 20, "data": {"health": "good"}},
        "follow": {"ts_ms": 1, "age_ms": 40, "data": {
            "phase": "coasting", "track_id": 12, "range_m": 10.2, "range_sigma_m": 0.6,
            "reacquire_mode": "smart", "coast_remaining_s": 1.4}},
        "nav": {"ts_ms": 1, "age_ms": 30, "data": {
            "mode": "external_nav", "active_set": 1, "extnav_health": "good",
            "odom_rate_hz": 30.0, "obstacle_rate_hz": 10.0, "min_obstacle_m": 3.0, "gps_fix": 0}},
        "scan": {"ts_ms": 1, "age_ms": 50, "data": {"ranges_m": [0.0, 4.5, 2.5, 0.0]}},
        "map": {"ts_ms": 1, "age_ms": 500, "data": {
            "placement": "hybrid", "sweep_points": 120000, "area_m2": 850.0, "voxel_m": 0.1,
            "memory_used_mb": 210, "memory_budget_mb": 614, "keyframes_dropped": 0}},
    })
}

#[test]
fn the_autonomy_block_shows_follow_nav_lidar_and_map_as_words() {
    let mut app = live_app();
    land(&mut app, ResourceId::NavStatus, nav_status());
    let text = draw(&mut app, 120, 40);
    for want in [
        "Autonomy",
        "coasting #12 1.4 s",
        "10.2 m ± 0.6 m",
        "smart",
        "set 1 · ext nav good",
        "30 Hz · GPS no fix",
        "min 2.5 m · 10 Hz",
        "120000 pts · 850 m²",
        "210/614 MB (34%) · voxel 10 cm",
    ] {
        assert!(text.contains(want), "missing {want:?}\n{text}");
    }
}

#[test]
fn the_autonomy_block_says_when_nothing_publishes() {
    let mut app = live_app();
    land(
        &mut app,
        ResourceId::NavStatus,
        json!({"pose": null, "scan": null, "nav": null, "follow": null, "map": null}),
    );
    let text = draw(&mut app, 120, 40);
    assert!(text.contains("no autonomy plugin is publishing"), "{text}");
}

fn pilot_frame() -> Value {
    serde_json::from_str(include_str!(
        "../../../../ados-control/tests/fixtures/pilot-frame.json"
    ))
    .expect("pilot frame fixture is JSON")
}

fn pilot_status(data: Value, age_ms: u64) -> Value {
    json!({"pilot": {"ts_ms": 1, "age_ms": age_ms, "data": data}})
}

#[test]
fn the_autonomy_block_shows_who_flies_and_how() {
    let mut app = live_app();
    land(
        &mut app,
        ResourceId::NavStatus,
        pilot_status(pilot_frame(), 100),
    );
    let text = draw(&mut app, 120, 40);
    for want in [
        "Owner     FOLLOW · ORBIT 8 m",
        "Mode      ADOS PILOT · NORMAL",
        "Avoid     brake · detour 2.4 m",
    ] {
        assert!(text.contains(want), "missing {want:?}\n{text}");
    }
    // The owner label already names the skill: it is not repeated.
    assert!(!text.contains("ORBIT 8 m · Follow"), "{text}");
    assert!(!text.contains("(stale)"), "{text}");
}

#[test]
fn an_old_pilot_kind_is_marked_stale_and_avoid_off_reads_off() {
    let mut app = live_app();
    let mut frame = pilot_frame();
    frame["owner"] = json!("pilot");
    frame["owner_label"] = json!("ADOS PILOT");
    frame["skill"] = Value::Null;
    frame["control_mode"] = json!("fc_manual");
    frame["profile"] = json!("sport");
    frame["avoid"] = json!("off");
    land(&mut app, ResourceId::NavStatus, pilot_status(frame, 9000));
    let text = draw(&mut app, 120, 40);
    for want in [
        "Owner     ADOS PILOT (stale)",
        "Mode      FC MANUAL · SPORT",
        "Avoid     OFF",
    ] {
        assert!(text.contains(want), "missing {want:?}\n{text}");
    }
}

#[test]
fn without_a_pilot_kind_the_pilot_rows_read_a_dash() {
    let mut app = live_app();
    land(&mut app, ResourceId::NavStatus, nav_status());
    let text = draw(&mut app, 120, 40);
    for want in ["Owner     –", "Mode      –", "Avoid     –"] {
        assert!(text.contains(want), "missing {want:?}\n{text}");
    }
}

#[test]
fn the_vehicle_tab_polls_nav_status_at_two_hertz() {
    let app = live_app();
    let subs = app.tabs.vehicle.subs(&app.core);
    let nav = subs
        .iter()
        .find(|s| s.id == ResourceId::NavStatus)
        .expect("nav status subscribed");
    assert_eq!(nav.path, "/api/v1/nav/status");
    assert_eq!(nav.period, Duration::from_millis(500));
}

fn vision_status(main: bool, belly: bool) -> Value {
    json!({
        "models": [],
        "backend_state": "ready",
        "detection": {"mode": "on_demand", "cameras": {
            "main": {"detecting": main, "leases": []},
            "belly": {"detecting": belly, "leases": []},
        }},
    })
}

#[test]
fn the_autonomy_block_names_the_cameras_being_detected() {
    let mut app = live_app();
    land(&mut app, ResourceId::NavStatus, nav_status());
    land(
        &mut app,
        ResourceId::VisionStatus,
        vision_status(true, false),
    );
    let text = draw(&mut app, 120, 40);
    assert!(text.contains("detecting: main"), "{text}");
    assert!(!text.contains("belly"), "{text}");
}

#[test]
fn the_autonomy_block_shows_where_depth_runs_and_its_scale() {
    let mut app = live_app();
    land(&mut app, ResourceId::NavStatus, nav_status());
    let mut status = vision_status(false, false);
    status["depth"] = json!({
        "state": "ready", "interval_ms": 500,
        "cameras": {"main": {"depth": true, "surfaces": true, "leases": [], "scale_source": "lidar", "runs": 4}},
    });
    land(&mut app, ResourceId::VisionStatus, status);
    let text = draw(&mut app, 120, 40);
    assert!(text.contains("Depth"), "{text}");
    assert!(text.contains("main (lidar) · zones: main"), "{text}");
}

#[test]
fn an_idle_detector_reads_idle_even_with_nothing_publishing() {
    let mut app = live_app();
    land(
        &mut app,
        ResourceId::NavStatus,
        json!({"pose": null, "scan": null, "nav": null, "follow": null, "map": null}),
    );
    land(
        &mut app,
        ResourceId::VisionStatus,
        vision_status(false, false),
    );
    let text = draw(&mut app, 120, 40);
    assert!(text.contains("Detector"), "{text}");
    assert!(text.contains("idle"), "{text}");
    assert!(text.contains("no autonomy plugin is publishing"), "{text}");
}

#[test]
fn an_unreachable_engine_drops_the_detector_row() {
    let mut app = live_app();
    land(&mut app, ResourceId::NavStatus, nav_status());
    land(
        &mut app,
        ResourceId::VisionStatus,
        vision_status(true, true),
    );
    let id = ResourceId::VisionStatus;
    if let Some(slot) = app.core.res.get_mut(&id) {
        slot.error = Some("vision engine unavailable".into());
        slot.code = Some(503);
    }
    app.tabs.vehicle.on_resource(&mut app.core, &id);
    let text = draw(&mut app, 120, 40);
    assert!(!text.contains("Detector"), "{text}");
}

#[test]
fn the_vehicle_tab_polls_vision_status_every_second() {
    let app = live_app();
    let subs = app.tabs.vehicle.subs(&app.core);
    let vision = subs
        .iter()
        .find(|s| s.id == ResourceId::VisionStatus)
        .expect("vision status subscribed");
    assert_eq!(vision.path, "/api/vision/status");
    assert_eq!(vision.period, Duration::from_millis(1000));
}
