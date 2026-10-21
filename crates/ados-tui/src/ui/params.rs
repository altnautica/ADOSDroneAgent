//! Tab 5, Params: the FC parameter editor.
//!
//! Live values come from `GET /api/params`, refetched when the agent's
//! `param_generation` counter (on `/api/telemetry`) moves and every 30 s while
//! the tab is open. Edits are staged locally and written as one sequential
//! batch. Metadata comes from the catalogs embedded in the binary.

use std::collections::HashMap;
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ados_protocol::rest::{ParamsResponse, RestError};
use ratatui::crossterm::event::{Event, KeyEvent};
use ratatui::layout::Rect;
use ratatui::Frame;
use serde_json::Value;
use tui_input::backend::crossterm::EventHandler;
use tui_input::Input;

use crate::app::{Core, FrameOut, Hit, Tab, TabId};
use crate::keymap::{Command, Ctx};
use crate::overlay::{OverlayResult, Ret};
use crate::palette::{PaletteAction, PaletteItem};
use crate::params::metadata::{self, Catalog};
use crate::params::store::{Filter, Store};
use crate::poll::{ResourceId, Sub};
use crate::worker::{Purpose, Reply, Route};

mod catalog;
mod edit;
#[cfg(test)]
mod tests;
mod view;

/// Search keystrokes settle this long before the list re-filters.
const DEBOUNCE: Duration = Duration::from_millis(150);
/// Fixed sidebar entries ahead of the groups.
const FIXED: usize = 4;
const ROUTE: Route = Route::tab(TabId::Params);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Focus {
    Sidebar,
    #[default]
    Table,
}

/// Whether this FC's parameters can be edited here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Avail {
    Ready,
    Msp,
    Waiting,
}

fn availability(core: &Core) -> Avail {
    let Some(d) = core.dash() else { return Avail::Waiting };
    match d.fc_firmware.as_deref() {
        Some("ardupilot") | Some("px4") => Avail::Ready,
        Some("betaflight") | Some("inav") => Avail::Msp,
        _ if matches!(d.fc_variant.as_deref(), Some("betaflight") | Some("inav")) => Avail::Msp,
        _ => Avail::Waiting,
    }
}

/// The sweep envelope around the parameter map.
#[derive(Debug, Clone, Copy, Default)]
struct Envelope {
    loaded: bool,
    priming: bool,
    timeout: bool,
    send_failed: bool,
    got: u64,
    expected: u64,
}

/// A parameter write in flight.
#[derive(Debug, Default)]
struct Batch {
    total: usize,
    done: usize,
    acked: usize,
    reboot: bool,
}

#[derive(Default)]
pub struct ParamsTab {
    store: Store,
    focus: Focus,
    side_sel: usize,
    side_off: usize,
    sel: usize,
    off: usize,
    /// The inline search field, while open.
    search: Option<Input>,
    /// When the search text last changed (debounce).
    pending: Option<Instant>,
    catalogs: HashMap<&'static str, Arc<Catalog>>,
    loading: Option<(&'static str, Receiver<Result<Catalog, String>>)>,
    failed: Vec<&'static str>,
    /// The catalog key favourites are kept under.
    fav_key: Option<&'static str>,
    last_gen: Option<u64>,
    env: Envelope,
    batch: Option<Batch>,
}

impl ParamsTab {
    pub fn staged_count(&self) -> usize {
        self.store.staged().len()
    }

    /// One palette entry per loaded parameter.
    pub fn palette_items(&self) -> Vec<PaletteItem> {
        self.store
            .live()
            .keys()
            .map(|name| {
                let label = match self.store.meta(name).map(|m| m.human_name.as_str()) {
                    Some(h) if !h.is_empty() => format!("{name}  {h}"),
                    _ => name.clone(),
                };
                PaletteItem::new(label, "param", PaletteAction::Param(name.clone()))
            })
            .collect()
    }

    /// Show `name` in the full list and select it.
    pub fn goto(&mut self, _core: &mut Core, name: &str) {
        self.search = None;
        self.pending = None;
        self.store.set_query("");
        self.store.set_filter(Filter::All);
        self.side_sel = 0;
        self.focus = Focus::Table;
        if let Some(i) = self.store.visible().iter().position(|n| n == name) {
            self.sel = i;
        }
    }

    fn focused(&self) -> Option<String> {
        self.store.visible().get(self.sel).cloned()
    }

    fn side_len(&self) -> usize {
        FIXED + self.store.groups().len()
    }

