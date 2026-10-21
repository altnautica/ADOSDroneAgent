//! Application state: the shared [`Core`] every screen reads and writes
//! through, the [`Tab`] trait each tab implements, and the tab registry.
//!
//! Event handling (keys, mouse, messages) lives in `app/dispatch.rs`; overlay
//! types in `overlay.rs`.

use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ados_protocol::rest::{RestClient, RestError};
use ratatui::crossterm::event::KeyEvent;
use ratatui::layout::Rect;
use ratatui::Frame;
use serde_json::Value;

use crate::args::Target;
use crate::keymap::{Command, Ctx};
use crate::model::Dashboard;
use crate::motion::{self, Fx};
use crate::overlay::{Overlay, OverlayResult, Ret};
use crate::poll::{AgentView, Poller, ResourceId, Sub};
use crate::prefs::PrefsStore;
use crate::symbols::{self, Symbols};
use crate::theme::{Theme, ThemeId, Tier};
use crate::toast::{ToastKind, Toasts};
use crate::update::LatestSlot;
use crate::worker::{Reply, Request, Worker};

mod auth;
mod dispatch;
mod effects;
mod palette_cmds;
mod tab_id;
mod tabs;
#[cfg(test)]
mod tests;
pub use dispatch::at;
pub use tab_id::{profile_label, TabId};
pub use tabs::Tabs;

/// Past this age with no successful poll, the shown snapshot is flagged stale
/// so a departed agent never keeps reading live under a moving clock.
pub const STALE_AFTER: Duration = Duration::from_secs(6);

/// The refusal every armed-guarded action shows.
pub const ARMED_REFUSAL: &str = "Refused: vehicle is armed or arm state unknown. Disarm first.";

/// The latest read of one resource.
#[derive(Debug, Clone)]
pub struct Slot {
    /// The last successful body (kept across a later failure).
    pub value: Option<Value>,
    /// The last failure, cleared by the next success.
    pub error: Option<String>,
    /// The HTTP status of the last failure, when the agent answered.
    pub code: Option<u16>,
    /// The failure's JSON body, when there was one.
    pub error_body: Option<Value>,
    pub at: Instant,
}

/// A mouse hit target recorded while rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hit {
    Tab(TabId),
    /// A sidebar entry (settings page, params group, extension).
    Sidebar(usize),
    /// A body row.
    Row(usize),
    /// A named, indexed target inside a screen.
    Item(&'static str, usize),
}

/// What one rendered frame reports back for effects and mouse hits.
#[derive(Debug, Default)]
pub struct FrameOut {
    pub hits: Vec<(Rect, Hit)>,
    /// Rows drawn under a key, so a flash can find its rect.
    pub rows: Vec<(String, Rect)>,
    pub body: Rect,
    pub overlay: Option<Rect>,
    pub toasts: Vec<(u64, Rect)>,
    pub armed_chip: Option<Rect>,
    pub offline_chip: Option<Rect>,
    /// Where to show the text cursor (a focused input).
    pub cursor: Option<(u16, u16)>,
    /// The value cell of the focused settings row, where an inline edit
    /// draws its field.
    pub focus_value: Option<Rect>,
}

impl FrameOut {
    pub fn hit(&mut self, rect: Rect, hit: Hit) {
        self.hits.push((rect, hit));
    }
    pub fn row(&mut self, key: impl Into<String>, rect: Rect) {
        self.rows.push((key.into(), rect));
    }
}

/// A shell-out the main loop runs (it needs the terminal).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellReq {
    /// A quick action by index into [`crate::action::ACTIONS`].
    Action(usize),
    /// `ados update`, y/N-confirmed on the plain terminal.
    Update,
    /// `ados plugin install <path>`; the path is in `Core::shell_arg`.
    PluginInstall,
}

