use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use serde_json::{json, Value};

use crate::app::{App, Slot, Tab, TabId};
use crate::model::ProfileRoutes;
use crate::poll::ResourceId;
use crate::ui::tests::{draw, draw_buffer, feed, test_app};

/// Put a resource body in the app and hand it to the Vehicle tab.
pub(super) fn land(app: &mut App, id: ResourceId, value: Value) {
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

pub(super) fn live_app() -> App {
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
        "7:00 to reserve",
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
        "Coasting #12 1.4 s",
        "10.2 m ± 0.6 m",
        "Reacquire Smart",
        "Set 1 · Ext nav Good",
        "30 Hz · GPS No fix",
        "min 2.5 m · obstacles 10 Hz",
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
        "State     Follow · Active",
        "Mode      ADOS Pilot · Normal",
        "Avoid     Brake · Avoiding 2.4 m",
    ] {
        assert!(text.contains(want), "missing {want:?}\n{text}");
    }
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
        "State     Link (stale)",
        "Mode      FC Manual · Sport (stale)",
        "Avoid     Avoidance off",
    ] {
        assert!(text.contains(want), "missing {want:?}\n{text}");
    }
}

#[test]
fn without_a_pilot_kind_the_state_reads_no_pilot() {
    let mut app = live_app();
    land(&mut app, ResourceId::NavStatus, nav_status());
    let text = draw(&mut app, 120, 40);
    assert!(text.contains("State     No pilot"), "{text}");
    assert!(!text.contains("Mode      –"), "{text}");
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

/// Record a failed read of `id` that keeps the last good body, as the poller
/// does, and hand it to the Vehicle tab.
fn fail(app: &mut App, id: ResourceId) {
    let slot = app.core.res.get_mut(&id).expect("an earlier read");
    slot.error = Some("HTTP 503: hub down".into());
    slot.code = Some(503);
    app.tabs.vehicle.on_resource(&mut app.core, &id);
}

/// The screen line holding `prefix`, from `prefix` on.
fn row<'a>(text: &'a str, prefix: &str) -> &'a str {
    let line = text
        .lines()
        .find(|l| l.contains(prefix))
        .unwrap_or_else(|| panic!("no row {prefix:?}\n{text}"));
    &line[line.find(prefix).unwrap_or(0)..]
}

/// The cell where `needle` first starts on screen.
fn cell_of(buf: &Buffer, needle: &str) -> (u16, u16) {
    for y in 0..buf.area.height {
        let mut line = String::new();
        let mut xs = Vec::new();
        for x in 0..buf.area.width {
            let sym = buf[(x, y)].symbol();
            xs.extend(sym.chars().map(|_| x));
            line.push_str(sym);
        }
        if let Some(at) = line.find(needle) {
            return (xs[line[..at].chars().count()], y);
        }
    }
    panic!("{needle:?} is not on screen");
}

/// `needle` is drawn in the theme's dim (muted) colour.
fn assert_dim(app: &App, buf: &Buffer, needle: &str) {
    let (x, y) = cell_of(buf, needle);
    assert_eq!(
        buf[(x, y)].fg,
        app.core.theme.muted(),
        "{needle:?} is not drawn dim"
    );
}

fn nav_with_pilot(pilot: Value) -> Value {
    let mut body = nav_status();
    body["pilot"] = pilot;
    body
}

#[test]
fn a_failed_nav_read_keeps_every_row_and_marks_it_stale() {
    let mut app = live_app();
    land(
        &mut app,
        ResourceId::NavStatus,
        nav_with_pilot(json!({"ts_ms": 1, "age_ms": 100, "data": pilot_frame()})),
    );
    let fresh = draw(&mut app, 160, 48);
    assert!(!fresh.contains("(stale)"), "{fresh}");
    fail(&mut app, ResourceId::NavStatus);
    let text = draw(&mut app, 160, 48);
    for prefix in [
        "Mode      ADOS Pilot",
        "Avoid     ",
        "Follow    ",
        "Range     10.2 m",
        "Reacquire Smart",
        "Nav       ",
        "Odometry  30 Hz",
        "Lidar     ",
        "Map       120000 pts",
        "Memory    ",
    ] {
        assert!(
            row(&text, prefix).contains("(stale)"),
            "{prefix:?} kept without a stale mark\n{text}"
        );
    }
    // The mark follows the state word, ahead of a tail the column may clip.
    assert!(
        row(&text, "Lidar     ").contains("6.5 Hz (stale) · warning · min"),
        "{text}"
    );
    assert!(text.contains("last good read 0 s ago"), "{text}");
    let buf = draw_buffer(&mut app, 160, 48);
    assert_dim(&app, &buf, "Coasting #12");
    assert_dim(&app, &buf, "120000 pts");
}