    fn side_filter(&self, i: usize) -> Filter {
        match i {
            0 => Filter::All,
            1 => Filter::Staged,
            2 => Filter::NonDefault,
            3 => Filter::Favourites,
            n => self
                .store
                .groups()
                .get(n - FIXED)
                .map(|(g, _)| Filter::Group(g.clone()))
                .unwrap_or(Filter::All),
        }
    }

    fn select_side(&mut self, i: usize) {
        self.side_sel = i.min(self.side_len().saturating_sub(1));
        self.store.set_filter(self.side_filter(self.side_sel));
        self.sel = 0;
        self.off = 0;
    }

    /// Keep the sidebar index on the active filter and the row in range.
    fn clamp(&mut self) {
        let want = match self.store.filter() {
            Filter::All => Some(0),
            Filter::Staged => Some(1),
            Filter::NonDefault => Some(2),
            Filter::Favourites => Some(3),
            Filter::Group(g) => self.store.groups().iter().position(|(n, _)| n == g).map(|p| p + FIXED),
        };
        match want {
            Some(i) => self.side_sel = i,
            // The group emptied out: fall back to everything.
            None => self.select_side(0),
        }
        let len = self.store.visible().len();
        self.sel = self.sel.min(len.saturating_sub(1));
    }

    fn step(&mut self, delta: isize) {
        let (cur, len) = match self.focus {
            Focus::Sidebar => (self.side_sel, self.side_len()),
            Focus::Table => (self.sel, self.store.visible().len()),
        };
        if len == 0 {
            return;
        }
        let next = (cur as isize + delta).clamp(0, len as isize - 1) as usize;
        match self.focus {
            Focus::Sidebar => self.select_side(next),
            Focus::Table => self.sel = next,
        }
    }

    // --- data --------------------------------------------------------------------

    fn on_params(&mut self, core: &Core) {
        let Some(value) = core.value(&ResourceId::Params) else { return };
        let Ok(resp) = serde_json::from_value::<ParamsResponse>(value.clone()) else { return };
        let progress = resp.progress.unwrap_or_default();
        self.env = Envelope {
            loaded: true,
            priming: resp.priming,
            timeout: resp.priming_timeout,
            send_failed: resp.priming_send_failed,
            got: progress.got,
            expected: progress.expected,
        };
        self.store.set_live(resp.params);
        self.clamp();
    }

    fn on_telemetry(&mut self, core: &Core) {
        let generation = core
            .value(&ResourceId::Telemetry)
            .and_then(|v| v.get("param_generation"))
            .and_then(Value::as_u64);
        let Some(generation) = generation else { return };
        if self.last_gen.is_some_and(|g| g != generation) {
            core.fetch_now(ResourceId::Params);
        }
        self.last_gen = Some(generation);
    }

    fn apply_search(&mut self) {
        if let Some(input) = &self.search {
            let q = input.value().to_string();
            self.store.set_query(&q);
        }
        self.pending = None;
        self.sel = 0;
        self.off = 0;
    }
}

impl Tab for ParamsTab {
    fn ctx(&self, _core: &Core) -> Vec<Ctx> {
        if self.search.is_some() {
            vec![Ctx::Input]
        } else {
            vec![Ctx::Params, Ctx::List]
        }
    }

    fn subs(&self, core: &Core) -> Vec<Sub> {
        let mut subs = vec![
            Sub::every_ms(ResourceId::Telemetry, "/api/telemetry", 2000),
            Sub::every_ms(ResourceId::Snapshot, "/api/v1/dashboard/snapshot", 5000),
        ];
        if availability(core) == Avail::Ready {
            // The sweep progress moves quickly while priming.
            let period = if self.env.priming { 2000 } else { 30_000 };
            subs.push(Sub::every_ms(ResourceId::Params, "/api/params", period));
        }
        subs
    }

    fn on_activate(&mut self, core: &mut Core) {
        self.ensure_catalog(core);
    }

    fn on_global(&mut self, core: &mut Core) {
        if core.tab == TabId::Params {
            self.ensure_catalog(core);
        }
    }

    fn on_resource(&mut self, core: &mut Core, id: &ResourceId) {
        match id {
            ResourceId::Params => self.on_params(core),
            ResourceId::Telemetry => self.on_telemetry(core),
            ResourceId::Snapshot => self.ensure_catalog(core),
            _ => {}
        }
    }