/// Everything shared across screens.
pub struct Core {
    pub target: Target,
    pub client: RestClient,
    pub store: PrefsStore,
    pub theme: Theme,
    pub sym: &'static Symbols,
    pub fx: Fx,
    pub toasts: Toasts,
    pub view: AgentView,
    pub res: HashMap<ResourceId, Slot>,
    pub overlays: Vec<Overlay>,
    pub poller: Poller,
    pub worker: Worker,
    pub latest: LatestSlot,
    pub tab: TabId,
    pub now: Instant,
    /// Animation tick, advanced every frame (spinners).
    pub frame: u64,
    /// Rows to flash after the next render: `(row key, ok)`.
    pub flashes: Vec<(String, bool)>,
    pub shell: Option<ShellReq>,
    pub shell_arg: Option<String>,
    pub quit: bool,
    pub dirty: bool,
    /// Mouse capture must be toggled by the main loop.
    pub mouse_changed: bool,
    /// A pending re-target: the main loop rebuilds the app around it.
    pub retarget: Option<(Target, Option<String>)>,
    /// The previous frame's hits, for mouse routing.
    pub last_hits: Vec<(Rect, Hit)>,
    pub last_body: Rect,
    pub(crate) pending_tab_fx: bool,
    pub(crate) pending_overlay_fx: bool,
    pub(crate) subs_sent: Vec<Sub>,
    pub(crate) tier: Tier,
    pub(crate) ssh: bool,
    pub(crate) update_splash_done: bool,
    pub(crate) had_data: bool,
    pub(crate) toasts_in: Vec<u64>,
    pub(crate) toasts_out: Vec<u64>,
    pub auth: auth::AuthCtl,
    /// Keys entered for a saved target without "remember", by target name.
    /// Kept in memory only, and carried across a re-target so reconnecting to
    /// the same target in this session does not ask again.
    pub session_keys: HashMap<String, String>,
    /// The tab under the mouse pointer (mouse capture on).
    pub hover_tab: Option<TabId>,
}