#[test]
fn every_row_built_from_a_stale_kind_is_marked_and_dim() {
    let mut app = live_app();
    let mut body = nav_with_pilot(json!({"ts_ms": 1, "age_ms": 5000, "data": pilot_frame()}));
    body["nav"]["age_ms"] = json!(9000);
    // No scan: the Lidar row falls back to the nav kind's nearest obstacle.
    body["scan"] = Value::Null;
    land(&mut app, ResourceId::NavStatus, body);
    let text = draw(&mut app, 160, 48);
    for prefix in [
        "Mode      ADOS Pilot",
        "Avoid     ",
        "Nav       ",
        "Odometry  ",
        "Lidar     ",
    ] {
        assert!(
            row(&text, prefix).contains("(stale)"),
            "{prefix:?} reads live from a stale kind\n{text}"
        );
    }
    assert!(
        row(&text, "Lidar     ").contains("6.5 Hz (stale) · warning · min"),
        "{text}"
    );
    assert!(!row(&text, "Follow    ").contains("(stale)"), "{text}");
    let buf = draw_buffer(&mut app, 160, 48);
    assert_dim(&app, &buf, "ADOS Pilot");
    assert_dim(&app, &buf, "warning · min");
}

#[test]
fn a_pilot_kind_without_an_age_reads_stale() {
    let mut app = live_app();
    land(
        &mut app,
        ResourceId::NavStatus,
        json!({"pilot": {"ts_ms": 1, "data": pilot_frame()}}),
    );
    let text = draw(&mut app, 120, 40);
    assert!(
        row(&text, "Mode      ADOS Pilot").contains("(stale)"),
        "{text}"
    );
}

#[test]
fn the_gps_panel_and_the_odometry_row_name_a_fix_alike() {
    for (fix, word) in [(0, "No fix"), (1, "No fix"), (7, "static"), (8, "PPP")] {
        let mut app = live_app();
        let mut tel = telemetry();
        tel["gps"]["fix_type"] = json!(fix);
        land(&mut app, ResourceId::Telemetry, tel);
        let mut body = nav_status();
        body["nav"]["data"]["gps_fix"] = json!(fix);
        land(&mut app, ResourceId::NavStatus, body);
        let text = draw(&mut app, 120, 40);
        assert!(
            row(&text, "Fix       ").starts_with(&format!("Fix       {word}")),
            "fix {fix}: GPS panel\n{text}"
        );
        assert!(
            row(&text, "Odometry  ").contains(&format!("GPS {word}")),
            "fix {fix}: Odometry row\n{text}"
        );
    }
}

#[test]
fn a_stale_battery_route_dims_the_block() {
    let mut app = live_app();
    let mut body = battery();
    body["stale"] = json!(true);
    land(&mut app, ResourceId::Battery, body);
    let text = draw(&mut app, 120, 40);
    assert!(row(&text, "Battery packs").contains("(stale)"), "{text}");
    let buf = draw_buffer(&mut app, 120, 40);
    assert_dim(&app, &buf, "15.15 V");
}

