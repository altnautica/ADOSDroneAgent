//! Tab 9, Settings: a sidebar of grouped pages, a vertical rule, and the
//! focused page. Row pages (agent config keys, terminal preferences, All
//! settings) are drawn and edited here; bespoke and extension pages draw
//! themselves and receive keys, replies and overlay results through the
//! [`Bespoke`] trait.

use std::collections::HashMap;

use ratatui::crossterm::event::{Event, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use serde_json::Value;
use tui_input::backend::crossterm::EventHandler;
use tui_input::Input;

use crate::app::{Core, FrameOut, Hit, Tab};
use crate::keymap::{Command, Ctx};
use crate::overlay::{OverlayResult, Ret};
use crate::palette::{PaletteAction, PaletteItem};
use crate::poll::{ResourceId, Sub};
use crate::settings::all_settings;
use crate::settings::config_rows;
use crate::settings::page::Bespoke;
use crate::settings::pages::Pages;
use crate::settings::plugin_rows::{self, PluginConfigPane};
use crate::settings::registry::{self, PageBody, PAGES};
use crate::settings::rows_view::{self, Item, RowsState};
use crate::worker::{Purpose, Reply, Route};

mod rowpage;
mod sidebar;
#[cfg(test)]
mod tests;

use sidebar::Entry;

/// The route page id extension config panes reply under.
const PLUGIN_ROUTE: &str = "plugin";

#[derive(Default)]
pub struct SettingsTab {
    /// The selected page id; empty selects the first page.
    page: String,
    /// Focus and scroll per row page.
    rows: HashMap<String, RowsState>,
    /// The `/` filter of the row page, and whether it is being typed.
    filter: Input,
    filtering: bool,
    pages: Pages,
    plugins: Vec<PluginConfigPane>,
    side_offset: usize,
    /// Labels of in-flight config writes, by flash key.
    pending: HashMap<String, String>,
    /// A config key to focus once the schema has loaded.
    pending_key: Option<String>,
}

impl SettingsTab {
    /// Palette entries: every page shown on this profile (extension pages
    /// included), and every config key once the schema has loaded.
    pub fn palette_items(&self, core: &Core) -> Vec<PaletteItem> {
        let mut out: Vec<PaletteItem> = sidebar::entries(core)
            .into_iter()
            .map(|e| {
                PaletteItem::new(
                    format!("Settings › {}", e.title),
                    "setting",
                    PaletteAction::SettingsPage(e.id),
                )
            })
            .collect();
        if let Some(leaves) = config_rows::leaves(core) {
            out.extend(leaves.iter().map(|l| {
                let label = config_rows::label_of(&l.key, Some(l));
                PaletteItem::new(
                    format!("{} · {label}", l.key),
                    "config",
                    PaletteAction::ConfigKey(l.key.clone()),
                )
            }));
        }
        out
    }

    /// Open page `id` (when it shows on this profile).
    pub fn goto_page(&mut self, core: &mut Core, id: &str) {
        if self.entry(core, id).is_some() {
            self.select(core, id.to_string());
        }
    }

    /// Open the All settings row of config `key`.
    pub fn goto_key(&mut self, core: &mut Core, key: &str) {
        self.select(core, "all_settings".to_string());
        self.pending_key = Some(key.to_string());
        self.focus_pending(core);
    }

    fn focus_pending(&mut self, core: &Core) {
        let Some(key) = self.pending_key.clone() else {
            return;
        };
        let Some(entry) = self.entry(core, "all_settings") else {
            return;
        };
        if config_rows::leaves(core).is_none() {
            return;
        }
        if let Some(items) = self.items(core, &entry) {
            self.state_mut("all_settings").focus_key(&items, &key);
            self.pending_key = None;
        }
    }

    fn entry(&self, core: &Core, id: &str) -> Option<Entry> {
        sidebar::entries(core).into_iter().find(|e| e.id == id)
    }

    /// The focused page: the selected one, else the first shown.
    fn current(&self, core: &Core) -> Entry {
        let entries = sidebar::entries(core);
        let pos = entries.iter().position(|e| e.id == self.page).unwrap_or(0);
        entries.into_iter().nth(pos).unwrap_or_else(|| Entry {
            id: PAGES[0].id.to_string(),
            title: PAGES[0].title.to_string(),
            group: PAGES[0].group,
            body: PAGES[0].body,
        })
    }

    fn state(&self, id: &str) -> RowsState {
        self.rows.get(id).cloned().unwrap_or_default()
    }

    fn state_mut(&mut self, id: &str) -> &mut RowsState {
        self.rows.entry(id.to_string()).or_default()
    }

    fn select(&mut self, core: &mut Core, id: String) {
        if id == self.page {
            return;
        }
        self.page = id;
        self.filter = Input::default();
        self.filtering = false;
        self.sync_plugins(core);
        let entry = self.current(core);
        if let Some(page) = self.bespoke_mut(&entry) {
            page.on_activate(core);
        }
    }

    fn step_page(&mut self, core: &mut Core, forward: bool) {
        let entries = sidebar::entries(core);
        if entries.is_empty() {
            return;
        }
        let cur = self.current(core);
        let pos = entries.iter().position(|e| e.id == cur.id).unwrap_or(0);
        let n = entries.len();
        let next = if forward {
            (pos + 1) % n
        } else {
            (pos + n - 1) % n
        };
        self.select(core, entries[next].id.clone());
    }

    /// Keep one config pane per configurable extension.
    fn sync_plugins(&mut self, core: &Core) {
        let ids: Vec<String> = plugin_rows::configurable(core)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        self.plugins
            .retain(|p| ids.iter().any(|id| id == p.plugin_id()));
        for id in ids {
            if !self.plugins.iter().any(|p| p.plugin_id() == id) {
                self.plugins
                    .push(PluginConfigPane::new(id, Route::page(PLUGIN_ROUTE)));
            }
        }
    }

    fn pane_mut(&mut self, plugin: &str) -> Option<&mut PluginConfigPane> {
        self.plugins.iter_mut().find(|p| p.plugin_id() == plugin)
    }

    fn bespoke(&self, entry: &Entry) -> Option<&dyn Bespoke> {
        match entry.body {
            PageBody::Bespoke(b) => Some(self.pages.get(b)),
            PageBody::Plugin => {
                let id = entry.plugin_id()?;
                self.plugins
                    .iter()
                    .find(|p| p.plugin_id() == id)
                    .map(|p| p as &dyn Bespoke)
            }
            _ => None,
        }
    }

    fn bespoke_mut(&mut self, entry: &Entry) -> Option<&mut dyn Bespoke> {
        match entry.body {
            PageBody::Bespoke(b) => Some(self.pages.get_mut(b)),
            PageBody::Plugin => {
                let id = entry.plugin_id()?.to_string();
                self.pane_mut(&id).map(|p| p as &mut dyn Bespoke)
            }
            _ => None,
        }
    }

    /// The page a routed reply or overlay belongs to.
    fn routed_mut(
        &mut self,
        core: &Core,
        page: &str,
        data: Option<&Value>,
    ) -> Option<&mut dyn Bespoke> {
        if page == PLUGIN_ROUTE {
            let named = data
                .and_then(|d| d.get("plugin"))
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| self.current(core).plugin_id().map(str::to_string))?;
            return self.pane_mut(&named).map(|p| p as &mut dyn Bespoke);
        }
        let b = registry::bespoke_by_id(page)?;
        Some(self.pages.get_mut(b))
    }

    /// Row-list navigation shared by every row page.
    fn navigate(&mut self, core: &Core, entry: &Entry, cmd: Command) -> bool {
        let Some(items) = self.items(core, entry) else {
            return false;
        };
        let all = entry.body == PageBody::AllSettings;
        let st = self.state_mut(&entry.id);
        match cmd {
            Command::Up => st.step(&items, false),
            Command::Down => st.step(&items, true),
            Command::PageUp if all => all_settings::jump_section(&items, st, false),
            Command::PageDown if all => all_settings::jump_section(&items, st, true),
            Command::PageUp => st.jump(&items, -10),
            Command::PageDown => st.jump(&items, 10),
            Command::Top => st.jump(&items, isize::MIN / 2),
            Command::Bottom => st.jump(&items, isize::MAX / 2),
            _ => return false,
        }
        true
    }
}

