//! The Pilot block from the `pilot` nav kind: the flight-state chip and its
//! secondary line (the cockpit spec's `flight-state.json`, the speed cap by
//! its reason), the place class (Open, or Confined with the key that switches
//! it while disarmed) and the route check of a confined mission with its
//! tightest point, the control mode and profile, who holds the sticks, the
//! gimbal, obstacle avoidance, the mission progress, the link-loss action or
//! countdown, the height above ground with its source, and the status pill's
//! alerts. The stall overlay hides the stall's own banners while it shows.
//! Every word is the cockpit spec's. A row from a stale pilot kind (a failed
//! read, or a kind past the spec's stale age counting the time since the last
//! good read) is dim and marked "(stale)". On a short terminal the chip,
//! failsafe, link loss, a confined place, a too-tight route and the alert
//! count stay; the rest is counted as "… N more".

use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::app::Core;
use crate::model::autonomy::{Follow, Pilot};
use crate::model::flight_state::readouts::{bottom_row_agl, link_loss_action_text, link_loss_line};
use crate::model::flight_state::{avoid_state_text, stale_after_ms, ChipView};
use crate::model::mission_stall::{banner_text, low_battery_line, stall_keys, stall_view};
use crate::model::pilot_alerts::{alerts_count_text, PillAlert, Severity};
use crate::model::route_check::{
    other_place_class, place_label, route_check_report, stall_shadows_banner, ReportSeverity,
};
use crate::model::spec_text::spec_text;
use crate::theme::Theme;
use crate::ui::widgets::{kv, kv_text, panel, truncate_line};

use super::autonomy::{mark_stale, LABEL};

/// A Pilot row, and whether it must show on a short terminal.
pub type Row = (Line<'static>, bool);

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
    /// The status pill's alerts.
    pub alerts: &'a [PillAlert],
    /// Whether the gimbal takes rate commands; `None` while unknown.
    pub gimbal: Option<bool>,
    /// Whether the place key answers now (a fresh, disarmed pilot).
    pub place_key: bool,
    /// Wall-clock now (epoch ms), for the banner's end time.
    pub now_ms: u64,
}

/// The pilot's countdowns and banner, critical first: the mission stall
/// with the keys that answer it, the low-battery return, then the banner.
/// Each countdown counts down by the frame's age (`age_ms`) since the pilot
/// sent it.
fn countdown_rows(core: &Core, p: &Pilot, age_ms: u64, now_ms: u64, stale: bool) -> Vec<Row> {
    let t = &core.theme;
    let elapsed_s = if age_ms == u64::MAX {
        0.0
    } else {
        age_ms as f64 / 1000.0
    };
    let critical = Style::default().fg(t.error());
    let mut out = Vec::new();
    if let Some(s) = &p.mission_stall {
        let mut s = s.clone();
        s.skip_in_s = s.skip_in_s.map(|v| v - elapsed_s);
        s.return_in_s = s.return_in_s.map(|v| v - elapsed_s);
        let view = stall_view(&s);
        let title = format!("{} {}", core.sym.err, view.title);
        out.extend(phrase_rows(t, "Stall", &title, critical, stale));
        // The keys answer only a fresh stall.
        if !stale {
            let keys = Line::from(vec![
                Span::styled(format!("{:<LABEL$} ", ""), t.dim()),
                Span::styled(stall_keys(&view), t.bright()),
            ]);
            out.push((keys, true));
        }
    }
    if let Some(s) = p.rth_countdown_s {
        out.extend(phrase_rows(
            t,
            "Battery",
            &low_battery_line(s - elapsed_s, p.low_battery_countdown_land),
            critical,
            stale,
        ));
    }
    // The stall overlay carries the live countdown: its own banners hide.
    let banner = p
        .banner
        .as_ref()
        .filter(|b| !stall_shadows_banner(&b.text_key, p.mission_stall.as_ref()));
    if let Some(text) = banner.and_then(|b| banner_text(b, now_ms)) {
        out.extend(phrase_rows(t, "Banner", &text, t.bright(), stale));
    }
    out
}

/// The link-loss window the pilot uses when its state does not report one.
const DEFAULT_LINK_LOSS_S: f64 = 10.0;

