//! Event handling: keys, mouse, poller and worker messages, tab switches and
//! the command palette. Post-render effects are in `effects.rs`.

use std::time::Instant;

use ados_protocol::rest::RestError;
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Position;
use serde_json::Value;

use super::{now_hms, profile_label, App, Core, Hit, ShellReq, Slot, Tab, TabId, Tabs};
use crate::action::ACTIONS;
use crate::keymap::{self, Command, Ctx};
use crate::overlay::{self, Outcome, Overlay, OverlayResult, Ret};
use crate::palette::{PaletteAction, PaletteCmd, PaletteItem, PaletteState};
use crate::poll::PollMsg;
use crate::theme::ThemeId;
use crate::toast::ToastKind;
use crate::worker::{Purpose, Route, WorkerMsg};

/// Tags of overlays the app itself opens.
const TAG_QUIT: &str = "app.quit";

impl App {
    pub fn new(core: Core) -> Self {
        let mut app = Self {
            core,
            tabs: Tabs::default(),
        };
        let profile = app.core.profile().to_string();
        app.tabs
            .get_mut(app.core.tab, &profile)
            .on_activate(&mut app.core);
        super::auth::start(&mut app.core);
        app.sync_subs();
        app
    }

    fn active(&mut self) -> (&mut dyn Tab, &mut Core) {
        let profile = self.core.profile().to_string();
        (self.tabs.get_mut(self.core.tab, &profile), &mut self.core)
    }

    fn routed(&mut self, route: Route) -> (&mut dyn Tab, &mut Core) {
        let profile = self.core.profile().to_string();
        (self.tabs.get_mut(route.tab, &profile), &mut self.core)
    }

    /// Key contexts right now, most specific first, `Global` last.
    pub fn ctx_stack(&self) -> Vec<Ctx> {
        if let Some(top) = self.core.overlays.last() {
            return top.ctx();
        }
        let mut stack = self
            .tabs
            .get(self.core.tab, self.core.profile())
            .ctx(&self.core);
        if stack.first() != Some(&Ctx::Input) {
            stack.push(Ctx::Global);
        }
        stack
    }

    /// Whether a command can run right now (drives hint visibility).
    pub fn enabled(&self, cmd: Command) -> bool {
        match cmd {
            Command::Actions => !self.core.target.remote,
            Command::Update => !self.core.target.remote && self.core.update_available().is_some(),
            Command::Search => self
                .tabs
                .get(self.core.tab, self.core.profile())
                .searchable(&self.core),
            Command::InstallFile => !self.core.target.remote,
            _ => true,
        }
    }

    /// Re-send the active subscriptions when they changed.
    pub fn sync_subs(&mut self) {
        let subs = self
            .tabs
            .get(self.core.tab, self.core.profile())
            .subs(&self.core);
        if subs != self.core.subs_sent {
            self.core.poller.set_active(subs.clone());
            self.core.subs_sent = subs;
        }
    }

    /// Switch tabs. A tab hidden on this profile refuses with a toast.
    pub fn activate_tab(&mut self, id: TabId) {
        let profile = self.core.profile().to_string();
        if !id.visible(&profile) {
            self.core.toast(
                ToastKind::Warn,
                format!("Not available on a {}", profile_label(&profile)),
            );
            return;
        }
        if id == self.core.tab {
            return;
        }
        self.tabs
            .get_mut(self.core.tab, &profile)
            .on_deactivate(&mut self.core);
        self.core.tab = id;
        self.core.store.prefs.last_tab = id.name().to_string();
        self.core.save_prefs();
        self.tabs.get_mut(id, &profile).on_activate(&mut self.core);
        self.core.pending_tab_fx = true;
        self.core.dirty = true;
        self.sync_subs();
    }

    fn cycle_tab(&mut self, forward: bool) {
        let profile = self.core.profile().to_string();
        let visible: Vec<TabId> = TabId::ALL
            .into_iter()
            .filter(|t| t.visible(&profile))
            .collect();
        let pos = visible
            .iter()
            .position(|t| *t == self.core.tab)
            .unwrap_or(0);
        let next = if forward {
            (pos + 1) % visible.len()
        } else {
            (pos + visible.len() - 1) % visible.len()
        };
        self.activate_tab(visible[next]);
    }

    // --- input -----------------------------------------------------------------

