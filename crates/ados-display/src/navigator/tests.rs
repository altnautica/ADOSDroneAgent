use super::*;
use crate::pages::PANEL_W;
use crate::pages::{
    about::AboutDetailPage, access_point::AccessPointDetailPage, channel_hops::ChannelHopsPage,
    dashboard::DashboardPage, diagnostics::DiagnosticsDetailPage, drone::DroneDetailPage,
    link_stats::LinkStatsPage, mesh::MeshDetailPage, more::MorePage,
    pair_drone::PairDroneDetailPage, plugin::PluginPage, radio_link::RadioLinkDetailPage,
    settings::SettingsPage, uplink::UplinkDetailPage, video::VideoPage,
};
use ados_hid::buttons::PressKind;
use ados_hid::touch::{TouchGesture, TouchMove};

/// Build the full set of registered pages the LCD service ships, matching
/// the Python bootstrap registration plus the detail pages the dashboard /
/// more page drill into.
fn all_pages() -> Vec<Box<dyn Page>> {
    vec![
        Box::new(DashboardPage),
        Box::new(VideoPage),
        Box::new(SettingsPage),
        Box::new(LinkStatsPage),
        Box::new(ChannelHopsPage),
        Box::new(MorePage::default()),
        Box::new(RadioLinkDetailPage::new()),
        Box::new(UplinkDetailPage),
        Box::new(DroneDetailPage),
        Box::new(MeshDetailPage::new()),
        Box::new(AboutDetailPage),
        // How to join this ground station's own WiFi. A panel is the only
        // surface that helps when the box is the thing you cannot reach.
        Box::new(AccessPointDetailPage),
        Box::new(PairDroneDetailPage::default()),
        Box::new(DiagnosticsDetailPage),
        Box::new(PluginPage::new()),
    ]
}

fn nav(dir: &std::path::Path) -> PageNavigator {
    PageNavigator::with_state_path(all_pages(), dir.join("lcd-state.json"))
}

fn tap(x: i32, y: i32) -> TouchGesture {
    TouchGesture {
        kind: GestureKind::Tap,
        start_x: x,
        start_y: y,
        end_x: x,
        end_y: y,
        start_t_ms: 0,
        end_t_ms: 50,
        duration_ms: 50,
        direction: None,
        velocity_px_per_s: 0.0,
        samples: vec![TouchMove {
            x_lcd: x,
            y_lcd: y,
            timestamp_ms: 0,
        }],
    }
}

fn button(action: &str) -> ButtonEvent {
    ButtonEvent {
        pin: 5,
        kind: PressKind::Short,
        action: Some(action.to_string()),
        timestamp_ms: 0,
    }
}

#[test]
fn defaults_to_dashboard_and_persists_on_first_boot() {
    let dir = tempfile::tempdir().unwrap();
    let n = nav(dir.path());
    assert_eq!(n.active_page_id(), "dashboard");
    // First-boot persist wrote the state file.
    let persisted = LcdState::load(&dir.path().join("lcd-state.json")).unwrap();
    assert_eq!(persisted.active_page_id.as_deref(), Some("dashboard"));
}

#[test]
fn restores_persisted_page_on_construction() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut n = nav(dir.path());
        assert!(n.go("video"));
    }
    // A fresh navigator over the same state path lands on video.
    let n2 = nav(dir.path());
    assert_eq!(n2.active_page_id(), "video");
}

#[test]
fn unknown_persisted_id_falls_back_to_default() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("lcd-state.json"),
        r#"{"active_page_id":"ghost"}"#,
    )
    .unwrap();
    let n = nav(dir.path());
    assert_eq!(n.active_page_id(), "dashboard");
}

