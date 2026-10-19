//! Page navigation state machine and physical-input routing for the LCD UI.
//!
//! The navigator owns the active tab id and a modal/detail stack, holds the
//! registry of every [`crate::pages::Page`] the panel can show, resolves the
//! page the render loop paints each tick, and translates physical input into
//! page transitions. It is the single place that decides what is on screen.
//!
//! Routing
//! -------
//!
//! * Every page names its [`crate::pages::Chrome`]. A tabbed page paints the
//!   32 px top bar and its content; the navigator paints the 44 px bottom tab
//!   bar over it from the tab list it owns (1 to 5 page ids, the
//!   ground-station set by default, a visor's screens when the panel config
//!   names them). A tap in the bottom strip maps its x position to one of
//!   those tabs, and any other tap is shifted into the page's content-local
//!   frame (subtract the top bar). A full-screen page (detail pages, the
//!   overflow menu, the plugin page) paints the whole panel with no tab bar,
//!   so its zones are panel-global and the bottom strip is page area.
//! * The shifted tap is tested against the active page's
//!   [`crate::pages::HitZone`]s. The first zone that contains the point has its
//!   [`crate::pages::HitAction`] dispatched: `GoTab` switches tabs, `OpenDetail`
//!   pushes a detail page as a modal, `Back` pops the modal stack (or returns a
//!   full-screen or non-tab route to the first tab), and `Custom` is handed
//!   back to the caller as a [`Dispatch::Custom`], which resolves it through
//!   [`PageNavigator::custom_action`].
//! * The four front-panel buttons resolve their action through the
//!   [`ados_hid::buttons`] mapping; the navigator maps `back` to a modal pop,
//!   `cycle_screen` to the next tab, `shutdown_prompt` to the power-off
//!   confirmation, `passthrough` to nothing (a plugin owns that press), and the
//!   menu / pairing / network / qr actions to their target routes.
//!
//! Persistence
//! -----------
//!
//! The active page id is restored from `/run/ados/lcd-state.json` at
//! construction and re-persisted (atomic tmp-sibling write) on every route
//! change, so a service restart returns the operator to the page they were on.
//! A remote `POST /api/v1/display/page` drops `/run/ados/lcd-page-request.json`;
//! [`PageNavigator::drain_page_request`] reads and unlinks it each tick and
//! applies the switch.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use ados_hid::buttons::ButtonEvent;
use ados_hid::touch::{GestureKind, TouchGesture};

use crate::graphics::palette::Palette;
use crate::graphics::primitives::Canvas;
use crate::pages::power::{POWER_OFF_KEY, POWER_PAGE_ID};
use crate::pages::{
    tab_width, HitAction, Page, PageContext, PanelAction, BOTTOM_BAR_H, MAX_TABS, PANEL_H,
};
use crate::sidecar::{self, LcdState, LCD_PAGE_REQUEST_PATH, LCD_STATE_PATH};
use crate::widgets::{draw_bottom_bar, draw_tab_pulse};

/// How long the inverse-fill pulse on a tapped tab lingers, in milliseconds.
/// Long enough for one or two frames at the post-tap boosted render rate.
pub const TAP_FEEDBACK_LINGER_MS: i64 = 200;

/// Top edge of the bottom tab bar in panel-global coordinates. A tap at or below
/// this y in the content region routes to the tab bar, never to the page.
pub const TAB_BAR_TOP_Y: i32 = PANEL_H as i32 - BOTTOM_BAR_H as i32;

/// The result of routing one physical-input event.
///
/// `RouteChanged` and `ModalChanged` tell the render loop the active surface
/// moved (so it can redraw immediately rather than wait for the page cadence).
/// `Custom` carries a page-defined action key back to the caller, which owns the
/// REST call / slider drag / list-row behaviour the key names. `None` means the
/// event landed nowhere actionable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dispatch {
    /// The active tab changed to the carried page id.
    RouteChanged(String),
    /// A detail page was pushed onto, or popped off, the modal stack. Carries
    /// the resulting topmost route id (the active page id when the stack is now
    /// empty).
    ModalChanged(String),
    /// A page-defined hit-zone action fired; the caller interprets the key.
    Custom(String),
    /// The event did not hit an actionable target.
    None,
}

