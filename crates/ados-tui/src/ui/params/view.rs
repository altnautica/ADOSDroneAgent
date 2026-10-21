//! Params tab rendering: group sidebar, parameter table, detail pane.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Gauge, Paragraph, Row, Table, Wrap};
use ratatui::Frame;

use super::{availability, Avail, Focus, ParamsTab, FIXED};
use crate::app::{Core, FrameOut, Hit};
use crate::params::metadata::{format_labeled, plain, ParamMeta};
use crate::params::same;
use crate::ui::widgets::{scroll_offset, truncate};

/// Detail pane height: a rule plus five rows.
const DETAIL_H: u16 = 6;

pub(super) fn render(tab: &mut ParamsTab, f: &mut Frame, area: Rect, core: &Core, out: &mut FrameOut) {
    let t = &core.theme;
    let message = match availability(core) {
        Avail::Ready => None,
        Avail::Msp => Some("MSP flight controller: parameters are edited in Mission Control's Configure tab"),
        Avail::Waiting => Some("Waiting for the flight controller…"),
    };
    if let Some(msg) = message {
        let y = area.y + area.height / 2;
        let rect = Rect::new(area.x, y.min(area.bottom().saturating_sub(1)), area.width, 1);
        f.render_widget(Paragraph::new(msg).style(t.dim()).centered(), rect);
        return;
    }

    let side_w = if area.width < 100 { 16 } else { 22 };
    let cols = Layout::horizontal([Constraint::Length(side_w), Constraint::Length(1), Constraint::Min(0)]).split(area);
    sidebar(tab, f, cols[0], core, out);
    let rule = Block::default()
        .borders(Borders::LEFT)
        .border_set(core.sym.border_set)
        .border_style(Style::default().fg(t.border()));
    f.render_widget(rule, cols[1]);

    let status = status_lines(tab, core);
    let rows = Layout::vertical([
        Constraint::Length(status.len() as u16),
        Constraint::Min(3),
        Constraint::Length(DETAIL_H),
    ])
    .split(cols[2]);
    let status_rects = Layout::vertical(vec![Constraint::Length(1); status.len()]).split(rows[0]);
    for (line, rect) in status.into_iter().zip(status_rects.iter()) {
        match line {
            Status::Text(l) => f.render_widget(Paragraph::new(l), *rect),
            Status::Gauge(ratio, label) => f.render_widget(
                Gauge::default()
                    .gauge_style(Style::default().fg(t.accent()))
                    .ratio(ratio.clamp(0.0, 1.0))
                    .label(Span::styled(label, Style::default().fg(t.on_accent()))),
                *rect,
            ),
            Status::Search(prefix) => {
                let value = tab.search.as_ref().map(|i| i.value().to_string()).unwrap_or_default();
                let cursor = tab.search.as_ref().map(|i| i.visual_cursor()).unwrap_or(0);
                let line = Line::from(vec![Span::styled(prefix.clone(), t.accent_bold()), Span::styled(value, t.bright())]);
                f.render_widget(Paragraph::new(line), *rect);
                let x = rect.x + (prefix.chars().count() + cursor) as u16;
                out.cursor = Some((x.min(rect.right().saturating_sub(1)), rect.y));
            }
        }
    }
    table(tab, f, rows[1], core, out);
    detail(tab, f, rows[2], core);
}

