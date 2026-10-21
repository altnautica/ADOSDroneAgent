//! Drawing the Logs tab: the status line, the poll-fallback banner, and the
//! log table.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::{LogsTab, Mode, LEVELS};
use crate::app::{Core, FrameOut, Hit};
use crate::stream::LogEntry;
use crate::ui::widgets::{scroll_offset, truncate};

const TIME_W: usize = 12;
const LEVEL_W: usize = 8;
const LOGGER_W: usize = 22;

/// One line as copied text: `time LEVEL logger message`.
pub fn plain_line(e: &LogEntry) -> String {
    format!("{} {} {} {}", e.time(), e.level_word(), e.logger, e.message)
}

/// Split `text` into pieces at most `width` characters wide.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return vec![String::new()];
    }
    chars.chunks(width.max(1)).map(|c| c.iter().collect()).collect()
}

fn level_style(core: &Core, e: &LogEntry) -> Style {
    let t = &core.theme;
    match e.rank() {
        0 | 1 => t.dim(),
        2 => t.text(),
        3 => Style::default().fg(t.warning()).add_modifier(Modifier::BOLD),
        _ => Style::default().fg(t.error()).add_modifier(Modifier::BOLD),
    }
}

fn status_line(core: &Core, tab: &LogsTab, shown: usize) -> Line<'static> {
    let t = &core.theme;
    let sym = core.sym;
    let mut spans = vec![
        Span::styled("Level ≥ ", t.dim()),
        Span::styled(LEVELS[tab.level], t.accent_bold()),
        Span::styled("  Service ", t.dim()),
        Span::styled(tab.service.clone().unwrap_or_else(|| "all".into()), t.bright()),
    ];
    if !tab.filter.is_empty() {
        spans.push(Span::styled("  Filter ", t.dim()));
        spans.push(Span::styled(format!("\"{}\"", tab.filter), t.accented()));
    }
    spans.push(Span::raw("  "));
    match tab.mode {
        Mode::Streaming => spans.push(Span::styled(
            format!("{} live", sym.ok),
            Style::default().fg(t.success()),
        )),
        Mode::Connecting | Mode::Idle => spans.push(Span::styled(
            format!("{} connecting…", sym.spinner_frame(core.frame as usize)),
            t.dim(),
        )),
        Mode::Polling => {
            spans.push(Span::styled(
                format!("{} polling (stream unavailable)", sym.warn),
                Style::default().fg(t.warning()),
            ));
            if let Some(why) = &tab.down_reason {
                spans.push(Span::styled(format!(" · {}", truncate(why, 48)), t.dim()));
            }
        }
    }
    spans.push(Span::styled(
        format!("  {shown}/{} lines", tab.ring.len()),
        t.dim(),
    ));
    if tab.paused_at.is_some() {
        spans.push(Span::styled("  PAUSED", Style::default().fg(t.warning()).add_modifier(Modifier::BOLD)));
    }
    Line::from(spans)
}

/// Draw one line at row `y` of `area` when it fits.
fn put(f: &mut Frame, area: Rect, y: u16, l: Line<'static>) {
    if y < area.bottom() {
        f.render_widget(Paragraph::new(l), Rect::new(area.x, y, area.width, 1));
    }
}

/// Draw the tab; returns the new scroll offset.
pub fn render(f: &mut Frame, area: Rect, core: &Core, tab: &LogsTab, out: &mut FrameOut) -> usize {
    let t = &core.theme;
    let visible = tab.visible();
    let selected = tab.selected_index(&visible);
    let mut y = area.y;
    put(f, area, y, status_line(core, tab, visible.len()));
    y += 1;
    if let Some(w) = &tab.warning {
        put(
            f,
            area,
            y,
            Line::from(Span::styled(format!("{} {w}", core.sym.warn), Style::default().fg(t.warning()))),
        );
        y += 1;
    }
    let msg_w = (area.width as usize).saturating_sub(2 + TIME_W + 1 + LEVEL_W + 1 + LOGGER_W + 1);
    put(
        f,
        area,
        y,
        Line::from(Span::styled(
            format!(
                "  {:<TIME_W$} {:<LEVEL_W$} {:<LOGGER_W$} Message",
                "Time", "Level", "Logger"
            ),
            t.dim(),
        )),
    );
    y += 1;
    let list = Rect::new(area.x, y, area.width, area.bottom().saturating_sub(y));
    if visible.is_empty() {
        let text = if tab.ring.is_empty() {
            "No log lines yet."
        } else {
            "No lines match the level or filter."
        };
        put(f, area, y, Line::from(Span::styled(format!("  {text}"), t.dim())));
        return 0;
    }
    let height = list.height as usize;
    let sel = selected.unwrap_or(0);
    let offset = if tab.sel.is_none() {
        visible.len().saturating_sub(height)
    } else {
        scroll_offset(sel, tab.offset, height)
    };
    for (row, (i, e)) in visible.iter().enumerate().skip(offset).take(height).enumerate() {
        let focused = tab.sel.is_some() && i == sel;
        let marker = if focused { core.sym.row_marker } else { " " };
        let msg_style = if focused { t.accented() } else { t.text() };
        let rect = Rect::new(list.x, list.y + row as u16, list.width, 1);
        let l = Line::from(vec![
            Span::styled(format!("{marker} "), t.accented()),
            Span::styled(format!("{:<TIME_W$} ", e.time()), t.dim()),
            Span::styled(format!("{:<LEVEL_W$} ", e.level_word()), level_style(core, e)),
            Span::styled(format!("{:<LOGGER_W$} ", truncate(&e.logger, LOGGER_W)), t.dim()),
            Span::styled(truncate(&e.message, msg_w), msg_style),
        ]);
        f.render_widget(Paragraph::new(l), rect);
        out.hit(rect, Hit::Row(i));
    }
    offset
}
