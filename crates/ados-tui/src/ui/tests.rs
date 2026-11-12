//! Render tests: draw whole frames to an off-screen backend and assert the
//! visible text (and, through `draw_buffer`, a style where it carries meaning).

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::Terminal;
use serde_json::{json, Value};

use crate::app::{App, Core, TabId};
use crate::args::Target;
use crate::model::ProfileRoutes;
use crate::overlay::Overlay;
use crate::poll::PollResult;
use crate::prefs::PrefsStore;

/// An app pointed at a port nothing listens on: the background poller fails
/// fast and its results are never drained, so the frame shows only what the
/// test folds in.
pub(crate) fn test_app() -> App {
    let target = Target::from_host(None, "127.0.0.1", 1);
    let latest = Arc::new(Mutex::new(None));
    App::new(Core::new(target, None, PrefsStore::load(None), latest))
}

/// Fold a setup status (and service list) into the app as one poll taken
/// `age` ago.
pub(crate) fn feed(app: &mut App, setup: Value, routes: ProfileRoutes, age: Duration) {
    let result = PollResult {
        setup: Ok(setup),
        fc_status: None,
        status_full: Some(json!({"services": [{"state": "active"}]})),
        profile: routes,
    };
    app.core
        .view
        .apply(result, Instant::now() - age, "12:00:00".into());
    app.core.now = Instant::now();
}

/// Render one frame at `w`×`h` and return the buffer, for the few tests that
/// assert a style (a stale row drawn dim).
pub(crate) fn draw_buffer(app: &mut App, w: u16, h: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
    terminal
        .draw(|f| {
            let _ = super::render(f, app);
        })
        .unwrap();
    terminal.backend().buffer().clone()
}

/// Render one frame at `w`×`h` and return the text, row by row.
pub(crate) fn draw(app: &mut App, w: u16, h: u16) -> String {
    let buf = draw_buffer(app, w, h);
    let mut out = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

/// A drone with no FC and an incomplete step.
fn sample() -> Value {
    json!({
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
        ],
        "hardware_check": {"items": [
            {"id": "fc", "label": "FC", "required": true, "state": "missing"},
            {"id": "cam", "label": "Camera", "required": true, "state": "ok"}
        ]}
    })
}

/// A healthy drone reporting mode and arm state.
fn live_drone() -> Value {
    json!({
        "version": "0.99.108",
        "device_name": "ados-x",
        "profile": "drone",
        "paired": true,
        "mavlink": {"connected": true},
        "telemetry": {"mode": "LOITER", "armed": false, "battery": {"remaining": 82.0}}
    })
}

fn fresh() -> Duration {
    Duration::ZERO
}

#[test]
fn overview_renders_links_fc_steps_and_hardware() {
    let mut app = test_app();
    feed(&mut app, sample(), ProfileRoutes::default(), fresh());
    let text = draw(&mut app, 120, 50);
    assert!(text.contains("Connect to GCS"), "{text}");
    assert!(text.contains("http://ados-x.local:8080  ●"), "{text}");
    assert!(text.contains("Conventional MAVLink"), "{text}");
    assert!(text.contains("command.altnautica.com"), "{text}");
    assert!(
        text.contains("No flight controller detected on USB/serial"),
        "{text}"
    );
    assert!(text.contains("Connect the FC and re-check."), "{text}");
    assert!(text.contains("hardware 1 ok  1 missing"), "{text}");
}

#[test]
fn the_update_badge_and_splash_show_a_newer_version() {
    let mut app = test_app();
    feed(&mut app, sample(), ProfileRoutes::default(), fresh());
    *app.core.latest.lock().unwrap() = Some("0.99.200".into());
    let text = draw(&mut app, 120, 50);
    assert!(text.contains("v0.99.108 ⬆ 0.99.200"), "{text}");

    app.core.overlays.push(Overlay::UpdateSplash);
    let text = draw(&mut app, 120, 50);
    assert!(text.contains("Update available"), "{text}");
    assert!(text.contains("update now"), "{text}");

    let mut same = test_app();
    feed(&mut same, sample(), ProfileRoutes::default(), fresh());
    *same.core.latest.lock().unwrap() = Some("0.99.108".into());
    assert!(!draw(&mut same, 120, 50).contains("⬆"));
}

#[test]
fn a_failed_poll_takes_the_verdict_line_over_the_last_snapshot() {
    let mut app = test_app();
    feed(&mut app, live_drone(), ProfileRoutes::default(), fresh());
    assert!(draw(&mut app, 120, 50).contains("HEALTHY"));

    app.core.view.error = Some("Agent unreachable: connection refused".into());
    let text = draw(&mut app, 120, 50);
    assert!(text.contains("UNREACHABLE"), "{text}");
    assert!(
        text.contains("Agent unreachable: connection refused"),
        "{text}"
    );
    assert!(!text.contains("HEALTHY"), "{text}");
}