enum Status {
    Text(Line<'static>),
    Gauge(f64, String),
    Search(String),
}

fn status_lines(tab: &ParamsTab, core: &Core) -> Vec<Status> {
    let t = &core.theme;
    let mut lines = Vec::new();
    if tab.search.is_some() {
        lines.push(Status::Search("/ ".into()));
    } else {
        let mut spans = vec![Span::styled(format!("{} shown", tab.store.visible().len()), t.dim())];
        if !tab.store.query().is_empty() {
            spans.push(Span::styled(format!(" · search \"{}\"", tab.store.query()), t.accented()));
        }
        match (tab.store.catalog(), &tab.loading) {
            (_, Some(_)) => spans.push(Span::styled(
                format!(" · {} loading metadata", core.sym.spinner_frame(core.frame as usize)),
                t.dim(),
            )),
            (Some(c), None) => spans.push(Span::styled(
                format!(
                    " · {} {} metadata ({} known)",
                    c.provenance.firmware, c.provenance.version, c.provenance.param_count
                ),
                t.dim(),
            )),
            (None, None) => {}
        }
        lines.push(Status::Text(Line::from(spans)));
    }
    if let Some(slot) = core.slot(&crate::poll::ResourceId::Params) {
        if let Some(err) = &slot.error {
            lines.push(Status::Text(warn_line(core, format!("Parameter read failed: {err}"), true)));
        }
    }
    let env = tab.env;
    if env.priming {
        let ratio = if env.expected > 0 { env.got as f64 / env.expected as f64 } else { 0.0 };
        lines.push(Status::Gauge(ratio, format!("Reading parameters {}/{}", env.got, env.expected)));
    }
    if env.timeout {
        lines.push(Status::Text(warn_line(
            core,
            "The flight controller did not answer the parameter request in time.".into(),
            false,
        )));
    }
    if env.send_failed {
        lines.push(Status::Text(warn_line(
            core,
            "The parameter request could not be sent to the flight controller.".into(),
            false,
        )));
    }
    if env.loaded && !env.priming && tab.store.live().is_empty() {
        lines.push(Status::Text(Line::from(Span::styled("No parameters cached yet.", t.dim()))));
    }
    if !env.loaded && core.slot(&crate::poll::ResourceId::Params).is_none() {
        lines.push(Status::Text(Line::from(Span::styled(
            format!("{} Loading parameters…", core.sym.spinner_frame(core.frame as usize)),
            t.dim(),
        ))));
    }
    lines
}

fn warn_line(core: &Core, text: String, error: bool) -> Line<'static> {
    let (glyph, color) = if error {
        (core.sym.err, core.theme.error())
    } else {
        (core.sym.warn, core.theme.warning())
    };
    Line::from(Span::styled(format!("{glyph} {text}"), Style::default().fg(color)))
}

fn sidebar(tab: &mut ParamsTab, f: &mut Frame, area: Rect, core: &Core, out: &mut FrameOut) {
    let t = &core.theme;
    let c = tab.store.counts();
    let mut entries: Vec<(String, usize)> = vec![
        ("All".into(), c.all),
        ("Staged".into(), c.staged),
        ("Non-default".into(), c.non_default),
        ("Favourites".into(), c.favourites),
    ];
    entries.extend(tab.store.groups().iter().cloned());
    let height = area.height as usize;
    tab.side_off = scroll_offset(tab.side_sel, tab.side_off, height);
    let width = area.width as usize;
    for (row, i) in (tab.side_off..entries.len()).take(height).enumerate() {
        let (label, count) = &entries[i];
        let rect = Rect::new(area.x, area.y + row as u16, area.width, 1);
        let selected = i == tab.side_sel;
        let marker = if selected { core.sym.page_marker } else { " " };
        let count = count.to_string();
        let room = width.saturating_sub(3 + count.len());
        let name = truncate(label, room);
        let pad = room.saturating_sub(name.chars().count());
        let style = match (selected, tab.focus == Focus::Sidebar) {
            (true, true) => t.on_accent_fill(),
            (true, false) => t.accent_bold(),
            (false, _) if i < FIXED => t.text(),
            _ => t.dim(),
        };
        let line = Line::from(vec![
            Span::styled(format!("{marker} "), t.accented()),
            Span::styled(format!("{name}{} {count}", " ".repeat(pad)), style),
        ]);
        f.render_widget(Paragraph::new(line), rect);
        out.hit(rect, Hit::Sidebar(i));
    }
}

