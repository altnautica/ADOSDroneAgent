//! The Pilot block ages with the time since the last good read, as the chip
//! does: a poll that hangs without failing still dims every row.

use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::app::{App, Slot, Tab, TabId};
use crate::model::ProfileRoutes;
use crate::poll::ResourceId;
use crate::ui::tests::{draw, feed, test_app};

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

fn app_with_pilot(age_ms: u64) -> App {
    let mut app = test_app();
    let drone = json!({
        "version": "0.99.108", "device_name": "ados-x", "profile": "drone", "paired": true,
        "mavlink": {"connected": true},
        "telemetry": {"mode": "LOITER", "armed": true}
    });
    feed(&mut app, drone, ProfileRoutes::default(), Duration::ZERO);
    app.activate_tab(TabId::Vehicle);
    land(
        &mut app,
        ResourceId::Telemetry,
        json!({
            "armed": true, "mode": "LOITER",
            "attitude": {"roll": 0.0, "pitch": 0.0, "yaw": 1.57},
            "gps": {"fix_type": 3, "satellites": 14, "eph": 0.8}
        }),
    );
    let mut frame: Value = serde_json::from_str(include_str!(
        "../../../../../ados-control/tests/fixtures/pilot-frame.json"
    ))
    .expect("pilot frame fixture is JSON");
    frame["link_loss_in_s"] = json!(3.4);
    frame["link_loss_action"] = json!("rth");
    frame["agl_m"] = json!(8.4);
    frame["agl_source"] = json!("home");
    land(
        &mut app,
        ResourceId::NavStatus,
        json!({"pilot": {"ts_ms": 1, "age_ms": age_ms, "data": frame}}),
    );
    app
}

/// The screen line holding `prefix`, from `prefix` on.
fn row<'a>(text: &'a str, prefix: &str) -> &'a str {
    let line = text
        .lines()
        .find(|l| l.contains(prefix))
        .unwrap_or_else(|| panic!("no row {prefix:?}\n{text}"));
    &line[line.find(prefix).unwrap_or(0)..]
}

const ROWS: [&str; 4] = [
    "Mode      ADOS PILOT",
    "Link loss ",
    "AGL       ",
    "Alerts    ",
];

#[test]
fn the_rows_dim_once_the_pilot_ages_past_the_threshold_between_reads() {
    let mut app = app_with_pilot(1_800);
    let text = draw(&mut app, 160, 48);
    for prefix in ROWS {
        assert!(
            !row(&text, prefix).contains("(stale)"),
            "{prefix:?} stale on a fresh read\n{text}"
        );
    }
    assert!(
        row(&text, "Link loss ").contains("Link lost: Return home in 4 s"),
        "{text}"
    );

    // No new read for 500 ms: the 1.8 s old frame is now 2.3 s old.
    app.core.now += Duration::from_millis(500);
    let text = draw(&mut app, 160, 48);
    assert!(text.contains("State     LINK (stale)"), "{text}");
    for prefix in ROWS {
        assert!(
            row(&text, prefix).contains("(stale)"),
            "{prefix:?} reads live from an aged pilot kind\n{text}"
        );
    }
}
