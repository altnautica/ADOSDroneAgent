//! The confined-place profile in the Pilot block, from the golden confined
//! frame: the place row with its Confined chip, the route-check report with
//! its tightest point, the confined-near cap on the chip's line, the route
//! banner beside the stall overlay, and the place key (`c`) that switches the
//! class through `/api/command` while the pilot is disarmed.

use serde_json::{json, Value};

use super::{app_with_frame, column_rows, command_body};
use crate::keymap::{Command, Ctx, PageKeys};
use crate::ui::tests::draw;

/// The golden confined frame, its banner slot moved to the far future.
fn confined_frame() -> Value {
    let mut frame: Value = serde_json::from_str(include_str!(
        "../../../../../../ados-control/tests/fixtures/pilot-frame-confined.json"
    ))
    .expect("confined frame fixture is JSON");
    frame["banner"]["until_ms"] = json!(u64::MAX / 2);
    frame
}

/// A frame on the ground: disarmed, landed, no stall.
fn on_ground(mut frame: Value) -> Value {
    frame["armed"] = json!(false);
    frame["landed"] = json!(true);
    let o = frame.as_object_mut().unwrap();
    o.remove("mission_stall");
    o.remove("banner");
    frame
}

fn open_frame() -> Value {
    serde_json::from_str(include_str!(
        "../../../../../../ados-control/tests/fixtures/pilot-frame.json"
    ))
    .expect("pilot frame fixture is JSON")
}

#[test]
fn the_golden_confined_frame_renders_place_route_check_and_cap() {
    let mut app = app_with_frame(1, confined_frame());
    let text = draw(&mut app, 160, 60);
    println!("{text}");
    let place = column_rows(&text, "Place     ", 1);
    assert_eq!(place[0], "Place     Confined", "{text}");
    let route = column_rows(&text, "Route     ", 3);
    assert_eq!(
        route[0], "Route     Route too tight: leg 3→4 has 0.52 m, needs 0.58 m",
        "{text}"
    );
    assert!(
        route[1].ends_with("At N 41.3 m, E -17.5 m, height 2.0 m"),
        "{text}"
    );
    assert!(
        text.contains("Near the clearance limit: 0.5 m/s max"),
        "the chip's line carries the cap\n{text}"
    );
    // The route banner says more than the overlay: it stays beside it.
    let banner = column_rows(&text, "Banner    ", 1);
    assert!(
        banner[0].ends_with("Route too tight at leg 3→4: 0.52 m of 0.58 m"),
        "{text}"
    );
    assert!(text.contains("Stall     "), "{text}");
}

#[test]
fn an_open_place_shows_open_and_no_route_check() {
    let mut app = app_with_frame(1, open_frame());
    let text = draw(&mut app, 160, 60);
    let place = column_rows(&text, "Place     ", 1);
    assert_eq!(place[0], "Place     Open", "{text}");
    assert!(!text.contains("Route     "), "{text}");
}

#[test]
fn on_the_ground_the_place_key_is_offered_and_sends_the_other_class() {
    for (frame, cmd, hint) in [
        (on_ground(confined_frame()), "placeclass_open", "c Open"),
        (
            on_ground(open_frame()),
            "placeclass_confined",
            "c Confined space",
        ),
    ] {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut app = app_with_frame(port, frame);
        let text = draw(&mut app, 160, 60);
        assert!(
            column_rows(&text, "Place     ", 1)[0].ends_with(hint),
            "{text}"
        );
        assert!(app.ctx_stack().contains(&Ctx::Page(PageKeys::Place)));
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
        let action = cmd.replace("placeclass_", "place_class:");
        assert_eq!(command_body(listener, &action), json!({"cmd": cmd}));
    }
}

#[test]
fn armed_the_place_key_is_neither_offered_nor_sent() {
    let mut app = app_with_frame(1, confined_frame());
    let text = draw(&mut app, 160, 60);
    assert!(
        !column_rows(&text, "Place     ", 1)[0].contains("c Open"),
        "{text}"
    );
    assert!(!app.ctx_stack().contains(&Ctx::Page(PageKeys::Place)));
    assert!(!app
        .tabs
        .vehicle
        .on_command(&mut app.core, Command::PlaceClass));
    // An unknown arm state may be airborne: no key either.
    let mut frame = on_ground(confined_frame());
    frame.as_object_mut().unwrap().remove("armed");
    let mut app = app_with_frame(1, frame);
    assert!(!app
        .tabs
        .vehicle
        .on_command(&mut app.core, Command::PlaceClass));
}
