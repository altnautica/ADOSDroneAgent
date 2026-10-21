//! Tab 7, Logs: the agent's live log tail.
//!
//! The stream (`GET /api/logs/stream`) runs on its own thread while the tab is
//! active. When it cannot connect or drops, the tab polls `GET /api/logs`
//! every 2 s instead and retries the stream every 10 s for as long as the tab
//! stays open.

use std::time::{Duration, Instant};

use ratatui::layout::Rect;
use ratatui::Frame;
use serde_json::{json, Value};

use crate::api_path::seg;
use crate::app::{Core, FrameOut, Hit, Tab, TabId};
use crate::keymap::{Command, Ctx};
use crate::overlay::{Overlay, OverlayResult, Ret};
use crate::poll::{ResourceId, Sub};
use crate::stream::{level_rank, LogEntry, LogMsg, LogRing, LogStream};
use crate::toast::ToastKind;
use crate::worker::Route;

// A `logs/` directory is ignored by the repository, so the submodules sit
// beside this file.
#[path = "logs_view.rs"]
mod view;

/// The minimum levels `l` cycles through.
pub const LEVELS: [&str; 4] = ["DEBUG", "INFO", "WARNING", "ERROR"];
/// The fixed stream retry interval (never backs off).
const RETRY: Duration = Duration::from_secs(10);
/// The poll-fallback interval.
const POLL_MS: u64 = 2000;

const TAG_SERVICE: &str = "logs.service";
const TAG_FILTER: &str = "logs.filter";

/// Where the lines come from right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// The tab is not active.
    #[default]
    Idle,
    Connecting,
    Streaming,
    /// The stream is down; `GET /api/logs` is polled.
    Polling,
}

pub struct LogsTab {
    pub ring: LogRing,
    stream: Option<LogStream>,
    pub mode: Mode,
    /// Why the stream last went down.
    pub down_reason: Option<String>,
    retry_at: Option<Instant>,
    /// Index into [`LEVELS`].
    pub level: usize,
    pub service: Option<String>,
    pub filter: String,
    /// The arrival number the view is frozen at.
    pub paused_at: Option<u64>,
    /// The selected line by arrival number; `None` follows the tail.
    pub sel: Option<u64>,
    offset: usize,
    /// The poll fallback's `warning`, shown verbatim.
    pub warning: Option<String>,
}

impl Default for LogsTab {
    fn default() -> Self {
        Self {
            ring: LogRing::default(),
            stream: None,
            mode: Mode::Idle,
            down_reason: None,
            retry_at: None,
            level: 1,
            service: None,
            filter: String::new(),
            paused_at: None,
            sel: None,
            offset: 0,
            warning: None,
        }
    }
}

impl LogsTab {
    fn stream_path(&self) -> String {
        format!(
            "/api/logs/stream?level={}&service={}",
            LEVELS[self.level],
            seg(self.service.as_deref().unwrap_or(""))
        )
    }

    /// The fallback read. The poll route matches `level` exactly rather than
    /// as a minimum, so it is left empty and the minimum is applied here.
    fn poll_path(&self) -> String {
        format!(
            "/api/logs?limit=200&level=&service={}",
            seg(self.service.as_deref().unwrap_or(""))
        )
    }

    fn start_stream(&mut self, core: &Core) {
        self.stream = Some(LogStream::spawn(core.client.clone(), self.stream_path()));
        if self.mode != Mode::Polling {
            self.mode = Mode::Connecting;
        }
        self.retry_at = None;
    }

    /// Filters changed: drop the buffer and reconnect (the stream replays
    /// recent rows under the new filters).
    fn restart(&mut self, core: &Core) {
        self.stream = None;
        self.ring.clear();
        self.warning = None;
        self.sel = None;
        self.offset = 0;
        self.paused_at = None;
        self.mode = Mode::Connecting;
        self.start_stream(core);
    }

