//! Avoidance beyond its state in the Pilot block: what the avoid mode does
//! (the spec's explain line), a detour round a blocked mission leg or a
//! return the pilot routes round a blocked one, and the waypoints the mission
//! skipped with why (`mission.gaps`), all in the cockpit spec's words.

use ratatui::style::Style;
use ratatui::text::Span;

use crate::model::autonomy::Pilot;
use crate::model::spec_text::spec_text;
use crate::theme::Theme;
use crate::ui::widgets::kv;

use super::super::autonomy::{mark_stale, LABEL};
use super::{phrase_rows, Row};

/// Under the Avoid row: the mode's explain line while the mode is not the
/// default Brake (Bypass routes round, Off avoids nothing; never on a
/// fixed-wing, where avoidance is only a height above ground), then the
/// detour or routed return while the pilot flies one, kept on a short
/// terminal.
pub(super) fn avoid_rows(t: &Theme, p: &Pilot, stale: bool) -> Vec<Row> {
    let mut out = Vec::new();
    if let Some(a) = p.avoid.as_deref().filter(|a| *a != "brake") {
        if p.vehicle_class.as_deref() != Some("fixedwing") {
            let text = spec_text(&format!("pilot.avoid.explain.{a}"), &[]);
            let line = kv(t, "", LABEL, Span::styled(text, t.dim()));
            out.push((mark_stale(t, line, stale), false));
        }
    }
    let routing = match (&p.mission_detour, &p.return_route) {
        (Some(d), _) => Some(spec_text(
            "chip.route_round.detour",
            &[("wp", &d.seq.to_string())],
        )),
        (None, Some(r)) => Some(spec_text(
            "chip.route_round.return",
            &[
                ("remaining_m", &r.remaining_m.round().to_string()),
                ("straight_m", &r.straight_m.round().to_string()),
            ],
        )),
        (None, None) => None,
    };
    if let Some(text) = routing {
        out.extend(phrase_rows(
            t,
            "",
            &text,
            Style::default().fg(t.warning()),
            stale,
        ));
    }
    out
}

/// Under the Mission row: one row per skipped waypoint, oldest first, with
/// why it was skipped (`WP 4 · Skipped · Inside an obstacle`).
pub(super) fn gap_rows(t: &Theme, p: &Pilot, stale: bool) -> Vec<Row> {
    let Some(m) = &p.mission else {
        return Vec::new();
    };
    m.gaps
        .iter()
        .map(|g| {
            let text = spec_text(
                "pilot.gap.line",
                &[
                    ("wp", &g.seq.to_string()),
                    ("reason", &gap_reason_text(&g.reason)),
                ],
            );
            let line = kv(
                t,
                "",
                LABEL,
                Span::styled(text, Style::default().fg(t.warning())),
            );
            (mark_stale(t, line, stale), false)
        })
        .collect()
}

/// A gap's reason in words (`pilot.gap.reason.<reason>`); the code with its
/// underscores as spaces for a reason the spec does not word.
fn gap_reason_text(reason: &str) -> String {
    let key = format!("pilot.gap.reason.{reason}");
    let text = spec_text(&key, &[]);
    if text == key {
        reason.replace('_', " ")
    } else {
        text
    }
}