#[test]
fn a_failed_battery_read_keeps_the_packs_and_marks_them_stale() {
    let mut app = live_app();
    let fresh = draw(&mut app, 120, 40);
    assert!(!row(&fresh, "Battery packs").contains("(stale)"), "{fresh}");
    fail(&mut app, ResourceId::Battery);
    let text = draw(&mut app, 120, 40);
    assert!(row(&text, "Battery packs").contains("(stale)"), "{text}");
    assert!(text.contains("15.15 V"), "{text}");
    assert!(text.contains("last good read 0 s ago"), "{text}");
    let buf = draw_buffer(&mut app, 120, 40);
    assert_dim(&app, &buf, "15.15 V");
}

#[test]
fn the_pilot_panel_reads_its_rows_from_the_spec() {
    let mut app = live_app();
    land(
        &mut app,
        ResourceId::NavStatus,
        pilot_status(pilot_frame(), 100),
    );
    let text = draw(&mut app, 160, 48);
    for want in [
        "State     Follow · Active",
        "Avoiding · ADOS Pilot",
        "Mode      ADOS Pilot · Normal",
        "Avoid     Brake · Avoiding 2.4 m",
        "Mission   No mission loaded on the drone",
        "Link loss → Return home",
        "AGL       4.2 m (AGL)",
        "Alerts    2 warnings",
        "Battery 1: cell divergence",
        "No downward rangefinder",
    ] {
        assert!(text.contains(want), "missing {want:?}\n{text}");
    }
    assert!(!text.contains("(stale)"), "{text}");
}

#[test]
fn the_pilot_panel_on_a_mission_with_the_link_lost() {
    let mut app = live_app();
    let mut tel = telemetry();
    tel["prearm"] = json!(["PreArm: Compass not calibrated"]);
    land(&mut app, ResourceId::Telemetry, tel);
    let mut frame = pilot_frame();
    frame["owner"] = json!("mission_fc");
    frame["skill"] = Value::Null;
    frame["mission"] = json!({"state": "active", "seq": 4, "total": 6, "landing_seq": false});
    frame["link_loss_in_s"] = json!(3.4);
    frame["link_loss_action"] = json!("land");
    frame["agl_m"] = json!(8.4);
    frame["agl_source"] = json!("home");
    frame["readiness"] = json!([]);
    frame["statustext"] = json!([
        {"severity": 4, "text": "EKF3 IMU0 yaw inconsistent", "age_ms": 900},
        {"severity": 2, "text": "EKF3 IMU0 yaw inconsistent", "age_ms": 400},
        {"severity": 4, "text": "PreArm: Compass not calibrated", "age_ms": 1200},
    ]);
    land(&mut app, ResourceId::NavStatus, pilot_status(frame, 100));
    let text = draw(&mut app, 160, 48);
    for want in [
        "State     Mission · WP 4/6",
        "Mission   WP 4/6",
        "Link loss Link lost: Land in 4 s",
        "AGL       8.4 m (HOME)",
        "Alerts    3 warnings",
        "Battery 1: cell divergence",
        "EKF3 IMU0 yaw inconsistent",
        "▲ Compass not calibrated",
    ] {
        assert!(text.contains(want), "missing {want:?}\n{text}");
    }
    // The FC repeats a text: one alert, at its worst severity.
    assert_eq!(
        text.matches("EKF3 IMU0 yaw inconsistent").count(),
        1,
        "{text}"
    );
}

#[test]
fn the_chip_reads_link_from_the_spec_stale_threshold() {
    let mut app = live_app();
    for age in [2500, 2000] {
        land(
            &mut app,
            ResourceId::NavStatus,
            pilot_status(pilot_frame(), age),
        );
        let text = draw(&mut app, 120, 40);
        assert!(text.contains("State     Link (stale)"), "{age} ms\n{text}");
    }
    land(
        &mut app,
        ResourceId::NavStatus,
        pilot_status(pilot_frame(), 1999),
    );
    let text = draw(&mut app, 120, 40);
    assert!(text.contains("State     Follow · Active"), "{text}");
}

/// The pilot frame with mission, link-loss and alert rows, beside a full
/// autonomy reading and the detector: the busiest Vehicle tab.
fn busy_nav_status() -> Value {
    let mut frame = pilot_frame();
    frame["statustext"] = json!([
        {"severity": 4, "text": "EKF3 IMU0 yaw inconsistent", "age_ms": 900},
        {"severity": 4, "text": "Compass variance", "age_ms": 400},
    ]);
    let mut nav = nav_status();
    nav["pilot"] = json!({"ts_ms": 1, "age_ms": 100, "data": frame});
    nav
}

