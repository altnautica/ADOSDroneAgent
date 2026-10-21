//! Tab 6, Extensions: the installed plugins and the catalog on the left; the
//! selected plugin's detail (identity, permissions, MCP tools, services,
//! published state) and its config rows on the right; lifecycle and install
//! actions.

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use serde_json::Value;

use crate::api_path::seg;
use crate::app::{Core, FrameOut, Hit, ShellReq, Tab, TabId};
use crate::ext::install::{Installer, TAG_PERMS};
use crate::ext::state::{self, CatalogEntry, Install};
use crate::ext::actions;
use crate::keymap::{Command, Ctx};
use crate::overlay::{Overlay, OverlayResult, Ret};
use crate::poll::{ResourceId, Sub};
use crate::settings::page::Bespoke;
use crate::settings::plugin_rows::{self, PluginConfigPane};
use crate::toast::ToastKind;
use crate::worker::{Purpose, Reply, Route};

const ROUTE: Route = Route::tab(TabId::Extensions);
const TAG_URL: &str = "ext.url";
const TAG_SHA: &str = "ext.sha";
const TAG_FILE: &str = "ext.file";

/// Which pane has the keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Focus {
    #[default]
    List,
    Config,
    Detail,
}

/// One left-list entry.
#[derive(Debug, Clone)]
enum Entry {
    Installed(Install),
    Catalog(CatalogEntry),
}

impl Entry {
    /// What the selection remembers: the section and the plugin id.
    fn key(&self) -> (bool, String) {
        match self {
            Entry::Installed(i) => (true, i.id.clone()),
            Entry::Catalog(c) => (false, c.id.clone()),
        }
    }
}

#[derive(Default)]
pub struct ExtensionsTab {
    sel: usize,
    /// The selected entry (installed?, plugin id), so a list refresh that
    /// reorders or grows the list keeps the same plugin selected.
    sel_key: Option<(bool, String)>,
    list_offset: usize,
    focus: Focus,
    detail_scroll: u16,
    detail_len: u16,
    pane: Option<PluginConfigPane>,
    installer: Installer,
}

fn entries(core: &Core) -> (Vec<Install>, Vec<CatalogEntry>) {
    let installs = core.value(&ResourceId::Plugins).map(state::installs).unwrap_or_default();
    let catalog = core.value(&ResourceId::PluginsCatalog).map(state::catalog).unwrap_or_default();
    let catalog = state::not_installed(catalog, &installs);
    (installs, catalog)
}

/// The display name of an installed plugin: the manifest name, else the
/// catalog name, else the id.
fn name_of(core: &Core, id: &str) -> String {
    core.value(&ResourceId::PluginDetail(id.to_string()))
        .and_then(|d| crate::app::at(d, "manifest.name"))
        .and_then(Value::as_str)
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .or_else(|| {
            core.value(&ResourceId::PluginsCatalog)
                .map(state::catalog)
                .and_then(|c| c.into_iter().find(|e| e.id == id).map(|e| e.name))
        })
        .unwrap_or_else(|| id.to_string())
}

impl ExtensionsTab {
    fn selected(&self, core: &Core) -> Option<Entry> {
        let (installs, catalog) = entries(core);
        let n = installs.len();
        if self.sel < n {
            installs.into_iter().nth(self.sel).map(Entry::Installed)
        } else {
            catalog.into_iter().nth(self.sel - n).map(Entry::Catalog)
        }
    }

    fn selected_install(&self, core: &Core) -> Option<Install> {
        match self.selected(core) {
            Some(Entry::Installed(i)) => Some(i),
            _ => None,
        }
    }

    fn count(core: &Core) -> usize {
        let (i, c) = entries(core);
        i.len() + c.len()
    }

    /// Keep the config pane on the selected installed plugin.
    fn sync_pane(&mut self, core: &Core) {
        let want = self.selected_install(core).map(|i| i.id);
        if self.pane.as_ref().map(|p| p.plugin_id().to_string()) != want {
            self.pane = want.map(|id| PluginConfigPane::new(id, ROUTE));
            if self.focus == Focus::Config {
                self.focus = Focus::List;
            }
        }
    }

    fn has_config(&self, core: &Core) -> bool {
        self.pane.as_ref().is_some_and(|p| p.has_rows(core))
    }

