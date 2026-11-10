//! The Autonomy block from `/api/v1/nav/status`: follow phase, lock and range,
//! the re-acquire mode, the navigation source, the lidar and the map session;
//! and, from `/api/vision/status`, the cameras the detector is running on.
//! Every state is a symbol and a word; colour only repeats what the word says.
//! Every row built from a stale kind (or from a failed read) is dim and
//! marked "(stale)".

use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::app::Core;
use crate::model::autonomy::detection::{DepthLayer, Detection};
use crate::model::autonomy::{health_tone, phase_tone, Autonomy, Tone};
use crate::model::fix_label;
use crate::theme::Theme;
use crate::ui::widgets::{kv, kv_text, panel};

/// Label column width.
pub(super) const LABEL: usize = 9;

/// The block's text when nothing has published any kind yet.
pub const NOTHING_TEXT: &str = "no autonomy plugin is publishing";

/// What a stale row reads after its value.
pub(super) const STALE_SUFFIX: &str = " (stale)";

pub(super) fn tone_span(core: &Core, tone: Tone, word: String) -> Span<'static> {
    let t = &core.theme;
    let (sym, color): (&str, Option<Color>) = match tone {
        Tone::Good => (core.sym.ok, Some(t.success())),
        Tone::Caution => (core.sym.warn, Some(t.warning())),
        Tone::Fault => (core.sym.err, Some(t.error())),
        Tone::Neutral => (core.sym.info, None),
    };
    let text = format!("{sym} {word}");
    match color {
        Some(c) => Span::styled(text, Style::default().fg(c)),
        None => Span::styled(text, t.text()),
    }
}

/// A `label value` row from a stale source: every value span dim, then
/// "(stale)". The label (the first span) keeps its style.
pub(super) fn mark_stale(t: &Theme, mut line: Line<'static>, stale: bool) -> Line<'static> {
    if stale {
        for span in line.spans.iter_mut().skip(1) {
            span.style = t.dim();
        }
        line.spans.push(Span::styled(STALE_SUFFIX, t.dim()));
    }
    line
}

fn opt<T>(v: Option<T>, fmt: impl Fn(T) -> String) -> String {
    v.map(fmt).unwrap_or_else(|| "–".to_string())
}

/// The detector row: `idle`, or the cameras it runs on, plus ` · always` when
/// it runs on every camera regardless of demand.
fn detector_line(core: &Core, d: &Detection) -> Line<'static> {
    let tone = if d.detecting.is_empty() {
        Tone::Neutral
    } else {
        Tone::Good
    };
    let mut word = d.word();
    if d.mode.as_deref() == Some("always") {
        word.push_str(" · always");
    }
    kv(&core.theme, "Detector", LABEL, tone_span(core, tone, word))
}

/// The depth row: where the depth grid and the landing zones run, `idle`, or
/// why the layer cannot run (no model installed, model failed).
fn depth_line(core: &Core, d: &DepthLayer) -> Line<'static> {
    let tone = match d.state.as_str() {
        "unavailable" => Tone::Fault,
        "no_model" => Tone::Neutral,
        _ if d.grids.is_empty() && d.surfaces.is_empty() => Tone::Neutral,
        _ => Tone::Good,
    };
    kv(&core.theme, "Depth", LABEL, tone_span(core, tone, d.word()))
}