    fn on_command(&mut self, core: &mut Core, cmd: Command) -> bool {
        if self.search.is_some() {
            match cmd {
                Command::Submit => {
                    self.apply_search();
                    self.search = None;
                }
                Command::Cancel => {
                    self.search = None;
                    self.store.set_query("");
                    self.pending = None;
                    self.clamp();
                }
                _ => return false,
            }
            return true;
        }
        if availability(core) != Avail::Ready {
            return false;
        }
        match cmd {
            Command::Up => self.step(-1),
            Command::Down => self.step(1),
            Command::PageUp => self.step(-10),
            Command::PageDown => self.step(10),
            Command::Top => self.step(-(isize::MAX / 2)),
            Command::Bottom => self.step(isize::MAX / 2),
            Command::Left => self.focus = Focus::Sidebar,
            Command::Right => self.focus = Focus::Table,
            Command::NextPane | Command::PrevPane => {
                self.focus = match self.focus {
                    Focus::Sidebar => Focus::Table,
                    Focus::Table => Focus::Sidebar,
                }
            }
            Command::Activate => match self.focus {
                Focus::Sidebar => self.focus = Focus::Table,
                Focus::Table => self.edit(core),
            },
            Command::Search => {
                self.search = Some(Input::new(self.store.query().to_string()));
                self.focus = Focus::Table;
            }
            Command::Back => {
                if !self.store.query().is_empty() {
                    self.store.set_query("");
                    self.clamp();
                } else if self.focus == Focus::Table {
                    self.focus = Focus::Sidebar;
                } else {
                    return false;
                }
            }
            Command::Copy => match self.focused() {
                Some(name) => {
                    let v = self.store.staged_value(&name).or(self.store.value(&name));
                    match v {
                        Some(v) => core.copy(&metadata::plain(v)),
                        None => return false,
                    }
                }
                None => return false,
            },
            Command::Unstage => self.unstage(core),
            Command::DiscardAll => self.discard_all(core),
            Command::Favourite => self.favourite(core),
            Command::StagedView => {
                let next = if *self.store.filter() == Filter::Staged { 0 } else { 1 };
                self.select_side(next);
            }
            Command::Export => self.export(core),
            Command::Import => self.import_prompt(core),
            Command::Write => self.write(core),
            _ => return false,
        }
        true
    }

    fn on_text(&mut self, core: &mut Core, key: KeyEvent) {
        if let Some(input) = &mut self.search {
            input.handle_event(&Event::Key(key));
            self.pending = Some(core.now);
        }
    }

    fn on_overlay(&mut self, core: &mut Core, ret: Ret, result: OverlayResult) {
        self.overlay_result(core, ret, result);
    }

    fn on_reply(&mut self, core: &mut Core, reply: Reply) {
        if let Purpose::ParamBatch(_) = reply.purpose {
            self.batch_done(core);
        }
    }

    fn on_progress(&mut self, core: &mut Core, name: String, value: f64, result: Result<Value, RestError>) {
        self.batch_item(core, name, value, result);
    }

    fn on_click(&mut self, _core: &mut Core, hit: &Hit) {
        match hit {
            Hit::Sidebar(i) => {
                self.focus = Focus::Sidebar;
                self.select_side(*i);
            }
            Hit::Row(i) => {
                self.focus = Focus::Table;
                self.sel = (*i).min(self.store.visible().len().saturating_sub(1));
            }
            _ => {}
        }
    }

    fn tick(&mut self, core: &mut Core) {
        self.drain_catalog(core);
        if self.pending.is_some_and(|t| core.now.saturating_duration_since(t) >= DEBOUNCE) {
            self.apply_search();
            core.dirty = true;
        }
    }

    fn animating(&self, _core: &Core) -> bool {
        self.pending.is_some() || self.loading.is_some() || self.batch.is_some() || self.env.priming
    }

    fn render(&mut self, f: &mut Frame, area: Rect, core: &Core, out: &mut FrameOut) {
        view::render(self, f, area, core, out);
    }

    fn context_line(&self, core: &Core) -> Option<String> {
        if availability(core) != Avail::Ready {
            return None;
        }
        let name = self.focused()?;
        let meta = self.store.meta(&name)?;
        let text = if meta.description.is_empty() {
            meta.human_name.clone()
        } else {
            meta.description.clone()
        };
        (!text.is_empty()).then_some(text)
    }

    fn status_hint(&self, _core: &Core) -> Option<String> {
        if let Some(b) = &self.batch {
            return Some(format!("writing {}/{}", b.done, b.total));
        }
        let n = self.staged_count();
        (n > 0).then(|| format!("{n} staged"))
    }

    fn searchable(&self, core: &Core) -> bool {
        availability(core) == Avail::Ready
    }
}