    /// Select index `sel`; the detail scroll resets only when this lands on a
    /// different plugin than before.
    fn select(&mut self, core: &Core, sel: usize) {
        let n = Self::count(core);
        self.sel = sel.min(n.saturating_sub(1));
        let key = self.selected(core).map(|e| e.key());
        if key != self.sel_key {
            self.detail_scroll = 0;
            self.sel_key = key;
        }
        self.sync_pane(core);
    }

    /// After a list refresh: find the remembered plugin again (an install moves
    /// it from the catalog to the installed section), else keep the index.
    fn reselect(&mut self, core: &Core) {
        let found = self.sel_key.as_ref().and_then(|(_, id)| {
            let (installs, catalog) = entries(core);
            installs
                .iter()
                .position(|i| i.id == *id)
                .or_else(|| catalog.iter().position(|c| c.id == *id).map(|p| p + installs.len()))
        });
        self.select(core, found.unwrap_or(self.sel));
    }

    fn cycle_focus(&mut self, core: &Core, forward: bool) {
        let mut order = vec![Focus::List];
        if self.selected(core).is_some() {
            if self.has_config(core) {
                order.push(Focus::Config);
            }
            order.push(Focus::Detail);
        }
        let pos = order.iter().position(|f| *f == self.focus).unwrap_or(0);
        let n = order.len();
        self.focus = order[if forward { (pos + 1) % n } else { (pos + n - 1) % n }];
    }

    fn nav(&mut self, core: &mut Core, cmd: Command) {
        match self.focus {
            Focus::List => {
                let n = Self::count(core) as isize;
                let step = match cmd {
                    Command::Up => -1,
                    Command::Down => 1,
                    Command::PageUp => -10,
                    Command::PageDown => 10,
                    Command::Top => -n,
                    _ => n,
                };
                let next = (self.sel as isize + step).clamp(0, (n - 1).max(0));
                self.select(core, next as usize);
            }
            Focus::Config => {
                if let Some(p) = self.pane.as_mut() {
                    p.on_command(core, cmd);
                }
            }
            Focus::Detail => {
                let max = self.detail_len.saturating_sub(1);
                self.detail_scroll = match cmd {
                    Command::Up => self.detail_scroll.saturating_sub(1),
                    Command::Down => (self.detail_scroll + 1).min(max),
                    Command::PageUp => self.detail_scroll.saturating_sub(10),
                    Command::PageDown => (self.detail_scroll + 10).min(max),
                    Command::Top => 0,
                    _ => max,
                };
            }
        }
    }

    fn activate(&mut self, core: &mut Core) {
        match (self.focus, self.selected(core)) {
            (Focus::Config, _) => {
                if let Some(p) = self.pane.as_mut() {
                    p.on_command(core, Command::Activate);
                }
            }
            (_, Some(Entry::Catalog(c))) => {
                self.installer
                    .start(core, ROUTE, c.download_url, Some(c.sha256), true, c.name);
            }
            (_, Some(Entry::Installed(_))) => {
                self.focus = if self.has_config(core) { Focus::Config } else { Focus::Detail };
            }
            (_, None) => {}
        }
    }

    /// Run a lifecycle action on the selected installed plugin.
    fn lifecycle(&mut self, core: &mut Core, cmd: Command) {
        let Some(inst) = self.selected_install(core) else {
            core.toast(ToastKind::Info, "Select an installed extension first");
            return;
        };
        let name = name_of(core, &inst.id);
        match cmd {
            Command::Enable => actions::toggle(core, ROUTE, &inst, &name, true),
            Command::Disable => actions::toggle(core, ROUTE, &inst, &name, false),
            Command::Remove => actions::remove(core, ROUTE, &inst, &name),
            Command::Grant => actions::grant(core, ROUTE, &inst, &name),
            _ => actions::revoke(core, ROUTE, &inst, &name),
        }
    }