/// Active-page state machine, modal stack, page registry, and input router.
///
/// One instance is built by the LCD service. The render loop calls
/// [`PageNavigator::render_active`] each tick and feeds touch gestures and
/// button events through [`PageNavigator::on_touch`] /
/// [`PageNavigator::on_button`].
pub struct PageNavigator {
    registry: HashMap<&'static str, Box<dyn Page>>,
    /// Insertion order of the registry, so `current_page` can fall through to a
    /// stable first page if the active id ever desyncs.
    order: Vec<&'static str>,
    /// The bottom-bar tabs, left to right: registered ids only, 1..=5 of them.
    /// The first is the default route and the target of a Back off a non-tab
    /// or full-screen route.
    tabs: Vec<&'static str>,
    active_page_id: String,
    /// Detail pages stacked above the active page, top of stack last. Stored as
    /// route ids resolved against the registry at render time.
    modal_stack: Vec<String>,
    /// The last tapped tab and when, on the caller's monotonic-ms clock.
    tab_tap: Option<(&'static str, i64)>,
    state_path: PathBuf,
}

impl PageNavigator {
    /// Build a navigator over `pages` with the bottom-bar `tabs`, restoring
    /// the persisted active id from the default `/run/ados/lcd-state.json`.
    pub fn new(pages: Vec<Box<dyn Page>>, tabs: &[&'static str]) -> Self {
        Self::with_state_path(pages, tabs, PathBuf::from(LCD_STATE_PATH))
    }

    /// Build a navigator with an explicit state-file path (used by tests so the
    /// persistence round-trips without touching `/run`).
    pub fn with_state_path(
        pages: Vec<Box<dyn Page>>,
        tabs: &[&'static str],
        state_path: PathBuf,
    ) -> Self {
        let mut registry: HashMap<&'static str, Box<dyn Page>> = HashMap::new();
        let mut order: Vec<&'static str> = Vec::with_capacity(pages.len());
        for page in pages {
            let id = page.id();
            if !registry.contains_key(id) {
                order.push(id);
            }
            registry.insert(id, page);
        }
        let tabs = resolve_tabs(&registry, &order, tabs);

        let mut active_page_id = tabs[0].to_string();
        // Restore the previous active id when it still resolves to a registered
        // page. A persisted id for a renamed/removed page falls back to the
        // default rather than leaving the loop with an unresolvable route.
        if let Some(prev) = LcdState::load(&state_path).and_then(|s| s.active_page_id) {
            if registry.contains_key(prev.as_str()) {
                active_page_id = prev;
            }
        }

        let nav = Self {
            registry,
            order,
            tabs,
            active_page_id,
            modal_stack: Vec::new(),
            tab_tap: None,
            state_path,
        };
        // Persist the resolved state at every start: the heartbeat consumer and
        // `GET /api/v1/display/page` see the page actually shown (a stale
        // modal stack or an unresolvable id is not carried over), and the
        // route ids this build registers reach `POST /api/v1/display/page`,
        // which accepts exactly those.
        nav.persist_active_id();
        nav
    }

    /// Replace the bottom-bar tab list in place (a panel config reload).
    /// Unregistered ids are dropped and the list is capped at five; an empty
    /// result keeps the ground-station tabs. When the active route was a tab
    /// that the new list no longer carries, the panel lands on the new first
    /// tab. Returns `true` when the visible route changed.
    pub fn set_tabs(&mut self, tabs: &[&'static str]) -> bool {
        let old = std::mem::replace(
            &mut self.tabs,
            resolve_tabs(&self.registry, &self.order, tabs),
        );
        let active = self.active_page_id.as_str();
        if old.contains(&active) && !self.tabs.contains(&active) {
            let first = self.tabs[0];
            return self.go(first);
        }
        false
    }

    /// The bottom-bar tabs, left to right.
    pub fn tabs(&self) -> &[&'static str] {
        &self.tabs
    }

    /// The first tab: the default route and where Back leaves a non-tab route.
    pub fn default_page_id(&self) -> &'static str {
        self.tabs[0]
    }

    // ── registry ────────────────────────────────────────────────────

    /// True when `page_id` resolves to a registered page.
    pub fn has(&self, page_id: &str) -> bool {
        self.registry.contains_key(page_id)
    }

    /// The active tab route id (ignores the modal stack).
    pub fn active_page_id(&self) -> &str {
        &self.active_page_id
    }

    /// The route ids stacked above the active page, bottom to top.
    pub fn modal_stack(&self) -> &[String] {
        &self.modal_stack
    }

    /// Every registered route id, in registration order.
    pub fn known_page_ids(&self) -> &[&'static str] {
        &self.order
    }

    /// Resolve the page that should paint this tick: the topmost modal when the
    /// stack is non-empty, otherwise the active tab page. Falls through to the
    /// first registered page if the active id ever fails to resolve, so the
    /// render loop never panics on a stale route.
    pub fn current_page(&self) -> &dyn Page {
        if let Some(top) = self.modal_stack.last() {
            if let Some(page) = self.registry.get(top.as_str()) {
                return page.as_ref();
            }
        }
        if let Some(page) = self.registry.get(self.active_page_id.as_str()) {
            return page.as_ref();
        }
        let first = self
            .order
            .first()
            .expect("navigator built with at least one page");
        self.registry
            .get(first)
            .expect("first registered id resolves")
            .as_ref()
    }

    /// The active page id when no modal is open, else the topmost modal id. This
    /// is the id the recent-touch ring and the cloud heartbeat report.
    pub fn current_page_id(&self) -> &str {
        self.modal_stack
            .last()
            .map(String::as_str)
            .unwrap_or(self.active_page_id.as_str())
    }

    // ── navigation ──────────────────────────────────────────────────

    /// Switch to `page_id`. Drops any open modal (a tab switch always lands at
    /// the tab root). Returns `true` when the visible route actually changed.
    pub fn go(&mut self, page_id: &str) -> bool {
        if !self.registry.contains_key(page_id) {
            return false;
        }
        // No change when already on this tab with no modal open.
        if page_id == self.active_page_id && self.modal_stack.is_empty() {
            return false;
        }
        let had_modal = !self.modal_stack.is_empty();
        self.modal_stack.clear();
        let route_moved = page_id != self.active_page_id;
        self.active_page_id = page_id.to_string();
        self.persist_active_id();
        route_moved || had_modal
    }

    /// Push a detail page above the current page. Unknown ids are ignored.
    /// Returns `true` when the page existed and was pushed.
    pub fn push_modal(&mut self, page_id: &str) -> bool {
        if !self.registry.contains_key(page_id) {
            return false;
        }
        self.modal_stack.push(page_id.to_string());
        self.persist_active_id();
        true
    }

    /// Pop the topmost modal. Returns the popped route id, or `None` when the
    /// stack was already empty.
    pub fn pop_modal(&mut self) -> Option<String> {
        let popped = self.modal_stack.pop();
        if popped.is_some() {
            self.persist_active_id();
        }
        popped
    }

    /// Pop every open modal back to the active tab root. Returns `true` when at
    /// least one modal was popped.
    pub fn pop_all_modals(&mut self) -> bool {
        if self.modal_stack.is_empty() {
            return false;
        }
        self.modal_stack.clear();
        self.persist_active_id();
        true
    }

    // ── render ──────────────────────────────────────────────────────

    /// Paint the currently-resolved page (topmost modal or active tab) into a
    /// full-panel canvas. Over a tabbed page the navigator paints the bottom
    /// tab bar (the visible tab accented), and a tab tapped less than
    /// [`TAP_FEEDBACK_LINGER_MS`] before `now_ms` is painted pressed.
    pub fn render_active(&self, ctx: &PageContext, palette: &Palette, now_ms: i64) -> Canvas {
        let page = self.current_page();
        let mut canvas = page.render(ctx, palette);
        if page.chrome().has_tab_bar() {
            draw_bottom_bar(&mut canvas, palette, &self.tabs, page.id());
            if let Some(tab) = self.tab_pulse(now_ms) {
                draw_tab_pulse(&mut canvas, palette, &self.tabs, tab);
            }
        }
        canvas
    }

    /// The active page's preferred redraw cadence, so the render loop can pace
    /// the panel to the surface that is showing.
    pub fn active_refresh_hz(&self) -> f32 {
        self.current_page().refresh_hz()
    }

    // ── input routing ───────────────────────────────────────────────

    /// Honour a remote `POST /api/v1/display/page` request. Reads and unlinks
    /// `/run/ados/lcd-page-request.json` from the default path; on a registered
    /// target it pops every open modal and switches to it. Returns the applied
    /// route id, or `None` when there was no valid request.
    pub fn drain_page_request(&mut self) -> Option<String> {
        self.drain_page_request_from(Path::new(LCD_PAGE_REQUEST_PATH))
    }

    /// [`PageNavigator::drain_page_request`] against an explicit path (tests).
    pub fn drain_page_request_from(&mut self, path: &Path) -> Option<String> {
        let page_id = sidecar::take_page_request(path)?;
        if !self.registry.contains_key(page_id.as_str()) {
            return None;
        }
        // A remote tab switch lands at the requested page's root, mirroring the
        // on-LCD tab-tap UX: clear the modal stack first.
        self.pop_all_modals();
        self.go(&page_id);
        Some(page_id)
    }

    // ── tap feedback ────────────────────────────────────────────────

    /// The tab to paint pressed at `now_ms`: the last tapped one, while its tap
    /// is younger than [`TAP_FEEDBACK_LINGER_MS`].
    pub fn tab_pulse(&self, now_ms: i64) -> Option<&'static str> {
        self.tab_tap
            .filter(|(_, at)| (0..TAP_FEEDBACK_LINGER_MS).contains(&(now_ms - at)))
            .map(|(tab, _)| tab)
    }

    // ── persistence ─────────────────────────────────────────────────

    /// Atomically write `{active_page_id, modal_stack, route_ids}` (unique temp
    /// sibling, fsync, rename) so a power cut mid-write cannot leave a
    /// half-flushed file. `route_ids` is every registered route in
    /// registration order: the remote page-request route validates against
    /// it, so the ids it accepts are the ones this navigator can land on. A
    /// write error is swallowed: persistence is best-effort and never blocks
    /// the render loop.
    fn persist_active_id(&self) {
        #[derive(serde::Serialize)]
        struct Blob<'a> {
            active_page_id: &'a str,
            modal_stack: &'a [String],
            route_ids: &'a [&'static str],
        }
        let blob = Blob {
            active_page_id: &self.active_page_id,
            modal_stack: &self.modal_stack,
            route_ids: &self.order,
        };
        let Ok(body) = serde_json::to_vec(&blob) else {
            return;
        };
        let _ = ados_protocol::sidecar::write_atomic(&self.state_path, &body, 0o644);
    }
}

/// The usable tab list from `wanted`: registered ids only, first occurrence
/// kept, at most [`MAX_TABS`]. An empty result falls back to the registered
/// ground-station tabs, then to the first registered page, so the list is
/// never empty.
fn resolve_tabs(
    registry: &HashMap<&'static str, Box<dyn Page>>,
    order: &[&'static str],
    wanted: &[&'static str],
) -> Vec<&'static str> {
    let pick = |ids: &mut dyn Iterator<Item = &'static str>| -> Vec<&'static str> {
        let mut tabs: Vec<&'static str> = Vec::with_capacity(MAX_TABS);
        for id in ids {
            if tabs.len() == MAX_TABS {
                break;
            }
            if registry.contains_key(id) && !tabs.contains(&id) {
                tabs.push(id);
            }
        }
        tabs
    };
    let tabs = pick(&mut wanted.iter().copied());
    if !tabs.is_empty() {
        return tabs;
    }
    let tabs = pick(&mut crate::pages::GS_TAB_IDS.iter().copied());
    if !tabs.is_empty() {
        return tabs;
    }
    vec![*order
        .first()
        .expect("navigator built with at least one page")]
}

mod input;

#[cfg(test)]
mod tests;
