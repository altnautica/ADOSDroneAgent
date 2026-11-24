//! Fitting the Pilot block's rows to the terminal and drawing it.

use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::app::Core;
use crate::theme::Theme;
use crate::ui::widgets::{panel, truncate_line};

use super::Row;

/// The rows that fit `rows`: all of them, or the rows that must show plus
/// as many others as fit, in their order, and a dim "… N more" line for the
/// rest, so a short terminal never drops the chip, failsafe, link loss or
/// alert count and says what it cut.
pub fn fit_kept(t: &Theme, rows: Vec<Row>, height: u16) -> Vec<Line<'static>> {
    let height = usize::from(height);
    if rows.len() <= height || height == 0 {
        return rows.into_iter().map(|(l, _)| l).collect();
    }
    let budget = height - 1;
    let mut must = rows.iter().filter(|(_, k)| *k).count().min(budget);
    let mut spare = budget - must;
    let total = rows.len();
    let mut out = Vec::new();
    for (line, keep) in rows {
        if keep && must > 0 {
            must -= 1;
            out.push(line);
        } else if !keep && spare > 0 {
            spare -= 1;
            out.push(line);
        }
    }
    let more = total - out.len();
    out.push(Line::from(Span::styled(
        format!("… {more} more (a taller terminal shows all)"),
        t.dim(),
    )));
    out
}

/// The rows the block keeps on the shortest terminal, borders included.
pub fn min_rows(rows: &[Row]) -> u16 {
    let kept = rows.iter().filter(|(_, k)| *k).count();
    let more = usize::from(kept < rows.len());
    (kept + more + 2) as u16
}

/// Draw `rows` fitted into a `title` panel (the Pilot block, or the alerts
/// in their own column).
pub fn render(
    f: &mut Frame,
    area: Rect,
    core: &Core,
    title: &str,
    rows: Vec<Row>,
    hidden: Option<&str>,
) {
    let t = &core.theme;
    let block = panel(t, core.sym, title);
    let inner = block.inner(area);
    f.render_widget(block, area);
    match hidden {
        Some(why) => f.render_widget(
            Paragraph::new(Span::styled(why.to_string(), t.dim())),
            inner,
        ),
        None => {
            let width = usize::from(inner.width);
            let lines: Vec<Line> = fit_kept(t, rows, inner.height)
                .into_iter()
                .map(|l| truncate_line(l, width))
                .collect();
            f.render_widget(Paragraph::new(lines), inner);
        }
    }
}
