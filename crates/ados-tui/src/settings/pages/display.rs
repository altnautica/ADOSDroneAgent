//! Settings page: SYSTEM & SAFETY › Display (ground station). Read-only view
//! of `GET /api/v1/setup/display/options`: the active kiosk display and the
//! overlays this board's HAL profile supports.

use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::Frame;
use serde_json::Value;

use super::kit;
use crate::app::{Core, FrameOut, Hit};
use crate::keymap::Command;
use crate::poll::{ResourceId, Sub};
use crate::settings::page::Bespoke;
use crate::settings::rows_view::{self, Item, RowsState};

#[derive(Default)]
pub struct Page {
    rows: RowsState,
}

fn items(core: &Core) -> Vec<Item> {
    let t = &core.theme;
    let mut items = vec![Item::Header("Local kiosk display".into())];
    items.push(rows_view::note(
        core,
        "Installing an overlay edits the boot config and needs a reboot; run it from the installer with --upgrade --display <id>.",
        false,
    ));
    if let Some(note) = kit::resource_note(core, &ResourceId::DisplayOptions, "the display options")
    {
        items.push(note);
        return items;
    }
    let opts = core.value(&ResourceId::DisplayOptions);
    let current = kit::text(opts, "current.display_id")
        .unwrap_or("none")
        .to_string();
    items.push(rows_view::info(core, "Active", current.clone()));
    let board = kit::text(opts, "board_id").unwrap_or("unknown");
    let supported = opts
        .and_then(|o| o.get("supported"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    items.push(Item::Header(format!("Supported on this board ({board})")));
    if supported.is_empty() {
        items.push(rows_view::note(
            core,
            "No display overlays declared in this board's HAL profile.",
            false,
        ));
    }
    for d in &supported {
        let id = d.get("id").and_then(Value::as_str).unwrap_or_default();
        let mut detail = d
            .get("label")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if let Some(res) = d
            .get("resolution")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            detail.push_str(&format!(" · {res}"));
        }
        if let Some(chip) = d
            .get("touch_chip")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            detail.push_str(&format!(" · touch {chip}"));
        }
        let mut spans = vec![Span::styled(format!("{id:<20}"), t.text())];
        if id == current {
            spans.push(Span::styled(
                format!("{} active  ", core.sym.ok),
                t.chip(t.success()),
            ));
        }
        spans.push(Span::styled(detail, t.dim()));
        items.push(Item::Entry {
            key: id.to_string(),
            line: Line::from(spans),
        });
    }
    items
}

impl Bespoke for Page {
    fn subs(&self, _core: &Core) -> Vec<Sub> {
        vec![Sub::every_ms(
            ResourceId::DisplayOptions,
            "/api/v1/setup/display/options",
            60_000,
        )]
    }

    fn on_command(&mut self, core: &mut Core, cmd: Command) -> bool {
        let list = items(core);
        if kit::nav(&mut self.rows, &list, cmd) {
            return true;
        }
        match cmd {
            Command::Copy => match self.rows.focused_key(&list) {
                Some(id) => {
                    let id = id.to_string();
                    core.copy(&id);
                    true
                }
                None => false,
            },
            _ => false,
        }
    }

    fn on_click(&mut self, core: &mut Core, hit: &Hit) {
        let list = items(core);
        kit::click(&mut self.rows, &list, hit);
    }

    fn render(&mut self, f: &mut Frame, area: Rect, core: &Core, out: &mut FrameOut) {
        let list = items(core);
        rows_view::render(f, area, core, &list, &mut self.rows, "display:", true, out);
    }

    fn context_line(&self, core: &Core) -> Option<String> {
        let list = items(core);
        self.rows
            .focused_key(&list)
            .map(|id| format!("Install with the installer: --upgrade --display {id}"))
    }
}
