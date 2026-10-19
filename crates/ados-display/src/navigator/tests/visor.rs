//! The data-driven tab list on a visor, the plugin-owned buttons, the power-off
//! prompt, and the navigator-painted tab bar.

use super::*;
use crate::graphics::palette::DARK;
use crate::pages::power::SHUTDOWN_PATH;
use crate::pages::AgentRequest;
use crate::panel_conf::{resolve_screens, tab_ids};
use ados_protocol::panel_ui::{PASSTHROUGH, SHUTDOWN_PROMPT};

const VISOR_TABS: [&str; 5] = ["plugin", "cameras", "sensors", "storage", "details.about"];

fn visor_nav(dir: &std::path::Path) -> PageNavigator {
    let tabs = tab_ids("visor", &resolve_screens("visor", None));
    PageNavigator::with_state_path(
        all_pages(&crate::button_source::ButtonLayout::default()),
        &tabs,
        dir.join("lcd-state.json"),
    )
}

#[test]
fn visor_tabs_come_from_the_default_screens() {
    let dir = tempfile::tempdir().unwrap();
    let n = visor_nav(dir.path());
    assert_eq!(n.tabs(), VISOR_TABS);
    // The first screen is the default route.
    assert_eq!(n.default_page_id(), "plugin");
    assert_eq!(n.active_page_id(), "plugin");
}

#[test]
fn cycle_screen_walks_the_visor_tabs_and_wraps() {
    let dir = tempfile::tempdir().unwrap();
    let mut n = visor_nav(dir.path());
    for next in VISOR_TABS[1..].iter().chain([&"plugin"]) {
        assert_eq!(
            n.on_button(&button("cycle_screen")),
            Dispatch::RouteChanged(next.to_string())
        );
        assert_eq!(n.current_page_id(), *next);
    }
}

#[test]
fn passthrough_is_inert_on_every_page() {
    let dir = tempfile::tempdir().unwrap();
    let mut n = visor_nav(dir.path());
    let ids: Vec<&'static str> = n.known_page_ids().to_vec();
    for id in ids {
        n.pop_all_modals();
        if id.starts_with("details.") && !n.tabs().contains(&id) {
            n.go(n.default_page_id());
            assert!(n.push_modal(id));
        } else {
            n.go(id);
        }
        assert_eq!(n.on_button(&button(PASSTHROUGH)), Dispatch::None, "{id}");
        assert_eq!(
            n.current_page_id(),
            id,
            "{id}: a passthrough moved the panel"
        );
    }
}

#[test]
fn shutdown_prompt_asks_then_powers_off() {
    let dir = tempfile::tempdir().unwrap();
    let mut n = visor_nav(dir.path());
    let ctx = PageContext::default();
    assert_eq!(
        n.on_button(&button(SHUTDOWN_PROMPT)),
        Dispatch::ModalChanged("details.power".to_string())
    );
    assert_eq!(n.current_page_id(), "details.power");
    // A second press on the open prompt is the shutdown write.
    let Dispatch::Custom(key) = n.on_button(&button(SHUTDOWN_PROMPT)) else {
        panic!("the confirming press must resolve to the power page's key");
    };
    let Some(PanelAction::Agent(AgentRequest { method, path, .. })) = n.custom_action(&key, &ctx)
    else {
        panic!("the confirming press must reach the agent");
    };
    assert_eq!((method, path), ("POST", SHUTDOWN_PATH));
    // Back cancels, landing where the prompt opened.
    assert_eq!(
        n.on_button(&button("back")),
        Dispatch::ModalChanged("plugin".to_string())
    );
    assert!(n.modal_stack().is_empty());
}

#[test]
fn power_prompt_cancel_target_pops_it() {
    let dir = tempfile::tempdir().unwrap();
    let mut n = visor_nav(dir.path());
    let ctx = PageContext::default();
    n.go("cameras");
    n.on_button(&button(SHUTDOWN_PROMPT));
    let cancel = n
        .current_page()
        .hit_zones(&ctx)
        .into_iter()
        .find(|z| z.action == HitAction::Back)
        .expect("the prompt paints Cancel");
    let d = n.on_touch(
        &ctx,
        &tap(cancel.x + cancel.w / 2, cancel.y + cancel.h / 2),
        1000,
    );
    assert_eq!(d, Dispatch::ModalChanged("cameras".to_string()));
}