fn range_text(meta: Option<&ParamMeta>) -> String {
    let Some(m) = meta else { return String::new() };
    if let Some(v) = m.enum_values() {
        format!("{} options", v.len())
    } else if let Some(b) = m.bits() {
        format!("bitmask {}", b.len())
    } else if let Some(r) = m.range {
        format!("{} .. {}", plain(r.min), plain(r.max))
    } else {
        String::new()
    }
}

fn markers(tab: &ParamsTab, core: &Core, name: &str, meta: Option<&ParamMeta>) -> Line<'static> {
    let t = &core.theme;
    let s = core.sym;
    let mut spans = Vec::new();
    if let Some(v) = tab.store.staged_value(name) {
        spans.push(Span::styled(s.staged, Style::default().fg(t.accent())));
        if tab.store.drift(name) {
            spans.push(Span::styled(tab.drift_glyph(core), Style::default().fg(t.warning())));
        }
        if meta.is_some_and(|m| m.out_of_range(v)) {
            spans.push(Span::styled(tab.out_of_range_glyph(core), Style::default().fg(t.warning())));
        }
    }
    if tab.store.non_default(name) {
        spans.push(Span::styled(s.non_default, Style::default().fg(t.fg())));
    }
    if tab.store.is_favourite(name) {
        spans.push(Span::styled(s.favourite, Style::default().fg(t.accent())));
    }
    if meta.is_some_and(|m| m.reboot_required) {
        spans.push(Span::styled(s.reboot, Style::default().fg(t.muted())));
    }
    if tab.store.read_only(name) {
        spans.push(Span::styled(s.read_only, Style::default().fg(t.muted())));
    }
    Line::from(spans)
}

fn table(tab: &mut ParamsTab, f: &mut Frame, area: Rect, core: &Core, out: &mut FrameOut) {
    let t = &core.theme;
    let body_h = area.height.saturating_sub(1) as usize;
    tab.off = scroll_offset(tab.sel, tab.off, body_h);
    let tab: &ParamsTab = tab;
    let mut rows = Vec::new();
    for (row, (i, name)) in tab.store.visible().iter().enumerate().skip(tab.off).take(body_h).enumerate() {
        let meta = tab.store.meta(name);
        let staged = tab.store.staged_value(name);
        let value = staged.or(tab.store.value(name));
        let value_text = value.map(|v| format_labeled(v, meta)).unwrap_or_else(|| "—".into());
        let value_style = if staged.is_some() { t.accent_bold() } else { t.bright() };
        let selected = i == tab.sel;
        let style = match (selected, tab.focus == Focus::Table) {
            (true, true) => t.on_accent_fill(),
            (true, false) => Style::default().add_modifier(Modifier::REVERSED),
            _ => Style::default(),
        };
        let desc = meta
            .map(|m| if m.description.is_empty() { m.human_name.clone() } else { m.description.clone() })
            .unwrap_or_default();
        let cells = vec![
            Cell::from(markers(tab, core, name, meta)),
            Cell::from(Span::styled(name.clone(), t.text())),
            Cell::from(Span::styled(value_text, value_style)),
            Cell::from(Span::styled(meta.and_then(|m| m.units.clone()).unwrap_or_default(), t.dim())),
            Cell::from(Span::styled(range_text(meta), t.dim())),
            Cell::from(Span::styled(desc, t.dim())),
        ];
        rows.push(Row::new(cells).style(style));
        let rect = Rect::new(area.x, area.y + 1 + row as u16, area.width, 1);
        out.row(format!("param:{name}"), rect);
        out.hit(rect, Hit::Row(i));
    }
    let header = Row::new(["", "Name", "Value", "Units", "Range/Options", "Description"]).style(t.dim().add_modifier(Modifier::UNDERLINED));
    let widths = [
        Constraint::Length(6),
        Constraint::Length(17),
        Constraint::Length(20),
        Constraint::Length(7),
        Constraint::Length(15),
        Constraint::Fill(1),
    ];
    f.render_widget(Table::new(rows, widths).header(header).column_spacing(1), area);
}

