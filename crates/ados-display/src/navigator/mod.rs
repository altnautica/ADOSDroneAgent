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
//!   32 px top bar and the 44 px bottom tab bar: a tap in the bottom strip maps
//!   its x position to one of the five tabs (dashboard, video, settings,
//!   link-stats, channel-hops), and any other tap is shifted into the page's
//!   content-local frame (subtract the top bar). A full-screen page (detail
//!   pages, the overflow menu, the plugin page) paints the whole panel with no
//!   tab bar, so its zones are panel-global and the bottom strip is page area.
//! * The shifted tap is tested against the active page's
//!   [`crate::pages::HitZone`]s. The first zone that contains the point has its
//!   [`crate::pages::HitAction`] dispatched: `GoTab` switches tabs, `OpenDetail`
//!   pushes a detail page as a modal, `Back` pops the modal stack (or returns a
//!   non-tab route to the dashboard), and `Custom` is handed back to the caller
//!   as a [`Dispatch::Custom`], which resolves it through
//!   [`PageNavigator::custom_action`].
//! * The four front-panel buttons resolve their action through the
//!   [`ados_hid::buttons`] mapping; the navigator maps `back` to a modal pop,
//!   `cycle_screen` to the next tab, and the menu / pairing / network / qr
//!   actions to their target routes.
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
use std::io::Write;
use std::path::{Path, PathBuf};

use ados_hid::buttons::ButtonEvent;
use ados_hid::touch::{GestureKind, TouchGesture};

use crate::graphics::palette::Palette;
use crate::graphics::primitives::Canvas;
use crate::pages::{
    HitAction, Page, PageContext, PanelAction, BOTTOM_BAR_H, PANEL_H, TAB_COUNT, TAB_PAGE_IDS,
    TAB_WIDTH,
};
use crate::sidecar::{self, LcdState, LCD_PAGE_REQUEST_PATH, LCD_STATE_PATH};
use crate::widgets::draw_tab_pulse;

/// The route the navigator falls back to when no page is persisted (and the
/// first tab in the tab bar).
pub const DEFAULT_PAGE_ID: &str = "dashboard";

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
    active_page_id: String,
    /// Detail pages stacked above the active page, top of stack last. Stored as
    /// route ids resolved against the registry at render time.
    modal_stack: Vec<String>,
    /// The last tapped tab and when, on the caller's monotonic-ms clock.
    tab_tap: Option<(&'static str, i64)>,
    state_path: PathBuf,
}

impl PageNavigator {
    /// Build a navigator over `pages`, restoring the persisted active id from
    /// the default `/run/ados/lcd-state.json`.
    pub fn new(pages: Vec<Box<dyn Page>>) -> Self {
        Self::with_state_path(pages, PathBuf::from(LCD_STATE_PATH))
    }

