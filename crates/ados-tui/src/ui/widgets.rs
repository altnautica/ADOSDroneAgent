//! Small shared widgets: panels, key/value rows, chips, stacked cells, and a
//! scrolling-list helper.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Gauge, Paragraph, Sparkline};
use ratatui::Frame;

use crate::symbols::Symbols;
use crate::theme::Theme;

/// A bordered panel titled with the `▌ ` accent marker.
pub fn panel<'a>(t: &Theme, sym: &Symbols, title: &str) -> Block<'a> {
    let marker = if sym.name == "ascii" { "|" } else { "▌" };
    Block::default()
        .borders(Borders::ALL)
        .border_set(sym.border_set)
        .border_style(Style::default().fg(t.border()))
        .title(Span::styled(format!(" {marker} {title} "), t.accent_bold()))
}

/// A `label   value` row: dim label padded to `width`, then the value.
pub fn kv(t: &Theme, label: &str, width: usize, value: Span<'static>) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<width$} "), t.dim()),
        value,
    ])
}

/// A `label value` row where the value is plain bright text.
pub fn kv_text(t: &Theme, label: &str, width: usize, value: impl Into<String>) -> Line<'static> {
    kv(t, label, width, Span::styled(value.into(), t.bright()))
}

/// A short filled chip, e.g. ` STABILIZE ` or ` ARMED `.
pub fn chip(t: &Theme, text: &str, color: Color) -> Span<'static> {
    Span::styled(format!(" {text} "), t.chip(color))
}

/// One row inside a text-and-widget panel.
pub enum Cell {
    Line(Line<'static>),
    Gauge {
        ratio: f64,
        label: String,
        color: Color,
    },
    Spark {
        data: Vec<u64>,
        max: Option<u64>,
        color: Color,
    },
}

/// Lay `cells` out as stacked one-row widgets inside `inner`. A trailing filler
/// keeps them top-aligned. Splitting yields one rect per cell regardless of the
/// available height (extra rects get zero height), so this never panics on a
/// short terminal.
pub fn render_cells(frame: &mut Frame, t: &Theme, inner: Rect, cells: Vec<Cell>) {
    if inner.height == 0 || cells.is_empty() {
        return;
    }
    let mut constraints: Vec<Constraint> = cells.iter().map(|_| Constraint::Length(1)).collect();
    constraints.push(Constraint::Min(0));
    let rects = Layout::vertical(constraints).split(inner);
    for (i, cell) in cells.into_iter().enumerate() {
        let rect = rects[i];
        match cell {
            Cell::Line(line) => frame.render_widget(Paragraph::new(line), rect),
            Cell::Gauge {
                ratio,
                label,
                color,
            } => frame.render_widget(
                Gauge::default()
                    .gauge_style(Style::default().fg(color))
                    .ratio(ratio.clamp(0.0, 1.0))
                    .label(Span::styled(label, Style::default().fg(t.on_accent()))),
                rect,
            ),
            Cell::Spark { data, max, color } => {
                let mut spark = Sparkline::default()
                    .data(newest(&data, rect.width))
                    .style(Style::default().fg(color));
                if let Some(m) = max {
                    spark = spark.max(m);
                }
                frame.render_widget(spark, rect);
            }
        }
    }
}

/// The newest `width` samples of a series. A sparkline draws from the left
/// and stops at its width, so a longer history would otherwise show only its
/// oldest samples.
pub fn newest<T>(data: &[T], width: u16) -> &[T] {
    &data[data.len().saturating_sub(usize::from(width))..]
}

/// A rect `w` × `h` cells, centred in `area` (clamped to it).
pub fn centered_fixed(w: u16, h: u16, area: Rect) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect {
        x: area.x + area.width.saturating_sub(w) / 2,
        y: area.y + area.height.saturating_sub(h) / 2,
        width: w,
        height: h,
    }
}

/// The first row index to draw so `selected` stays visible in `height` rows,
/// keeping the previous `offset` when it already shows the selection.
pub fn scroll_offset(selected: usize, offset: usize, height: usize) -> usize {
    if height == 0 {
        return 0;
    }
    if selected < offset {
        selected
    } else if selected >= offset + height {
        selected + 1 - height
    } else {
        offset
    }
}

/// Truncate to `width` display columns, ending in `…` when cut.
pub fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut out: String = text.chars().take(width - 1).collect();
    out.push('…');
    out
}

/// A one-row-per-item rect split of `area` (zero-height rects past the end).
pub fn rows(area: Rect) -> impl Iterator<Item = Rect> {
    (0..area.height).map(move |i| Rect {
        x: area.x,
        y: area.y + i,
        width: area.width,
        height: 1,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scroll_keeps_the_selection_in_view() {
        assert_eq!(scroll_offset(0, 0, 5), 0);
        assert_eq!(scroll_offset(7, 0, 5), 3);
        assert_eq!(scroll_offset(4, 3, 5), 3);
        assert_eq!(scroll_offset(1, 3, 5), 1);
    }

    #[test]
    fn a_long_series_keeps_its_newest_samples() {
        let data: Vec<u64> = (0..150).collect();
        assert_eq!(newest(&data, 66).first(), Some(&84));
        assert_eq!(newest(&data, 66).last(), Some(&149));
        assert_eq!(newest(&data[..10], 66).len(), 10);
    }

    #[test]
    fn truncate_marks_the_cut() {
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(truncate("abc", 4), "abc");
    }
}