fn detail(tab: &ParamsTab, f: &mut Frame, area: Rect, core: &Core) {
    let t = &core.theme;
    let rule = Block::default()
        .borders(Borders::TOP)
        .border_set(core.sym.border_set)
        .border_style(Style::default().fg(t.border()));
    let inner = rule.inner(area);
    f.render_widget(rule, area);
    let Some(name) = tab.store.visible().get(tab.sel).cloned() else { return };
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(inner);
    let meta = tab.store.meta(&name);

    let mut head = vec![Span::styled(name.clone(), t.accent_bold())];
    if let Some(h) = meta.map(|m| m.human_name.as_str()).filter(|h| !h.is_empty()) {
        head.push(Span::styled(format!("  {h}"), t.bright()));
    }
    if meta.is_some_and(|m| m.reboot_required) {
        head.push(Span::styled(format!("  {} reboot required", core.sym.reboot), Style::default().fg(t.warning())));
    }
    if tab.store.read_only(&name) {
        head.push(Span::styled(format!("  {} read-only", core.sym.read_only), t.dim()));
    }
    f.render_widget(Paragraph::new(Line::from(head)), rows[0]);

    let desc = match (meta, &tab.loading) {
        (Some(m), _) => m.description.clone(),
        (None, Some(_)) => "Loading metadata…".into(),
        (None, None) => "No metadata for this parameter.".into(),
    };
    f.render_widget(Paragraph::new(desc).style(t.text()).wrap(Wrap { trim: true }), rows[1]);

    let mut facts: Vec<String> = Vec::new();
    if let Some(m) = meta {
        if let Some(r) = m.range {
            facts.push(format!("range {} .. {}", plain(r.min), plain(r.max)));
        }
        if let Some(i) = m.increment {
            facts.push(format!("step {}", plain(i)));
        }
        if let Some(u) = m.units.as_deref().filter(|u| !u.is_empty()) {
            facts.push(format!("units {u}"));
        }
        if let Some(d) = m.default_value {
            facts.push(format!("default {}", format_labeled(d, Some(m))));
        }
    }
    let live = tab.store.value(&name);
    match (live, tab.store.staged_value(&name)) {
        (Some(l), Some(s)) => {
            let mut text = format!("live {} → staged {}", format_labeled(l, meta), format_labeled(s, meta));
            if tab.store.drift(&name) {
                text.push_str(&format!(" {} changed on the FC since staging", tab.drift_glyph(core)));
            }
            if meta.is_some_and(|m| m.out_of_range(s)) {
                text.push_str(&format!(" {} out of range", tab.out_of_range_glyph(core)));
            }
            facts.push(text);
        }
        (Some(l), None) => facts.push(format!("live {}", format_labeled(l, meta))),
        (None, Some(s)) => facts.push(format!("staged {}", format_labeled(s, meta))),
        (None, None) => {}
    }
    f.render_widget(Paragraph::new(facts.join(" · ")).style(t.dim()), rows[2]);

    let cur = tab.store.staged_value(&name).or(live);
    let listing = match meta {
        Some(m) if m.enum_values().is_some() => m
            .enum_values()
            .unwrap_or_default()
            .iter()
            .map(|(code, label)| {
                let on = cur.is_some_and(|c| same(c, *code));
                format!("{}{} {label}", if on { core.sym.page_marker } else { "" }, plain(*code))
            })
            .collect::<Vec<_>>()
            .join(" · "),
        Some(m) if m.bits().is_some() => {
            let raw = cur.unwrap_or(0.0).max(0.0) as u64;
            m.bits()
                .unwrap_or_default()
                .iter()
                .map(|(bit, label)| {
                    let mark = if raw & (1u64 << bit) != 0 { "[x]" } else { "[ ]" };
                    format!("{mark} {bit} {label}")
                })
                .collect::<Vec<_>>()
                .join("  ")
        }
        _ => String::new(),
    };
    let width = rows[3].width as usize;
    f.render_widget(Paragraph::new(truncate(&listing, width)).style(t.dim()), rows[3]);
}