    fn install_file(&self, core: &mut Core) {
        if core.target.remote {
            core.toast(
                ToastKind::Warn,
                "Local file install runs on the agent's own terminal; use I (install from URL) for a remote agent",
            );
            return;
        }
        if !core.guard_disarmed() {
            return;
        }
        let mut s = Overlay::input(Ret::new(ROUTE, TAG_FILE), "Install from file", "Path to a .adosplug file", "");
        s.validate = Some(|t: &str| {
            if t.trim().ends_with(".adosplug") {
                Ok(())
            } else {
                Err("Enter the path of a .adosplug file".into())
            }
        });
        s.hint = Some("Runs `ados plugin install <path>`, which asks for permissions itself".into());
        core.open(Overlay::Input(s));
    }

    fn copy(&mut self, core: &mut Core) {
        match (self.focus, self.selected(core)) {
            (Focus::Config, _) => {
                if let Some(p) = self.pane.as_mut() {
                    p.on_command(core, Command::Copy);
                }
            }
            (_, Some(Entry::Installed(i))) => core.copy(&i.id),
            (_, Some(Entry::Catalog(c))) => core.copy(&c.download_url),
            (_, None) => {}
        }
    }
}

impl Tab for ExtensionsTab {
    fn ctx(&self, _core: &Core) -> Vec<Ctx> {
        vec![Ctx::Extensions, Ctx::List]
    }

    fn subs(&self, core: &Core) -> Vec<Sub> {
        let mut subs = vec![
            Sub::every_ms(ResourceId::Plugins, "/api/plugins", 8_000),
            Sub::once(ResourceId::PluginsCatalog, "/api/v1/plugins/catalog"),
        ];
        if let Some(inst) = self.selected_install(core) {
            let id = &inst.id;
            subs.extend(PluginConfigPane::new(id.clone(), ROUTE).subs(core));
            subs.push(Sub::every_ms(
                ResourceId::PluginReadiness(id.clone()),
                format!("/api/plugins/{}/readiness", seg(id)),
                5_000,
            ));
            subs.push(Sub::every_ms(
                ResourceId::PluginState(id.clone()),
                format!("/api/plugins/{}/state", seg(id)),
                5_000,
            ));
        }
        subs
    }

    fn on_activate(&mut self, core: &mut Core) {
        self.sync_pane(core);
    }

    fn on_resource(&mut self, core: &mut Core, id: &ResourceId) {
        if matches!(id, ResourceId::Plugins | ResourceId::PluginsCatalog) {
            self.reselect(core);
        }
    }

    fn on_command(&mut self, core: &mut Core, cmd: Command) -> bool {
        self.sync_pane(core);
        match cmd {
            Command::Up | Command::Down | Command::PageUp | Command::PageDown | Command::Top | Command::Bottom => {
                self.nav(core, cmd)
            }
            Command::Left | Command::Right if self.focus == Focus::Config => {
                if let Some(p) = self.pane.as_mut() {
                    p.on_command(core, cmd);
                }
            }
            Command::NextPane => self.cycle_focus(core, true),
            Command::PrevPane => self.cycle_focus(core, false),
            Command::Activate => self.activate(core),
            Command::Enable | Command::Disable | Command::Remove | Command::Grant | Command::RevokePerm => {
                self.lifecycle(core, cmd)
            }
            Command::InstallUrl => {
                let mut s = Overlay::input(Ret::new(ROUTE, TAG_URL), "Install from URL", "Archive URL (.adosplug)", "");
                s.validate = Some(state::validate_url);
                s.hint = Some("The agent only downloads from its allowlisted hosts".into());
                core.open(Overlay::Input(s));
            }
            Command::InstallFile => self.install_file(core),
            Command::Copy => self.copy(core),
            Command::Back if self.focus != Focus::List => self.focus = Focus::List,
            _ => return false,
        }
        true
    }

