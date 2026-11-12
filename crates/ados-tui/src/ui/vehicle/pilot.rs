//! The Pilot block from the `pilot` nav kind: the flight-state chip and its
//! secondary line (the cockpit spec's `flight-state.json`), the control mode
//! and profile, obstacle avoidance, the mission progress, the link-loss action
//! or countdown, the height above ground with its source, and the alerts the
//! cockpit's status pill draws from the pilot and the flight controller. Every
//! word is the cockpit spec's. A row from a stale pilot kind (a failed read,
//! or a kind past the spec's stale age counting the time since the last good
//! read) is dim and marked "(stale)".

use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::app::Core;
use crate::model::autonomy::{Follow, Pilot};
use crate::model::flight_state::readouts::{bottom_row_agl, link_loss_action_text, link_loss_line};
use crate::model::flight_state::{avoid_state_text, stale_after_ms, ChipView};
use crate::model::pilot_alerts::{alerts_count_text, pilot_alerts, Severity};
use crate::model::spec_text::spec_text;
use crate::theme::Theme;
use crate::ui::widgets::{kv, kv_text, panel};

use super::autonomy::{fit, mark_stale, LABEL};

/// A spec state colour name (`tokens.json` `state.*`) in this palette.
fn state_color(t: &Theme, name: &str) -> Color {
    match name {
        "ready" => t.success(),
        "not_ready" | "blocked" => t.error(),
        "rth" | "tuning" | "avoiding" => t.warning(),
        "pilot" | "goto" | "swarm" | "skill" => t.info(),
        "mission" => t.accent(),
        "stale" => t.muted(),
        _ => t.fg(),
    }
}

/// What the block shows beside the pilot kind.
pub struct PilotInputs<'a> {
    pub pilot: Option<&'a Pilot>,
    /// The pilot kind's age now: its age at the agent plus the time since
    /// the read (`u64::MAX` when unknown).
    pub age_ms: u64,
    /// The chip for `pilot`; `None` when its owner is not one the spec knows.
    pub chip: Option<&'a ChipView>,
    /// The follow kind, for the height above ground when the pilot has none.
    pub follow: Option<&'a Follow>,
    /// The follow kind's age now, as `age_ms`.
    pub follow_age_ms: u64,
    /// The FC's PreArm/Arm texts, while the telemetry is shown.
    pub prearm: Option<&'a [String]>,
}

/// A whole number for a count the wire sends as a JSON number.
fn whole(v: Option<f64>) -> String {
    v.filter(|v| v.is_finite())
        .map(|v| (v.round() as i64).to_string())
        .unwrap_or_else(|| "–".into())
}