    pub fn on_key(&mut self, key: KeyEvent) {
        self.core.dirty = true;
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.core.quit = true;
            return;
        }
        if !self.core.overlays.is_empty() {
            self.overlay_key(key);
        } else {
            let stack = self.ctx_stack();
            match keymap::resolve(&stack, &key) {
                Some(cmd) => self.command(cmd),
                None if stack.first() == Some(&Ctx::Input) => {
                    let (tab, core) = self.active();
                    tab.on_text(core, key);
                }
                None => {}
            }
        }
        self.sync_subs();
    }

    /// Run a command: the active tab first, then the app default.
    pub fn command(&mut self, cmd: Command) {
        let (tab, core) = self.active();
        if tab.on_command(core, cmd) {
            return;
        }
        match cmd {
            Command::Tab(t) => self.activate_tab(t),
            Command::PrevTab => self.cycle_tab(false),
            Command::NextTab => self.cycle_tab(true),
            Command::Palette | Command::SearchAll => self.open_palette(),
            Command::Help => self.core.open(Overlay::Help),
            Command::Refresh => {
                self.core.auth.dismissed = false;
                self.core.refetch_all();
            }
            Command::Actions => {
                if self.core.target.remote {
                    self.core.toast(
                        ToastKind::Warn,
                        "Quick actions run on the agent host; not available remotely",
                    );
                } else {
                    self.core.open(Overlay::Actions(0));
                }
            }
            Command::Update => {
                if self.enabled(Command::Update) {
                    self.core.shell = Some(ShellReq::Update);
                }
            }
            Command::Quit => {
                let staged = self.tabs.params.staged_count();
                if staged > 0 {
                    self.core.open(Overlay::confirm(
                        Ret::new(Route::tab(self.core.tab), TAG_QUIT),
                        "Quit?",
                        vec![format!("{staged} staged parameter change(s) will be lost.")],
                    ));
                } else {
                    self.core.quit = true;
                }
            }
            Command::ForceQuit => self.core.quit = true,
            _ => {}
        }
    }

    fn overlay_key(&mut self, key: KeyEvent) {
        let history_len = self.core.toasts.history().len();
        let Some(top) = self.core.overlays.last_mut() else {
            return;
        };
        match overlay::handle_key(top, key, ACTIONS.len(), history_len) {
            Outcome::None => {}
            Outcome::Close => {
                if let Some(Overlay::Auth(_)) = self.core.overlays.pop() {
                    self.core.auth.dismissed = true;
                }
            }
            Outcome::Copy(text) => self.core.copy(&text),
            Outcome::Done(ret, result) => {
                self.core.overlays.pop();
                self.deliver(ret, result);
            }
            Outcome::Palette(action) => {
                self.core.overlays.pop();
                self.run_palette(action);
            }
            Outcome::Action(i) => {
                self.core.overlays.pop();
                self.core.shell = Some(ShellReq::Action(i));
            }
            Outcome::Update => {
                self.core.overlays.pop();
                if !self.core.target.remote {
                    self.core.shell = Some(ShellReq::Update);
                }
            }
            Outcome::AuthSubmit(text) => self.auth_submit(text),
            Outcome::AuthRemember(yes) => self.auth_remember(yes),
        }
    }

    /// Hand an overlay result to the screen that asked.
    pub fn deliver(&mut self, ret: Ret, result: OverlayResult) {
        if ret.tag == TAG_QUIT {
            if result == OverlayResult::Confirmed {
                self.core.quit = true;
            }
            return;
        }
        let (tab, core) = self.routed(ret.route);
        tab.on_overlay(core, ret, result);
    }

    pub fn on_mouse(&mut self, ev: MouseEvent) {
        if !self.core.overlays.is_empty() {
            return;
        }
        match ev.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let pos = Position::new(ev.column, ev.row);
                let hit = self
                    .core
                    .last_hits
                    .iter()
                    .rev()
                    .find(|(r, _)| r.contains(pos))
                    .map(|(_, h)| h.clone());
                match hit {
                    Some(Hit::Tab(t)) => self.activate_tab(t),
                    Some(h) => {
                        let (tab, core) = self.active();
                        tab.on_click(core, &h);
                    }
                    None => {}
                }
            }
            MouseEventKind::ScrollUp => self.command(Command::Up),
            MouseEventKind::ScrollDown => self.command(Command::Down),
            MouseEventKind::Moved => {
                let pos = Position::new(ev.column, ev.row);
                let hover = self.core.last_hits.iter().find_map(|(r, h)| match h {
                    Hit::Tab(t) if r.contains(pos) => Some(*t),
                    _ => None,
                });
                if hover == self.core.hover_tab {
                    return;
                }
                self.core.hover_tab = hover;
            }
            _ => return,
        }
        self.core.dirty = true;
        self.sync_subs();
    }

    // --- messages --------------------------------------------------------------

    /// Drain the poller and worker, tick the active tab, expire toasts.
    pub fn pump(&mut self) {
        self.core.now = Instant::now();
        let msgs: Vec<PollMsg> = self.core.poller.drain().collect();
        for msg in msgs {
            self.core.dirty = true;
            match msg {
                PollMsg::Global(result) => {
                    self.core.view.apply(*result, self.core.now, now_hms());
                    self.after_global();
                }
                PollMsg::Resource(id, result) => {
                    let slot = self.core.res.entry(id.clone()).or_insert(Slot {
                        value: None,
                        error: None,
                        code: None,
                        error_body: None,
                        at: self.core.now,
                    });
                    slot.at = self.core.now;
                    match result {
                        Ok(v) => {
                            slot.value = Some(v);
                            slot.error = None;
                            slot.code = None;
                            slot.error_body = None;
                        }
                        Err(e) => {
                            slot.code = e.code();
                            slot.error = Some(super::error_text(&e));
                            slot.error_body = e.body().cloned();
                            self.check_auth(&e, true);
                        }
                    }
                    let (tab, core) = self.active();
                    tab.on_resource(core, &id);
                }
            }
        }
        let replies: Vec<WorkerMsg> = self.core.worker.drain().collect();
        for msg in replies {
            self.core.dirty = true;
            match msg {
                WorkerMsg::Progress {
                    route,
                    name,
                    value,
                    result,
                } => {
                    let (tab, core) = self.routed(route);
                    tab.on_progress(core, name, value, result);
                }
                WorkerMsg::Done(reply) => {
                    if super::auth::owns(&reply) {
                        super::auth::on_reply(&mut self.core, reply);
                        continue;
                    }
                    if let Err(e) = &reply.result {
                        self.check_auth(e, false);
                    }
                    if let Purpose::Generic { ok_toast } = &reply.purpose {
                        match &reply.result {
                            Ok(_) => self.core.toast(ToastKind::Success, ok_toast.clone()),
                            Err(e) => self.core.toast_error("", e),
                        }
                    }
                    let (tab, core) = self.routed(reply.route);
                    tab.on_reply(core, reply);
                }
            }
        }
        {
            let (tab, core) = self.active();
            tab.tick(core);
        }
        if self.core.toasts.prune(self.core.now) {
            self.core.dirty = true;
        }
        // Raise the launch update splash once, the first time an update is seen
        // (never over another overlay, never for a remote agent).
        if !self.core.update_splash_done
            && self.core.overlays.is_empty()
            && !self.core.target.remote
            && self.core.update_available().is_some()
        {
            self.core.update_splash_done = true;
            self.core.open(Overlay::UpdateSplash);
        }
        self.sync_subs();
    }

    fn after_global(&mut self) {
        let profile = self.core.profile().to_string();
        // The active tab may have just become hidden (the profile arrived).
        if !self.core.tab.visible(&profile) {
            self.core.tab = TabId::Overview;
            self.tabs.overview.on_activate(&mut self.core);
        }
        for tab in self.tabs.all_mut() {
            tab.on_global(&mut self.core);
        }
        if let Some(code) = self.core.view.error_code {
            super::auth::on_denied(&mut self.core, code, true);
        }
        // A remote agent's hotspot passphrase is not this machine's file.
        if self.core.target.remote {
            if let Some(d) = self.core.view.dash.as_mut() {
                d.hotspot_passphrase.clear();
            }
        }
    }

    /// Open the auth prompt when a refusal is about credentials (see
    /// [`super::auth::on_denied`]). `read` is false for an operator's write.
    fn check_auth(&mut self, err: &RestError, read: bool) {
        if let Some(code) = err.code() {
            super::auth::on_denied(&mut self.core, code, read);
        }
    }

    fn auth_submit(&mut self, text: String) {
        super::auth::submit(&mut self.core, text);
    }

    fn auth_remember(&mut self, yes: bool) {
        super::auth::remember(&mut self.core, yes);
    }

    // --- palette ---------------------------------------------------------------

    fn open_palette(&mut self) {
        let mut items: Vec<PaletteItem> = Vec::new();
        let profile = self.core.profile().to_string();
        for t in TabId::ALL.into_iter().filter(|t| t.visible(&profile)) {
            items.push(PaletteItem::new(
                t.label(&profile),
                "tab",
                PaletteAction::Tab(t),
            ));
        }
        items.extend(self.tabs.settings.palette_items(&self.core));
        items.extend(self.tabs.params.palette_items());
        if !self.core.target.remote {
            for (i, a) in ACTIONS.iter().enumerate() {
                items.push(PaletteItem::new(
                    a.label,
                    "action",
                    PaletteAction::QuickAction(i),
                ));
            }
        }
        items.push(PaletteItem::new(
            "Cycle theme",
            "command",
            PaletteAction::Cmd(PaletteCmd::CycleTheme),
        ));
        items.push(PaletteItem::new(
            "Cycle motion",
            "command",
            PaletteAction::Cmd(PaletteCmd::CycleMotion),
        ));
        items.push(PaletteItem::new(
            "Notifications",
            "command",
            PaletteAction::Cmd(PaletteCmd::Notifications),
        ));
        items.push(PaletteItem::new(
            "Copy agent URLs",
            "command",
            PaletteAction::Cmd(PaletteCmd::CopyUrls),
        ));
        self.core
            .open(Overlay::Palette(Box::new(PaletteState::new(items, ""))));
    }

    fn run_palette(&mut self, action: PaletteAction) {
        match action {
            PaletteAction::Tab(t) => self.activate_tab(t),
            PaletteAction::SettingsPage(id) => {
                self.activate_tab(TabId::Settings);
                self.tabs.settings.goto_page(&mut self.core, &id);
            }
            PaletteAction::ConfigKey(key) => {
                self.activate_tab(TabId::Settings);
                self.tabs.settings.goto_key(&mut self.core, &key);
            }
            PaletteAction::Param(name) => {
                self.activate_tab(TabId::Params);
                self.tabs.params.goto(&mut self.core, &name);
            }
            PaletteAction::QuickAction(i) => {
                if !self.core.target.remote {
                    self.core.shell = Some(ShellReq::Action(i));
                }
            }
            PaletteAction::Cmd(PaletteCmd::CycleTheme) => {
                let cur = ThemeId::from_name(&self.core.store.prefs.theme);
                let pos = ThemeId::ALL.iter().position(|t| *t == cur).unwrap_or(0);
                let next = ThemeId::ALL[(pos + 1) % ThemeId::ALL.len()];
                self.core.store.prefs.theme = next.name().to_string();
                self.core.apply_prefs();
                self.core.save_prefs();
                self.core
                    .toast(ToastKind::Info, format!("Theme: {}", next.name()));
            }
            PaletteAction::Cmd(PaletteCmd::CycleMotion) => {
                const LEVELS: [&str; 4] = ["auto", "full", "reduced", "off"];
                let cur = LEVELS
                    .iter()
                    .position(|l| *l == self.core.store.prefs.motion)
                    .unwrap_or(0);
                let next = LEVELS[(cur + 1) % LEVELS.len()];
                self.core.store.prefs.motion = next.to_string();
                self.core.apply_prefs();
                self.core.save_prefs();
                self.core.toast(ToastKind::Info, format!("Motion: {next}"));
            }
            PaletteAction::Cmd(PaletteCmd::Notifications) => {
                self.core.open(Overlay::Notifications(0))
            }
            PaletteAction::Cmd(PaletteCmd::CopyUrls) => {
                let urls: Vec<String> = self
                    .core
                    .dash()
                    .map(|d| d.access_urls.iter().map(|u| u.url.clone()).collect())
                    .unwrap_or_default();
                if urls.is_empty() {
                    self.core
                        .toast(ToastKind::Warn, "No agent URLs reported yet");
                } else {
                    let text = urls.join("\n");
                    self.core.copy(&text);
                }
            }
        }
        self.sync_subs();
    }
}

/// Look up a value inside a resource body by dotted path.
pub fn at<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').try_fold(v, |acc, k| acc.get(k))
}
