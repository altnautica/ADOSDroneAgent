use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::{Pane, SystemTab};
use crate::app::{Slot, Tab, TabId};
use crate::keymap::Command;
use crate::model::ProfileRoutes;
use crate::poll::ResourceId;
use crate::ui::tests::{draw, feed, test_app};

fn slot(v: Value) -> Slot {
    Slot { value: Some(v), error: None, code: None, error_body: None, at: Instant::now() }
}

fn fixtures(app: &mut crate::app::App) {
    feed(
        app,
        json!({"version": "0.99.108", "device_name": "ados-x", "profile": "drone"}),
        ProfileRoutes::default(),
        Duration::ZERO,
    );
    let res = &mut app.core.res;
    res.insert(
        ResourceId::System,
        slot(json!({
            "cpu_percent": 12.3, "cpu_count": 4,
            "memory_total_mb": 4096, "memory_used_mb": 3072, "memory_percent": 75.0,
            "disk_total_gb": 32.0, "disk_used_gb": 8.0, "disk_percent": 25.0,
            "swap_total_mb": 0, "swap_percent": 0.0,
            "temperatures": {"cpu_thermal": 47.5, "gpu_thermal": 82.0}
        })),
    );
    res.insert(
        ResourceId::Services,
        slot(json!({"systemd_available": true, "services": [
            {"name": "ados-mavlink", "active": true, "state": "active", "sub_state": "running", "memory_mb": 21.4},
            {"name": "ados-video", "active": false, "state": "failed", "sub_state": "failed", "memory_mb": null}
        ]})),
    );
    res.insert(
        ResourceId::Peripherals,
        slot(json!({"peripherals": [
            {"id": "ados.gps", "display_name": "GNSS receiver", "connected": true, "transport": "usb"}
        ], "count": 1})),
    );
    res.insert(
        ResourceId::HardwareCheck,
        slot(json!({"items": [
            {"id": "fc", "label": "Flight controller", "required": true, "state": "missing",
             "detail": "No FC on USB", "fix_hint": "Plug the FC in over USB"},
            {"id": "cam", "label": "Camera", "required": true, "state": "ok", "detail": "CSI camera"}
        ]})),
    );
}

#[test]
fn render_system_120x40() {
    let mut app = test_app();
    fixtures(&mut app);
    app.activate_tab(TabId::System);
    let tab = &mut app.tabs.system;
    tab.on_resource(&mut app.core, &ResourceId::System);
    assert_eq!(tab.cpu.len(), 1);
    let text = draw(&mut app, 120, 40);
    for want in [
        "Resources",
        "CPU 12.3% · 4 cores",
        "Memory 3072/4096 MB",
        "Disk 8.0/32.0 GB",
        "cpu_thermal",
        "82.0 °C",
        "0.99.108",
        "ados-mavlink",
        "running",
        "21.4 MB",
        "failed",
        "GNSS receiver",
        "connected",
        "Flight controller",
        "No FC on USB",
        "restart the supervisor",
    ] {
        assert!(text.contains(want), "missing {want:?}\n{text}");
    }
}

#[test]
fn panes_cycle_and_selection_clamps() {
    let mut app = test_app();
    fixtures(&mut app);
    let mut tab = SystemTab::default();
    assert!(tab.on_command(&mut app.core, Command::Down));
    assert!(tab.on_command(&mut app.core, Command::Down));
    assert_eq!(tab.sel[Pane::Services as usize], 1, "clamped to the last service");
    tab.on_command(&mut app.core, Command::NextPane);
    tab.on_command(&mut app.core, Command::NextPane);
    assert_eq!(tab.pane, Pane::Hardware);
    assert_eq!(
        tab.context_line(&app.core).as_deref(),
        Some("Plug the FC in over USB")
    );
}

#[test]
fn armed_or_unknown_refuses_lifecycle_actions() {
    let mut app = test_app();
    fixtures(&mut app);
    let mut tab = SystemTab::default();
    // No arm state reported: the restart is refused before any dialog opens.
    tab.on_command(&mut app.core, Command::RestartService);
    tab.on_command(&mut app.core, Command::RestartSupervisor);
    tab.on_command(&mut app.core, Command::Reboot);
    assert!(app.core.overlays.is_empty());
    assert!(app.core.shell.is_none());
}
