//! Params tab render and flow tests (no network: results are folded in).

use std::time::Instant;

use serde_json::json;

use super::*;
use crate::app::{App, Slot};
use crate::model::ProfileRoutes;
use crate::ui::tests::{draw, feed, test_app};

/// A disarmed ArduPilot drone with two parameters loaded on the Params tab.
fn params_app() -> App {
    let mut app = test_app();
    feed(
        &mut app,
        json!({
            "version": "0.99.108",
            "device_name": "ados-x",
            "profile": "drone",
            "paired": true,
            "mavlink": {"connected": true},
            "telemetry": {"mode": "LOITER", "armed": false}
        }),
        ProfileRoutes::default(),
        std::time::Duration::ZERO,
    );
    if let Some(d) = app.core.view.dash.as_mut() {
        d.fc_firmware = Some("ardupilot".into());
        d.armed = Some(false);
    }
    app.core.res.insert(
        ResourceId::Params,
        Slot {
            value: Some(json!({
                "params": {"ATC_RAT_RLL_P": 0.135, "WPNAV_SPEED": 500.0},
                "count": 2, "cached": 2, "priming": false,
                "priming_timeout": false, "priming_send_failed": false,
                "progress": {"got": 2, "expected": 2}
            })),
            error: None,
            code: None,
            error_body: None,
            at: Instant::now(),
        },
    );
    app.activate_tab(TabId::Params);
    app.tabs.params.on_resource(&mut app.core, &ResourceId::Params);
    app
}

fn row_with<'a>(text: &'a str, needle: &str) -> &'a str {
    text.lines().find(|l| l.contains(needle)).unwrap_or("")
}

#[test]
fn a_staged_row_shows_the_staged_marker() {
    let mut app = params_app();
    app.tabs.params.store.stage("ATC_RAT_RLL_P", 0.2);
    let text = draw(&mut app, 120, 40);
    assert!(text.contains("All"), "{text}");
    assert!(text.contains("Staged"), "{text}");
    let row = row_with(&text, "ATC_RAT_RLL_P");
    assert!(row.contains('●'), "{text}");
    assert!(row.contains("0.2"), "{text}");
    assert!(!row_with(&text, "WPNAV_SPEED").contains('●'), "{text}");
}

#[test]
fn msp_and_unknown_fcs_explain_instead_of_listing() {
    let mut app = params_app();
    if let Some(d) = app.core.view.dash.as_mut() {
        d.fc_firmware = Some("betaflight".into());
    }
    assert!(draw(&mut app, 120, 40).contains("MSP flight controller: parameters are edited"));
    if let Some(d) = app.core.view.dash.as_mut() {
        d.fc_firmware = None;
    }
    assert!(draw(&mut app, 120, 40).contains("Waiting for the flight controller"));
}

#[test]
fn a_write_is_refused_without_a_disarmed_reading() {
    let mut app = params_app();
    app.tabs.params.store.stage("WPNAV_SPEED", 750.0);
    if let Some(d) = app.core.view.dash.as_mut() {
        d.armed = None;
    }
    app.tabs.params.write(&mut app.core);
    assert!(app.core.overlays.is_empty());
    let last = app.core.toasts.history().last().map(|t| t.text.clone());
    assert_eq!(last.as_deref(), Some(edit::WRITE_REFUSAL));
}

#[test]
fn a_batch_unstages_acked_items_and_reports_the_tally() {
    let mut app = params_app();
    app.tabs.params.store.stage("WPNAV_SPEED", 750.0);
    app.tabs.params.store.stage("ATC_RAT_RLL_P", 0.2);
    app.tabs.params.write(&mut app.core);
    assert_eq!(app.core.overlays.len(), 1, "the confirm lists the batch");
    app.core.overlays.clear();
    app.tabs.params.submit_batch(&mut app.core);

    let tab = &mut app.tabs.params;
    tab.batch_item(&mut app.core, "WPNAV_SPEED".into(), 750.0, Ok(json!({"ack": true, "cached_value": 750.0})));
    tab.batch_item(&mut app.core, "ATC_RAT_RLL_P".into(), 0.2, Ok(json!({"ack": false})));
    tab.batch_done(&mut app.core);
    assert_eq!(tab.store.staged_value("WPNAV_SPEED"), None);
    assert_eq!(tab.store.value("WPNAV_SPEED"), Some(750.0));
    assert_eq!(tab.store.staged_value("ATC_RAT_RLL_P"), Some(0.2), "unconfirmed stays staged");
    let texts: Vec<String> = app.core.toasts.history().map(|t| t.text.clone()).collect();
    assert!(texts.iter().any(|t| t == "ATC_RAT_RLL_P: sent, not confirmed"), "{texts:?}");
    assert_eq!(texts.last().map(String::as_str), Some("Wrote 1/2"));
}