#[test]
fn persisted_state_lists_every_registered_route() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lcd-state.json");
    // A stale file from an earlier build, with a modal the fresh navigator
    // does not show and no route list.
    std::fs::write(
        &path,
        r#"{"active_page_id":"video","modal_stack":["details.about"]}"#,
    )
    .unwrap();
    let n = nav(dir.path());
    let blob: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let routes: Vec<&str> = blob["route_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    let registered: Vec<&str> = all_pages().iter().map(|p| p.id()).collect();
    assert_eq!(routes, registered);
    assert!(routes.contains(&"plugin") && routes.contains(&"channel_hops"));
    // The restored page is kept; the modal it no longer shows is not.
    assert_eq!(n.active_page_id(), "video");
    assert_eq!(blob["active_page_id"], "video");
    assert_eq!(blob["modal_stack"], serde_json::json!([]));
}

#[test]
fn go_switches_tab_and_drops_modal() {
    let dir = tempfile::tempdir().unwrap();
    let mut n = nav(dir.path());
    assert!(n.push_modal("details.radio_link"));
    assert_eq!(n.modal_stack().len(), 1);
    // Switching tabs clears the modal stack.
    assert!(n.go("settings"));
    assert!(n.modal_stack().is_empty());
    assert_eq!(n.active_page_id(), "settings");
    // Going to the same tab with no modal is a no-op.
    assert!(!n.go("settings"));
}

#[test]
fn go_unknown_page_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let mut n = nav(dir.path());
    assert!(!n.go("nope"));
    assert_eq!(n.active_page_id(), "dashboard");
}

#[test]
fn current_page_resolves_topmost_modal() {
    let dir = tempfile::tempdir().unwrap();
    let mut n = nav(dir.path());
    assert_eq!(n.current_page().id(), "dashboard");
    n.push_modal("details.drone");
    assert_eq!(n.current_page().id(), "details.drone");
    n.push_modal("details.about");
    assert_eq!(n.current_page().id(), "details.about");
    assert_eq!(n.current_page_id(), "details.about");
    n.pop_modal();
    assert_eq!(n.current_page().id(), "details.drone");
}

#[test]
fn tab_tap_routes_by_x_band() {
    let dir = tempfile::tempdir().unwrap();
    let mut n = nav(dir.path());
    let ctx = PageContext::default();
    // x in band 1 (96..192) -> video.
    let d = n.on_touch(&ctx, &tap(120, 300), 1000);
    assert_eq!(d, Dispatch::RouteChanged("video".to_string()));
    assert_eq!(n.active_page_id(), "video");
    // x in band 2 -> settings.
    let d = n.on_touch(&ctx, &tap(200, 305), 1100);
    assert_eq!(d, Dispatch::RouteChanged("settings".to_string()));
    // far-right band clamps to channel_hops (index 4) even past 480.
    let d = n.on_touch(&ctx, &tap(470, 300), 1200);
    assert_eq!(d, Dispatch::RouteChanged("channel_hops".to_string()));
    // The tapped tab pulses for the linger window, then stops.
    assert_eq!(n.tab_pulse(1200), Some("channel_hops"));
    assert_eq!(
        n.tab_pulse(1200 + TAP_FEEDBACK_LINGER_MS - 1),
        Some("channel_hops")
    );
    assert_eq!(n.tab_pulse(1200 + TAP_FEEDBACK_LINGER_MS), None);
}

#[test]
fn bottom_strip_tap_does_not_escape_a_full_screen_page() {
    let dir = tempfile::tempdir().unwrap();
    let mut n = nav(dir.path());
    let ctx = PageContext::default();
    n.push_modal("details.radio_link");
    // A detail page is full-screen: the bottom strip is page area, so a tap
    // there must not switch tabs or pop the modal.
    let d = n.on_touch(&ctx, &tap(10, 300), 1000);
    assert_eq!(d, Dispatch::None);
    assert_eq!(n.current_page_id(), "details.radio_link");
}

#[test]
fn dashboard_tile_tap_opens_detail() {
    let dir = tempfile::tempdir().unwrap();
    let mut n = nav(dir.path());
    let ctx = PageContext::default();
    // The top-left dashboard tile sits at page-local (8,8); add the 32 px
    // chrome offset to hit it in panel coordinates.
    let d = n.on_touch(&ctx, &tap(8 + 4, 32 + 8 + 4), 1000);
    assert_eq!(d, Dispatch::ModalChanged("details.radio_link".to_string()));
    assert_eq!(n.current_page().id(), "details.radio_link");
}

