//! Touch and button input routing.

use super::*;

impl PageNavigator {
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
    pub(super) fn route_tab_tap(&mut self, start_x: i32, now_ms: i64) -> Dispatch {
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
    pub(super) fn dispatch_action(&mut self, action: HitAction, now_ms: i64) -> Dispatch {
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
    pub(super) fn back(&mut self) -> Dispatch {
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
    pub(super) fn next_tab_id(&self) -> &'static str {
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
    pub(super) fn jump_to_first_present(&mut self, candidates: &[&'static str]) -> Dispatch {
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
}
