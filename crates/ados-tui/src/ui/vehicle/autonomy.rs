//! The Autonomy block from `/api/v1/nav/status`: who flies the vehicle (owner,
//! control mode and profile, obstacle avoidance), follow phase, lock and range,
//! the re-acquire mode, the navigation source, the lidar and the map session;
//! and, from `/api/vision/status`, the cameras the detector is running on.
//! Every state is a symbol and a word; colour only repeats what the word says.

use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::app::Core;
use crate::model::autonomy::{
    gps_fix_word, health_tone, phase_tone, Autonomy, DepthLayer, Detection, Pilot, Tone,
};
use crate::ui::widgets::{kv, kv_text, panel};

/// Label column width.
const LABEL: usize = 9;

/// The block's text when nothing has published any kind yet.
pub const NOTHING_TEXT: &str = "no autonomy plugin is publishing";

fn tone_span(core: &Core, tone: Tone, word: String) -> Span<'static> {
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

fn stale_suffix(stale: bool) -> &'static str {
    if stale {
        " (stale)"
    } else {
        ""
    }
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

/// A control mode as the pilot names it.
fn control_mode_word(mode: &str) -> String {
    match mode {
        "ados" => "ADOS PILOT".into(),
        "fc_manual" => "FC MANUAL".into(),
        "elrs" => "ELRS".into(),
        other => other.to_uppercase(),
    }
}

/// The Owner, Mode and Avoid rows; `–` throughout when nothing publishes `pilot`.
fn pilot_lines(core: &Core, p: Option<&Pilot>) -> Vec<Line<'static>> {
    let t = &core.theme;
    let Some(p) = p else {
        return ["Owner", "Mode", "Avoid"]
            .into_iter()
            .map(|k| kv_text(t, k, LABEL, "–".to_string()))
            .collect();
    };
    let mut owner = p
        .owner_label
        .clone()
        .or_else(|| p.owner.clone())
        .unwrap_or_else(|| "–".into());
    if p.owner.as_deref() == Some("skill") {
        // The pilot's owner label usually names the skill already.
        if let Some(label) = &p.skill_label {
            if !owner.to_lowercase().contains(&label.to_lowercase()) {
                owner.push_str(&format!(" · {label}"));
            }
        }
    }
    owner.push_str(stale_suffix(p.stale));
    let mode = match (p.control_mode.as_deref(), p.profile.as_deref()) {
        (Some(m), Some(pr)) => format!("{} · {}", control_mode_word(m), pr.to_uppercase()),
        (Some(m), None) => control_mode_word(m),
        (None, Some(pr)) => pr.to_uppercase(),
        (None, None) => "–".into(),
    };
    let avoid = if p.avoid.as_deref() == Some("off") {
        kv(
            t,
            "Avoid",
            LABEL,
            Span::styled("OFF", Style::default().fg(t.error())),
        )
    } else {
        let mut word = opt(p.avoid.clone(), |a| a);
        if let Some(state) = &p.avoid_state {
            word.push_str(&format!(" · {state}"));
        }
        if let Some(m) = p.min_clearance_m {
            word.push_str(&format!(" {m:.1} m"));
        }
        kv_text(t, "Avoid", LABEL, word)
    };
    vec![
        kv_text(t, "Owner", LABEL, owner),
        kv_text(t, "Mode", LABEL, mode),
        avoid,
    ]
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
    out.extend(pilot_lines(core, a.pilot.as_ref()));
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
        word.push_str(stale_suffix(follow.stale));
        out.push(kv(
            t,
            "Follow",
            LABEL,
            tone_span(core, phase_tone(&phase), word),
        ));
        let range = match (follow.range_m, follow.range_sigma_m) {
            (Some(r), Some(s)) => format!("{r:.1} m ± {s:.1} m"),
            (Some(r), None) => format!("{r:.1} m"),
            _ => "–".to_string(),
        };
        out.push(kv_text(t, "Range", LABEL, range));
        out.push(kv_text(
            t,
            "Reacquire",
            LABEL,
            follow.reacquire_mode.clone().unwrap_or_else(|| "–".into()),
        ));
    }
    if let Some(nav) = &a.nav {
        let health = nav
            .extnav_health
            .clone()
            .unwrap_or_else(|| "unknown".into());
        let word = format!(
            "set {} · ext nav {health}{}",
            opt(nav.active_set, |s| s.to_string()),
            stale_suffix(nav.stale)
        );
        out.push(kv(
            t,
            "Nav",
            LABEL,
            tone_span(core, health_tone(&health), word),
        ));
        out.push(kv_text(
            t,
            "Odometry",
            LABEL,
            format!(
                "{} · GPS {}",
                opt(nav.odom_rate_hz, |r| format!("{r:.0} Hz")),
                opt(nav.gps_fix, |f| gps_fix_word(f).to_string())
            ),
        ));
    }
    let nearest = a
        .lidar
        .as_ref()
        .and_then(|l| l.min_range_m)
        .or_else(|| a.nav.as_ref().and_then(|n| n.min_obstacle_m));
    if a.lidar.is_some() || a.nav.as_ref().is_some_and(|n| n.min_obstacle_m.is_some()) {
        let rate = a.nav.as_ref().and_then(|n| n.obstacle_rate_hz);
        let stale = a.lidar.as_ref().is_some_and(|l| l.stale);
        out.push(kv_text(
            t,
            "Lidar",
            LABEL,
            format!(
                "min {} · {}{}",
                opt(nearest, |m| format!("{m:.1} m")),
                opt(rate, |r| format!("{r:.0} Hz")),
                stale_suffix(stale)
            ),
        ));
    }
    if let Some(map) = &a.map {
        out.push(kv_text(
            t,
            "Map",
            LABEL,
            format!(
                "{} pts · {}{}",
                opt(map.sweep_points, |p| p.to_string()),
                opt(map.area_m2, |m| format!("{m:.0} m²")),
                stale_suffix(map.stale)
            ),
        ));
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
        out.push(kv(t, "Memory", LABEL, tone_span(core, tone, word)));
    }
    out
}

pub fn render(
    f: &mut Frame,
    area: Rect,
    core: &Core,
    a: Option<&Autonomy>,
    det: Option<&Detection>,
    hidden: Option<&str>,
) {
    let t = &core.theme;
    let block = panel(t, core.sym, "Autonomy");
    let inner = block.inner(area);
    f.render_widget(block, area);
    let note = |text: &str| Paragraph::new(Span::styled(text.to_string(), t.dim()));
    if let Some(why) = hidden {
        f.render_widget(note(why), inner);
        return;
    }
    let empty = Autonomy::default();
    match a.filter(|a| a.any()) {
        Some(a) => f.render_widget(Paragraph::new(lines(core, a, det)), inner),
        None if det.is_some() => {
            let mut l = lines(core, &empty, det);
            l.push(Line::from(Span::styled(NOTHING_TEXT.to_string(), t.dim())));
            f.render_widget(Paragraph::new(l), inner);
        }
        None => f.render_widget(note(NOTHING_TEXT), inner),
    }
}