    /// The lines shown: frozen at the pause point, at or above the minimum
    /// level, and matching the text filter.
    pub fn visible(&self) -> Vec<&LogEntry> {
        let min = level_rank(LEVELS[self.level]);
        let needle = self.filter.to_lowercase();
        self.ring
            .iter()
            .filter(|e| self.paused_at.is_none_or(|p| e.n < p))
            .filter(|e| e.rank() >= min)
            .filter(|e| {
                needle.is_empty()
                    || e.message.to_lowercase().contains(&needle)
                    || e.logger.to_lowercase().contains(&needle)
            })
            .collect()
    }

    /// The selected index into `visible`, or the last line when following.
    fn selected_index(&self, visible: &[&LogEntry]) -> Option<usize> {
        if visible.is_empty() {
            return None;
        }
        match self.sel {
            None => Some(visible.len() - 1),
            Some(n) => Some(visible.partition_point(|e| e.n < n).min(visible.len() - 1)),
        }
    }

    fn selected(&self) -> Option<LogEntry> {
        let visible = self.visible();
        self.selected_index(&visible).map(|i| visible[i].clone())
    }

    fn step(&mut self, delta: isize) {
        let visible = self.visible();
        let Some(cur) = self.selected_index(&visible) else { return };
        let next = cur.saturating_add_signed(delta).min(visible.len() - 1);
        self.sel = if next + 1 == visible.len() && self.sel.is_none() {
            None
        } else {
            Some(visible[next].n)
        };
    }

    fn handle(&mut self, core: &mut Core, msg: LogMsg) {
        match msg {
            LogMsg::Up => {
                self.mode = Mode::Streaming;
                self.down_reason = None;
                self.warning = None;
            }
            LogMsg::Entry(e) => {
                self.ring.push(e);
            }
            LogMsg::Down(why) => {
                self.stream = None;
                self.mode = Mode::Polling;
                self.down_reason = Some(why);
                self.retry_at = Some(core.now + RETRY);
            }
        }
        core.dirty = true;
    }

    fn service_names(core: &Core) -> Vec<String> {
        core.value(&ResourceId::Services)
            .and_then(|v| v.get("services"))
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|s| s.get("name").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn open_detail(&self, core: &mut Core) {
        let Some(e) = self.selected() else { return };
        let mut lines = vec![
            (ToastKind::Info, format!("Time    {}", e.timestamp.clone().unwrap_or_else(|| "unknown".into()))),
            (ToastKind::Info, format!("Level   {}", e.level_word())),
            (ToastKind::Info, format!("Logger  {}", e.logger)),
        ];
        if let Some(seq) = e.seq {
            lines.push((ToastKind::Info, format!("Seq     {seq}")));
        }
        lines.push((ToastKind::Info, String::new()));
        for part in e.message.lines() {
            for chunk in view::wrap(part, 80) {
                lines.push((ToastKind::Info, chunk));
            }
        }
        let mut overlay = Overlay::result("Log line", lines);
        if let Overlay::Result(r) = &mut overlay {
            r.copy = Some(view::plain_line(&e));
        }
        core.open(overlay);
    }
}

impl Tab for LogsTab {
    fn ctx(&self, _core: &Core) -> Vec<Ctx> {
        vec![Ctx::Logs, Ctx::List]
    }

    fn subs(&self, _core: &Core) -> Vec<Sub> {
        let mut subs = vec![Sub::once(ResourceId::Services, "/api/services")];
        if self.mode == Mode::Polling {
            subs.push(Sub::every_ms(ResourceId::LogsPoll, self.poll_path(), POLL_MS));
        }
        subs
    }

    fn on_activate(&mut self, core: &mut Core) {
        self.mode = Mode::Connecting;
        self.start_stream(core);
    }

    fn on_deactivate(&mut self, _core: &mut Core) {
        self.stream = None;
        self.mode = Mode::Idle;
        self.retry_at = None;
    }

    fn on_resource(&mut self, core: &mut Core, id: &ResourceId) {
        if *id != ResourceId::LogsPoll || self.mode != Mode::Polling {
            return;
        }
        let Some(v) = core.value(id) else { return };
        self.warning = v.get("warning").and_then(Value::as_str).map(str::to_string);
        let entries: Vec<LogEntry> = v
            .get("entries")
            .and_then(Value::as_array)
            .map(|a| a.iter().rev().filter_map(LogEntry::from_value).collect())
            .unwrap_or_default();
        for e in entries {
            self.ring.push(e);
        }
        core.dirty = true;
    }