#[test]
fn no_agent_names_the_host_and_the_retry_key_not_the_transport_error() {
    let refused = ados_protocol::rest::RestError::Transport(
        "http://127.0.0.1:1/api/v1/setup/status: Connection refused".into(),
    );
    let mut app = test_app();
    app.core.view.error = Some(crate::app::error_text(&refused));
    let text = draw(&mut app, 120, 40);
    assert!(
        text.contains("Cannot reach the agent at 127.0.0.1. r retry"),
        "{text}"
    );
    assert!(!text.contains("transport error"), "{text}");

    feed(&mut app, live_drone(), ProfileRoutes::default(), fresh());
    app.core.view.error = Some(crate::app::error_text(&refused));
    let text = draw(&mut app, 120, 40);
    assert!(text.contains("UNREACHABLE"), "{text}");
    assert!(
        text.contains("Cannot reach the agent at 127.0.0.1"),
        "{text}"
    );
}

#[test]
fn a_stale_snapshot_hides_its_flight_readings() {
    let mut app = test_app();
    feed(&mut app, live_drone(), ProfileRoutes::default(), fresh());
    let live = draw(&mut app, 120, 50);
    assert!(
        live.contains("DISARMED") && live.contains("battery 82%"),
        "{live}"
    );

    let mut old = test_app();
    feed(
        &mut old,
        live_drone(),
        ProfileRoutes::default(),
        Duration::from_secs(30),
    );
    let text = draw(&mut old, 120, 50);
    assert!(text.contains("STALE"), "{text}");
    assert!(!text.contains("HEALTHY"), "{text}");
    assert!(!text.contains("DISARMED"), "{text}");
    assert!(!text.contains("LOITER"), "{text}");
    assert!(!text.contains("battery 82%"), "{text}");
    assert!(text.contains("flight readings hidden"), "{text}");
}

fn profile_status(profile: &str) -> Value {
    json!({
        "version": "0.99.108", "device_name": "ados-x", "profile": profile,
        "paired": true, "mavlink": {"connected": false}
    })
}

#[test]
fn visor_renders_its_panel_row() {
    let mut app = test_app();
    let routes = crate::model::profile_tests::visor_routes(true, false);
    feed(&mut app, profile_status("visor"), routes, fresh());
    let text = draw(&mut app, 120, 50);
    assert!(text.contains("Visor"), "{text}");
    assert!(text.contains("capture REC 06:41"), "{text}");
    assert!(text.contains("HEALTHY"), "{text}");
}

#[test]
fn ground_station_renders_radios_gnss_and_lora() {
    let mut app = test_app();
    let routes = crate::model::profile_tests::gs_routes();
    feed(&mut app, profile_status("ground_station"), routes, fresh());
    let text = draw(&mut app, 140, 50);
    assert!(text.contains("Ground station"), "{text}");
    assert!(
        text.contains("wlan1   rxtx   -48 dBm  loss 0.1 %"),
        "{text}"
    );
    assert!(text.contains("GNSS 3d 14 sats base survey"), "{text}");
}

#[test]
fn a_stale_snapshot_hides_the_profile_row() {
    let mut app = test_app();
    let routes = crate::model::profile_tests::gs_routes();
    feed(
        &mut app,
        profile_status("ground_station"),
        routes,
        Duration::from_secs(30),
    );
    let text = draw(&mut app, 140, 50);
    assert!(!text.contains("LoRa IN865"), "{text}");
    assert!(text.contains("readings hidden (stale)"), "{text}");
}

/// The tab-bar row of a frame.
fn tab_row(text: &str) -> String {
    text.lines().nth(1).unwrap_or_default().to_string()
}

#[test]
fn the_tab_bar_lists_only_the_tabs_the_profile_shows() {
    let mut drone = test_app();
    feed(
        &mut drone,
        profile_status("drone"),
        ProfileRoutes::default(),
        fresh(),
    );
    let row = tab_row(&draw(&mut drone, 120, 40));
    for label in [
        "Overview",
        "Vehicle",
        "Link",
        "Video",
        "Params",
        "Extensions",
        "Agent logs",
        "System",
        "Settings",
    ] {
        assert!(row.contains(label), "{label} missing: {row}");
    }

    let mut gs = test_app();
    feed(
        &mut gs,
        profile_status("ground_station"),
        ProfileRoutes::default(),
        fresh(),
    );
    let row = tab_row(&draw(&mut gs, 120, 40));
    assert!(row.contains("Ground") && row.contains("Link"), "{row}");
    assert!(
        !row.contains("Vehicle") && !row.contains("Video") && !row.contains("Params"),
        "{row}"
    );

    // Narrow terminals show the short labels.
    let row = tab_row(&draw(&mut drone, 80, 24));
    assert!(row.contains("1 Ovw") && row.contains("5 Prm"), "{row}");
}

#[test]
fn a_hidden_tab_refuses_its_digit() {
    let mut gs = test_app();
    feed(
        &mut gs,
        profile_status("ground_station"),
        ProfileRoutes::default(),
        fresh(),
    );
    gs.activate_tab(TabId::Params);
    assert_eq!(gs.core.tab, TabId::Overview);
    let text = draw(&mut gs, 120, 40);
    assert!(text.contains("Not available on a ground station"), "{text}");
}

