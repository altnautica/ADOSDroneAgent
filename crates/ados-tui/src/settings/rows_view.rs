//! The settings row list: section headers, `label   value` rows with the
//! focus marker, non-default and read-only markers, and plain info lines.
//! Every settings page and the extension config pane draws through this, so
//! the look and the keys are identical everywhere.

use std::time::{Duration, Instant};

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::row::{display, EditRow};
use crate::app::{Core, FrameOut, Hit};
use crate::ui::widgets::{scroll_offset, truncate};

/// How long `v` reveals a secret.
pub const REVEAL_FOR: Duration = Duration::from_secs(10);
/// Labels pad to the widest label on the page, up to this.
pub const LABEL_MAX: usize = 28;

/// One line of a settings page.
#[derive(Debug, Clone)]
pub enum Item {
    /// A sub-section header.
    Header(String),
    /// An editable (or read-only) value row.
    Row(EditRow),
    /// A plain line (a status card line, a table row) that is not focusable.
    Text(Line<'static>),
    /// A focusable non-value line (a list entry such as a network or token);
    /// `key` identifies it.
    Entry { key: String, line: Line<'static> },
}

impl Item {
    fn focusable(&self) -> bool {
        matches!(self, Item::Row(_) | Item::Entry { .. })
    }
}

/// Focus, scroll and secret-reveal state of one row list.
#[derive(Debug, Clone, Default)]
pub struct RowsState {
    /// Index among focusable items.
    pub selected: usize,
    offset: usize,
    /// The key revealed by `v`, and when.
    revealed: Option<(String, Instant)>,
}

impl RowsState {
    /// Move the focus up or down over focusable items.
    pub fn step(&mut self, items: &[Item], down: bool) {
        let n = items.iter().filter(|i| i.focusable()).count();
        if n == 0 {
            self.selected = 0;
        } else if down {
            self.selected = (self.selected + 1).min(n - 1);
        } else {
            self.selected = self.selected.saturating_sub(1);
        }
    }

    /// Jump by `n` focusable items.
    pub fn jump(&mut self, items: &[Item], n: isize) {
        let count = items.iter().filter(|i| i.focusable()).count();
        let target = (self.selected as isize + n).clamp(0, count.saturating_sub(1) as isize);
        self.selected = target as usize;
    }

    /// Focus the item whose key is `key`, when present.
    pub fn focus_key(&mut self, items: &[Item], key: &str) {
        if let Some(i) = items
            .iter()
            .filter(|i| i.focusable())
            .position(|i| item_key(i) == Some(key))
        {
            self.selected = i;
        }
    }

    /// Keep the selection inside the list after it changed length.
    pub fn clamp(&mut self, items: &[Item]) {
        let n = items.iter().filter(|i| i.focusable()).count();
        self.selected = self.selected.min(n.saturating_sub(1));
    }

    /// The focused item.
    pub fn focused<'a>(&self, items: &'a [Item]) -> Option<&'a Item> {
        items.iter().filter(|i| i.focusable()).nth(self.selected)
    }

    /// The focused value row.
    pub fn focused_row<'a>(&self, items: &'a [Item]) -> Option<&'a EditRow> {
        match self.focused(items) {
            Some(Item::Row(r)) => Some(r),
            _ => None,
        }
    }

    /// The focused entry key (row key or entry key).
    pub fn focused_key<'a>(&self, items: &'a [Item]) -> Option<&'a str> {
        self.focused(items).and_then(item_key)
    }

    /// Reveal the focused secret for [`REVEAL_FOR`].
    pub fn reveal(&mut self, items: &[Item], now: Instant) -> bool {
        match self.focused_row(items) {
            Some(r) if r.secret => {
                self.revealed = Some((r.key.clone(), now));
                true
            }
            _ => false,
        }
    }

    fn is_revealed(&self, key: &str, now: Instant) -> bool {
        matches!(&self.revealed, Some((k, at)) if k == key && now.saturating_duration_since(*at) < REVEAL_FOR)
    }

    /// Whether a reveal is counting down (the screen redraws to re-mask).
    pub fn revealing(&self, now: Instant) -> bool {
        matches!(&self.revealed, Some((_, at)) if now.saturating_duration_since(*at) < REVEAL_FOR)
    }

    /// Select the n-th focusable item (mouse).
    pub fn select(&mut self, items: &[Item], n: usize) {
        let count = items.iter().filter(|i| i.focusable()).count();
        if n < count {
            self.selected = n;
        }
    }
}

fn item_key(item: &Item) -> Option<&str> {
    match item {
        Item::Row(r) => Some(&r.key),
        Item::Entry { key, .. } => Some(key),
        _ => None,
    }
}