#[test]
fn pilot_and_autonomy_fit_a_120_by_40_terminal() {
    let mut app = live_app();
    land(&mut app, ResourceId::NavStatus, busy_nav_status());
    land(
        &mut app,
        ResourceId::VisionStatus,
        vision_status(true, false),
    );
    let text = draw(&mut app, 120, 40);
    // Every pilot row through the alert count, and the autonomy block's
    // first rows, are on screen; whatever is cut is counted, never lost silently.
    for want in [
        "State     Follow · Active",
        "Mode      ADOS Pilot · Normal",
        "Mission",
        "Link loss → Return home",
        "AGL       4.2 m (AGL)",
        "Offers    Fly here",
        "Alerts    ",
        "Autonomy",
        "Detector",
        "Follow    ",
    ] {
        assert!(text.contains(want), "missing {want:?}\n{text}");
    }
    assert!(
        text.contains("more (a taller terminal shows all)"),
        "{text}"
    );
    // A tall terminal shows everything, charts included, with nothing cut.
    let text = draw(&mut app, 160, 60);
    for want in ["Memory", "Compass variance", "Offers    Fly here"] {
        assert!(text.contains(want), "missing {want:?}\n{text}");
    }
    assert!(
        !text.contains("more (a taller terminal shows all)"),
        "{text}"
    );
}

#[test]
fn the_pilot_panel_shows_surface_ops_and_the_failsafe_cause() {
    let mut app = live_app();
    let mut frame = pilot_frame();
    frame["owner"] = json!("fc_mode");
    frame["skill"] = Value::Null;
    frame["fc_failsafe"] = json!(true);
    frame["failsafe_cause"] = json!("rc_or_failsafe");
    frame["surface_ops"] = json!([]);
    land(&mut app, ResourceId::NavStatus, pilot_status(frame, 100));
    let text = draw(&mut app, 160, 48);
    // The alerts list carries the failsafe as the pill does.
    for want in [
        "Failsafe  Failsafe · RC switch or failsafe",
        "Offers    –",
        "Failsafe: RC switch or failsafe",
    ] {
        assert!(text.contains(want), "missing {want:?}\n{text}");
    }
    let mut frame = pilot_frame();
    frame["fc_failsafe"] = json!(true);
    frame["failsafe_cause"] = json!("fc");
    land(&mut app, ResourceId::NavStatus, pilot_status(frame, 100));
    let text = draw(&mut app, 160, 48);
    for want in [
        "Failsafe  Failsafe · flight controller",
        "Offers    Fly here",
        "Failsafe: flight controller",
    ] {
        assert!(text.contains(want), "missing {want:?}\n{text}");
    }
}

#[test]
fn the_chip_reads_the_owning_skills_detail_from_its_plugin() {
    let mut app = live_app();
    land(
        &mut app,
        ResourceId::NavStatus,
        pilot_status(pilot_frame(), 100),
    );
    let plugin = "com.example.follow".to_string();
    let subs = app.tabs.vehicle.subs(&app.core);
    assert!(
        subs.iter()
            .any(|s| s.id == ResourceId::PluginState(plugin.clone())),
        "{subs:?}"
    );
    land(
        &mut app,
        ResourceId::PluginDetail(plugin.clone()),
        json!({"manifest": {"gcs": {"contributes": {"skills": [
            {"id": "follow-me", "state": {"topic": "follow.state"}}]}}}}),
    );
    land(
        &mut app,
        ResourceId::PluginState(plugin),
        json!({"follow.state": {"payload": {"follow_mode": "orbit", "orbit_radius_m": 8}}}),
    );
    let text = draw(&mut app, 160, 48);
    assert!(text.contains("State     Follow · Orbit 8 m"), "{text}");
}

