//! The per-profile panel row (visor capture, ground-station radios/GNSS/LoRa,
//! drone sensor rates and LoRa), drawn from [`Dashboard::profile_row`].

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::widgets::panel;
use crate::app::Core;
use crate::model::{Dashboard, Level};
use crate::theme::Theme;

fn level_style(t: &Theme, level: Level) -> Style {
    match level {
        Level::Label => t.dim(),
        Level::Value => t.text(),
        Level::Warn => Style::default().fg(t.warning()),
        Level::Error => Style::default().fg(t.error()).add_modifier(Modifier::BOLD),
    }
}

pub(super) fn profile_panel(frame: &mut Frame, area: Rect, core: &Core, dash: &Dashboard, stale: bool) {
    let t = &core.theme;
    let title = match dash.profile.as_str() {
        "visor" => "Visor",
        "ground_station" => "Ground station",
        _ => "Sensors",
    };
    let block = panel(t, core.sym, title);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    // A snapshot the agent no longer confirms is not shown as live readings.
    if stale {
        frame.render_widget(Paragraph::new(Span::styled("readings hidden (stale)", t.dim())), inner);
        return;
    }
    frame.render_widget(Paragraph::new(packed_lines(t, dash, inner.width)), inner);
}

/// The profile row as terminal lines no wider than `width`: each row's
/// segments are packed greedily, starting a new line at a segment boundary when
/// the next segment would not fit (a row never loses a trailing segment).
pub(super) fn packed_lines(t: &Theme, dash: &Dashboard, width: u16) -> Vec<Line<'static>> {
    let width = usize::from(width).max(1);
    let mut out = Vec::new();
    for row in &dash.profile_row {
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut used = 0;
        let mut wrapped = false;
        for (i, (text, level)) in row.0.iter().enumerate() {
            let len = text.chars().count();
            let separator = *level == Level::Label && text.trim() == "│";
            // A label wraps together with the value that follows it.
            let unit = match row.0.get(i + 1) {
                Some((next, lv)) if *level == Level::Label && !separator && *lv != Level::Label => {
                    len + next.chars().count()
                }
                _ => len,
            };
            if used > 0 && used + unit > width {
                out.push(Line::from(std::mem::take(&mut spans)));
                used = 0;
                wrapped = true;
            }
            // A separator never starts a continuation line.
            if used == 0 && wrapped && separator {
                continue;
            }
            spans.push(Span::styled(text.clone(), level_style(t, *level)));
            used += len;
        }
        out.push(Line::from(spans));
    }
    out
}
