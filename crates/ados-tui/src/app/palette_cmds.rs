//! The command palette: building its items and running the chosen one.

use super::{App, ShellReq, TabId};
use crate::action::ACTIONS;
use crate::overlay::Overlay;
use crate::palette::{PaletteAction, PaletteCmd, PaletteItem, PaletteState};
use crate::theme::ThemeId;
use crate::toast::ToastKind;

impl App {
    pub(super) fn open_palette(&mut self) {
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

    pub(super) fn run_palette(&mut self, action: PaletteAction) {
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
