//! The Autonomy block from `/api/v1/nav/status`: follow phase, lock and range,
//! the re-acquire mode, the navigation source, the sensor rows (flow, range,
//! lidar, cameras; `sensor_rows.rs`) and the map session; and, from
//! `/api/vision/status`, the cameras the detector is running on.
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
use crate::model::autonomy::{health_tone, Autonomy, Tone};
use crate::model::spec_text::{has_spec_text, spec_text};
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

/// `Overhead clearance ahead: 3.9 m in 3 m` while the pilot reports leaves or
/// a ceiling along the way; `None` otherwise.
pub(super) fn overhead_line(p: &crate::model::autonomy::Pilot) -> Option<String> {
    let o = p.overhead_ahead.as_ref()?;
    let (h, d) = (o.fly_under_agl_m?, o.ahead_m?);
    Some(spec_text(
        "pilot.overhead_ahead",
        &[
            ("h", &format!("{h:.1}")),
            ("d", &format!("{:.0}", d.round())),
        ],
    ))
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

/// A spec word by key, or `–` when the spec has none for this value.
fn spec_or_dash(key: &str) -> String {
    if has_spec_text(key) {
        spec_text(key, &[])
    } else {
        "–".to_string()
    }
}

/// A follow phase's glyph and colour, in the shared HUD's role
/// (`PHASE_TONE`, `web-shared/src/hud/follow-chrome.ts`): following in the
/// `track` role; predicting, repositioning and searching in the `coast` role;
/// holding as a caution; holding for the operator as a fault. Each phase has
/// its own glyph, so the word never rides on colour.
fn phase_mark(core: &Core, phase: &str) -> (&'static str, Color) {
    let t = &core.theme;
    let ascii = core.sym.name == "ascii";
    let pick = |glyph: &'static str, plain: &'static str| if ascii { plain } else { glyph };
    match phase {
        "following" => (pick("◆", "*"), t.track()),
        "acquiring" => (pick("◌", "?"), t.warning()),
        "holding" => (pick("‖", "="), t.warning()),
        "predicting" => (pick("◇", "o"), t.coast()),
        "repositioning" => (pick("↗", ">"), t.coast()),
        "searching" => (pick("◎", "@"), t.coast()),
        "holding-await-operator" => (pick("✕", "x"), t.error()),
        _ => (core.sym.info, t.fg()),
    }
}

/// The phases a lost target is recovered in, against the search budget.
const RECOVERY_PHASES: [&str; 3] = ["predicting", "repositioning", "searching"];

/// The GPS fix as the nav-source spec words it (`nav_source.fix.*`; no GPS
/// and no fix are both "No fix"). Fix types the spec has no word for
/// (static, PPP) keep the MAVLink label. The GPS panel uses it too, so both
/// places name a fix alike.
pub(super) fn fix_word(fix: i64) -> String {
    let key = match fix {
        0 | 1 => "nav_source.q.no_fix",
        2 => "nav_source.fix.2d",
        3 => "nav_source.fix.3d",
        4 => "nav_source.fix.dgps",
        5 => "nav_source.fix.rtk_float",
        6 => "nav_source.fix.rtk_fixed",
        other => return crate::model::fix_label(other),
    };
    spec_text(key, &[])
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
        let phase = follow.phase.clone().unwrap_or_else(|| "stale".into());
        let mut word = spec_text(&format!("follow.phase.{phase}"), &[]);
        if !has_spec_text(&format!("follow.phase.{phase}")) {
            word = phase.clone();
        }
        if let Some(id) = follow.track_id {
            word.push_str(&format!(" #{id}"));
        }
        if RECOVERY_PHASES.contains(&phase.as_str()) {
            if let Some(s) = follow.reacquire_remaining_s {
                word.push_str(&format!(" {s:.1} s"));
            }
        }
        let (glyph, color) = phase_mark(core, &phase);
        let range = match (follow.range_m, follow.range_sigma_m) {
            (Some(r), Some(s)) => format!("{r:.1} m ± {s:.1} m"),
            (Some(r), None) => format!("{r:.1} m"),
            _ => "–".to_string(),
        };
        let reacquire = match follow.reacquire_mode.as_deref() {
            Some(m @ ("smart" | "strict")) => spec_text(&format!("follow.mode.{m}"), &[]),
            _ => "–".to_string(),
        };
        let phase_span = Span::styled(format!("{glyph} {word}"), Style::default().fg(color));
        let mut rows = vec![kv(t, "Follow", LABEL, phase_span)];
        // The waiting prompt sits right under the phase, so a short terminal
        // keeps it, and without a label, so the whole title fits the column.
        if let Some(title) = &follow.prompt_title {
            rows.push(kv(t, "", 0, tone_span(core, Tone::Fault, title.clone())));
        }
        // Why it holds, in the spec's words (`follow.hold_reason.*`).
        if matches!(phase.as_str(), "holding" | "holding-await-operator") {
            if let Some(reason) = &follow.hold_reason {
                let key = format!("follow.hold_reason.{reason}");
                let word = if has_spec_text(&key) {
                    spec_text(&key, &[])
                } else {
                    reason.clone()
                };
                rows.push(kv_text(t, "Reason", LABEL, word));
            }
        }
        let search: Vec<String> = [
            follow.search_step.map(|n| format!("step {}", n + 1)),
            follow
                .occluder_kind
                .as_ref()
                .map(|k| spec_text(&format!("follow.occluder.{k}"), &[])),
        ]
        .into_iter()
        .flatten()
        .collect();
        if !search.is_empty() {
            rows.push(kv_text(t, "Search", LABEL, search.join(" · ")));
        }
        rows.push(kv_text(t, "Range", LABEL, range));
        // Lowered under leaves: the height flown, then why on its own line.
        if let Some(h) = follow.height_lowered_m {
            let text = spec_text("follow.height_lowered", &[("v", &format!("{h:.1}"))]);
            rows.push(kv(t, "", 0, tone_span(core, Tone::Caution, text)));
            if let Some(why) = &follow.height_reason {
                let why = spec_text(&format!("follow.height_reason.{why}"), &[]);
                rows.push(kv(t, "", 0, Span::styled(why, t.dim())));
            }
        }
        rows.push(kv_text(t, "Reacquire", LABEL, reacquire));
        for line in rows {
            out.push(mark_stale(t, line, follow.stale));
        }
    }
    // The overhead clearance the pilot reports ahead, only while it is fresh.
    if let Some(line) = a
        .pilot
        .as_ref()
        .filter(|p| !p.stale)
        .and_then(overhead_line)
    {
        out.push(kv_text(t, "", 0, line));
    }
    if let Some(nav) = &a.nav {
        let health = nav.extnav_health.as_deref().unwrap_or("unknown");
        // The pilot's own verdict first, else Navigation's reason on a
        // degraded or lost pose: a crit word, never a calm "degraded".
        let unverified = a
            .pilot
            .as_ref()
            .filter(|p| !p.stale)
            .and_then(|p| p.position_unverified.as_ref())
            .map(|p| p.detail.as_deref().unwrap_or(&p.reason))
            .or(nav
                .health_reason
                .as_deref()
                .filter(|_| matches!(health, "degraded" | "lost")))
            .map(crate::model::nav_source::unverified_word);
        let quality = unverified.map_or_else(
            || spec_or_dash(&format!("nav_source.q.{health}")),
            |k| spec_text(k, &[]),
        );
        let tone = if unverified.is_some() {
            crate::model::autonomy::Tone::Fault
        } else {
            health_tone(health)
        };
        let word = format!(
            "{} {} · {} {}",
            spec_text("nav_source.set", &[]),
            opt(nav.active_set, |s| s.to_string()),
            spec_text("nav_source.ext_nav", &[]),
            quality,
        );
        let odometry = format!(
            "{} · {} {}",
            opt(nav.odom_rate_hz, |r| format!("{r:.0} Hz")),
            spec_text("nav_source.gps", &[]),
            nav.gps_fix.map_or_else(|| "–".to_string(), fix_word),
        );
        let mut rows = vec![
            kv(t, "Nav", LABEL, tone_span(core, tone, word)),
            kv_text(t, "Odometry", LABEL, odometry),
        ];
        if let Some(ms) = nav.pose_age_ms {
            rows.push(kv_text(t, "Pose", LABEL, format!("{ms:.0} ms")));
        }
        for line in rows {
            out.push(mark_stale(t, line, nav.stale));
        }
    }
    out.extend(super::sensor_rows::rows(core, a));
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
/// The lines that fit `rows`: all of them, or the first ones and a dim
/// "… N more" line in place of the rest, so a short terminal says what it cut.
pub(super) fn fit(t: &Theme, mut lines: Vec<Line<'static>>, rows: u16) -> Vec<Line<'static>> {
    let rows = rows as usize;
    if lines.len() <= rows || rows == 0 {
        return lines;
    }
    let kept = rows - 1;
    let more = lines.len() - kept;
    lines.truncate(kept);
    lines.push(Line::from(Span::styled(
        format!("… {more} more (a taller terminal shows all)"),
        t.dim(),
    )));
    lines
}

/// The number of lines the block draws for these inputs.
pub fn line_count(core: &Core, a: Option<&Autonomy>, det: Option<&Detection>) -> usize {
    match a.filter(|a| a.any()) {
        Some(a) => lines(core, a, det).len(),
        None => lines(core, &Autonomy::default(), det).len() + 1,
    }
}

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
    f.render_widget(Paragraph::new(fit(t, out, inner.height)), inner);
}

#[cfg(test)]
mod phase_tests {
    use super::phase_mark;
    use crate::ui::tests::test_app;

    #[test]
    fn each_follow_phase_has_its_own_glyph_and_the_shared_role_colour() {
        let app = test_app();
        let (core, t) = (&app.core, &app.core.theme);
        let phases = [
            "idle",
            "acquiring",
            "following",
            "holding",
            "predicting",
            "repositioning",
            "searching",
            "holding-await-operator",
        ];
        let mut glyphs: Vec<_> = phases.iter().map(|p| phase_mark(core, p).0).collect();
        glyphs.sort_unstable();
        glyphs.dedup();
        assert_eq!(glyphs.len(), phases.len(), "{glyphs:?}");
        assert_eq!(phase_mark(core, "following").1, t.track());
        for p in ["predicting", "repositioning", "searching"] {
            assert_eq!(phase_mark(core, p).1, t.coast(), "{p}");
        }
        assert_eq!(phase_mark(core, "holding").1, t.warning());
        assert_eq!(phase_mark(core, "holding-await-operator").1, t.error());
    }
}