/// The place class (Confined in the warning colour, with the key that
/// switches it while the pilot is disarmed) and, in a confined place with a
/// mission loaded, the route check and its tightest point. A confined place
/// and a too-tight route stay on a short terminal.
fn place_rows(core: &Core, p: &Pilot, place_key: bool, stale: bool) -> Vec<Row> {
    let t = &core.theme;
    let confined = p.place_class == "confined";
    let class = if confined { "confined" } else { "open" };
    let style = if confined {
        Style::default().fg(t.warning())
    } else {
        t.bright()
    };
    let mut spans = vec![
        Span::styled(
            format!("{:<LABEL$} ", spec_text("flow.launch.place_class", &[])),
            t.dim(),
        ),
        Span::styled(spec_text(&format!("pilot.place_class.{class}"), &[]), style),
    ];
    if place_key {
        spans.push(Span::styled(" · ", t.dim()));
        spans.push(Span::styled(
            format!("c {}", place_label(other_place_class(class))),
            t.bright(),
        ));
    }
    let mut out = vec![(mark_stale(t, Line::from(spans), stale), confined)];
    // With no RC transmitter: what stops the drone and what a lost link ends
    // in, kept on a short terminal like a confined place.
    if p.rc_absent {
        let action = p.link_loss_action.as_deref().unwrap_or("rth");
        let s = p
            .link_loss_s
            .unwrap_or(DEFAULT_LINK_LOSS_S)
            .round()
            .to_string();
        let text = spec_text(
            "follow.no_rc_strip",
            &[("action", &link_loss_action_text(Some(action))), ("s", &s)],
        );
        out.extend(phrase_rows(
            t,
            "RC",
            &text,
            Style::default().fg(t.warning()),
            stale,
        ));
    }
    if let Some(rc) = &p.route_check {
        let report = route_check_report(rc);
        let (style, keep) = match report.severity {
            ReportSeverity::Critical => (Style::default().fg(t.error()), true),
            ReportSeverity::Warn => (Style::default().fg(t.warning()), false),
            ReportSeverity::Ok => (t.bright(), false),
        };
        // The Pilot block is narrow: the report wraps at its clauses too, so
        // the required clearance is never cut off.
        let line = report.line.replace(", ", " · ");
        out.extend(
            phrase_rows(t, "Route", &line, style, stale)
                .into_iter()
                .map(|(line, _)| (line, keep)),
        );
        if let Some(at) = report.pinch_at {
            out.push((kv(t, "", LABEL, Span::styled(at, t.bright())), keep));
        }
    }
    out
}

/// A `label value` row whose value breaks after each ` · ` phrase onto its
/// own line under the label, so the narrow Pilot column never cuts off a
/// countdown. A stale value dims and says so on its last line.
fn phrase_rows(t: &Theme, label: &str, text: &str, style: Style, stale: bool) -> Vec<Row> {
    let style = if stale { t.dim() } else { style };
    let phrases: Vec<&str> = text.split(" · ").collect();
    let last = phrases.len() - 1;
    phrases
        .into_iter()
        .enumerate()
        .map(|(n, phrase)| {
            let head = if n == 0 { label } else { "" };
            let line = kv(t, head, LABEL, Span::styled(phrase.to_string(), style));
            (mark_stale(t, line, stale && n == last), true)
        })
        .collect()
}

/// A surface operation the pilot offers, in the spec's words; one the spec
/// does not know reads as `Other (op)`.
fn op_text(op: &str) -> String {
    match op {
        "goto" => spec_text("map.fly_here", &[]),
        other => format!("Other ({other})"),
    }
}

/// The alert count and one line per alert. The alerts are computed now from
/// the readings the tab holds (an aged pilot kind adds its own "not
/// answering" item), so they never read as stale with the pilot kind.
fn alert_rows(core: &Core, alerts: &[PillAlert]) -> Vec<Row> {
    let t = &core.theme;
    let count = alerts_count_text(alerts.len()).unwrap_or_else(|| "–".into());
    let mut out = vec![(kv_text(t, "Alerts", LABEL, count), true)];
    for a in alerts {
        let (sym, color) = match a.severity {
            Severity::Critical => (core.sym.err, t.error()),
            Severity::Warning => (core.sym.warn, t.warning()),
        };
        let line = Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("{sym} {}", a.text), Style::default().fg(color)),
        ]);
        out.push((line, false));
    }
    out
}

/// A whole number for a count the wire sends as a JSON number.
fn whole(v: Option<f64>) -> String {
    v.filter(|v| v.is_finite())
        .map(|v| (v.round() as i64).to_string())
        .unwrap_or_else(|| "–".into())
}

