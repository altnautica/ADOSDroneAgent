//! Render tests of the Extensions tab (visible text only).

use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::app::{App, Slot, Tab, TabId};
use crate::model::ProfileRoutes;
use crate::poll::ResourceId;
use crate::ui::tests::{draw, feed, test_app};

const ID: &str = "com.example.follow";

fn put(app: &mut App, id: ResourceId, value: Value) {
    app.core.res.insert(
        id,
        Slot {
            value: Some(value),
            error: None,
            code: None,
            error_body: None,
            at: Instant::now(),
        },
    );
}

fn fixture() -> App {
    let mut app = test_app();
    feed(
        &mut app,
        json!({"version": "0.99.108", "device_name": "bench", "profile": "drone", "paired": true}),
        ProfileRoutes::default(),
        Duration::ZERO,
    );
    put(
        &mut app,
        ResourceId::Plugins,
        json!({"installs": [{
            "plugin_id": ID, "version": "0.2.10", "status": "running", "signer_id": "example",
            "failure_reason": null, "revocation_pending": false,
            "permissions": {"mavlink.read": {"granted": true, "granted_at": 1}}
        }]}),
    );
    put(
        &mut app,
        ResourceId::PluginsCatalog,
        json!({"plugins": [
            {"id": ID, "name": "Follow", "version": "0.2.10", "risk": "high",
             "download_url": "https://example.com/f.adosplug", "archive_sha256": "aa"},
            {"id": "com.example.gimbal", "name": "Gimbal Controller", "version": "1.4.1", "risk": "medium",
             "download_url": "https://example.com/g.adosplug", "archive_sha256": "bb"}
        ]}),
    );
    put(
        &mut app,
        ResourceId::PluginDetail(ID.into()),
        json!({
            "install": {},
            "manifest": {
                "id": ID, "version": "0.2.10", "name": "Follow Subject", "risk": "high",
                "permissions": [{"id": "mavlink.read", "required": true, "label": "Read MAVLink"}],
                "mcp": {"tools": [{"name": "follow.start", "safety_class": "flight"}], "resources": [], "prompts": []},
                "gcs": {"contributes": {
                    "parameters": [
                        {"key": "distance_m", "schema": {"type": "number", "minimum": 3, "maximum": 30, "default": 8},
                         "ui": {"label": "Follow distance", "group": "Follow"}}
                    ],
                    "settings": []
                }}
            }
        }),
    );
    put(&mut app, ResourceId::PluginConfig(ID.into()), json!({"plugin_id": ID, "values": {"distance_m": 12}}));
    put(
        &mut app,
        ResourceId::PluginReadiness(ID.into()),
        json!({"ok": true, "plugin_id": ID, "services": [{"name": "tracker", "ready": true, "reason": null}]}),
    );
    put(
        &mut app,
        ResourceId::PluginState(ID.into()),
        json!({"follow.state": {"payload": {"active": true}, "ts_ms": 5}}),
    );
    app.activate_tab(TabId::Extensions);
    app
}

#[test]
fn tab_renders_list_detail_and_config_at_120x40() {
    let mut app = fixture();
    let screen = draw(&mut app, 120, 40);
    assert!(screen.contains("INSTALLED (1)"), "{screen}");
    assert!(screen.contains("Follow Subject"), "{screen}");
    assert!(screen.contains("CATALOG (1)"), "{screen}");
    assert!(screen.contains("Gimbal Controller"), "{screen}");
    assert!(screen.contains("Permissions"), "{screen}");
    assert!(screen.contains("mavlink.read"), "{screen}");
    assert!(screen.contains("follow.start"), "{screen}");
    assert!(screen.contains("tracker"), "{screen}");
    assert!(screen.contains("follow.state"), "{screen}");
    assert!(screen.contains("Follow distance"), "{screen}");
    assert!(screen.contains("12"), "{screen}");
}

#[test]
fn catalog_entry_shows_its_card() {
    let mut app = fixture();
    app.command(crate::keymap::Command::Down);
    let screen = draw(&mut app, 120, 40);
    assert!(screen.contains("com.example.gimbal"), "{screen}");
    assert!(screen.contains("Enter installs"), "{screen}");
}

#[test]
fn a_list_refresh_keeps_the_selection_and_detail_scroll() {
    let mut app = fixture();
    app.tabs.extensions.on_resource(&mut app.core, &ResourceId::Plugins);
    app.tabs.extensions.detail_scroll = 4;

    // A periodic refresh that adds an install sorting ahead of the selected one.
    let installs = json!({"installs": [
        {"plugin_id": "com.example.aaa", "version": "1.0.0", "status": "running", "permissions": {}},
        {"plugin_id": ID, "version": "0.2.10", "status": "running", "permissions": {}}
    ]});
    put(&mut app, ResourceId::Plugins, installs.clone());
    app.tabs.extensions.on_resource(&mut app.core, &ResourceId::Plugins);
    let tab = &app.tabs.extensions;
    assert_eq!(tab.sel_key.as_ref().map(|(_, id)| id.as_str()), Some(ID));
    assert_eq!(tab.selected_install(&app.core).map(|i| i.id).as_deref(), Some(ID));
    assert_eq!(tab.detail_scroll, 4, "a refresh is not a selection change");

    put(&mut app, ResourceId::Plugins, installs);
    app.tabs.extensions.on_resource(&mut app.core, &ResourceId::PluginsCatalog);
    assert_eq!(app.tabs.extensions.detail_scroll, 4);

    // Moving to another plugin is a real change.
    app.command(crate::keymap::Command::Up);
    assert_eq!(app.tabs.extensions.detail_scroll, 0);
}