impl Tab for SettingsTab {
    fn ctx(&self, core: &Core) -> Vec<Ctx> {
        if self.filtering {
            return vec![Ctx::Input];
        }
        let entry = self.current(core);
        let mut out = Vec::with_capacity(3);
        if let Some(keys) = self.bespoke(&entry).and_then(|p| p.keys()) {
            out.push(Ctx::Page(keys));
        }
        out.extend([Ctx::Settings, Ctx::List]);
        out
    }

    fn subs(&self, core: &Core) -> Vec<Sub> {
        let entry = self.current(core);
        let mut subs = match entry.body {
            PageBody::Config(_) | PageBody::AllSettings => config_rows::subs(core),
            PageBody::Prefs(_) => Vec::new(),
            PageBody::Bespoke(_) | PageBody::Plugin => self
                .bespoke(&entry)
                .map(|p| p.subs(core))
                .unwrap_or_default(),
        };
        // The schema once per session, for the palette's config keys.
        subs.extend(
            config_rows::subs(core)
                .into_iter()
                .filter(|s| s.id == ResourceId::ConfigSchema),
        );
        subs.extend(plugin_rows::listing_subs(core));
        let mut seen: Vec<ResourceId> = Vec::with_capacity(subs.len());
        subs.retain(|s| {
            if seen.contains(&s.id) {
                false
            } else {
                seen.push(s.id.clone());
                true
            }
        });
        subs
    }

