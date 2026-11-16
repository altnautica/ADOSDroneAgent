//! Steady take-off in the Pilot block: a running launch's status line and
//! detail, the launch-failed banner with the stop's reason, the Balance
//! check and steady take-off rows at their level, and `b` ("I rebalanced
//! it") sending the pilot's `airframe_reset` through `/api/command` while the
//! pilot is disarmed.

use serde_json::{json, Value};

use super::{alert_block, app_with_frame, column_rows, command_body, stall_frame};
use crate::app::Tab;
use crate::keymap::{Command, Ctx, PageKeys};
use crate::ui::tests::draw;

const NOSE: &str = "Balance: nose-heavy. Off by about 35 mm. Move the battery back.";
const OFF: &str = "Standard take-off. Steady take-off is off.";

/// A frame with no stall and no banner, armed as given.
fn quiet(armed: bool) -> Value {
    let mut frame = stall_frame();
    let o = frame.as_object_mut().unwrap();
    o.remove("mission_stall");
    o.remove("banner");
    frame["armed"] = json!(armed);
    frame["landed"] = json!(!armed);
    frame
}

/// On the ground with the airframe balance check at `severity`.
fn unbalanced(severity: &str, armed: bool) -> Value {
    let mut frame = quiet(armed);
    frame["readiness"] = json!([
        {"id": "airframe_balance", "ok": false, "severity": severity, "msg": NOSE},
        {"id": "supervised_liftoff", "ok": false, "severity": "warn", "msg": OFF},
    ]);
    frame
}

#[test]
fn a_running_launch_shows_its_status_line_and_detail() {
    let mut frame = quiet(true);
    frame["sequence"] = json!({"step": "takeoff", "after": "hold", "error": null,
        "liftoff": {"phase": "climb", "text": "Airborne", "detail": "Climbing to 3 m", "trim": "learned", "tilt_deg": 0.4}});
    let mut app = app_with_frame(1, frame);
    let text = draw(&mut app, 160, 60);
    let rows = column_rows(&text, "Take-off  ", 2);
    assert!(rows[0].ends_with("Airborne"), "{text}");
    assert!(rows[1].ends_with("Climbing to 3 m"), "{text}");
}

#[test]
fn a_stopped_lift_off_reads_on_the_launch_failed_banner() {
    let reason = "Take-off stopped. The drone started to tip. Motors off.";
    let mut frame = quiet(false);
    frame["sequence"] = json!({"step": "takeoff", "after": "hold", "error": "liftoff_tipping",
        "liftoff": {"phase": "stopped", "text": "Take-off stopped", "detail": "The drone started to tip. Motors off.", "trim": "probe", "tilt_deg": 2.0}});
    frame["banner"] = json!({"text_key": "pilot.banner.launch_failed", "args": {"reason": reason}, "until_ms": u64::MAX / 2});
    let mut app = app_with_frame(1, frame);
    let text = draw(&mut app, 200, 60);
    assert!(!text.contains("Take-off  "), "{text}");
    assert!(text.contains("Launch failed: Take-off stopped"), "{text}");
}

#[test]
fn the_balance_rows_read_at_their_level_with_the_reset_key() {
    use crate::model::pilot_alerts::Severity;
    for (severity, level) in [("warn", Severity::Warning), ("block", Severity::Critical)] {
        let mut app = app_with_frame(1, unbalanced(severity, false));
        let text = draw(&mut app, 220, 60);
        let block = alert_block(&text);
        // The narrow Pilot column may cut an alert's text; the key below it
        // stays whole.
        let at = block
            .iter()
            .position(|l| l.contains("Balance: nose-heavy."))
            .unwrap_or_else(|| panic!("{text}"));
        assert!(block[at + 1].contains("b I rebalanced it"), "{text}");
        assert!(block.iter().any(|l| l.contains(OFF)), "{text}");
        let alerts = app.tabs.vehicle.alerts(&app.core);
        let find = |id: &str| alerts.iter().find(|a| a.id == id).map(|a| a.severity);
        assert_eq!(find("pilot:airframe_balance"), Some(level));
        assert_eq!(find("pilot:supervised_liftoff"), Some(Severity::Warning));
        assert!(app.ctx_stack().contains(&Ctx::Page(PageKeys::Airframe)));
    }
}

#[test]
fn the_reset_key_sends_airframe_reset_on_the_ground_only() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut app = app_with_frame(port, unbalanced("block", false));
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    app.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE));
    assert_eq!(
        command_body(listener, "airframe_reset"),
        json!({"cmd": "airframereset"})
    );

    // Armed, or with the check passing: no key.
    let mut app = app_with_frame(1, unbalanced("warn", true));
    assert!(!app.ctx_stack().contains(&Ctx::Page(PageKeys::Airframe)));
    assert!(!app
        .tabs
        .vehicle
        .on_command(&mut app.core, Command::AirframeReset));
    let mut app = app_with_frame(1, quiet(false));
    assert!(!app
        .tabs
        .vehicle
        .on_command(&mut app.core, Command::AirframeReset));
}