#[test]
fn back_zone_pops_the_modal() {
    let dir = tempfile::tempdir().unwrap();
    let mut n = nav(dir.path());
    let ctx = PageContext::default();
    n.push_modal("details.radio_link");
    // A detail page is full-screen: the Back chevron is painted at panel
    // (8,8,40,32), so its centre is (28,24).
    let d = n.on_touch(&ctx, &tap(28, 24), 1000);
    assert_eq!(d, Dispatch::ModalChanged("dashboard".to_string()));
    assert!(n.modal_stack().is_empty());
}

#[test]
fn a_full_screen_page_owns_the_bottom_strip() {
    let dir = tempfile::tempdir().unwrap();
    let mut n = nav(dir.path());
    let mut ctx = PageContext::default();
    ctx.paired_drone.device_id = Some("drone-a".to_string());
    n.push_modal("details.pair_drone");
    // The Unpair button sits in the bottom strip; a tap on it is the button,
    // not a tab switch.
    let zone = n
        .current_page()
        .hit_zones(&ctx)
        .into_iter()
        .find(|z| z.action == HitAction::Custom("pair.unpair".to_string()))
        .expect("paired page paints Unpair");
    let d = n.on_touch(&ctx, &tap(zone.x + zone.w / 2, zone.y + zone.h / 2), 1000);
    assert_eq!(d, Dispatch::Custom("pair.unpair".to_string()));
    assert_eq!(n.current_page_id(), "details.pair_drone");
}

#[test]
fn more_rows_open_the_row_that_is_painted() {
    let dir = tempfile::tempdir().unwrap();
    let mut n = nav(dir.path());
    let ctx = PageContext::default();
    n.go("more");
    // Row 1 (Diagnostics) is painted at panel y 48..96; its middle is 72.
    let d = n.on_touch(&ctx, &tap(200, 72), 1000);
    assert_eq!(d, Dispatch::ModalChanged("details.diagnostics".to_string()));
}

#[test]
fn back_from_a_non_tab_route_returns_to_the_dashboard() {
    let dir = tempfile::tempdir().unwrap();
    let mut n = nav(dir.path());
    let ctx = PageContext::default();
    n.go("plugin");
    let d = n.on_touch(&ctx, &tap(28, 24), 1000);
    assert_eq!(d, Dispatch::RouteChanged("dashboard".to_string()));
}

/// Every zone of every registered page is tapped at its centre, in panel
/// coordinates, and must resolve to itself; and every zone must cover
/// painted pixels, so a zone cannot drift away from the control it names.
#[test]
fn every_zone_resolves_where_its_control_is_painted() {
    use crate::graphics::palette::DARK;
    let dir = tempfile::tempdir().unwrap();
    let mut n = nav(dir.path());
    let mut ctx = PageContext::default();
    ctx.paired_drone.device_id = Some("drone-a".to_string());
    ctx.link.tx_power_dbm = Some(10);
    ctx.link.state = Some("connected".to_string());
    let ids: Vec<&'static str> = n.known_page_ids().to_vec();
    for id in ids {
        n.pop_all_modals();
        if id.starts_with("details.") {
            n.go(DEFAULT_PAGE_ID);
            assert!(n.push_modal(id));
        } else {
            n.go(id);
        }
        let page = n.current_page();
        assert_eq!(page.id(), id);
        let origin = page.chrome().origin_y();
        let canvas = page.render(&ctx, &DARK);
        for z in page.hit_zones(&ctx) {
            let (cx, cy) = (z.x + z.w / 2, z.y + z.h / 2 + origin);
            assert_eq!(
                n.zone_at(&ctx, cx, cy),
                Some(z.action.clone()),
                "{id}: centre of {:?} resolves elsewhere",
                z.action
            );
            let painted = (z.y + origin..z.y + origin + z.h).any(|y| {
                (z.x..z.x + z.w).any(|x| {
                    x >= 0
                        && y >= 0
                        && (x as u32) < canvas.width()
                        && (y as u32) < canvas.height()
                        && canvas.pixel(x, y) != DARK.bg_primary
                })
            });
            assert!(painted, "{id}: zone {:?} covers nothing painted", z.action);
        }
    }
}