/// Wall-clock UTC `HH:MM:SS`.
pub fn now_hms() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        % 86_400;
    format!(
        "{:02}:{:02}:{:02}",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

impl Core {
    pub fn new(target: Target, key: Option<String>, store: PrefsStore, latest: LatestSlot) -> Self {
        let tier = crate::theme::env_tier();
        let ssh = motion::env_ssh();
        let prefs = &store.prefs;
        let theme = Theme::new(tier, ThemeId::from_name(&prefs.theme), prefs.color_blind);
        let sym = symbols::by_name(&prefs.symbols);
        let fx = Fx::new(motion::resolve(&prefs.motion, ssh, tier));
        let client = RestClient::new(target.base.clone());
        client.set_api_key(key);
        let poller = Poller::spawn(client.clone(), Duration::from_millis(prefs.refresh_ms));
        let worker = Worker::spawn(client.clone());
        let tab = if prefs.start_tab == "last" {
            TabId::from_name(&prefs.last_tab).unwrap_or(TabId::Overview)
        } else {
            TabId::Overview
        };
        let writable = store.writable;
        let mut core = Self {
            target,
            client,
            store,
            theme,
            sym,
            fx,
            toasts: Toasts::default(),
            view: AgentView::default(),
            res: HashMap::new(),
            overlays: Vec::new(),
            poller,
            worker,
            latest,
            tab,
            now: Instant::now(),
            frame: 0,
            flashes: Vec::new(),
            shell: None,
            shell_arg: None,
            quit: false,
            dirty: true,
            mouse_changed: false,
            retarget: None,
            last_hits: Vec::new(),
            last_body: Rect::default(),
            pending_tab_fx: false,
            pending_overlay_fx: false,
            subs_sent: Vec::new(),
            tier,
            ssh,
            update_splash_done: false,
            had_data: false,
            toasts_in: Vec::new(),
            toasts_out: Vec::new(),
            auth: auth::AuthCtl::default(),
            session_keys: HashMap::new(),
            hover_tab: None,
        };
        if !writable && core.store.path().is_some_and(|p| p.exists()) {
            core.toast(
                ToastKind::Warn,
                "tui.json unreadable; preferences will not be saved this session",
            );
        }
        core
    }

    // --- reads ---------------------------------------------------------------

    pub fn dash(&self) -> Option<&Dashboard> {
        self.view.dash.as_ref()
    }

    /// The reported profile; empty until the first poll.
    pub fn profile(&self) -> &str {
        self.dash().map(|d| d.profile.as_str()).unwrap_or("")
    }

    pub fn stale(&self) -> bool {
        self.view.is_stale(self.now, STALE_AFTER)
    }

    pub fn slot(&self, id: &ResourceId) -> Option<&Slot> {
        self.res.get(id)
    }

    /// The last good body of a resource.
    pub fn value(&self, id: &ResourceId) -> Option<&Value> {
        self.res.get(id).and_then(|s| s.value.as_ref())
    }

    /// The newer agent version, when the update check found one. Local only:
    /// the check reads this machine's release channel, which says nothing
    /// about a remote agent's.
    pub fn update_available(&self) -> Option<String> {
        if self.target.remote {
            return None;
        }
        let latest = self.latest.lock().ok().and_then(|g| g.clone())?;
        let installed = &self.dash()?.version;
        crate::update::is_newer(&latest, installed).then_some(latest)
    }

    /// The latest version the update check reported, whether newer or not.
    pub fn latest_version(&self) -> Option<String> {
        self.latest.lock().ok().and_then(|g| g.clone())
    }

    /// `Ok` only when a fresh snapshot says nothing can be flying: a vehicle
    /// that reports disarmed, or a node with no autopilot (a ground station,
    /// or a visor with no flight controller link). An unknown arm state on a
    /// drone is a refusal: a lost FC link can be a vehicle in the air.
    pub fn require_disarmed(&self) -> Result<(), &'static str> {
        let Some(d) = self.dash() else {
            return Err(ARMED_REFUSAL);
        };
        if self.stale() {
            return Err(ARMED_REFUSAL);
        }
        let no_autopilot =
            d.profile == "ground_station" || (d.profile == "visor" && !d.mavlink_connected);
        match d.armed {
            Some(false) => Ok(()),
            None if no_autopilot => Ok(()),
            _ => Err(ARMED_REFUSAL),
        }
    }

    /// `require_disarmed`, toasting the refusal. True when the action may run.
    pub fn guard_disarmed(&mut self) -> bool {
        match self.require_disarmed() {
            Ok(()) => true,
            Err(why) => {
                self.toast(ToastKind::Error, why);
                false
            }
        }
    }

    // --- writes ----------------------------------------------------------------

    pub fn toast(&mut self, kind: ToastKind, text: impl Into<String>) {
        self.toasts.push(kind, text, Instant::now(), now_hms());
        self.dirty = true;
    }

    /// Toast a failed request: the armed refusal for a 409 `E_ARMED`, else the
    /// server's detail (or the transport error).
    pub fn toast_error(&mut self, prefix: &str, err: &RestError) {
        let text = error_text(err);
        let msg = if prefix.is_empty() {
            text
        } else {
            format!("{prefix}: {text}")
        };
        self.toast(ToastKind::Error, msg);
    }

    pub fn submit(&mut self, req: Request) {
        self.worker.submit(req);
    }

    pub fn open(&mut self, overlay: Overlay) {
        self.overlays.push(overlay);
        self.pending_overlay_fx = true;
        self.dirty = true;
    }

    pub fn fetch_now(&self, id: ResourceId) {
        self.poller.fetch_now(id);
    }

    /// Re-read everything now: the global poll and every active resource.
    pub fn refetch_all(&self) {
        self.poller.refresh();
        for sub in &self.subs_sent {
            self.poller.fetch_now(sub.id.clone());
        }
    }

    /// Copy to the clipboard (OSC 52) and say so.
    pub fn copy(&mut self, text: &str) {
        crate::clipboard::copy(text);
        self.toast(ToastKind::Info, "Copied (OSC 52)");
    }

    /// Flash a rendered row after the next frame.
    pub fn flash(&mut self, row: impl Into<String>, ok: bool) {
        self.flashes.push((row.into(), ok));
    }

    /// Persist preferences, warning once when the write fails.
    pub fn save_prefs(&mut self) {
        if let Err(e) = self.store.save() {
            self.toast(ToastKind::Warn, format!("Could not save preferences: {e}"));
        }
    }

    /// Re-apply theme, symbols, motion, refresh and mouse from the prefs.
    pub fn apply_prefs(&mut self) {
        let p = &self.store.prefs;
        self.theme = Theme::new(self.tier, ThemeId::from_name(&p.theme), p.color_blind);
        self.sym = symbols::by_name(&p.symbols);
        self.fx
            .set_level(motion::resolve(&p.motion, self.ssh, self.tier));
        self.poller
            .set_interval(Duration::from_millis(p.refresh_ms));
        self.mouse_changed = true;
        self.dirty = true;
    }
}

