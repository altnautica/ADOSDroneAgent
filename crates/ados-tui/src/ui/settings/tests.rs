//! Settings tab tests: sidebar grouping and profile gates (rendered text),
//! the ←/→ write helper, live preference changes, confirmation rows, the
//! filter and the All settings browser.

use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::rowpage::{cycle_write, filtered};
use crate::app::{App, Slot, TabId};
use crate::keymap::Command;
use crate::model::ProfileRoutes;
use crate::overlay::Overlay;
use crate::poll::ResourceId;
use crate::prefs::PrefsStore;
use crate::settings::row::EditRow;
use crate::settings::rows_view::Item;
use crate::settings::schema::Kind;
use crate::theme::ThemeId;
use crate::ui::tests::{draw, feed, test_app};

fn profile_app(profile: &str) -> App {
    let mut app = test_app();
    let status = json!({
        "version": "0.99.108", "device_name": "ados-x", "profile": profile,
        "paired": true, "mavlink": {"connected": false}
    });
    feed(&mut app, status, ProfileRoutes::default(), Duration::ZERO);
    app.activate_tab(TabId::Settings);
    app
}

fn put(app: &mut App, id: ResourceId, v: Value) {
    app.core.res.insert(
        id,
        Slot {
            value: Some(v),
            error: None,
            code: None,
            error_body: None,
            at: Instant::now(),
        },
    );
}

fn schema() -> Value {
    json!({
        "properties": {
            "agent": {"properties": {"name": {"type": "string", "title": "Name", "default": "my-drone"}}},
            "cameras": {"type": "array", "default": []},
            "discovery": {"properties": {"mdns_enabled": {"type": "boolean", "default": true}}},
            "mavlink": {"properties": {
                "source": {"type": "string", "enum": ["auto", "serial", "udp", "tcp"], "default": "auto"},
                "serial_port": {"type": "string", "default": ""},
                "baud_rate": {"type": "integer", "minimum": 0, "default": 57600},
                "system_id": {"type": "integer", "default": 1},
                "component_id": {"type": "integer", "default": 191}
            }}
        }
    })
}

fn with_config(app: &mut App) {
    put(app, ResourceId::ConfigSchema, schema());
    put(
        app,
        ResourceId::Config,
        json!({
            "agent": {"name": "bench"},
            "cameras": [{"id": "front"}],
            "discovery": {"mdns_enabled": true},
            "mavlink": {"source": "auto", "serial_port": "/dev/ttyACM0", "baud_rate": 115200, "system_id": 1, "component_id": 191}
        }),
    );
}