#[test]
fn a_failsafe_frame_keeps_its_pilot_rows_at_80_by_24() {
    let mut app = live_app();
    let mut nav = busy_nav_status();
    nav["pilot"]["data"]["fc_failsafe"] = json!(true);
    nav["pilot"]["data"]["failsafe_cause"] = json!("fc");
    land(&mut app, ResourceId::NavStatus, nav);
    land(
        &mut app,
        ResourceId::VisionStatus,
        vision_status(true, false),
    );
    let text = draw(&mut app, 80, 24);
    for want in ["State ", "Failsafe", "Link loss", "Alerts "] {
        assert!(text.contains(want), "missing {want:?}\n{text}");
    }
}

#[test]
fn a_stale_feed_draws_no_trend_charts_from_its_history() {
    let mut app = live_app();
    assert!(draw(&mut app, 120, 40).contains("Altitude 42.5 m"));
    feed(
        &mut app,
        drone(),
        ProfileRoutes::default(),
        Duration::from_secs(30),
    );
    let text = draw(&mut app, 120, 40);
    assert!(text.contains("flight readings hidden"), "{text}");
    for gone in [
        "Altitude 42.5",
        "Voltage 16.4",
        "Current 12.1",
        "Remaining 87",
    ] {
        assert!(!text.contains(gone), "{gone:?} drawn while stale\n{text}");
    }
}

#[test]
fn the_detector_row_counts_the_newest_fresh_batch() {
    let mut app = live_app();
    land(
        &mut app,
        ResourceId::VisionStatus,
        vision_status(true, false),
    );
    land(
        &mut app,
        ResourceId::DetectionsLatest,
        json!({"available": true, "stale": false, "camera_id": "main",
               "detections": [{}, {}, {}]}),
    );
    let text = draw(&mut app, 120, 40);
    assert!(text.contains("3 objects on main"), "{text}");

    land(
        &mut app,
        ResourceId::DetectionsLatest,
        json!({"available": true, "stale": true, "camera_id": "main",
               "detections": [{}, {}, {}]}),
    );
    assert!(!draw(&mut app, 120, 40).contains("3 objects"));
}

#[test]
fn the_pose_row_prints_its_age_once() {
    let mut app = live_app();
    let mut body = nav_status();
    body["nav"]["data"]["pose_age_ms"] = json!(42.0);
    land(&mut app, ResourceId::NavStatus, body);
    let text = draw(&mut app, 160, 48);
    let pose = row(&text, "Pose      ");
    assert!(pose.starts_with("Pose      42 ms"), "{text}");
    assert_eq!(pose.matches("Pose").count(), 1, "{text}");
}

#[test]
fn the_pilot_block_names_the_gimbal_once_it_answers() {
    let mut app = live_app();
    land(
        &mut app,
        ResourceId::NavStatus,
        pilot_status(pilot_frame(), 100),
    );
    assert!(!draw(&mut app, 160, 48).contains("takes rate commands"));
    land(&mut app, ResourceId::GimbalRate, json!({"available": true}));
    let text = draw(&mut app, 160, 48);
    assert!(text.contains("takes rate commands"), "{text}");
    assert!(
        row(&text, "Sticks    ").starts_with("Sticks    Sticks are with"),
        "{text}"
    );
}

#[test]
fn the_sticks_and_gimbal_rows_read_their_words_from_the_spec() {
    use crate::model::spec_text::{has_spec_text, spec_text};
    for key in ["cockpit.row.sticks", "cockpit.gimbal.takes_rates"] {
        assert!(has_spec_text(key), "no spec key {key}");
    }
    let mut app = live_app();
    land(
        &mut app,
        ResourceId::NavStatus,
        pilot_status(pilot_frame(), 100),
    );
    land(&mut app, ResourceId::GimbalRate, json!({"available": true}));
    let text = draw(&mut app, 160, 48);
    let sticks = format!("{:<10}", spec_text("cockpit.row.sticks", &[]));
    assert!(row(&text, &sticks).starts_with(&sticks), "{text}");
    let gimbal = format!("{:<10}", spec_text("cockpit.gimbal_pad", &[]));
    assert!(
        row(&text, &gimbal).contains(&spec_text("cockpit.gimbal.takes_rates", &[])),
        "{text}"
    );
}

