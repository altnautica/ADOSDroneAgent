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