#[test]
fn tab_taps_route_across_five_visor_tabs() {
    let dir = tempfile::tempdir().unwrap();
    let mut n = visor_nav(dir.path());
    let ctx = PageContext::default();
    // The bar shows on the tabbed visor pages; each band is 96 px wide.
    for (x, want) in [(10, "plugin"), (300, "storage"), (470, "details.about")] {
        n.go("cameras");
        let d = n.on_touch(&ctx, &tap(x, 300), 1000);
        assert_eq!(d, Dispatch::RouteChanged(want.to_string()), "x {x}");
    }
    n.go("sensors");
    let d = n.on_touch(&ctx, &tap(120, 300), 2000);
    assert_eq!(d, Dispatch::RouteChanged("cameras".to_string()));
}

#[test]
fn full_screen_tabs_leave_by_back_to_the_first_tab() {
    let dir = tempfile::tempdir().unwrap();
    let mut n = visor_nav(dir.path());
    let ctx = PageContext::default();
    // The about page keeps its own back chevron; on a visor it is a tab with
    // no bar, so Back returns to the first tab.
    n.go("details.about");
    let d = n.on_touch(&ctx, &tap(28, 24), 1000);
    assert_eq!(d, Dispatch::RouteChanged("plugin".to_string()));
    // On the first tab itself Back has nowhere to go, and the plugin page's
    // back zone is the same Back.
    assert_eq!(n.on_button(&button("back")), Dispatch::None);
    assert_eq!(n.on_touch(&ctx, &tap(28, 24), 1100), Dispatch::None);
    // A tabbed root stays put, as on the ground station.
    n.go("storage");
    assert_eq!(n.on_button(&button("back")), Dispatch::None);
}

/// The navigator paints the bar over a tabbed page with the accent under the
/// visible tab, for the visor's five tabs and the ground station's.
#[test]
fn navigator_paints_the_bar_with_the_accent_under_the_active_tab() {
    let dir = tempfile::tempdir().unwrap();
    let accent_cells = |n: &PageNavigator| -> Vec<bool> {
        let c = n.render_active(&PageContext::default(), &DARK, 0);
        let w = tab_width(n.tabs().len());
        (0..n.tabs().len() as i32)
            .map(|i| c.pixel(i * w + 4, TAB_BAR_TOP_Y + 1) == DARK.accent_primary)
            .collect()
    };
    let mut n = visor_nav(dir.path());
    n.go("cameras");
    assert_eq!(accent_cells(&n), [false, true, false, false, false]);
    n.go("storage");
    assert_eq!(accent_cells(&n), [false, false, false, true, false]);

    let mut g = nav(dir.path());
    g.go("settings");
    assert_eq!(accent_cells(&g), [false, false, true, false, false]);
    // The bar background spans the strip below the content region.
    let c = g.render_active(&PageContext::default(), &DARK, 0);
    assert_eq!(c.pixel(2, PANEL_H as i32 - 2), DARK.bg_secondary);
}

#[test]
fn set_tabs_moves_off_a_dropped_tab() {
    let dir = tempfile::tempdir().unwrap();
    let mut n = nav(dir.path());
    assert_eq!(n.active_page_id(), "dashboard");
    assert!(n.set_tabs(&VISOR_TABS));
    assert_eq!(n.tabs(), VISOR_TABS);
    assert_eq!(n.active_page_id(), "plugin");
    // Unregistered ids are dropped; nothing usable keeps the GS tabs.
    n.set_tabs(&["ghost", "storage"]);
    assert_eq!(n.tabs(), ["storage"]);
    n.set_tabs(&["ghost"]);
    assert_eq!(n.tabs(), GS_TAB_IDS);
}