/// The block's lines for `a` and the detector state `det`.
pub fn lines(core: &Core, a: &Autonomy, det: Option<&Detection>) -> Vec<Line<'static>> {
    let t = &core.theme;
    let mut out = Vec::new();
    if let Some(d) = det {
        out.push(detector_line(core, d));
        if let Some(depth) = &d.depth {
            out.push(depth_line(core, depth));
        }
    }
    if let Some(follow) = &a.follow {
        let phase = follow.phase.clone().unwrap_or_else(|| "unknown".into());
        let mut word = phase.clone();
        if let Some(id) = follow.track_id {
            word.push_str(&format!(" #{id}"));
        }
        if phase == "coasting" {
            if let Some(s) = follow.coast_remaining_s {
                word.push_str(&format!(" {s:.1} s"));
            }
        }
        let range = match (follow.range_m, follow.range_sigma_m) {
            (Some(r), Some(s)) => format!("{r:.1} m ± {s:.1} m"),
            (Some(r), None) => format!("{r:.1} m"),
            _ => "–".to_string(),
        };
        let reacquire = follow.reacquire_mode.clone().unwrap_or_else(|| "–".into());
        for line in [
            kv(
                t,
                "Follow",
                LABEL,
                tone_span(core, phase_tone(&phase), word),
            ),
            kv_text(t, "Range", LABEL, range),
            kv_text(t, "Reacquire", LABEL, reacquire),
        ] {
            out.push(mark_stale(t, line, follow.stale));
        }
    }
    if let Some(nav) = &a.nav {
        let health = nav
            .extnav_health
            .clone()
            .unwrap_or_else(|| "unknown".into());
        let word = format!(
            "set {} · ext nav {health}",
            opt(nav.active_set, |s| s.to_string()),
        );
        let odometry = format!(
            "{} · GPS {}",
            opt(nav.odom_rate_hz, |r| format!("{r:.0} Hz")),
            opt(nav.gps_fix, fix_label)
        );
        for line in [
            kv(t, "Nav", LABEL, tone_span(core, health_tone(&health), word)),
            kv_text(t, "Odometry", LABEL, odometry),
        ] {
            out.push(mark_stale(t, line, nav.stale));
        }
    }
    // The nearest return: the scan's, else the nav kind's nearest obstacle;
    // each value carries the staleness of the kind it came from.
    let from_scan = a
        .lidar
        .as_ref()
        .and_then(|l| l.min_range_m.map(|m| (m, l.stale)));
    let from_nav = a
        .nav
        .as_ref()
        .and_then(|n| n.min_obstacle_m.map(|m| (m, n.stale)));
    if a.lidar.is_some() || from_nav.is_some() {
        let nearest = from_scan.or(from_nav);
        let rate = a
            .nav
            .as_ref()
            .and_then(|n| n.obstacle_rate_hz.map(|r| (r, n.stale)));
        let stale = nearest.map_or_else(|| a.lidar.as_ref().is_some_and(|l| l.stale), |(_, s)| s)
            || rate.is_some_and(|(_, s)| s);
        let line = kv_text(
            t,
            "Lidar",
            LABEL,
            format!(
                "min {} · {}",
                opt(nearest, |(m, _)| format!("{m:.1} m")),
                opt(rate, |(r, _)| format!("{r:.0} Hz")),
            ),
        );
        out.push(mark_stale(t, line, stale));
    }
    if let Some(map) = &a.map {
        let memory = match (map.memory_used_mb, map.memory_budget_mb) {
            (Some(u), Some(b)) if b > 0.0 => {
                format!("{u:.0}/{b:.0} MB ({:.0}%)", 100.0 * u / b)
            }
            (Some(u), _) => format!("{u:.0} MB"),
            _ => "–".to_string(),
        };
        let tone = match (map.memory_used_mb, map.memory_budget_mb) {
            (Some(u), Some(b)) if b > 0.0 && u / b >= 0.9 => Tone::Caution,
            _ => Tone::Neutral,
        };
        let word = format!(
            "{memory} · voxel {}",
            opt(map.voxel_m, |v| format!("{:.0} cm", v * 100.0))
        );
        let summary = format!(
            "{} pts · {}",
            opt(map.sweep_points, |p| p.to_string()),
            opt(map.area_m2, |m| format!("{m:.0} m²")),
        );
        for line in [
            kv_text(t, "Map", LABEL, summary),
            kv(t, "Memory", LABEL, tone_span(core, tone, word)),
        ] {
            out.push(mark_stale(t, line, map.stale));
        }
    }
    out
}

/// The block. `failed` is the note for a failed read: beside the title while
/// the last good reading stays (every row marked stale), else in the body.
pub fn render(
    f: &mut Frame,
    area: Rect,
    core: &Core,
    a: Option<&Autonomy>,
    det: Option<&Detection>,
    hidden: Option<&str>,
    failed: Option<&str>,
) {
    let t = &core.theme;
    let a = a.filter(|a| a.any()).filter(|_| hidden.is_none());
    let title = match (a, failed) {
        (Some(_), Some(why)) => format!("Autonomy · {why}"),
        _ => "Autonomy".to_string(),
    };
    let block = panel(t, core.sym, &title);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let note = |text: &str| Line::from(Span::styled(text.to_string(), t.dim()));
    if let Some(why) = hidden {
        f.render_widget(Paragraph::new(note(why)), inner);
        return;
    }
    let out = match a {
        Some(a) => lines(core, a, det),
        None => {
            let mut out = Vec::new();
            if det.is_some() {
                out.extend(lines(core, &Autonomy::default(), det));
            }
            // A failed first read is not "nothing is publishing".
            out.push(note(failed.unwrap_or(NOTHING_TEXT)));
            out
        }
    };
    f.render_widget(Paragraph::new(out), inner);
}