    /// Build a navigator with an explicit state-file path (used by tests so the
    /// persistence round-trips without touching `/run`).
    pub fn with_state_path(pages: Vec<Box<dyn Page>>, state_path: PathBuf) -> Self {
        let mut registry: HashMap<&'static str, Box<dyn Page>> = HashMap::new();
        let mut order: Vec<&'static str> = Vec::with_capacity(pages.len());
        for page in pages {
            let id = page.id();
            if !registry.contains_key(id) {
                order.push(id);
            }
            registry.insert(id, page);
        }

        let mut active_page_id = DEFAULT_PAGE_ID.to_string();
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
    /// full-panel canvas. On a page that paints the tab bar, a tab tapped less
    /// than [`TAP_FEEDBACK_LINGER_MS`] before `now_ms` is painted pressed.
    pub fn render_active(&self, ctx: &PageContext, palette: &Palette, now_ms: i64) -> Canvas {
        let page = self.current_page();
        let mut canvas = page.render(ctx, palette);
        if page.chrome().has_tab_bar() {
            if let Some(tab) = self.tab_pulse(now_ms) {
                draw_tab_pulse(&mut canvas, palette, tab);
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

    /// Route one classified touch gesture against the live [`PageContext`] (some
    /// pages, e.g. the settings list, lay their hit zones out from context).
    /// On a tabbed page a tap in the bottom 44 px strip switches tabs; every
    /// other gesture is tested against the active page's zones in the page's own
    /// frame. Non-tap gestures (swipe / drag / long press) are tested too so a
    /// page that wants drag-on-a-zone sees them via the returned
    /// [`Dispatch::Custom`]. Returns what the event resolved to.
    pub fn on_touch(&mut self, ctx: &PageContext, gesture: &TouchGesture, now_ms: i64) -> Dispatch {
        let start_x = gesture.start_x;
        let start_y = gesture.start_y;

        // Tab-bar band: only a page that paints the tab bar owns it.
        let chrome = self.current_page().chrome();
        if chrome.has_tab_bar() && start_y >= TAB_BAR_TOP_Y && gesture.kind == GestureKind::Tap {
            return self.route_tab_tap(start_x, now_ms);
        }

        match self.zone_at(ctx, start_x, start_y) {
            Some(action) => self.dispatch_action(action, now_ms),
            None => Dispatch::None,
        }
    }

    /// The action of the first zone of the current page under the panel-global
    /// point `(x, y)`, after shifting it into the page's frame. A point above a
    /// tabbed page's content region (the top status bar) resolves to nothing.
    pub fn zone_at(&self, ctx: &PageContext, x: i32, y: i32) -> Option<HitAction> {
        let page = self.current_page();
        let local_y = y - page.chrome().origin_y();
        if local_y < 0 {
            return None;
        }
        page.hit_zones(ctx)
            .into_iter()
            .find(|z| z.contains(x, local_y))
            .map(|z| z.action)
    }

    /// Resolve a [`Dispatch::Custom`] key against the page that emitted it.
    pub fn custom_action(&self, key: &str, ctx: &PageContext) -> Option<PanelAction> {
        self.current_page().on_custom(key, ctx)
    }

    /// Map an x position in the tab bar to a tab and switch to it, popping any
    /// open modal first so the operator lands at the tab root. Records the tap
    /// for the pulse regardless of whether the route changed.
    fn route_tab_tap(&mut self, start_x: i32, now_ms: i64) -> Dispatch {
        let index = (start_x / TAB_WIDTH).clamp(0, TAB_COUNT as i32 - 1) as usize;
        let page_id = TAB_PAGE_IDS[index];
        self.tab_tap = Some((page_id, now_ms));
        if !self.registry.contains_key(page_id) {
            return Dispatch::None;
        }
        if self.go(page_id) {
            Dispatch::RouteChanged(page_id.to_string())
        } else {
            Dispatch::None
        }
    }

    /// Dispatch a resolved [`HitAction`] into a navigator transition or a
    /// caller-handled custom key.
    fn dispatch_action(&mut self, action: HitAction, now_ms: i64) -> Dispatch {
        match action {
            HitAction::GoTab(id) => {
                self.tab_tap = Some((id, now_ms));
                if self.go(id) {
                    Dispatch::RouteChanged(id.to_string())
                } else {
                    Dispatch::None
                }
            }
            HitAction::OpenDetail(id) => {
                if self.push_modal(id) {
                    Dispatch::ModalChanged(id.to_string())
                } else {
                    Dispatch::None
                }
            }
            HitAction::Back => self.back(),
            HitAction::Custom(key) => Dispatch::Custom(key),
        }
    }

    /// Route one front-panel button event through its resolved action name. The
    /// LCD page system reuses the ground-station button mapping: `back` pops the
    /// modal stack, `cycle_screen` advances to the next tab, and the menu /
    /// pairing / network / qr actions jump to their routes. `confirm`,
    /// `toggle_backlight`, and any unmapped action are returned as a
    /// [`Dispatch::Custom`] so the service can act on them (backlight, confirm a
    /// modal's primary action) without the navigator owning hardware.
    pub fn on_button(&mut self, event: &ButtonEvent) -> Dispatch {
        let Some(action) = event.action.as_deref() else {
            return Dispatch::None;
        };
        match action {
            // Back: pop one modal, or leave a non-tab route for the dashboard.
            "back" => self.back(),
            // Carousel-style next: advance to the next tab in bar order, wrapping.
            "cycle_screen" => {
                let next = self.next_tab_id();
                if self.go(next) {
                    Dispatch::RouteChanged(next.to_string())
                } else {
                    Dispatch::None
                }
            }
            // Menu / overflow: jump to the More overflow page when present, else
            // settings (the maintenance rows moved under Settings).
            "menu" => self.jump_to_first_present(&["more", "settings"]),
            // Pairing: open the pair-drone detail above whatever is showing.
            "pair_drone" => {
                if self.push_modal("details.pair_drone") {
                    Dispatch::ModalChanged("details.pair_drone".to_string())
                } else {
                    Dispatch::None
                }
            }
            // Network + QR shortcuts route to the uplink detail (network info)
            // and the pair-drone detail (which carries the pairing QR).
            "show_network" => {
                if self.push_modal("details.uplink") {
                    Dispatch::ModalChanged("details.uplink".to_string())
                } else {
                    Dispatch::None
                }
            }
            "show_qr" => {
                if self.push_modal("details.pair_drone") {
                    Dispatch::ModalChanged("details.pair_drone".to_string())
                } else {
                    Dispatch::None
                }
            }
            // Anything else (confirm, toggle_backlight, custom remaps) is handed
            // to the caller as a named action.
            other => Dispatch::Custom(other.to_string()),
        }
    }

    /// Pop one modal. With no modal open, a non-tab route (the overflow menu or
    /// the plugin page, reached by a button or a remote page request) returns to
    /// the dashboard, so a painted Back is never inert. On a tab root it is.
    fn back(&mut self) -> Dispatch {
        if self.pop_modal().is_some() {
            return Dispatch::ModalChanged(self.current_page_id().to_string());
        }
        if !TAB_PAGE_IDS.contains(&self.active_page_id.as_str()) && self.go(DEFAULT_PAGE_ID) {
            return Dispatch::RouteChanged(DEFAULT_PAGE_ID.to_string());
        }
        Dispatch::None
    }

    /// The tab id one step right of the active tab (wraps). When the active page
    /// is not itself a tab (a remote page-request set a non-tab route), this
    /// starts the cycle at the first tab.
    fn next_tab_id(&self) -> &'static str {
        let here = TAB_PAGE_IDS
            .iter()
            .position(|id| *id == self.active_page_id);
        match here {
            Some(i) => TAB_PAGE_IDS[(i + 1) % TAB_COUNT],
            None => TAB_PAGE_IDS[0],
        }
    }

    /// Push the first present detail page from `candidates`, or `go` to it when
    /// it is a top-level route. Used by the menu button which prefers the
    /// overflow page but falls back to settings.
    fn jump_to_first_present(&mut self, candidates: &[&'static str]) -> Dispatch {
        for id in candidates {
            if self.registry.contains_key(id) {
                // Top-level tabs switch routes; detail-style pages push as a
                // modal. The overflow `more` page and `settings` are top-level.
                if id.starts_with("details.") {
                    if self.push_modal(id) {
                        return Dispatch::ModalChanged((*id).to_string());
                    }
                } else if self.go(id) {
                    return Dispatch::RouteChanged((*id).to_string());
                }
                return Dispatch::None;
            }
        }
        Dispatch::None
    }

    // ── remote page request ─────────────────────────────────────────

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

    /// Atomically write `{active_page_id, modal_stack, route_ids}` (tmp
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
        let _ = atomic_write(&self.state_path, &body);
    }
}

/// Atomic tmp-sibling write (tmp name disambiguated by pid), mirroring the
/// sidecar latency writer: create the parent, write the tmp, fsync, rename.
fn atomic_write(path: &Path, body: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("lcd-state.json");
    let tmp = parent.join(format!("{}.{}.tmp", file_name, std::process::id()));

    let write_result = (|| -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        f.write_all(body)?;
        f.flush()?;
        f.sync_all()?;
        Ok(())
    })();

    if write_result.is_err() {
        let _ = std::fs::remove_file(&tmp);
        return write_result;
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
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
            Box::new(PairDroneDetailPage),
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
        let blob: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
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
}