#[test]
fn a_terminal_below_80_by_24_shows_the_size_notice() {
    let mut app = test_app();
    feed(&mut app, sample(), ProfileRoutes::default(), fresh());
    let text = draw(&mut app, 79, 23);
    assert!(
        text.contains("Terminal too small: need 80×24, have 79×23"),
        "{text}"
    );
    assert!(!draw(&mut app, 80, 24).contains("Terminal too small"));
    let narrow = draw(&mut app, 40, 10);
    assert!(narrow.contains("Too small: 40×10, need 80×24"), "{narrow}");
}

#[test]
fn the_overview_shows_the_flight_state_chip_and_alert_count() {
    use crate::app::{Slot, Tab};
    use crate::poll::ResourceId;
    let mut app = test_app();
    feed(&mut app, live_drone(), ProfileRoutes::default(), fresh());
    let frame: Value = serde_json::from_str(include_str!(
        "../../../ados-control/tests/fixtures/pilot-frame.json"
    ))
    .expect("pilot frame fixture is JSON");
    let id = ResourceId::NavStatus;
    app.core.res.insert(
        id.clone(),
        Slot {
            value: Some(json!({"pilot": {"ts_ms": 1, "age_ms": 100, "data": frame}})),
            error: None,
            code: None,
            error_body: None,
            at: Instant::now(),
        },
    );
    app.tabs.overview.on_resource(&mut app.core, &id);
    let text = draw(&mut app, 120, 40);
    assert!(text.contains("HEALTHY"), "{text}");
    assert!(text.contains("State     FOLLOW · ACTIVE"), "{text}");
    assert!(text.contains("Alerts    "), "{text}");
    let subs = app.tabs.overview.subs(&app.core);
    assert!(subs.iter().any(|s| s.id == ResourceId::NavStatus));
}

#[test]
fn every_confirmed_quick_action_keeps_its_confirm_mark_at_80_by_24() {
    let mut app = test_app();
    feed(&mut app, sample(), ProfileRoutes::default(), fresh());
    app.core.open(Overlay::Actions(0));
    let text = draw(&mut app, 80, 24);
    let want = crate::action::ACTIONS.iter().filter(|a| a.confirm).count();
    assert!(want > 0);
    assert_eq!(text.matches("(confirm)").count(), want, "{text}");
}

#[test]
fn the_hint_bar_keeps_whole_hints_and_always_help() {
    let hints = [
        ("Tab", "pane"),
        ("/", "search"),
        ("^K", "palette"),
        ("?", "help"),
        ("q", "quit"),
    ];
    let kept = crate::ui::chrome::fit_hints(&hints, 22);
    assert!(kept.contains(&("?", "help")), "{kept:?}");
    assert!(kept.contains(&("q", "quit")), "{kept:?}");
    let narrow = crate::ui::chrome::fit_hints(&hints, 8);
    assert_eq!(narrow, [("?", "help")]);
    assert_eq!(crate::ui::chrome::fit_hints(&hints, 200), hints);
}

#[test]
fn help_at_80_by_24_keeps_its_close_line() {
    for tab in [TabId::Params, TabId::Logs, TabId::Extensions, TabId::System] {
        let mut app = test_app();
        feed(&mut app, sample(), ProfileRoutes::default(), fresh());
        app.activate_tab(tab);
        app.core.open(Overlay::Help);
        let text = draw(&mut app, 80, 24);
        assert!(text.contains("Esc close"), "{tab:?}\n{text}");
    }
}

#[test]
fn a_checklist_with_its_own_footer_drops_the_generic_hint() {
    use crate::overlay::{CheckItem, ChecklistState, Ret};
    let mut app = test_app();
    feed(&mut app, sample(), ProfileRoutes::default(), fresh());
    app.core.open(Overlay::Checklist(ChecklistState {
        title: "Remove".into(),
        items: vec![CheckItem {
            label: "Remove its data".into(),
            checked: false,
            locked: false,
            bit: None,
        }],
        selected: 0,
        footer: Some("Space toggles · Enter removes · Esc cancels".into()),
        ret: Ret::new(crate::worker::Route::tab(TabId::Extensions), "remove"),
    }));
    let text = draw(&mut app, 120, 40);
    assert!(text.contains("Enter removes"), "{text}");
    assert!(!text.contains("Enter done"), "{text}");
}

#[test]
fn a_gauge_label_below_half_uses_the_text_colour() {
    use crate::ui::widgets::{render_cells, Cell};
    let app = test_app();
    let t = &app.core.theme;
    let mut term = Terminal::new(TestBackend::new(20, 1)).unwrap();
    term.draw(|f| {
        let cell = Cell::Gauge {
            ratio: 0.2,
            label: "20 %".into(),
            color: t.accent(),
        };
        render_cells(f, t, f.area(), vec![cell]);
    })
    .unwrap();
    let buf = term.backend().buffer().clone();
    let x = (0..20u16)
        .find(|&x| buf[(x, 0)].symbol() == "2")
        .expect("label drawn");
    assert_eq!(buf[(x, 0)].fg, t.text().fg.unwrap());
}