/// Draw `items` into `area`. `row_prefix` namespaces flash keys (rows are
/// recorded as `<prefix><key>`); `focused` is false while another pane has
/// focus (the marker is then dim).
#[allow(clippy::too_many_arguments)] // one call shape shared by every settings page
pub fn render(
    f: &mut Frame,
    area: Rect,
    core: &Core,
    items: &[Item],
    state: &mut RowsState,
    row_prefix: &str,
    focused: bool,
    out: &mut FrameOut,
) {
    let t = &core.theme;
    state.clamp(items);
    let label_w = items
        .iter()
        .filter_map(|i| match i {
            Item::Row(r) => Some(r.label.chars().count()),
            _ => None,
        })
        .max()
        .unwrap_or(0)
        .min(LABEL_MAX);

    // The line index of the focused item, to scroll it into view.
    let mut focus_line = 0;
    let mut n = 0;
    for (li, item) in items.iter().enumerate() {
        if item.focusable() {
            if n == state.selected {
                focus_line = li;
            }
            n += 1;
        }
    }
    let height = area.height as usize;
    state.offset = scroll_offset(focus_line, state.offset, height);

    let mut fi = items.iter().take(state.offset).filter(|i| i.focusable()).count();
    for (row_i, item) in items.iter().enumerate().skip(state.offset).take(height) {
        let y = area.y + (row_i - state.offset) as u16;
        let rect = Rect::new(area.x, y, area.width, 1);
        let line = match item {
            Item::Header(text) => Line::from(Span::styled(
                text.clone(),
                t.text().add_modifier(Modifier::UNDERLINED | Modifier::BOLD),
            )),
            Item::Text(line) => line.clone(),
            Item::Entry { key, line } => {
                let sel = fi == state.selected;
                out.hit(rect, Hit::Row(fi));
                out.row(format!("{row_prefix}{key}"), rect);
                fi += 1;
                let mut spans = vec![marker(core, sel, focused)];
                spans.extend(line.spans.iter().cloned().map(|s| {
                    if sel && focused {
                        let style = s.style.add_modifier(Modifier::BOLD);
                        s.style(style)
                    } else {
                        s
                    }
                }));
                Line::from(spans)
            }
            Item::Row(row) => {
                let sel = fi == state.selected;
                out.hit(rect, Hit::Row(fi));
                out.row(format!("{row_prefix}{}", row.key), rect);
                fi += 1;
                let revealed = state.is_revealed(&row.key, core.now);
                let value = display(&row.kind, row.value.as_ref(), row.secret, revealed);
                let label = truncate(&row.label, label_w);
                let (label_style, value_style) = if row.read_only {
                    (t.dim(), t.dim())
                } else if sel && focused {
                    (t.accent_bold(), t.accent_bold())
                } else if row.non_default() {
                    (t.text(), t.text())
                } else {
                    (t.text(), t.dim())
                };
                let mut spans = vec![
                    marker(core, sel, focused),
                    Span::styled(format!("{label:<label_w$}  "), label_style),
                    Span::styled(
                        truncate(&value, (area.width as usize).saturating_sub(label_w + 8)),
                        value_style,
                    ),
                ];
                if row.non_default() && !row.read_only {
                    spans.push(Span::styled(format!(" {}", core.sym.non_default), t.accented()));
                }
                if row.read_only {
                    spans.push(Span::styled(format!(" {}", core.sym.read_only), t.dim()));
                }
                Line::from(spans)
            }
        };
        f.render_widget(Paragraph::new(line), rect);
    }
}

fn marker(core: &Core, selected: bool, focused: bool) -> Span<'static> {
    if selected {
        let style = if focused {
            core.theme.accent_bold()
        } else {
            core.theme.dim()
        };
        Span::styled(format!("{} ", core.sym.row_marker), style)
    } else {
        Span::raw("  ")
    }
}

/// A one-line `label  value` info row (not focusable), aligned like value rows.
pub fn info(core: &Core, label: &str, value: impl Into<String>) -> Item {
    let t = &core.theme;
    Item::Text(Line::from(vec![
        Span::raw("  "),
        Span::styled(format!("{label:<20}  "), t.dim()),
        Span::styled(value.into(), t.text()),
    ]))
}

/// A one-line status message (warning or error) as a non-focusable line.
pub fn note(core: &Core, text: impl Into<String>, warn: bool) -> Item {
    let color = if warn { core.theme.warning() } else { core.theme.muted() };
    Item::Text(Line::from(Span::styled(format!("  {}", text.into()), Style::default().fg(color))))
}