/// The block's rows.
pub fn lines(core: &Core, i: &PilotInputs) -> Vec<Row> {
    let t = &core.theme;
    let Some(p) = i.pilot else {
        // Nothing publishes the pilot kind: the chip's absent state.
        let state = kv_text(t, "State", LABEL, spec_text("chip.no_pilot", &[]));
        let mut out = vec![(state, true)];
        out.extend(alert_rows(core, i.alerts));
        return out;
    };
    let p_stale = p.stale || i.age_ms >= stale_after_ms();
    let stale = |line: Line<'static>| mark_stale(t, line, p_stale);
    let mut out: Vec<Row> = Vec::new();

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
    out.push((stale(kv(t, "State", LABEL, state)), true));
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
        out.push((stale(Line::from(spans)), true));
    }

    // An FC failsafe and what tripped it, in the spec's words.
    if p.fc_failsafe {
        let mut text = spec_text("chip.failsafe", &[]);
        if let Some(cause) = &p.failsafe_cause {
            text.push_str(" · ");
            text.push_str(&spec_text(&format!("pilot.banner.cause.{cause}"), &[]));
        }
        let label = spec_text("cockpit.row.failsafe", &[]);
        let value = Span::styled(text, Style::default().fg(t.error()));
        out.push((stale(kv(t, &label, LABEL, value)), true));
    }
    out.extend(countdown_rows(core, p, i.age_ms, i.now_ms, p_stale));
    out.extend(place_rows(core, p, i.place_key, p_stale));

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
    let mode = if mode.is_empty() { "–".into() } else { mode };
    let label = spec_text("cockpit.row.mode", &[]);
    out.push((stale(kv_text(t, &label, LABEL, mode)), false));

    // The limits the sticks fly at now and, while they fly, what they ask
    // against the shaped speed the pilot sends.
    if let Some(l) = &p.profile_limits {
        let n = |v: f32| format!("{}", (f64::from(v) * 10.0).round() / 10.0);
        let text = spec_text(
            "pilot.limits.line",
            &[
                ("v_h", &n(l.v_h_mps)),
                ("v_up", &n(l.v_up_mps)),
                ("v_down", &n(l.v_down_mps)),
                ("yaw", &n(l.yaw_dps)),
                ("stop", &n(l.stop_m)),
            ],
        );
        let label = spec_text("cockpit.row.limits", &[]);
        out.extend(phrase_rows(t, &label, &text, t.bright(), p_stale));
    }
    if let Some(s) = &p.stick {
        let h = |v: [f32; 2]| format!("{:.1}", f64::from(v[0]).hypot(f64::from(v[1])));
        let mut text = spec_text(
            "pilot.stick.line",
            &[
                ("ask", &h(s.ask_ne_mps)),
                ("sent", &h(s.v_ne_mps)),
                ("vz", &format!("{:.1}", s.vz_mps)),
                ("yaw", &format!("{:.0}", s.yaw_rate_dps)),
            ],
        );
        if s.braking {
            text.push_str(" · ");
            text.push_str(&spec_text("pilot.stick.braking", &[]));
        }
        let label = spec_text("cockpit.row.stick", &[]);
        out.extend(phrase_rows(t, &label, &text, t.bright(), p_stale));
    }

    // The operations the pilot offers a surface now (`goto` is Fly Here).
    let ops = p
        .surface_ops
        .iter()
        .map(|op| op_text(op))
        .collect::<Vec<_>>()
        .join(" · ");
    let ops = if ops.is_empty() { "–".into() } else { ops };
    let label = spec_text("cockpit.row.offers", &[]);
    out.push((stale(kv_text(t, &label, LABEL, ops)), false));

    // Who holds the sticks, in the dashboard's words.
    if let Some(label) = &p.input_label {
        let text = spec_text("flow.sticks_with", &[("label", label)]);
        let row = spec_text("cockpit.row.sticks", &[]);
        out.push((stale(kv_text(t, &row, LABEL, text)), false));
    }
    if let Some(available) = i.gimbal {
        let value = if available {
            Span::styled(
                format!(
                    "{} {}",
                    core.sym.ok,
                    spec_text("cockpit.gimbal.takes_rates", &[])
                ),
                Style::default().fg(t.success()),
            )
        } else {
            Span::styled(spec_text("cockpit.unavailable", &[]), t.dim())
        };
        out.push((
            kv(t, &spec_text("cockpit.gimbal_pad", &[]), LABEL, value),
            false,
        ));
    }

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
    out.push((stale(Line::from(avoid)), false));

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
    out.push((stale(kv_text(t, "Mission", LABEL, mission)), false));

    let link = match link_loss_line(p) {
        Some(countdown) => Span::styled(countdown, Style::default().fg(t.warning())),
        None => Span::styled(
            format!("→ {}", link_loss_action_text(p.link_loss_action.as_deref())),
            t.bright(),
        ),
    };
    out.push((stale(kv(t, "Link loss", LABEL, link)), true));

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
    out.push((agl, false));
    out.extend(alert_rows(core, i.alerts));
    out
}

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

pub fn render(f: &mut Frame, area: Rect, core: &Core, rows: Vec<Row>, hidden: Option<&str>) {
    let t = &core.theme;
    let block = panel(t, core.sym, "Pilot");
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

#[cfg(test)]
mod tests;