#[test]
fn custom_zone_returns_the_key() {
    let dir = tempfile::tempdir().unwrap();
    let mut n = nav(dir.path());
    let mut ctx = PageContext::default();
    ctx.link.tx_power_dbm = Some(10);
    n.push_modal("details.radio_link");
    let zone = n
        .current_page()
        .hit_zones(&ctx)
        .into_iter()
        .find(|z| matches!(z.action, HitAction::Custom(_)))
        .expect("the radio page has a stepper");
    let HitAction::Custom(key) = zone.action.clone() else {
        unreachable!()
    };
    let d = n.on_touch(&ctx, &tap(zone.x + zone.w / 2, zone.y + zone.h / 2), 1000);
    assert_eq!(d, Dispatch::Custom(key.clone()));
    assert!(matches!(
        n.custom_action(&key, &ctx),
        Some(PanelAction::Agent(_))
    ));
}

#[test]
fn tap_in_top_bar_is_inert() {
    let dir = tempfile::tempdir().unwrap();
    let mut n = nav(dir.path());
    let ctx = PageContext::default();
    // y < 32 is the top status bar, no page zones live there.
    assert_eq!(n.on_touch(&ctx, &tap(100, 10), 1000), Dispatch::None);
}

#[test]
fn button_back_pops_modal_then_is_inert() {
    let dir = tempfile::tempdir().unwrap();
    let mut n = nav(dir.path());
    n.push_modal("details.uplink");
    let d = n.on_button(&button("back"));
    assert_eq!(d, Dispatch::ModalChanged("dashboard".to_string()));
    // On the tab root, back is inert.
    assert_eq!(n.on_button(&button("back")), Dispatch::None);
}

#[test]
fn button_cycle_screen_advances_tabs_wrapping() {
    let dir = tempfile::tempdir().unwrap();
    let mut n = nav(dir.path());
    assert_eq!(
        n.on_button(&button("cycle_screen")),
        Dispatch::RouteChanged("video".to_string())
    );
    n.go("channel_hops"); // last tab
    assert_eq!(
        n.on_button(&button("cycle_screen")),
        Dispatch::RouteChanged("dashboard".to_string())
    );
}

#[test]
fn button_menu_pairing_network_qr_route() {
    let dir = tempfile::tempdir().unwrap();
    let mut n = nav(dir.path());
    // menu -> more overflow page (present).
    assert_eq!(
        n.on_button(&button("menu")),
        Dispatch::RouteChanged("more".to_string())
    );
    assert_eq!(
        n.on_button(&button("pair_drone")),
        Dispatch::ModalChanged("details.pair_drone".to_string())
    );
    n.pop_all_modals();
    assert_eq!(
        n.on_button(&button("show_network")),
        Dispatch::ModalChanged("details.uplink".to_string())
    );
    n.pop_all_modals();
    assert_eq!(
        n.on_button(&button("show_qr")),
        Dispatch::ModalChanged("details.pair_drone".to_string())
    );
}

#[test]
fn button_confirm_and_unmapped_surface_as_custom() {
    let dir = tempfile::tempdir().unwrap();
    let mut n = nav(dir.path());
    assert_eq!(
        n.on_button(&button("confirm")),
        Dispatch::Custom("confirm".to_string())
    );
    assert_eq!(
        n.on_button(&button("toggle_backlight")),
        Dispatch::Custom("toggle_backlight".to_string())
    );
    // No action on the event is inert.
    let mut ev = button("back");
    ev.action = None;
    assert_eq!(n.on_button(&ev), Dispatch::None);
}

