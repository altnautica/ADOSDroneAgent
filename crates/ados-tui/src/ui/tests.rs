use super::*;
use crate::model::Dashboard;
use ratatui::backend::TestBackend;
use ratatui::Terminal;
use serde_json::json;

/// Render one frame to an off-screen backend and return every cell's symbol,
/// row by row, so a test can assert a rendered string is present.
fn buffer_text(dash: Option<&Dashboard>, update_latest: Option<&str>, splash: bool) -> String {
    draw_text(dash, update_latest, splash, None, false)
}

fn draw_text(
    dash: Option<&Dashboard>,
    update_latest: Option<&str>,
    splash: bool,
    error: Option<&str>,
    stale: bool,
) -> String {
    let backend = TestBackend::new(120, 50);
    let mut terminal = Terminal::new(backend).unwrap();
    let history = History::default();
    terminal
        .draw(|f| {
            render(
                f,
                dash,
                &history,
                Some("12:00:00"),
                stale,
                error,
                None,
                update_latest,
                splash,
            )
        })
        .unwrap();
    let buf = terminal.backend().buffer().clone();
    let area = buf.area;
    let mut out = String::new();
    for y in 0..area.height {
        for x in 0..area.width {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

/// A drone with no FC and an incomplete step, exercising every new surface.
fn sample() -> Dashboard {
    let data = json!({
        "version": "0.99.108",
        "device_name": "ados-x",
        "profile": "drone",
        "paired": true,
        "access_urls": [
            {"kind": "setup", "label": "mDNS setup", "url": "http://ados-x.local:8080/setup", "primary": true},
            {"kind": "setup", "label": "LAN setup", "url": "http://192.168.1.5:8080/setup"},
            {"kind": "mission_control", "label": "Mission Control", "url": "https://command.altnautica.com"},
            {"kind": "video", "label": "viewer", "url": "http://ados-x.local:8889/main/"},
            {"kind": "mavlink", "label": "MAVLink WS", "url": "ws://ados-x.local:8765/"}
        ],
        "mavlink": {"connected": false},
        "steps": [
            {"label": "Flight controller", "state": "needs_action", "detail": "Connect the FC and re-check."},
            {"label": "Profile", "state": "complete", "detail": "Confirmed as drone"}
        ]
    });
    let mut dash = Dashboard::from_status(&data);
    dash.merge_services(&json!({"services": [{"state": "active"}]}));
    dash
}

#[test]
fn renders_links_fc_and_update_badge() {
    let text = buffer_text(Some(&sample()), Some("0.99.200"), false);
    // The new Connect-to-GCS section + its bare base address (the `  ●`
    // primary marker distinguishes it from the Console `…/setup` row).
    assert!(text.contains("Connect to GCS"), "{text}");
    assert!(text.contains("http://ados-x.local:8080  ●"), "{text}");
    // Reworded Ground control + hosted Mission Control.
    assert!(text.contains("Conventional MAVLink"), "{text}");
    assert!(text.contains("command.altnautica.com"), "{text}");
    // FC-down diagnostic sub-line (Autopilot cell).
    assert!(
        text.contains("No flight controller detected on USB/serial"),
        "{text}"
    );
    // The agent's per-step detail is surfaced in the Services cell.
    assert!(text.contains("Connect the FC and re-check."), "{text}");
    // Footer bottom-right update badge.
    assert!(text.contains("update available"), "{text}");
}

#[test]
fn renders_update_splash() {
    let text = buffer_text(Some(&sample()), Some("0.99.200"), true);
    assert!(text.contains("Update available"), "{text}");
    assert!(text.contains("update now"), "{text}");
}

#[test]
fn no_update_badge_when_up_to_date() {
    let text = buffer_text(Some(&sample()), Some("0.99.108"), false);
    assert!(!text.contains("update available"), "{text}");
    assert!(text.contains("up to date"), "{text}");
}

/// A healthy, armed-state-reporting drone as the last good snapshot.
fn live_drone() -> Dashboard {
    let mut dash = Dashboard::from_status(&json!({
        "version": "0.99.108",
        "device_name": "ados-x",
        "profile": "drone",
        "paired": true,
        "mavlink": {"connected": true},
        "telemetry": {
            "mode": "LOITER",
            "armed": false,
            "battery": {"remaining": 82.0}
        }
    }));
    dash.merge_services(&json!({"services": [{"state": "active"}]}));
    dash
}

#[test]
fn a_failed_poll_takes_the_verdict_line_over_the_last_snapshot() {
    let dash = live_drone();
    assert!(draw_text(Some(&dash), None, false, None, false).contains("HEALTHY"));

    let text = draw_text(
        Some(&dash),
        None,
        false,
        Some("Agent unreachable: connection refused"),
        false,
    );
    assert!(text.contains("UNREACHABLE"), "{text}");
    assert!(
        text.contains("Agent unreachable: connection refused"),
        "{text}"
    );
    assert!(!text.contains("HEALTHY"), "{text}");
}

#[test]
fn a_stale_snapshot_hides_its_flight_readings() {
    let dash = live_drone();
    let live = draw_text(Some(&dash), None, false, None, false);
    assert!(
        live.contains("DISARMED") && live.contains("battery 82%"),
        "{live}"
    );

    let text = draw_text(
        Some(&dash),
        None,
        false,
        Some("Agent unreachable: timed out"),
        true,
    );
    assert!(text.contains("UNREACHABLE"), "{text}");
    assert!(!text.contains("HEALTHY"), "{text}");
    assert!(!text.contains("DISARMED"), "{text}");
    assert!(!text.contains("LOITER"), "{text}");
    assert!(!text.contains("battery 82%"), "{text}");
    assert!(text.contains("flight readings hidden"), "{text}");

    // A hung poll with no error yet still loses the verdict once stale.
    let hung = draw_text(Some(&dash), None, false, None, true);
    assert!(hung.contains("STALE"), "{hung}");
    assert!(!hung.contains("HEALTHY"), "{hung}");
}