/// The operator-facing text of a failed request.
pub fn error_text(err: &RestError) -> String {
    if err.code() == Some(409)
        && err
            .body()
            .and_then(|b| b.get("error"))
            .and_then(Value::as_str)
            == Some("E_ARMED")
    {
        return ARMED_REFUSAL.to_string();
    }
    let detail = err.detail();
    if detail.is_empty() {
        err.to_string()
    } else {
        detail
    }
}

/// One tab body. Every method but `render`/`ctx`/`on_command` has a no-op
/// default.
pub trait Tab {
    /// Key contexts, most specific first (`Global` is appended by the app).
    fn ctx(&self, core: &Core) -> Vec<Ctx>;
    /// Resources polled while this tab is active.
    fn subs(&self, _core: &Core) -> Vec<Sub> {
        Vec::new()
    }
    fn on_activate(&mut self, _core: &mut Core) {}
    fn on_deactivate(&mut self, _core: &mut Core) {}
    /// After every global poll (called on every tab, active or not).
    fn on_global(&mut self, _core: &mut Core) {}
    /// After a subscribed resource landed in `core.res`.
    fn on_resource(&mut self, _core: &mut Core, _id: &ResourceId) {}
    /// A command; `true` when handled (else the app's default runs).
    fn on_command(&mut self, core: &mut Core, cmd: Command) -> bool;
    /// A text key while the tab reports `Ctx::Input` (an inline field).
    fn on_text(&mut self, _core: &mut Core, _key: KeyEvent) {}
    /// An overlay this tab opened finished.
    fn on_overlay(&mut self, _core: &mut Core, _ret: Ret, _result: OverlayResult) {}
    /// A worker request this tab submitted finished.
    fn on_reply(&mut self, _core: &mut Core, _reply: Reply) {}
    /// One item of a parameter batch finished.
    fn on_progress(
        &mut self,
        _core: &mut Core,
        _name: String,
        _value: f64,
        _result: Result<Value, RestError>,
    ) {
    }
    /// A mouse click on a hit this tab recorded.
    fn on_click(&mut self, _core: &mut Core, _hit: &Hit) {}
    /// Called every loop iteration while active (drain own channels).
    fn tick(&mut self, _core: &mut Core) {}
    /// Whether the tab animates (a spinner) and needs periodic redraws.
    fn animating(&self, _core: &Core) -> bool {
        false
    }
    fn render(&mut self, f: &mut Frame, area: Rect, core: &Core, out: &mut FrameOut);
    /// The context line under the body: help for the focused row.
    fn context_line(&self, _core: &Core) -> Option<String> {
        None
    }
    /// Extra hint-bar text (e.g. `PAUSED +12`).
    fn status_hint(&self, _core: &Core) -> Option<String> {
        None
    }
    /// Whether `/` searches on this screen (shows the hint).
    fn searchable(&self, _core: &Core) -> bool {
        false
    }
}

/// The whole application: shared core plus tab state.
pub struct App {
    pub core: Core,
    pub tabs: Tabs,
}
