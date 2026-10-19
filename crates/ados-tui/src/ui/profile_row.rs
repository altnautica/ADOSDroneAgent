//! The per-profile panel row (visor capture, ground-station radios/GNSS/LoRa,
//! drone sensor rates and LoRa), drawn from [`Dashboard::profile_row`].

use super::*;
use crate::model::Level;

fn level_style(level: Level) -> Style {
    match level {
        Level::Label => dim(),
        Level::Value => Style::default().fg(theme::heading()),
        Level::Warn => Style::default().fg(theme::warning()),
        Level::Error => Style::default()
            .fg(theme::danger())
            .add_modifier(Modifier::BOLD),
    }
}

pub(super) fn profile_panel(frame: &mut Frame, area: Rect, dash: &Dashboard, stale: bool) {
    let title = match dash.profile.as_str() {
        "visor" => "Visor",
        "ground_station" => "Ground station",
        _ => "Sensors",
    };
    let block = panel(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    // A snapshot the agent no longer confirms is not shown as live readings.
    if stale {
        frame.render_widget(
            Paragraph::new(Span::styled("readings hidden (stale)", dim())),
            inner,
        );
        return;
    }
    frame.render_widget(Paragraph::new(packed_lines(dash, inner.width)), inner);
}

/// The profile row as terminal lines no wider than `width`: each row's
/// segments are packed greedily, starting a new line at a segment boundary when
/// the next segment would not fit (a row never loses a trailing segment).
pub(super) fn packed_lines(dash: &Dashboard, width: u16) -> Vec<Line<'static>> {
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
            spans.push(Span::styled(text.clone(), level_style(*level)));
            used += len;
        }
        out.push(Line::from(spans));
    }
    out
}