    fn on_overlay(&mut self, core: &mut Core, ret: Ret, result: OverlayResult) {
        match ret.tag {
            plugin_rows::TAG => {
                let id = ret.data.get("plugin").and_then(Value::as_str).unwrap_or_default().to_string();
                match self.pane.as_mut().filter(|p| p.plugin_id() == id) {
                    Some(p) => p.on_overlay(core, ret, result),
                    None => PluginConfigPane::new(id, ROUTE).on_overlay(core, ret, result),
                }
            }
            TAG_PERMS => self.installer.on_perms(core, ROUTE, result),
            TAG_URL => {
                if let OverlayResult::Text(url) = result {
                    let ret = Ret::new(ROUTE, TAG_SHA).with(serde_json::json!({"url": url.trim()}));
                    let mut s = Overlay::input(ret, "Install from URL", "Archive sha256 (optional)", "");
                    s.validate = Some(state::validate_sha);
                    s.hint = Some("Empty: the install is pinned to the archive the preview verified".into());
                    core.open(Overlay::Input(s));
                }
            }
            TAG_SHA => {
                if let OverlayResult::Text(sha) = result {
                    let url = ret.data.get("url").and_then(Value::as_str).unwrap_or_default().to_string();
                    let name = url.rsplit('/').next().unwrap_or(&url).to_string();
                    self.installer.start(core, ROUTE, url, Some(sha), false, name);
                }
            }
            TAG_FILE => {
                if let OverlayResult::Text(path) = result {
                    if core.guard_disarmed() {
                        core.shell_arg = Some(path.trim().to_string());
                        core.shell = Some(ShellReq::PluginInstall);
                    }
                }
            }
            _ => {
                actions::on_overlay(core, ROUTE, &ret, &result);
            }
        }
    }

    fn on_reply(&mut self, core: &mut Core, reply: Reply) {
        match &reply.purpose {
            Purpose::PluginParse => self.installer.on_parse(core, ROUTE, &reply.result),
            Purpose::PluginInstall => self.installer.on_install(core, ROUTE, &reply.result),
            Purpose::Page { tag: plugin_rows::TAG, data } => {
                let id = data.get("plugin").and_then(Value::as_str).unwrap_or_default().to_string();
                match self.pane.as_mut().filter(|p| p.plugin_id() == id) {
                    Some(p) => p.on_reply(core, reply),
                    None => PluginConfigPane::new(id, ROUTE).on_reply(core, reply),
                }
            }
            _ => {
                actions::on_reply(core, &reply);
                if let Some(i) = self.selected_install(core) {
                    core.fetch_now(ResourceId::PluginDetail(i.id));
                }
            }
        }
    }

    fn on_click(&mut self, core: &mut Core, hit: &Hit) {
        match hit {
            Hit::Sidebar(i) => {
                self.focus = Focus::List;
                self.select(core, *i);
            }
            Hit::Row(_) => {
                self.focus = Focus::Config;
                if let Some(p) = self.pane.as_mut() {
                    p.on_click(core, hit);
                }
            }
            _ => {}
        }
    }

    fn animating(&self, _core: &Core) -> bool {
        self.installer.status().is_some()
    }

    fn render(&mut self, f: &mut Frame, area: Rect, core: &Core, out: &mut FrameOut) {
        self.sync_pane(core);
        let left_w = if area.width < 100 { 28 } else { 36 }.min(area.width / 2);
        let left = Rect::new(area.x, area.y, left_w, area.height);
        let rule = Rect::new(area.x + left_w, area.y, 1, area.height);
        let right = Rect::new(
            area.x + left_w + 2,
            area.y,
            area.width.saturating_sub(left_w + 2),
            area.height,
        );
        self.render_list(f, left, core, out);
        let bar = if core.sym.name == "ascii" { "|" } else { "│" };
        let rule_lines: Vec<Line> = (0..area.height)
            .map(|_| Line::from(Span::styled(bar, Style::default().fg(core.theme.border()))))
            .collect();
        f.render_widget(Paragraph::new(rule_lines), rule);
        self.render_detail(f, right, core, out);
    }

    fn context_line(&self, core: &Core) -> Option<String> {
        match (self.focus, self.selected(core)) {
            (Focus::Config, _) => self.pane.as_ref().and_then(|p| Bespoke::context_line(p, core)),
            (Focus::Detail, _) => Some("Detail: ↑↓ scroll · Tab next pane · Esc back to the list".into()),
            (_, Some(Entry::Catalog(c))) => Some(c.description).filter(|d| !d.is_empty()),
            (_, Some(Entry::Installed(i))) => i.failure_reason.or_else(|| {
                self.has_config(core)
                    .then(|| "Enter or Tab: edit this extension's config rows".to_string())
            }),
            (_, None) => None,
        }
    }
}

#[path = "extensions_view.rs"]
mod view;

#[cfg(test)]
#[path = "extensions_tests.rs"]
mod tests;