    fn on_activate(&mut self, core: &mut Core) {
        self.sync_plugins(core);
        let entry = self.current(core);
        if let Some(page) = self.bespoke_mut(&entry) {
            page.on_activate(core);
        }
    }

    fn on_resource(&mut self, core: &mut Core, id: &ResourceId) {
        if matches!(id, ResourceId::ConfigSchema) {
            self.focus_pending(core);
        }
        self.sync_plugins(core);
        let entry = self.current(core);
        if let Some(page) = self.bespoke_mut(&entry) {
            page.on_resource(core, id);
        }
    }

    fn on_command(&mut self, core: &mut Core, cmd: Command) -> bool {
        if self.filtering {
            match cmd {
                Command::Submit => self.filtering = false,
                Command::Cancel => {
                    self.filtering = false;
                    self.filter = Input::default();
                }
                _ => return false,
            }
            return true;
        }
        match cmd {
            Command::NextPane => {
                self.step_page(core, true);
                return true;
            }
            Command::PrevPane => {
                self.step_page(core, false);
                return true;
            }
            _ => {}
        }
        let entry = self.current(core);
        if let Some(page) = self.bespoke_mut(&entry) {
            return page.on_command(core, cmd);
        }
        if self.navigate(core, &entry, cmd) {
            return true;
        }
        match cmd {
            Command::Left => self.cycle_focused(core, &entry, false),
            Command::Right => self.cycle_focused(core, &entry, true),
            Command::Activate => self.edit_focused(core, &entry),
            Command::Reveal => {
                if let Some(items) = self.items(core, &entry) {
                    let now = core.now;
                    self.state_mut(&entry.id).reveal(&items, now);
                }
            }
            Command::Copy => self.copy_focused(core, &entry),
            Command::Search => {
                self.filtering = true;
                self.state_mut(&entry.id).selected = 0;
            }
            Command::Back if !self.filter.value().is_empty() => self.filter = Input::default(),
            _ => return false,
        }
        true
    }

    fn on_text(&mut self, core: &mut Core, key: KeyEvent) {
        if !self.filtering {
            return;
        }
        self.filter.handle_event(&Event::Key(key));
        let id = self.current(core).id;
        self.state_mut(&id).selected = 0;
    }

    fn on_overlay(&mut self, core: &mut Core, ret: Ret, result: OverlayResult) {
        match ret.route.page {
            None => self.row_overlay(core, ret, result),
            Some(page) => {
                let data = ret.data.clone();
                if let Some(p) = self.routed_mut(core, page, Some(&data)) {
                    p.on_overlay(core, ret, result);
                }
            }
        }
    }

    fn on_reply(&mut self, core: &mut Core, reply: Reply) {
        match reply.route.page {
            None => self.row_reply(core, reply),
            Some(page) => {
                let data = match &reply.purpose {
                    Purpose::Page { data, .. } => Some(data.clone()),
                    _ => None,
                };
                if let Some(p) = self.routed_mut(core, page, data.as_ref()) {
                    p.on_reply(core, reply);
                }
            }
        }
    }

