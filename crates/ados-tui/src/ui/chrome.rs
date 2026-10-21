//! Screen chrome: header, tab bar, context line, hint bar, toasts, and the
//! too-small notice.

use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use ratatui::Frame;

use super::widgets::{chip, truncate};
use crate::app::{Core, FrameOut, Hit, TabId};
use crate::model::FcLink;
use crate::toast::ToastKind;

/// Width of a span list in display columns.
fn width(spans: &[Span]) -> u16 {
    spans.iter().map(|s| s.width() as u16).sum()
}

/// Row 0: word-mark, identity chips, target, clock.
pub fn header(f: &mut Frame, area: Rect, core: &Core, out: &mut FrameOut) {
    let t = &core.theme;
    let stale = core.stale();
    let mut left: Vec<Span> = vec![
        Span::styled(
            if core.sym.name == "ascii" { "|" } else { "▌" },
            t.accented(),
        ),
        Span::styled("ADOS", t.accent_bold()),
        Span::raw("  "),
    ];
    let mut armed_at: Option<(u16, u16)> = None;
    match core.dash() {
        Some(d) => {
            left.push(Span::styled(d.device_name.clone(), t.bright()));
            left.push(Span::raw("  "));
            if !d.profile.is_empty() {
                left.push(Span::styled(d.profile.to_uppercase(), t.accent_bold()));
                left.push(Span::raw("  "));
            }
            let alive = matches!(d.fc_link(), FcLink::Connected | FcLink::Msp);
            let family = d.fc_family_label().or_else(|| d.fc_variant_label());
            if d.profile != "ground_station" {
                if alive && !stale {
                    left.push(Span::styled("● ", Style::default().fg(t.success())));
                    left.push(Span::styled(
                        format!("FC {}", family.unwrap_or_default())
                            .trim_end()
                            .to_string(),
                        t.text(),
                    ));
                } else {
                    left.push(Span::styled("○ FC", t.dim()));
                }
                left.push(Span::raw("  "));
            }
            if !stale {
                if let Some(armed) = d.armed {
                    let x = width(&left);
                    let s = if armed {
                        chip(t, "ARMED", t.error())
                    } else {
                        chip(t, "DISARMED", t.success())
                    };
                    armed_at = Some((x, s.width() as u16));
                    left.push(s);
                    left.push(Span::raw("  "));
                }
                if let Some(mode) = &d.mode {
                    left.push(Span::styled(mode.clone(), t.bright()));
                }
            }
        }
        None => left.push(Span::styled("connecting…", t.dim())),
    }
    if let Some((x, w)) = armed_at {
        out.armed_chip = Some(Rect::new(area.x + x, area.y, w, 1));
    }

    let mut right: Vec<Span> = vec![
        Span::styled("⇄ ", t.dim()),
        Span::styled(core.target.label(), t.text()),
        Span::raw("   "),
    ];
    let locked = core.view.locked();
    let offline = !locked && core.view.error.is_some() && (core.dash().is_none() || stale);
    let state_chip = if locked {
        Some(chip(t, "LOCKED", t.warning()))
    } else if offline {
        Some(chip(t, "OFFLINE", t.error()))
    } else if stale {
        Some(chip(t, "STALE", t.warning()))
    } else {
        None
    };
    let right_w_before = width(&right);
    match state_chip {
        Some(c) => {
            let cw = c.width() as u16;
            right.push(c);
            let rw = width(&right);
            let x = area.x + area.width.saturating_sub(rw) + right_w_before;
            // A lock waits on the operator, not the agent: it does not pulse.
            if !locked {
                out.offline_chip = Some(Rect::new(x, area.y, cw, 1));
            }
        }
        None => right.push(Span::styled(
            core.view
                .refreshed
                .clone()
                .unwrap_or_else(|| "--:--:--".into()),
            t.dim(),
        )),
    }
    let rw = width(&right) + 1;
    let cols = Layout::horizontal([Constraint::Fill(1), Constraint::Length(rw)]).split(area);
    f.render_widget(Paragraph::new(Line::from(left)), cols[0]);
    f.render_widget(
        Paragraph::new(Line::from(right)).alignment(Alignment::Right),
        cols[1],
    );
}

