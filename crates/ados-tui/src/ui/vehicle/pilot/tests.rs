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
            "gps": {"fix_type": 3, "satellites": 14, "eph": 0.8},
            "battery": {"voltage": 14.1, "remaining": 8}
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

const ROWS: [&str; 3] = ["Mode      ADOS Pilot", "Link loss ", "AGL       "];

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

/// The Alerts row and every alert line under it.
fn alert_block(text: &str) -> Vec<&str> {
    let lines: Vec<&str> = text.lines().collect();
    let at = lines
        .iter()
        .position(|l| l.contains("Alerts    "))
        .unwrap_or_else(|| panic!("no Alerts row\n{text}"));
    lines[at..]
        .iter()
        .take_while(|l| !l.trim().is_empty())
        .copied()
        .collect()
}

#[test]
fn current_alerts_never_read_as_stale_with_the_pilot_kind() {
    let mut app = app_with_pilot(1_800);
    // The pilot stalls: its rows dim, but the battery alert is current.
    app.core.now += Duration::from_millis(500);
    let text = draw(&mut app, 160, 48);
    assert!(text.contains("State     LINK (stale)"), "{text}");
    let block = alert_block(&text);
    assert!(
        block.iter().any(|l| l.contains("Battery critical")),
        "{text}"
    );
    for line in block {
        assert!(!line.contains("(stale)"), "{line:?} reads stale\n{text}");
    }
}

#[test]
fn the_rows_read_the_spec_threshold_as_stale() {
    let mut app = app_with_pilot(1_999);
    let text = draw(&mut app, 160, 48);
    assert!(!text.contains("State     LINK"), "1999 ms is fresh\n{text}");
    for prefix in ROWS {
        assert!(
            !row(&text, prefix).contains("(stale)"),
            "{prefix:?} stale at 1999 ms\n{text}"
        );
    }

    // One more millisecond: the frame is 2000 ms old, the threshold itself.
    app.core.now += Duration::from_millis(1);
    let text = draw(&mut app, 160, 48);
    assert!(text.contains("State     LINK (stale)"), "{text}");
    for prefix in ROWS {
        assert!(
            row(&text, prefix).contains("(stale)"),
            "{prefix:?} reads live at the threshold\n{text}"
        );
    }
}

#[test]
fn the_follow_height_reads_the_spec_threshold_as_stale() {
    let mut app = app_with_pilot(0);
    let mut frame: Value = serde_json::from_str(include_str!(
        "../../../../../ados-control/tests/fixtures/pilot-frame.json"
    ))
    .expect("pilot frame fixture is JSON");
    // No height of the pilot's own: the row reads the follow kind's.
    frame.as_object_mut().expect("object").remove("agl_m");
    land(
        &mut app,
        ResourceId::NavStatus,
        json!({
            "pilot": {"ts_ms": 1, "age_ms": 0, "data": frame},
            "follow": {"ts_ms": 1, "age_ms": 1_999,
                "data": {"phase": "following", "agl_m": 6.5, "agl_source": "baro"}}
        }),
    );
    let text = draw(&mut app, 160, 48);
    let agl = row(&text, "AGL       ");
    assert!(agl.contains("6.5 m"), "{text}");
    assert!(!agl.contains("(stale)"), "1999 ms is fresh\n{text}");

    app.core.now += Duration::from_millis(1);
    let text = draw(&mut app, 160, 48);
    assert!(
        row(&text, "AGL       ").contains("(stale)"),
        "a 2000 ms old follow height reads live\n{text}"
    );
}

#[test]
fn the_row_labels_are_the_spec_strings() {
    use crate::model::spec_text::{has_spec_text, spec_text};
    for key in [
        "cockpit.row.failsafe",
        "cockpit.row.mode",
        "cockpit.row.offers",
    ] {
        assert!(has_spec_text(key), "the spec has no {key}");
    }
    let mut app = app_with_pilot(0);
    let text = draw(&mut app, 160, 48);
    for key in ["cockpit.row.mode", "cockpit.row.offers"] {
        let label = format!("{:<width$} ", spec_text(key, &[]), width = super::LABEL);
        assert!(text.contains(&label), "no {label:?} row\n{text}");
    }
}

#[test]
fn an_operation_the_spec_does_not_know_reads_as_other() {
    assert_eq!(super::op_text("orbit_here"), "Other (orbit_here)");
    assert_ne!(super::op_text("goto"), "Other (goto)");
}
