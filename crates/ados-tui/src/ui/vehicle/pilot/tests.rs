//! The Pilot block ages with the time since the last good read, as the chip
//! does: a poll that hangs without failing still dims every row.

use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::app::{App, Slot, Tab, TabId};
use crate::model::ProfileRoutes;
use crate::poll::ResourceId;
use crate::ui::tests::{draw, draw_buffer, feed, test_app};

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
    assert!(text.contains("State     Link (stale)"), "{text}");
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
    assert!(text.contains("State     Link (stale)"), "{text}");
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
    assert!(!text.contains("State     Link"), "1999 ms is fresh\n{text}");
    for prefix in ROWS {
        assert!(
            !row(&text, prefix).contains("(stale)"),
            "{prefix:?} stale at 1999 ms\n{text}"
        );
    }

    // One more millisecond: the frame is 2000 ms old, the threshold itself.
    app.core.now += Duration::from_millis(1);
    let text = draw(&mut app, 160, 48);
    assert!(text.contains("State     Link (stale)"), "{text}");
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

#[test]
fn the_sensing_tier_and_its_speed_limit_show_only_while_the_pilot_reports_them() {
    let mut frame: Value = serde_json::from_str(include_str!(
        "../../../../../ados-control/tests/fixtures/pilot-frame.json"
    ))
    .expect("pilot frame fixture is JSON");
    let text = draw(&mut app_with_frame(1, frame.clone()), 160, 48);
    assert!(!text.contains("Sensing:"), "{text}");

    frame["sensing"] = json!({"tier": "lidar3d", "clear_range_m": 11.6, "speed_limit_mps": 4.2});
    let text = draw(&mut app_with_frame(1, frame.clone()), 160, 48);
    assert!(text.contains("Sensing: 3D lidar · ≤ 4.2 m/s"), "{text}");

    // A tier this build does not know reads as no sensing.
    frame["sensing"] = json!({"tier": "radar", "clear_range_m": 11.6, "speed_limit_mps": 4.2});
    let text = draw(&mut app_with_frame(1, frame), 160, 48);
    assert!(!text.contains("Sensing:"), "{text}");
}

// --- the mission stall, from the golden stall frame -------------------------

/// The golden stall frame, its banner slot moved to the far future so it
/// shows whatever today's date is.
fn stall_frame() -> Value {
    let mut frame: Value = serde_json::from_str(include_str!(
        "../../../../../ados-control/tests/fixtures/pilot-frame-stall.json"
    ))
    .expect("stall frame fixture is JSON");
    frame["banner"]["until_ms"] = json!(u64::MAX / 2);
    frame
}

/// An app on `port` with the Vehicle tab showing `frame` fresh.
fn app_with_frame(port: u16, frame: Value) -> App {
    use std::sync::{Arc, Mutex};

    use crate::app::Core;
    use crate::args::Target;
    use crate::prefs::PrefsStore;

    let target = Target::from_host(None, "127.0.0.1", port);
    let mut app = App::new(Core::new(
        target,
        None,
        PrefsStore::load(None),
        Arc::new(Mutex::new(None)),
    ));
    let drone = json!({
        "version": "0.99.108", "device_name": "ados-x", "profile": "drone", "paired": true,
        "mavlink": {"connected": true},
        "telemetry": {"mode": "GUIDED", "armed": true}
    });
    feed(&mut app, drone, ProfileRoutes::default(), Duration::ZERO);
    app.activate_tab(TabId::Vehicle);
    land(
        &mut app,
        ResourceId::Telemetry,
        json!({
            "armed": true, "mode": "GUIDED",
            "attitude": {"roll": 0.0, "pitch": 0.0, "yaw": 1.57},
            "gps": {"fix_type": 3, "satellites": 14, "eph": 0.8},
            "battery": {"voltage": 15.9, "remaining": 62}
        }),
    );
    land(
        &mut app,
        ResourceId::NavStatus,
        json!({"pilot": {"ts_ms": 1, "age_ms": 0, "data": frame}}),
    );
    app
}