/// The sidebar column of every body line.
fn sidebar_text(frame: &str) -> String {
    frame
        .lines()
        .skip(3)
        .map(|l| l.chars().skip(1).take(22).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn sidebar_groups_render_in_order() {
    let mut app = profile_app("drone");
    let side = sidebar_text(&draw(&mut app, 120, 40));
    let groups = [
        "THIS TERMINAL",
        "IDENTITY",
        "LINK & NETWORK",
        "VIDEO & VISION",
        "CLOUD & REMOTE",
        "ACCESS & SECURITY",
        "SYSTEM & SAFETY",
        "ALL SETTINGS",
    ];
    let mut last = 0;
    for g in groups {
        let at = side.find(g).unwrap_or_else(|| panic!("{g} missing:\n{side}"));
        assert!(at >= last, "{g} out of order:\n{side}");
        last = at;
    }
    assert!(side.contains("▸ Appearance"), "{side}");
}

#[test]
fn a_drone_and_a_ground_station_gate_their_pages() {
    let mut drone = profile_app("drone");
    let side = sidebar_text(&draw(&mut drone, 120, 40));
    for page in ["MAVLink", "Radio", "Camera", "Video link", "Vision", "Sensors", "Swarm", "Region", "Battery"] {
        assert!(side.contains(page), "drone lacks {page}:\n{side}");
    }
    for page in ["Capture", "Display"] {
        assert!(!side.contains(page), "drone shows {page}:\n{side}");
    }

    let mut gs = profile_app("ground_station");
    let side = sidebar_text(&draw(&mut gs, 120, 40));
    for page in ["Radio", "Region", "Display", "Wi-Fi", "All settings"] {
        assert!(side.contains(page), "ground station lacks {page}:\n{side}");
    }
    for page in ["MAVLink", "Camera", "Video link", "Vision", "Sensors", "Swarm", "Battery", "Capture"] {
        assert!(!side.contains(page), "ground station shows {page}:\n{side}");
    }
}

#[test]
fn the_narrow_sidebar_still_lists_pages() {
    let mut app = profile_app("drone");
    let frame = draw(&mut app, 80, 24);
    assert!(frame.contains("THIS TERMINAL"), "{frame}");
    assert!(frame.contains("Appearance"), "{frame}");
}

fn bool_row(value: bool) -> EditRow {
    EditRow {
        key: "discovery.mdns_enabled".into(),
        label: "mDNS".into(),
        help: String::new(),
        kind: Kind::Bool,
        value: Some(json!(value)),
        default: Some(json!(true)),
        secret: false,
        nullable: false,
        read_only: false,
    }
}

#[test]
fn left_right_on_a_bool_row_writes_the_toggled_bool() {
    let w = cycle_write(&bool_row(true), true).unwrap();
    assert_eq!((w.key.as_str(), &w.value), ("discovery.mdns_enabled", &json!(false)));
    let w = cycle_write(&bool_row(false), false).unwrap();
    assert_eq!((w.key.as_str(), &w.value), ("discovery.mdns_enabled", &json!(true)));

    let mut ro = bool_row(true);
    ro.read_only = true;
    assert!(cycle_write(&ro, true).is_none());

    let mut num = bool_row(true);
    num.kind = Kind::Int { min: None, max: None };
    num.value = Some(json!(3));
    assert!(cycle_write(&num, true).is_none(), "numbers edit with Enter, not ←/→");

    let mut e = bool_row(true);
    e.kind = Kind::Enum(vec![json!("a"), json!("b"), json!("c")]);
    e.value = Some(json!("c"));
    assert_eq!(cycle_write(&e, true).unwrap().value, json!("a"));
    assert_eq!(cycle_write(&e, false).unwrap().value, json!("b"));
}

#[test]
fn prefs_pages_apply_live_and_persist() {
    let dir = std::env::temp_dir().join(format!("ados-tui-settings-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("ados").join("tui.json");
    let mut app = test_app();
    app.core.store = PrefsStore::load(Some(path.clone()));
    app.activate_tab(TabId::Settings);
    app.tabs.settings.goto_page(&mut app.core, "appearance");

    // Theme (first row): amber → brand-dark.
    app.command(Command::Right);
    assert_eq!(app.core.store.prefs.theme, "brand-dark");
    let brand_dark = crate::theme::Theme::new(app.core.theme.tier, ThemeId::BrandDark, false);
    assert_eq!(app.core.theme.accent(), brand_dark.accent());

    // Symbols (third row): unicode → nerd.
    app.command(Command::Down);
    app.command(Command::Down);
    app.command(Command::Right);
    assert_eq!(app.core.store.prefs.symbols, "nerd");
    assert_eq!(app.core.sym.name, "nerd");

    // Motion (fourth row): auto → full.
    app.command(Command::Down);
    app.command(Command::Right);
    assert_eq!(app.core.store.prefs.motion, "full");
    assert_eq!(app.core.fx.level, crate::motion::resolve("full", app.core.ssh, app.core.tier));

    let saved = PrefsStore::load(Some(path));
    assert_eq!(saved.prefs.theme, "brand-dark");
    assert_eq!(saved.prefs.symbols, "nerd");
    assert_eq!(saved.prefs.motion, "full");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn behaviour_rows_render_with_their_values() {
    let mut app = profile_app("drone");
    app.tabs.settings.goto_page(&mut app.core, "behaviour");
    let frame = draw(&mut app, 120, 40);
    assert!(frame.contains("Refresh interval (ms)") && frame.contains("2000"), "{frame}");
    assert!(frame.contains("Mouse") && frame.contains("off"), "{frame}");
    assert!(frame.contains("Start tab") && frame.contains("overview"), "{frame}");
}

#[test]
fn a_confirm_row_asks_before_writing() {
    let mut app = profile_app("drone");
    with_config(&mut app);
    app.tabs.settings.goto_page(&mut app.core, "mavlink");
    app.command(Command::Right);
    match app.core.overlays.last() {
        Some(Overlay::Confirm(c)) => {
            assert!(c.title.contains("Flight-controller transport"), "{}", c.title);
            assert!(c.body[0].contains("serial"), "{:?}", c.body);
        }
        other => panic!("expected a confirm, got {other:?}"),
    }
}

#[test]
fn config_pages_show_schema_rows_with_curated_labels() {
    let mut app = profile_app("drone");
    with_config(&mut app);
    app.tabs.settings.goto_page(&mut app.core, "mavlink");
    let frame = draw(&mut app, 120, 40);
    assert!(frame.contains("Flight-controller transport"), "{frame}");
    assert!(frame.contains("Baud rate") && frame.contains("115200"), "{frame}");
    assert!(frame.contains("Agent identity"), "{frame}");
    assert!(frame.contains("How the agent reaches the flight controller"), "{frame}");
}

#[test]
fn the_filter_keeps_matching_rows_and_their_headers() {
    let items = vec![
        Item::Header("Transport".into()),
        Item::Row(bool_row(true)),
        Item::Header("Other".into()),
        Item::Row(EditRow {
            key: "x.y".into(),
            label: "Unrelated".into(),
            ..bool_row(false)
        }),
    ];
    let kept = filtered(items, "MDNS");
    assert_eq!(kept.len(), 2);
    assert!(matches!(&kept[0], Item::Header(h) if h == "Transport"));
}

#[test]
fn right_on_a_bool_config_row_submits_without_confirming() {
    let mut app = profile_app("drone");
    with_config(&mut app);
    app.tabs.settings.goto_key(&mut app.core, "discovery.mdns_enabled");
    app.command(Command::Right);
    assert!(app.core.overlays.is_empty(), "mDNS needs no confirmation");
    assert_eq!(
        app.tabs.settings.pending.get("all_settings/discovery.mdns_enabled").map(String::as_str),
        Some("Mdns enabled"),
        "the write is in flight under its flash key"
    );
}

#[test]
fn a_configurable_extension_gets_a_page_under_extensions() {
    let mut app = profile_app("ground_station");
    put(
        &mut app,
        ResourceId::Plugins,
        json!({"installs": [{"plugin_id": "com.example.a", "version": "1.0.0", "status": "running",
            "signer_id": "example", "failure_reason": null, "revocation_pending": false, "permissions": {}}]}),
    );
    put(
        &mut app,
        ResourceId::PluginDetail("com.example.a".into()),
        json!({"manifest": {"name": "Tracker", "gcs": {"contributes": {"parameters": [
            {"key": "mode", "schema": {"type": "string", "enum": ["auto", "manual"], "default": "auto"}}
        ]}}}}),
    );
    let side = sidebar_text(&draw(&mut app, 120, 40));
    let ext = side.find("EXTENSIONS").unwrap_or_else(|| panic!("{side}"));
    let tracker = side.find("Tracker").unwrap_or_else(|| panic!("{side}"));
    let all = side.find("ALL SETTINGS").unwrap_or_else(|| panic!("{side}"));
    assert!(ext < tracker && tracker < all, "{side}");
}

#[test]
fn all_settings_lists_sections_and_jumps_to_a_key() {
    let mut app = profile_app("drone");
    with_config(&mut app);
    app.tabs.settings.goto_key(&mut app.core, "mavlink.baud_rate");
    let frame = draw(&mut app, 120, 40);
    for s in ["agent", "cameras", "discovery", "mavlink"] {
        assert!(frame.contains(s), "{s} missing:\n{frame}");
    }
    assert!(frame.contains("baud_rate") && frame.contains("115200"), "{frame}");
    assert!(frame.contains("[{\"id\":\"front\"}]"), "{frame}");
    assert!(frame.contains("mavlink.baud_rate: Serial line speed"), "{frame}");

    let items = app.tabs.settings.palette_items(&app.core);
    assert!(items.iter().any(|i| i.label.starts_with("mavlink.baud_rate")));
    assert!(items.iter().any(|i| i.label == "Settings › All settings"));
}