    fn on_click(&mut self, core: &mut Core, hit: &Hit) {
        match hit {
            Hit::Sidebar(i) => {
                if let Some(e) = sidebar::entries(core).into_iter().nth(*i) {
                    self.select(core, e.id);
                }
            }
            Hit::Item(sidebar::SECTION_HIT, i) => {
                let entry = self.current(core);
                let (Some(items), Some(leaves)) =
                    (self.items(core, &entry), config_rows::leaves(core))
                else {
                    return;
                };
                if let Some(section) = all_settings::sections(&leaves).get(*i) {
                    all_settings::focus_section(&items, self.state_mut(&entry.id), section);
                }
            }
            other => {
                let entry = self.current(core);
                if let Some(page) = self.bespoke_mut(&entry) {
                    page.on_click(core, other);
                } else if let (Hit::Row(i), Some(items)) = (other, self.items(core, &entry)) {
                    self.state_mut(&entry.id).select(&items, *i);
                }
            }
        }
    }

    fn animating(&self, core: &Core) -> bool {
        let entry = self.current(core);
        match self.bespoke(&entry) {
            Some(p) => p.animating(core),
            None => self
                .rows
                .get(&entry.id)
                .is_some_and(|s| s.revealing(core.now)),
        }
    }

    fn render(&mut self, f: &mut Frame, area: Rect, core: &Core, out: &mut FrameOut) {
        self.sync_plugins(core);
        let narrow = f.area().width < 100;
        let side_w = if narrow {
            sidebar::NARROW
        } else {
            sidebar::WIDE
        };
        let [side, rule, body] = Layout::horizontal([
            Constraint::Length(side_w),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .areas(area);
        let entries = sidebar::entries(core);
        let entry = self.current(core);
        let selected = entries.iter().position(|e| e.id == entry.id).unwrap_or(0);
        sidebar::render(
            f,
            side,
            core,
            &entries,
            selected,
            &mut self.side_offset,
            out,
        );
        sidebar::rule(f, rule, core);
        let body = Rect {
            x: body.x + 1,
            width: body.width.saturating_sub(1),
            ..body
        };
        if let Some(page) = self.bespoke_mut(&entry) {
            page.render(f, body, core, out);
            return;
        }
        let Some(items) = self.items(core, &entry) else {
            return;
        };
        let mut rows_area = body;
        if self.filtering || !self.filter.value().is_empty() {
            let line = Rect { height: 1, ..body };
            let text = Line::from(vec![
                Span::styled("/ ", core.theme.accent_bold()),
                Span::styled(self.filter.value().to_string(), core.theme.bright()),
            ]);
            f.render_widget(Paragraph::new(text), line);
            if self.filtering {
                let x = line.x + 2 + self.filter.visual_cursor() as u16;
                out.cursor = Some((x.min(line.right().saturating_sub(1)), line.y));
            }
            rows_area = Rect {
                y: body.y + 1,
                height: body.height.saturating_sub(1),
                ..body
            };
        }
        if let (PageBody::AllSettings, Some(leaves)) = (entry.body, config_rows::leaves(core)) {
            let sections = all_settings::sections(&leaves);
            let focused = all_settings::focused_section(&items, &self.state(&entry.id));
            let sel = focused
                .as_deref()
                .and_then(|s| sections.iter().position(|x| *x == s))
                .unwrap_or(0);
            rows_area = sidebar::sections(f, rows_area, core, &sections, sel, out);
        }
        let prefix = format!("{}/", entry.id);
        let st = self.state_mut(&entry.id);
        rows_view::render(f, rows_area, core, &items, st, &prefix, true, out);
    }

    fn context_line(&self, core: &Core) -> Option<String> {
        let entry = self.current(core);
        if let Some(page) = self.bespoke(&entry) {
            return page.context_line(core);
        }
        let items = self.items(core, &entry)?;
        if !self.filter.value().is_empty() && !items.iter().any(|i| matches!(i, Item::Row(_))) {
            return Some("No rows match the filter".into());
        }
        self.state(&entry.id)
            .focused_row(&items)
            .map(|r| r.help.clone())
            .filter(|h| !h.is_empty())
    }

    fn status_hint(&self, _core: &Core) -> Option<String> {
        (!self.filtering && !self.filter.value().is_empty())
            .then(|| format!("filter: {}", self.filter.value()))
    }

    fn searchable(&self, core: &Core) -> bool {
        self.bespoke(&self.current(core)).is_none()
    }
}