/// The `n` Pilot-column lines from the one holding `prefix`, read in that
/// column: a countdown breaks after each phrase onto the next line.
fn column_rows(text: &str, prefix: &str, n: usize) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    let at = lines
        .iter()
        .position(|l| l.contains(prefix))
        .unwrap_or_else(|| panic!("no row {prefix:?}\n{text}"));
    let line = lines[at];
    let col = line[..line.find(prefix).unwrap_or(0)].chars().count();
    lines[at..]
        .iter()
        .take(n)
        .map(|l| {
            let cell: String = l.chars().skip(col).take_while(|c| *c != '│').collect();
            cell.trim_end().to_string()
        })
        .collect()
}

#[test]
fn the_golden_stall_renders_its_overlay_keys_and_hides_its_banner() {
    let mut app = app_with_frame(1, stall_frame());
    let text = draw(&mut app, 160, 48);
    // `cargo test -- --nocapture` prints the frame (the buffer dump).
    println!("{text}");
    let stall = column_rows(&text, "Stall     ", 3);
    assert!(
        stall[0].contains("No progress to waypoint 3 for 15 s"),
        "{text}"
    );
    assert!(stall[1].ends_with("Skipping in 33 s"), "{text}");
    assert!(
        stall[2].ends_with("s Skip waypoint · r Return · l Land here"),
        "{text}"
    );
    // The stall's own banner hides behind the overlay, which carries the
    // live countdown.
    assert!(!text.contains("Banner    "), "{text}");
    // The status pill lists it too.
    let block = alert_block(&text);
    assert!(
        block
            .iter()
            .any(|l| l.contains("No progress to waypoint 3 for 15 s")),
        "{text}"
    );
}

#[test]
fn the_stall_row_is_critical() {
    use ratatui::style::Color;

    use crate::theme::{Theme, ThemeId, Tier};

    let mut app = app_with_frame(1, stall_frame());
    app.core.theme = Theme::new(Tier::Truecolor, ThemeId::BrandDark, false);
    let buf = draw_buffer(&mut app, 160, 48);
    let error = app.core.theme.error();
    let mut found = false;
    for y in 0..buf.area.height {
        let line: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
        if let Some(at) = line.find("No progress to waypoint 3 for 15 s") {
            let x = line[..at].chars().count() as u16;
            assert_eq!(
                buf[(x, y)].fg,
                error,
                "the stall title is not the error colour"
            );
            assert_ne!(error, Color::Reset);
            found = true;
            break;
        }
    }
    assert!(found, "no stall title on screen");
}

#[test]
fn a_blocked_return_offers_land_only() {
    let mut frame = stall_frame();
    frame["mission_stall"] = json!({"kind": "return", "seq": null, "wp_label": null, "since_ms": 1, "skip_in_s": 40.2, "window_s": 20.0});
    let mut app = app_with_frame(1, frame);
    let text = draw(&mut app, 160, 48);
    let stall = column_rows(&text, "Stall     ", 2);
    assert!(stall[0].ends_with("Return blocked"), "{text}");
    assert!(stall[1].ends_with("Landing in 41 s"), "{text}");
    assert!(text.contains("l Land here"), "{text}");
    assert!(!text.contains("s Skip waypoint"), "{text}");
    assert_eq!(
        app.ctx_stack().first(),
        Some(&crate::keymap::Ctx::Page(
            crate::keymap::PageKeys::ReturnStall
        ))
    );
}

#[test]
fn the_low_battery_countdown_shows_with_the_spec_text() {
    let mut frame = stall_frame();
    frame.as_object_mut().unwrap().remove("mission_stall");
    frame["rth_countdown_s"] = json!(9.2);
    let mut app = app_with_frame(1, frame);
    let text = draw(&mut app, 160, 48);
    let battery = column_rows(&text, "Battery   Battery reserve reached", 2);
    assert!(
        battery[1].ends_with("Returning along the flown path in 10 s"),
        "{text}"
    );
    assert!(!text.contains("Stall     "), "{text}");
    assert_eq!(app.ctx_stack().first(), Some(&crate::keymap::Ctx::Global));
}

#[test]
fn a_stale_stall_dims_and_takes_no_keys() {
    let mut app = app_with_frame(1, stall_frame());
    app.core.now += Duration::from_millis(2_500);
    let text = draw(&mut app, 160, 48);
    let stall = column_rows(&text, "Stall     ", 2);
    assert!(stall[1].ends_with("(stale)"), "{text}");
    assert!(!text.contains("s Skip waypoint"), "{text}");
    assert_eq!(app.ctx_stack().first(), Some(&crate::keymap::Ctx::Global));
}

