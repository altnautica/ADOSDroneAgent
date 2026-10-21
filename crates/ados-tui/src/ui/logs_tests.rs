use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::{LogsTab, Mode};
use crate::app::{Slot, Tab, TabId};
use crate::model::ProfileRoutes;
use crate::poll::ResourceId;
use crate::stream::LogEntry;
use crate::ui::tests::{draw, feed, test_app};

fn line(seq: u64, level: &str, msg: &str) -> LogEntry {
    LogEntry::from_value(&json!({
        "seq": seq, "timestamp": "2026-10-21T12:04:31.123Z",
        "level": level, "logger": "ados.mavlink", "message": msg,
    }))
    .unwrap()
}

fn setup() -> Value {
    json!({"version": "0.99.108", "device_name": "ados-x", "profile": "drone"})
}

#[test]
fn level_filter_hides_lower_levels() {
    let mut tab = LogsTab::default();
    tab.ring.push(line(1, "DEBUG", "d"));
    tab.ring.push(line(2, "INFO", "i"));
    tab.ring.push(line(3, "WARN", "w"));
    tab.ring.push(line(4, "ERROR", "e"));
    tab.level = 0;
    assert_eq!(tab.visible().len(), 4);
    tab.level = 2;
    let shown: Vec<&str> = tab.visible().iter().map(|e| e.message.as_str()).collect();
    assert_eq!(shown, vec!["w", "e"]);
    tab.level = 1;
    tab.filter = "E".into();
    assert_eq!(
        tab.visible().len(),
        1,
        "the text filter is case-insensitive"
    );
}

#[test]
fn pause_freezes_view_and_counts_new_lines() {
    let mut app = test_app();
    let tab = &mut app.tabs.logs;
    tab.ring.push(line(1, "INFO", "a"));
    tab.paused_at = Some(tab.ring.next_n());
    tab.ring.push(line(2, "INFO", "b"));
    tab.ring.push(line(3, "INFO", "c"));
    assert_eq!(tab.visible().len(), 1);
    assert_eq!(tab.status_hint(&app.core).as_deref(), Some("PAUSED +2"));
}

#[test]
fn poll_fallback_folds_entries_oldest_first_without_duplicates() {
    let mut app = test_app();
    app.tabs.logs.mode = Mode::Polling;
    app.tabs.logs.ring.push(line(10, "INFO", "old"));
    let body = json!({
        "entries": [
            {"seq": 12, "timestamp": null, "level": "INFO", "logger": "x", "message": "newest"},
            {"seq": 11, "timestamp": null, "level": "INFO", "logger": "x", "message": "middle"},
            {"seq": 10, "timestamp": null, "level": "INFO", "logger": "x", "message": "old"}
        ],
        "total": 3, "limit": 200, "offset": 0,
        "warning": "logging store unavailable"
    });
    app.core.res.insert(
        ResourceId::LogsPoll,
        Slot {
            value: Some(body),
            error: None,
            code: None,
            error_body: None,
            at: Instant::now(),
        },
    );
    app.tabs
        .logs
        .on_resource(&mut app.core, &ResourceId::LogsPoll);
    let msgs: Vec<&str> = app
        .tabs
        .logs
        .ring
        .iter()
        .map(|e| e.message.as_str())
        .collect();
    assert_eq!(msgs, vec!["old", "middle", "newest"]);
    assert_eq!(
        app.tabs.logs.warning.as_deref(),
        Some("logging store unavailable")
    );
}

#[test]
fn render_logs_120x40() {
    let mut app = test_app();
    feed(&mut app, setup(), ProfileRoutes::default(), Duration::ZERO);
    app.activate_tab(TabId::Logs);
    let tab = &mut app.tabs.logs;
    tab.ring.push(line(1, "INFO", "router started"));
    tab.ring.push(line(2, "WARN", "heartbeat late"));
    tab.ring.push(line(3, "ERROR", "serial port lost"));
    tab.mode = Mode::Polling;
    tab.down_reason = Some("http 503: too many subscribers".into());
    tab.warning = Some("logging store unavailable".into());
    let text = draw(&mut app, 120, 40);
    assert!(text.contains("Level ≥ INFO"), "{text}");
    assert!(text.contains("polling (stream unavailable)"), "{text}");
    assert!(text.contains("logging store unavailable"), "{text}");
    assert!(text.contains("12:04:31.123"), "{text}");
    assert!(text.contains("WARNING"), "{text}");
    assert!(text.contains("ERROR"), "{text}");
    assert!(text.contains("ados.mavlink"), "{text}");
    assert!(text.contains("serial port lost"), "{text}");
    assert!(text.contains("3/3 lines"), "{text}");
}