    fn on_command(&mut self, core: &mut Core, cmd: Command) -> bool {
        match cmd {
            Command::Up => self.step(-1),
            Command::Down => self.step(1),
            Command::PageUp => self.step(-20),
            Command::PageDown => self.step(20),
            Command::Top => {
                self.sel = self.visible().first().map(|e| e.n);
            }
            Command::Follow | Command::Bottom => self.sel = None,
            Command::Activate => self.open_detail(core),
            Command::Copy => match self.selected() {
                Some(e) => core.copy(&view::plain_line(&e)),
                None => core.toast(ToastKind::Info, "No log line selected"),
            },
            Command::CycleLevel => {
                self.level = (self.level + 1) % LEVELS.len();
                self.restart(core);
            }
            Command::PickService => {
                let names = Self::service_names(core);
                let mut items = vec!["all".to_string()];
                items.extend(names.iter().cloned());
                let selected = self
                    .service
                    .as_ref()
                    .and_then(|s| names.iter().position(|n| n == s))
                    .map_or(0, |i| i + 1);
                let ret = Ret::new(Route::tab(TabId::Logs), TAG_SERVICE).with(json!(names));
                core.open(Overlay::picker(ret, "Service", items, selected));
            }
            Command::Pause => {
                self.paused_at = match self.paused_at {
                    Some(_) => None,
                    None => Some(self.ring.next_n()),
                };
            }
            Command::Search => {
                let ret = Ret::new(Route::tab(TabId::Logs), TAG_FILTER);
                let mut input = Overlay::input(ret, "Filter logs", "Text in logger or message", &self.filter);
                input.hint = Some("Empty clears the filter".into());
                core.open(Overlay::Input(input));
            }
            Command::Back if !self.filter.is_empty() => self.filter.clear(),
            _ => return false,
        }
        core.dirty = true;
        true
    }

    fn on_overlay(&mut self, core: &mut Core, ret: Ret, result: OverlayResult) {
        match (ret.tag, result) {
            (TAG_SERVICE, OverlayResult::Picked(i)) => {
                let service = if i == 0 {
                    None
                } else {
                    ret.data
                        .as_array()
                        .and_then(|a| a.get(i - 1))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                };
                if service != self.service {
                    self.service = service;
                    self.restart(core);
                }
            }
            (TAG_FILTER, OverlayResult::Text(text)) => {
                self.filter = text.trim().to_string();
                self.sel = None;
            }
            _ => {}
        }
    }

    fn on_click(&mut self, core: &mut Core, hit: &Hit) {
        if let Hit::Row(i) = hit {
            self.sel = self.visible().get(*i).map(|e| e.n);
            core.dirty = true;
        }
    }

    fn tick(&mut self, core: &mut Core) {
        let msgs = self.stream.as_ref().map(LogStream::drain).unwrap_or_default();
        for msg in msgs {
            self.handle(core, msg);
        }
        if self.mode == Mode::Polling
            && self.stream.is_none()
            && self.retry_at.is_some_and(|at| core.now >= at)
        {
            self.start_stream(core);
            self.retry_at = Some(core.now + RETRY);
        }
    }

    fn animating(&self, _core: &Core) -> bool {
        self.mode == Mode::Connecting
    }

    fn render(&mut self, f: &mut Frame, area: Rect, core: &Core, out: &mut FrameOut) {
        self.offset = view::render(f, area, core, self, out);
    }

    fn context_line(&self, _core: &Core) -> Option<String> {
        self.sel?;
        self.selected().map(|e| format!("{}: {}", e.logger, e.message))
    }

    fn status_hint(&self, _core: &Core) -> Option<String> {
        self.paused_at
            .map(|p| format!("PAUSED +{}", self.ring.next_n().saturating_sub(p)))
    }

    fn searchable(&self, _core: &Core) -> bool {
        true
    }
}

#[cfg(test)]
#[path = "logs_tests.rs"]
mod tests;