/// Row 1: the tabs visible on this profile.
pub fn tab_bar(f: &mut Frame, area: Rect, core: &Core, out: &mut FrameOut) {
    let t = &core.theme;
    let profile = core.profile();
    let tabs: Vec<TabId> = TabId::ALL
        .into_iter()
        .filter(|tb| tb.visible(profile))
        .collect();
    // Widest form that fits: icon + label, then label, then the short label
    // (always below 100 columns).
    let label = |tab: TabId, mode: u8| {
        let icon = core.sym.tab_icons[tab.index()];
        match mode {
            0 if !icon.is_empty() => format!(" {} {} {} ", tab.digit(), icon, tab.label(profile)),
            0 | 1 => format!(" {} {} ", tab.digit(), tab.label(profile)),
            _ => format!(" {} {} ", tab.digit(), tab.short(profile)),
        }
    };
    let fits = |mode: u8| {
        let w: usize = tabs
            .iter()
            .map(|tb| Span::raw(label(*tb, mode)).width() + 1)
            .sum();
        w < area.width as usize
    };
    let mode = if area.width < 100 {
        2
    } else if fits(0) {
        0
    } else if fits(1) {
        1
    } else {
        2
    };
    let mut x = area.x + 1;
    let mut spans: Vec<Span> = vec![Span::raw(" ")];
    for tab in tabs {
        let style = if tab == core.tab {
            t.on_accent_fill().add_modifier(Modifier::BOLD)
        } else if core.hover_tab == Some(tab) {
            t.text()
        } else {
            t.dim()
        };
        let span = Span::styled(label(tab, mode), style);
        let w = span.width() as u16;
        out.hit(Rect::new(x, area.y, w, 1), Hit::Tab(tab));
        x += w + 1;
        spans.push(span);
        spans.push(Span::raw(" "));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// The dim description line under the body.
pub fn context_line(f: &mut Frame, area: Rect, core: &Core, text: Option<String>) {
    if let Some(text) = text {
        let line = Line::from(Span::styled(
            format!(
                " {}",
                truncate(&text, area.width.saturating_sub(2) as usize)
            ),
            core.theme.dim(),
        ));
        f.render_widget(Paragraph::new(line), area);
    }
}

/// The bottom row: key hints on the left; the screen's status (e.g. `PAUSED
/// +12`), the version and the update badge on the right, so the status is
/// never clipped by a long hint list.
pub fn hint_bar(
    f: &mut Frame,
    area: Rect,
    core: &Core,
    hints: &[(&str, &str)],
    extra: Option<String>,
) {
    let t = &core.theme;
    let mut left: Vec<Span> = vec![Span::raw(" ")];
    for (i, (key, word)) in hints.iter().enumerate() {
        if i > 0 {
            left.push(Span::styled(" · ", t.dim()));
        }
        left.push(Span::styled(key.to_string(), t.accented()));
        left.push(Span::styled(format!(" {word}"), t.dim()));
    }
    let mut right: Vec<Span> = Vec::new();
    if let Some(extra) = extra {
        right.push(Span::styled(extra, t.accent_bold()));
        right.push(Span::raw("   "));
    }
    if let Some(d) = core.dash() {
        right.push(Span::styled(format!("v{}", d.version), t.dim()));
        if let Some(latest) = core.update_available() {
            right.push(Span::styled(
                format!(" ⬆ {latest}"),
                Style::default().fg(t.warning()),
            ));
        }
    }
    let rw = width(&right) + 1;
    let cols = Layout::horizontal([Constraint::Fill(1), Constraint::Length(rw)]).split(area);
    f.render_widget(Paragraph::new(Line::from(left)), cols[0]);
    f.render_widget(
        Paragraph::new(Line::from(right)).alignment(Alignment::Right),
        cols[1],
    );
}

/// Toasts, stacked bottom-right with the newest lowest; the last row of
/// `area` sits just above the hint bar.
pub fn toasts(f: &mut Frame, area: Rect, core: &Core, out: &mut FrameOut) {
    let t = &core.theme;
    let shown = core.toasts.visible(core.now);
    let n = shown.len() as u16;
    for (i, toast) in shown.into_iter().enumerate() {
        let (glyph, color) = match toast.kind {
            ToastKind::Info => (core.sym.info, t.info()),
            ToastKind::Success => (core.sym.ok, t.success()),
            ToastKind::Warn => (core.sym.warn, t.warning()),
            ToastKind::Error => (core.sym.err, t.error()),
        };
        let max_w = area.width.saturating_sub(4).min(72) as usize;
        let text = truncate(&toast.text, max_w.saturating_sub(4));
        let w = (text.chars().count() as u16 + 5).min(area.width);
        let y = area.y + area.height.saturating_sub(n - i as u16);
        let rect = Rect::new(area.x + area.width.saturating_sub(w + 1), y, w, 1);
        f.render_widget(Clear, rect);
        let line = Line::from(vec![
            Span::styled("▌", Style::default().fg(color)),
            Span::styled(
                format!("{glyph} "),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("{text} "), t.text()),
        ]);
        let mut p = Paragraph::new(line);
        if let Some(bg) = t.canvas() {
            p = p.style(Style::default().bg(bg));
        }
        f.render_widget(p, rect);
        out.toasts.push((toast.id, rect));
    }
}

/// Below 80×24 the whole screen is this notice.
pub fn too_small(f: &mut Frame, area: Rect, core: &Core) {
    let msg = format!(
        "Terminal too small: need {}×{}, have {}×{}",
        super::MIN_W,
        super::MIN_H,
        area.width,
        area.height
    );
    let y = area.y + area.height / 2;
    let rect = Rect::new(
        area.x,
        y.min(area.bottom().saturating_sub(1)),
        area.width,
        1.min(area.height),
    );
    f.render_widget(
        Paragraph::new(Span::styled(msg, Style::default().fg(core.theme.warning())))
            .alignment(Alignment::Center),
        rect,
    );
}