/// The block's lines.
pub fn lines(core: &Core, i: &PilotInputs) -> Vec<Line<'static>> {
    let t = &core.theme;
    let Some(p) = i.pilot else {
        // Nothing publishes the pilot kind: the chip's absent state.
        return vec![kv_text(t, "State", LABEL, spec_text("chip.no_pilot", &[]))];
    };
    let p_stale = p.stale || i.age_ms >= stale_after_ms();
    let stale = |line: Line<'static>| mark_stale(t, line, p_stale);
    let mut out = Vec::new();

    let state = match i.chip {
        Some(chip) => {
            let mut style = Style::default().fg(state_color(t, &chip.color));
            if chip.dim {
                style = t.dim();
            }
            Span::styled(chip.text.clone(), style)
        }
        None => Span::styled("–", t.dim()),
    };
    out.push(stale(kv(t, "State", LABEL, state)));
    if let Some(chip) = i.chip.filter(|c| !c.secondary.is_empty()) {
        let mut spans = vec![Span::styled(format!("{:<LABEL$} ", ""), t.dim())];
        for (n, part) in chip.secondary.iter().enumerate() {
            if n > 0 {
                spans.push(Span::styled(" · ", t.dim()));
            }
            spans.push(Span::styled(
                part.text.clone(),
                Style::default().fg(state_color(t, &part.color)),
            ));
        }
        out.push(stale(Line::from(spans)));
    }

    // An FC failsafe and what tripped it, in the spec's words.
    if p.fc_failsafe {
        let mut text = spec_text("chip.failsafe", &[]);
        if let Some(cause) = &p.failsafe_cause {
            text.push_str(" · ");
            text.push_str(&spec_text(&format!("pilot.banner.cause.{cause}"), &[]));
        }
        out.push(stale(kv(
            t,
            &spec_text("cockpit.row.failsafe", &[]),
            LABEL,
            Span::styled(text, Style::default().fg(t.error())),
        )));
    }

    let mode = [
        p.control_mode
            .as_ref()
            .map(|m| spec_text(&format!("pilot.control_mode.{m}"), &[])),
        p.profile
            .as_ref()
            .map(|pr| spec_text(&format!("pilot.profile.{pr}"), &[])),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" · ");
    out.push(stale(kv_text(
        t,
        &spec_text("cockpit.row.mode", &[]),
        LABEL,
        if mode.is_empty() { "–".into() } else { mode },
    )));

    // The operations the pilot offers a surface now (`goto` is Fly Here).
    let ops = p
        .surface_ops
        .iter()
        .map(|op| match op.as_str() {
            "goto" => spec_text("map.fly_here", &[]),
            other => other.to_string(),
        })
        .collect::<Vec<_>>()
        .join(" · ");
    out.push(stale(kv_text(
        t,
        &spec_text("cockpit.row.offers", &[]),
        LABEL,
        if ops.is_empty() { "–".into() } else { ops },
    )));

    let mut avoid = vec![Span::styled(format!("{:<LABEL$} ", "Avoid"), t.dim())];
    match p.avoid.as_deref() {
        Some("off") => avoid.push(Span::styled(
            spec_text("pilot.avoid.off", &[]),
            Style::default().fg(t.error()),
        )),
        Some(a) => avoid.push(Span::styled(
            spec_text(&format!("pilot.avoid.{a}"), &[]),
            t.bright(),
        )),
        None => avoid.push(Span::styled("–", t.bright())),
    }
    if let Some(state) = p.avoid_state.as_deref().and_then(avoid_state_text) {
        avoid.push(Span::styled(" · ", t.dim()));
        avoid.push(Span::styled(
            state.text,
            Style::default().fg(state_color(t, &state.color)),
        ));
    }
    if let Some(m) = p.min_clearance_m {
        avoid.push(Span::styled(format!(" {m:.1} m"), t.bright()));
    }
    out.push(stale(Line::from(avoid)));

    let mission = match &p.mission {
        None => "–".to_string(),
        Some(m)
            if matches!(m.state.as_deref(), None | Some("none"))
                || m.total.unwrap_or(0.0) <= 0.0 =>
        {
            spec_text("flow.launch.after.mission_none", &[])
        }
        Some(m) => format!("WP {}/{}", whole(m.seq), whole(m.total)),
    };
    out.push(stale(kv_text(t, "Mission", LABEL, mission)));

    let link = match link_loss_line(p) {
        Some(countdown) => Span::styled(countdown, Style::default().fg(t.warning())),
        None => Span::styled(
            format!("→ {}", link_loss_action_text(p.link_loss_action.as_deref())),
            t.bright(),
        ),
    };
    out.push(stale(kv(t, "Link loss", LABEL, link)));

    let agl = match bottom_row_agl(Some(p), i.follow) {
        Some((m, src, from_follow)) => {
            let mark = if src.marked() {
                Style::default().fg(t.warning())
            } else {
                t.dim()
            };
            let line = Line::from(vec![
                Span::styled(format!("{:<LABEL$} ", "AGL"), t.dim()),
                Span::styled(format!("{m:.1} m "), t.bright()),
                Span::styled(format!("({})", src.text()), mark),
            ]);
            let from_stale = if from_follow {
                i.follow
                    .is_some_and(|f| f.stale || i.follow_age_ms >= stale_after_ms())
            } else {
                p_stale
            };
            mark_stale(t, line, from_stale)
        }
        None => stale(kv_text(t, "AGL", LABEL, "–".to_string())),
    };
    out.push(agl);

    let alerts = pilot_alerts(p, Some(i.age_ms), i.prearm);
    out.push(stale(kv_text(
        t,
        "Alerts",
        LABEL,
        alerts_count_text(alerts.len()).unwrap_or_else(|| "–".into()),
    )));
    for a in &alerts {
        let (sym, color) = match a.severity {
            Severity::Critical => (core.sym.err, t.error()),
            Severity::Warning => (core.sym.warn, t.warning()),
        };
        let line = Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("{sym} {}", a.text), Style::default().fg(color)),
        ]);
        out.push(stale(line));
    }
    out
}

pub fn render(
    f: &mut Frame,
    area: Rect,
    core: &Core,
    lines: Vec<Line<'static>>,
    hidden: Option<&str>,
) {
    let t = &core.theme;
    let block = panel(t, core.sym, "Pilot");
    let inner = block.inner(area);
    f.render_widget(block, area);
    match hidden {
        Some(why) => f.render_widget(
            Paragraph::new(Span::styled(why.to_string(), t.dim())),
            inner,
        ),
        None => f.render_widget(Paragraph::new(fit(t, lines, inner.height)), inner),
    }
}

#[cfg(test)]
mod tests;
