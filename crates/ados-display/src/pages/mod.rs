//! Page composers for the native-resolution LCD render path.
//!
//! Each page lays the [`crate::graphics`] widgets out into a full-panel canvas
//! and returns it for packing and blitting. The ground-station landscape
//! dashboard (top status bar, content tiles, bottom system bar), the pairing /
//! detail screens, and the menu live here.
//!
//! This module is the shared home for the page system. It defines:
//!
//! * [`PageContext`] — every data field any screen reads, gathered into nested
//!   sub-structs ([`LinkCtx`], [`DroneCtx`], [`CloudCtx`], and the rest). The
//!   per-page modules only READ this context; they never extend it.
//! * [`Page`] — the contract a page implements (id, refresh cadence, render,
//!   hit zones).
//! * [`HitZone`] — a rectangular touch target on a page.
//! * The panel geometry consts (top status bar, bottom tab bar, content region)
//!   and [`tile_rects`] for the dashboard's 2x2 grid.
//!
//! The per-page composer modules each own one screen and lay it out from the
//! shared [`PageContext`]. [`blank_panel`] is the starting canvas every page
//! fills before painting its chrome and surfaces.

use std::cell::Cell;
use std::time::{Duration, Instant};

use crate::graphics::palette::Palette;
use crate::graphics::primitives::Canvas;

pub mod about;
pub mod access_point;
pub mod calibration;
pub mod cameras;
pub mod channel_hops;
mod context;
pub mod dashboard;
pub mod diagnostics;
pub mod drone;
mod geometry;
pub mod gnss;
pub mod gs_ctx;
pub mod link_stats;
pub mod lora;
pub mod mesh;
pub mod more;
pub mod pair_drone;
pub mod plugin;
pub mod power;
pub mod radio_link;
pub mod radios;
pub mod reading;
pub mod sensing_ctx;
pub mod sensors;
pub mod settings;
mod status_ctx;
pub mod storage;
pub mod uplink;
pub mod video;

pub use context::*;
pub use geometry::*;
pub use status_ctx::*;

// ── hit zones ───────────────────────────────────────────────────────

/// The action a hit zone dispatches when tapped. The navigator stage maps these
/// to route changes, modal pushes, or REST calls; the page layer only needs to
/// label its zones.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HitAction {
    /// Switch to a top-level tab by page id.
    GoTab(&'static str),
    /// Drill into a detail page by page id.
    OpenDetail(&'static str),
    /// Pop the current modal / detail page back to its parent.
    Back,
    /// A page-defined action keyed by a stable string id (button taps, slider
    /// regions, list rows). The owning page interprets the key.
    Custom(String),
}

/// A rectangular touch target on a page, in the page's own coordinate frame:
/// panel-global for a [`Chrome::FullScreen`] page, content-local (origin at the
/// top-left of the 480x244 content region) for a [`Chrome::Tabbed`] one. See
/// [`Chrome::origin_y`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HitZone {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub action: HitAction,
}

impl HitZone {
    /// Build a zone with the given rectangle and action.
    pub fn new(x: i32, y: i32, w: i32, h: i32, action: HitAction) -> Self {
        Self { x, y, w, h, action }
    }

    /// Return true if `(px, py)` lies inside this zone (half-open on the far
    /// edges, matching the page hit-test convention).
    pub fn contains(&self, px: i32, py: i32) -> bool {
        px >= self.x && px < self.x + self.w && py >= self.y && py < self.y + self.h
    }
}

// ── page trait ──────────────────────────────────────────────────────

/// Which chrome a page paints, and so which coordinate frame its hit zones use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chrome {
    /// Top status bar + bottom tab bar. The page paints the top bar and its
    /// content; the navigator paints the tab bar and routes taps in its band.
    /// Zones are content-local: y = 0 is the first row under the top bar.
    Tabbed,
    /// The whole 480x320 panel is the page (detail pages, the overflow menu, the
    /// plugin page). Zones are panel-global and there is no tab band.
    FullScreen,
}