#[test]
fn drain_page_request_applies_and_pops_modals() {
    let dir = tempfile::tempdir().unwrap();
    let mut n = nav(dir.path());
    n.push_modal("details.drone");
    let req = dir.path().join("lcd-page-request.json");
    std::fs::write(&req, r#"{"page":"video"}"#).unwrap();
    let applied = n.drain_page_request_from(&req);
    assert_eq!(applied.as_deref(), Some("video"));
    assert_eq!(n.active_page_id(), "video");
    assert!(n.modal_stack().is_empty());
    // The request file is consumed.
    assert!(!req.exists());
}

#[test]
fn routes_to_the_reserved_plugin_page_and_renders_it() {
    let dir = tempfile::tempdir().unwrap();
    let mut n = nav(dir.path());
    // The reserved page is registered (a non-tab route).
    assert!(n.has("plugin"));
    assert!(!TAB_PAGE_IDS.contains(&"plugin"));

    // A page-switch request for the reserved id routes to it, exactly the
    // path a remote page request takes.
    let req = dir.path().join("lcd-page-request.json");
    std::fs::write(&req, r#"{"page":"plugin"}"#).unwrap();
    assert_eq!(n.drain_page_request_from(&req).as_deref(), Some("plugin"));
    assert_eq!(n.active_page_id(), "plugin");
    assert_eq!(n.current_page().id(), "plugin");

    // It paints a full panel like every other page.
    let c = n.render_active(&PageContext::default(), &crate::graphics::palette::DARK, 0);
    assert_eq!(c.width(), PANEL_W);
    assert_eq!(c.height(), PANEL_H);

    // A direct go() to the reserved page works too.
    assert!(n.go("dashboard"));
    assert!(n.go("plugin"));
    assert_eq!(n.active_page_id(), "plugin");
}

#[test]
fn drain_page_request_ignores_unknown_page() {
    let dir = tempfile::tempdir().unwrap();
    let mut n = nav(dir.path());
    let req = dir.path().join("lcd-page-request.json");
    std::fs::write(&req, r#"{"page":"ghost"}"#).unwrap();
    assert!(n.drain_page_request_from(&req).is_none());
    assert_eq!(n.active_page_id(), "dashboard");
    assert!(!req.exists());
}

#[test]
fn render_active_paints_full_panel() {
    let dir = tempfile::tempdir().unwrap();
    let mut n = nav(dir.path());
    let ctx = PageContext::default();
    let c = n.render_active(&ctx, &crate::graphics::palette::DARK, 0);
    assert_eq!(c.width(), PANEL_W);
    assert_eq!(c.height(), PANEL_H);
    // After a drill-in the modal paints instead.
    n.push_modal("details.about");
    let c2 = n.render_active(&ctx, &crate::graphics::palette::DARK, 0);
    assert_eq!(c2.width(), PANEL_W);
}

/// A tab tap paints that tab pressed in the next frame, and only until the
/// linger window closes.
#[test]
fn a_tapped_tab_is_painted_pressed_then_released() {
    let dir = tempfile::tempdir().unwrap();
    let mut n = nav(dir.path());
    let ctx = PageContext::default();
    let palette = &crate::graphics::palette::DARK;
    n.on_touch(&ctx, &tap(120, 300), 1000);
    let pressed = n.render_active(&ctx, palette, 1000);
    let released = n.render_active(&ctx, palette, 1000 + TAP_FEEDBACK_LINGER_MS);
    // The video tab's cell (band 1) differs between the two frames; the
    // content above the bar does not.
    let cell_differs = (TAB_BAR_TOP_Y + 2..PANEL_H as i32).any(|y| {
        (TAB_WIDTH + 2..2 * TAB_WIDTH - 2).any(|x| pressed.pixel(x, y) != released.pixel(x, y))
    });
    assert!(cell_differs, "the tapped tab must be painted pressed");
    let content_same = (40..TAB_BAR_TOP_Y - 1)
        .all(|y| (0..PANEL_W as i32).all(|x| pressed.pixel(x, y) == released.pixel(x, y)));
    assert!(content_same, "only the tab cell changes");
}

#[test]
fn tab_geometry_matches_the_chrome() {
    // Five 96 px tabs filling the 480 px panel.
    assert_eq!(TAB_WIDTH, 96);
    assert_eq!(TAB_COUNT, 5);
    assert_eq!(TAB_BAR_TOP_Y, 276);
    assert_eq!(TAB_PAGE_IDS.len(), TAB_COUNT);
}