/// Land a failed read of `id`, answered with `code` (`None`: no answer).
fn land_failed(app: &mut App, id: ResourceId, code: Option<u16>) {
    app.core.res.insert(
        id.clone(),
        Slot {
            value: None,
            error: Some("read failed".into()),
            code,
            error_body: None,
            at: Instant::now(),
        },
    );
    app.tabs.vehicle.on_resource(&mut app.core, &id);
}

#[test]
fn only_a_missing_gimbal_route_reads_unavailable() {
    let mut app = live_app();
    land(
        &mut app,
        ResourceId::NavStatus,
        pilot_status(pilot_frame(), 100),
    );
    let unavailable = crate::model::spec_text::spec_text("cockpit.unavailable", &[]);
    for code in [None, Some(401), Some(500), Some(503)] {
        land_failed(&mut app, ResourceId::GimbalRate, code);
        let text = draw(&mut app, 160, 48);
        assert!(!text.contains(&unavailable), "{code:?}\n{text}");
    }
    land_failed(&mut app, ResourceId::GimbalRate, Some(404));
    let text = draw(&mut app, 160, 48);
    assert!(row(&text, "Gimbal").contains(&unavailable), "{text}");
}

#[test]
fn detections_are_read_only_while_a_detector_runs() {
    let polls_detections = |app: &App| {
        app.tabs
            .vehicle
            .subs(&app.core)
            .iter()
            .any(|s| s.id == ResourceId::DetectionsLatest)
    };
    let mut app = live_app();
    assert!(!polls_detections(&app), "no vision status yet");
    land(
        &mut app,
        ResourceId::VisionStatus,
        vision_status(false, false),
    );
    assert!(!polls_detections(&app), "detector idle");
    land(
        &mut app,
        ResourceId::VisionStatus,
        vision_status(true, false),
    );
    assert!(polls_detections(&app), "detector running");
    land(
        &mut app,
        ResourceId::DetectionsLatest,
        json!({"available": true, "stale": false, "camera_id": "main",
               "detections": [{}, {}, {}]}),
    );
    assert!(draw(&mut app, 120, 40).contains("3 objects on main"));
    // The detector stops: its last count goes with it.
    land(
        &mut app,
        ResourceId::VisionStatus,
        vision_status(false, false),
    );
    assert!(!polls_detections(&app), "detector stopped");
    assert!(!draw(&mut app, 120, 40).contains("3 objects"));
}

#[test]
fn trend_charts_hide_while_telemetry_fails_or_the_fc_is_down() {
    let gone = [
        "Altitude 42.5",
        "Voltage 16.4",
        "Current 12.1",
        "Remaining 87",
    ];
    let mut app = live_app();
    assert!(draw(&mut app, 120, 40).contains("Altitude 42.5 m"));
    land_failed(&mut app, ResourceId::Telemetry, Some(503));
    let text = draw(&mut app, 120, 40);
    assert!(text.contains("telemetry unavailable"), "{text}");
    for g in gone {
        assert!(!text.contains(g), "{g:?} drawn on a failed read\n{text}");
    }

    let mut app = live_app();
    land(&mut app, ResourceId::Telemetry, json!({"armed": null}));
    let text = draw(&mut app, 120, 40);
    assert!(
        text.contains("flight controller is not connected"),
        "{text}"
    );
    for g in gone {
        assert!(!text.contains(g), "{g:?} drawn with the FC down\n{text}");
    }
}

#[test]
fn the_chip_shows_the_gnss_denied_cap() {
    let mut app = live_app();
    let mut frame = pilot_frame();
    frame["speed_cap"] = json!({"reason": "gnss-denied", "v_h_mps": 2.0});
    land(&mut app, ResourceId::NavStatus, pilot_status(frame, 100));
    let text = draw(&mut app, 160, 48);
    assert!(text.contains("GNSS-denied: 2 m/s max"), "{text}");
}
