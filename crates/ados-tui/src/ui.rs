//! ratatui rendering of the terminal UI.
//!
//! Every frame: header, tab bar, the active tab inside a bordered body, the
//! context line, the hint bar, toasts, then any overlays. Each tab renders its
//! own body (`ui/<tab>.rs`); the chrome and overlays are shared.

use ratatui::layout::{Constraint, Layout};
use ratatui::style::Style;
use ratatui::widgets::{Block, Borders};
use ratatui::Frame;

use crate::app::{App, FrameOut};
use crate::keymap;

pub mod chrome;
pub mod extensions;
pub mod ground;
pub mod link;
pub mod logs;
pub mod overlays;
pub mod overview;
mod panels;
pub mod params;
mod profile_row;
pub mod settings;
pub mod system;
pub mod vehicle;
pub mod video;
pub mod widgets;

#[cfg(test)]
pub(crate) mod tests;

/// Smallest usable terminal.
pub const MIN_W: u16 = 80;
pub const MIN_H: u16 = 24;

/// Draw one frame and report the rects effects and mouse hits need.
pub fn render(frame: &mut Frame, app: &mut App) -> FrameOut {
    let stack = app.ctx_stack();
    let hints = keymap::hints(&stack, |c| app.enabled(c));
    // The key contexts of the screen under any overlay, for the help overlay.
    let under: Vec<keymap::Ctx> = {
        let mut s = app.tabs.get(app.core.tab, app.core.profile()).ctx(&app.core);
        s.push(keymap::Ctx::Global);
        s
    };
    let App { core, tabs } = app;
    let core = &*core;
    let area = frame.area();
    let mut out = FrameOut::default();

    // Paint the canvas once per frame on tiers that can show it; widgets drawn
    // on top keep it unless they set their own background.
    if let Some(bg) = core.theme.canvas() {
        frame.buffer_mut().set_style(area, Style::default().bg(bg));
    }
    if area.width < MIN_W || area.height < MIN_H {
        chrome::too_small(frame, area, core);
        return out;
    }

    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(area);

    chrome::header(frame, rows[0], core, &mut out);
    chrome::tab_bar(frame, rows[1], core, &mut out);

    let body = Block::default()
        .borders(Borders::ALL)
        .border_set(core.sym.border_set)
        .border_style(Style::default().fg(core.theme.border()));
    let inner = body.inner(rows[2]);
    frame.render_widget(body, rows[2]);
    out.body = rows[2];

    let profile = core.profile().to_string();
    let tab = tabs.get_mut(core.tab, &profile);
    tab.render(frame, inner, core, &mut out);

    // The focused row's help stays visible while it is edited in place.
    let inline_edit = matches!(core.overlays.last(), Some(crate::overlay::Overlay::Input(s)) if s.inline);
    let context = if core.overlays.is_empty() || inline_edit {
        tab.context_line(core)
    } else {
        None
    };
    chrome::context_line(frame, rows[3], core, context);
    let extra = tab.status_hint(core);
    chrome::hint_bar(frame, rows[4], core, &hints, extra);

    // Toasts sit above the hint bar, bottom-right.
    let toast_area = ratatui::layout::Rect::new(area.x, rows[2].y, area.width, rows[3].bottom() - rows[2].y);
    chrome::toasts(frame, toast_area, core, &mut out);

    overlays::render(frame, core, &under, &mut out);
    if let Some((x, y)) = out.cursor {
        frame.set_cursor_position((x, y));
    }
    out
}