/// Serve `n` requests on `listener` until a `POST /api/command` arrives;
/// answer it as the route does in ADOS control and return its body. Anything
/// else (the background poll) gets a 404.
fn command_body(listener: std::net::TcpListener, wanted: &str) -> Value {
    use std::io::{BufRead, BufReader, Read, Write};

    let deadline = Instant::now() + Duration::from_secs(10);
    listener.set_nonblocking(true).unwrap();
    while Instant::now() < deadline {
        let Ok((stream, _)) = listener.accept() else {
            std::thread::sleep(Duration::from_millis(10));
            continue;
        };
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut head = String::new();
        let mut len = 0usize;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                break;
            }
            if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                len = v.trim().parse().unwrap_or(0);
            }
            head.push_str(&line);
        }
        let mut body = vec![0; len];
        reader.read_exact(&mut body).unwrap();
        let mut stream = stream;
        if head.starts_with("POST /api/command ") {
            let body: Value = serde_json::from_slice(&body).unwrap();
            let reply =
                json!({"status": "ok", "cmd": body["cmd"], "via": "pilot", "action": wanted})
                    .to_string();
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                reply.len()
            );
            return body;
        }
        let _ = stream
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    }
    panic!("no POST /api/command within 10 s");
}

#[test]
fn each_stall_key_sends_its_command_to_the_agent() {
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    for (key, cmd, action) in [
        ('s', "missionskip", "mission_skip"),
        ('r', "rtl", "rth"),
        ('l', "land", "land"),
    ] {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut app = app_with_frame(port, stall_frame());
        assert_eq!(
            app.ctx_stack().first(),
            Some(&crate::keymap::Ctx::Page(crate::keymap::PageKeys::Stall))
        );
        app.on_key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE));
        let body = command_body(listener, action);
        assert_eq!(body, json!({"cmd": cmd}), "key {key:?}");
    }
}

#[test]
fn a_key_without_a_stall_sends_nothing() {
    let mut frame = stall_frame();
    frame.as_object_mut().unwrap().remove("mission_stall");
    let mut app = app_with_frame(1, frame);
    let tab = &mut app.tabs.vehicle;
    assert!(!tab.on_command(&mut app.core, crate::keymap::Command::StallSkip));
    // A blocked return answers Land here only.
    let mut frame = stall_frame();
    frame["mission_stall"]["kind"] = json!("return");
    let mut app = app_with_frame(1, frame);
    let tab = &mut app.tabs.vehicle;
    assert!(!tab.on_command(&mut app.core, crate::keymap::Command::StallSkip));
    assert!(!tab.on_command(&mut app.core, crate::keymap::Command::StallReturn));
}

#[test]
fn the_reply_toasts_the_pilot_answer() {
    use crate::model::mission_stall::StallAction;
    use crate::worker::Reply;

    let mut app = app_with_frame(1, stall_frame());
    let req = crate::ui::vehicle::stall_request(StallAction::Skip);
    assert_eq!((req.method, req.path.as_str()), ("POST", "/api/command"));
    assert_eq!(req.body, Some(json!({"cmd": "missionskip"})));
    let reply = |result| Reply {
        route: req.route,
        purpose: req.purpose.clone(),
        result,
    };
    let ok = reply(Ok(
        json!({"status": "ok", "via": "pilot", "action": "mission_skip"}),
    ));
    app.tabs.vehicle.on_reply(&mut app.core, ok);
    let refused = reply(Ok(
        json!({"status": "error", "message": "pilot refused mission_skip: no_fc"}),
    ));
    app.tabs.vehicle.on_reply(&mut app.core, refused);
    let texts: Vec<String> = app.core.toasts.history().map(|t| t.text.clone()).collect();
    assert!(
        texts.iter().any(|t| t == "Skip waypoint: Accepted"),
        "{texts:?}"
    );
    // The pilot's refusal code reads through the refusal table.
    assert!(
        texts
            .iter()
            .any(|t| t == "Skip waypoint: No flight controller"),
        "{texts:?}"
    );
}

mod confined;
mod liftoff;