impl Chrome {
    /// Panel-global y of the page's zone frame origin.
    pub fn origin_y(self) -> i32 {
        match self {
            Self::Tabbed => CONTENT_Y as i32,
            Self::FullScreen => 0,
        }
    }

    /// Whether the page paints the bottom tab bar (and so owns the tab band).
    pub fn has_tab_bar(self) -> bool {
        matches!(self, Self::Tabbed)
    }
}

/// An agent write a panel control performs over the local API.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentRequest {
    /// `POST`, `PUT` or `DELETE`.
    pub method: &'static str,
    pub path: &'static str,
    /// The JSON body, when the route takes one.
    pub body: Option<serde_json::Value>,
    /// What the operator asked for, for the acknowledgement line.
    pub label: String,
}

/// What a page does with one of its own custom hit-zone keys.
#[derive(Debug, Clone, PartialEq)]
pub enum PanelAction {
    /// Handled inside the page (an overlay opened or closed); repaint now.
    Repaint,
    /// Perform this agent write and show its outcome.
    Agent(AgentRequest),
}

/// How long a first tap on a destructive control stays armed for the
/// confirming second tap.
pub const CONFIRM_WINDOW: Duration = Duration::from_secs(5);

/// Two-tap confirmation for a destructive panel control.
///
/// The first tap arms one keyed choice for [`CONFIRM_WINDOW`] (the page then
/// relabels the control "Tap again to ..."); a second tap on the same key
/// inside the window confirms. A tap on a different key re-arms that key
/// instead, and a confirmed action disarms, so the next tap starts over.
#[derive(Debug, Default)]
pub struct TwoTapConfirm {
    armed: Cell<Option<(&'static str, Instant)>>,
}

impl TwoTapConfirm {
    /// Whether `key` is armed and still inside the confirm window.
    pub fn is_armed(&self, key: &str) -> bool {
        self.armed
            .get()
            .is_some_and(|(k, at)| k == key && at.elapsed() < CONFIRM_WINDOW)
    }

    /// Register a tap on `key`. Returns `true` when this tap confirms an armed
    /// choice (and disarms it); `false` when it only armed `key`.
    pub fn tap(&self, key: &'static str) -> bool {
        if self.is_armed(key) {
            self.armed.set(None);
            return true;
        }
        self.armed.set(Some((key, Instant::now())));
        false
    }

    /// Drop any armed choice.
    pub fn disarm(&self) {
        self.armed.set(None);
    }
}

/// The contract every LCD page implements.
///
/// The render loop reads [`Page::refresh_hz`] to pace the page, calls
/// [`Page::render`] each tick to paint a full panel canvas, and queries
/// [`Page::hit_zones`] to route taps. A page is a stateless composer over the
/// shared [`PageContext`]; per-page transient state (drag, scroll) lives on the
/// concrete page struct.
pub trait Page {
    /// Stable route id the navigator and persistence key on.
    fn id(&self) -> &'static str;

    /// The chrome the page paints, which fixes the frame of its hit zones.
    fn chrome(&self) -> Chrome;

    /// Preferred redraw cadence in hertz.
    fn refresh_hz(&self) -> f32;

    /// Paint the full 480x320 panel for this page. A tabbed page leaves the
    /// bottom tab band to the navigator, which paints it after this returns.
    fn render(&self, ctx: &PageContext, palette: &Palette) -> Canvas;

    /// Return the page's active hit zones, in the frame [`Page::chrome`] names.
    fn hit_zones(&self, ctx: &PageContext) -> Vec<HitZone>;

    /// Resolve one of this page's [`HitAction::Custom`] keys. `None` when the
    /// key does nothing right now (for example a stepper whose current value is
    /// unknown). Pages without custom zones keep the default.
    fn on_custom(&self, _key: &str, _ctx: &PageContext) -> Option<PanelAction> {
        None
    }
}

/// Allocate a blank full-panel canvas filled with the palette background.
///
/// Every page starts from this and paints its chrome and surfaces over it, so
/// the background is always cleared before a page composes its frame.
pub fn blank_panel(palette: &Palette) -> Canvas {
    Canvas::new(PANEL_W, PANEL_H, palette.bg_primary)
}

#[cfg(test)]
mod tests;
