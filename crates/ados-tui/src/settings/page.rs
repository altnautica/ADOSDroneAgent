//! The contract every bespoke settings page implements. The Settings tab owns
//! the sidebar and routes keys, replies, overlay results and resources to the
//! focused page through this trait.

use ratatui::layout::Rect;
use ratatui::Frame;

use crate::app::{Core, FrameOut, Hit};
use crate::keymap::{Command, PageKeys};
use crate::overlay::{OverlayResult, Ret};
use crate::poll::{ResourceId, Sub};
use crate::worker::Reply;

/// Every bespoke page. The page id (route key) is [`BespokeId::id`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BespokeId {
    Targets,
    Profile,
    Wifi,
    Hotspot,
    Cellular,
    MacPin,
    Discovery,
    Radio,
    VideoLink,
    Sensors,
    Capture,
    Cloud,
    RemoteAccess,
    Pairing,
    Security,
    Mcp,
    Region,
    Display,
    Advanced,
}

impl BespokeId {
    /// The page id used in routes, palette entries and flash keys.
    pub fn id(self) -> &'static str {
        match self {
            BespokeId::Targets => "agents",
            BespokeId::Profile => "profile",
            BespokeId::Wifi => "wifi",
            BespokeId::Hotspot => "hotspot",
            BespokeId::Cellular => "cellular",
            BespokeId::MacPin => "mac_pin",
            BespokeId::Discovery => "discovery",
            BespokeId::Radio => "radio",
            BespokeId::VideoLink => "video_link",
            BespokeId::Sensors => "sensors",
            BespokeId::Capture => "capture",
            BespokeId::Cloud => "cloud",
            BespokeId::RemoteAccess => "remote_access",
            BespokeId::Pairing => "pairing",
            BespokeId::Security => "security",
            BespokeId::Mcp => "mcp",
            BespokeId::Region => "region",
            BespokeId::Display => "display",
            BespokeId::Advanced => "advanced",
        }
    }
}

/// One bespoke settings page.
///
/// Navigation commands (`Up`/`Down`/`PageUp`/`PageDown`/`Top`/`Bottom`/
/// `Left`/`Right`/`Activate`/`Reveal`/`Copy`/`Search`) and the page's own
/// action commands (from its [`PageKeys`] context) arrive in `on_command`.
/// Return `true` when handled; the tab then does nothing else with it.
pub trait Bespoke {
    /// Resources polled while the page is focused.
    fn subs(&self, _core: &Core) -> Vec<Sub> {
        Vec::new()
    }
    /// The page's own key context, when it has action keys.
    fn keys(&self) -> Option<PageKeys> {
        None
    }
    /// The page became focused.
    fn on_activate(&mut self, _core: &mut Core) {}
    fn on_command(&mut self, core: &mut Core, cmd: Command) -> bool;
    /// An overlay this page opened (with `Route::page(self id)`) finished.
    fn on_overlay(&mut self, _core: &mut Core, _ret: Ret, _result: OverlayResult) {}
    /// A worker request this page submitted (with `Route::page(self id)`)
    /// finished.
    fn on_reply(&mut self, _core: &mut Core, _reply: Reply) {}
    /// A subscribed resource landed in `core.res`.
    fn on_resource(&mut self, _core: &mut Core, _id: &ResourceId) {}
    /// A click on a hit this page recorded.
    fn on_click(&mut self, _core: &mut Core, _hit: &Hit) {}
    fn render(&mut self, f: &mut Frame, area: Rect, core: &Core, out: &mut FrameOut);
    /// Help for the focused row (the context line).
    fn context_line(&self, _core: &Core) -> Option<String> {
        None
    }
    /// The page draws a spinner or a countdown and needs periodic frames.
    fn animating(&self, _core: &Core) -> bool {
        false
    }
}
