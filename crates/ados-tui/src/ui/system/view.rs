//! Drawing the System tab: resources, temperatures, the agent block, the
//! services and peripherals tables, and the hardware check.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use serde_json::Value;

use super::{hw_items, peripheral_name, peripherals, s, services, Pane, SystemTab};
use crate::app::{Core, FrameOut, Hit};
use crate::poll::ResourceId;
use crate::ui::widgets::{kv, kv_text, panel, render_cells, scroll_offset, truncate, Cell};

/// Draw the tab; returns the new per-pane scroll offsets.
pub fn render(f: &mut Frame, area: Rect, core: &Core, tab: &SystemTab, out: &mut FrameOut) -> [usize; 3] {
    let hw_len = hw_items(core).len() as u16;
    let rows = Layout::vertical([
        Constraint::Length(8),
        Constraint::Min(6),
        Constraint::Length((hw_len + 3).clamp(4, 12)),
    ])
    .split(area);
    let top = Layout::horizontal([
        Constraint::Percentage(42),
        Constraint::Percentage(22),
        Constraint::Percentage(36),
    ])
    .split(rows[0]);
    resources(f, top[0], core, tab);
    temperatures(f, top[1], core);
    agent(f, top[2], core);
    let mid = Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)]).split(rows[1]);
    let mut offsets = tab.offsets;
    offsets[0] = services_table(f, mid[0], core, tab, out);
    offsets[1] = peripherals_table(f, mid[1], core, tab, out);
    offsets[2] = hardware(f, rows[2], core, tab, out);
    offsets
}

fn num(v: &Value, key: &str) -> Option<f64> {
    v.get(key).and_then(Value::as_f64)
}

fn resources(f: &mut Frame, area: Rect, core: &Core, tab: &SystemTab) {
    let t = &core.theme;
    let block = panel(t, core.sym, "Resources");
    let inner = block.inner(area);
    f.render_widget(block, area);
    let Some(v) = core.value(&ResourceId::System) else {
        f.render_widget(Paragraph::new(Span::styled(loading(core, &ResourceId::System), t.dim())), inner);
        return;
    };
    let Some(cpu) = num(v, "cpu_percent") else {
        f.render_widget(
            Paragraph::new(Span::styled("Resource figures unavailable on this node", t.dim())),
            inner,
        );
        return;
    };
    let gauge_color = |pct: f64| match pct {
        p if p >= 90.0 => t.error(),
        p if p >= 75.0 => t.warning(),
        _ => t.accent(),
    };
    let cores = v.get("cpu_count").and_then(Value::as_u64).map(|n| format!(" · {n} cores")).unwrap_or_default();
    let mut cells = vec![
        Cell::Gauge { ratio: cpu / 100.0, label: format!("CPU {cpu:.1}%{cores}"), color: gauge_color(cpu) },
        Cell::Spark {
            data: tab.cpu.iter().map(|c| c.round().max(0.0) as u64).collect(),
            max: Some(100),
            color: t.accent(),
        },
    ];
    if let (Some(pct), Some(used), Some(total)) =
        (num(v, "memory_percent"), num(v, "memory_used_mb"), num(v, "memory_total_mb"))
    {
        cells.push(Cell::Gauge {
            ratio: pct / 100.0,
            label: format!("Memory {used:.0}/{total:.0} MB ({pct:.0}%)"),
            color: gauge_color(pct),
        });
    }
    if let (Some(pct), Some(used), Some(total)) =
        (num(v, "disk_percent"), num(v, "disk_used_gb"), num(v, "disk_total_gb"))
    {
        cells.push(Cell::Gauge {
            ratio: pct / 100.0,
            label: format!("Disk {used:.1}/{total:.1} GB ({pct:.0}%)"),
            color: gauge_color(pct),
        });
    }
    if let (Some(pct), Some(total)) = (num(v, "swap_percent"), num(v, "swap_total_mb")) {
        if total > 0.0 {
            cells.push(Cell::Line(kv_text(t, "Swap", 6, format!("{pct:.0}% of {total:.0} MB"))));
        }
    }
    render_cells(f, t, inner, cells);
}

fn loading(core: &Core, id: &ResourceId) -> String {
    match core.slot(id).and_then(|s| s.error.clone()) {
        Some(e) => format!("Unavailable: {e}"),
        None => "Loading…".to_string(),
    }
}

fn temperatures(f: &mut Frame, area: Rect, core: &Core) {
    let t = &core.theme;
    let block = panel(t, core.sym, "Temperatures");
    let inner = block.inner(area);
    f.render_widget(block, area);
    let temps: Vec<(String, f64)> = core
        .value(&ResourceId::System)
        .and_then(|v| v.get("temperatures"))
        .and_then(Value::as_object)
        .map(|m| m.iter().filter_map(|(k, v)| v.as_f64().map(|c| (k.clone(), c))).collect())
        .unwrap_or_default();
    if temps.is_empty() {
        f.render_widget(Paragraph::new(Span::styled("No sensors reported", t.dim())), inner);
        return;
    }
    let w = (inner.width as usize).saturating_sub(12).max(4);
    let lines: Vec<Line> = temps
        .iter()
        .map(|(name, c)| {
            let (glyph, color) = match *c {
                c if c >= 80.0 => (core.sym.err, t.error()),
                c if c >= 70.0 => (core.sym.warn, t.warning()),
                _ => (" ", t.fg()),
            };
            kv(
                t,
                &truncate(name, w),
                w,
                Span::styled(format!("{c:>5.1} °C {glyph}"), Style::default().fg(color)),
            )
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

fn agent(f: &mut Frame, area: Rect, core: &Core) {
    let t = &core.theme;
    let block = panel(t, core.sym, "Agent");
    let inner = block.inner(area);
    f.render_widget(block, area);
    let version = core.dash().map(|d| d.version.clone()).filter(|v| !v.is_empty());
    let latest = core.latest_version();
    let latest_span = match (&latest, core.update_available()) {
        (_, Some(newer)) => Span::styled(format!("{newer} ⬆ update available"), t.accent_bold()),
        (Some(l), None) => Span::styled(format!("{l} (up to date)"), t.text()),
        (None, None) => Span::styled("unknown", t.dim()),
    };
    let target = if core.target.remote {
        core.target.host.clone()
    } else {
        "local".to_string()
    };
    let mut lines = vec![
        kv_text(t, "Version", 9, version.unwrap_or_else(|| "unknown".into())),
        kv(t, "Latest", 9, latest_span),
        kv_text(t, "Target", 9, target),
        Line::default(),
    ];
    let key = |k: &str, what: &str, on: bool| {
        Line::from(vec![
            Span::styled(format!("{k:<2}"), if on { t.accent_bold() } else { t.dim() }),
            Span::styled(what.to_string(), if on { t.text() } else { t.dim() }),
        ])
    };
    let local = !core.target.remote;
    lines.push(key("u", if local { "update the agent" } else { "update (local only)" }, local));
    lines.push(key("S", "restart the supervisor", true));
    lines.push(key("X", "reboot the host", true));
    f.render_widget(Paragraph::new(lines), inner);
}

/// Draw `rows` in `inner` with the selection marker; records hits and returns
/// the scroll offset.
fn list(
    f: &mut Frame,
    inner: Rect,
    core: &Core,
    rows: Vec<Line<'static>>,
    sel: Option<(usize, usize)>,
    hit: &'static str,
    out: &mut FrameOut,
) -> usize {
    let height = inner.height as usize;
    let offset = sel.map_or(0, |(s, o)| scroll_offset(s, o, height));
    for (row, (i, line)) in rows.into_iter().enumerate().skip(offset).take(height).enumerate() {
        let focused = sel.is_some_and(|(s, _)| s == i);
        let mut spans = vec![Span::styled(
            if focused { format!("{} ", core.sym.row_marker) } else { "  ".to_string() },
            core.theme.accented(),
        )];
        spans.extend(line.spans);
        let rect = Rect::new(inner.x, inner.y + row as u16, inner.width, 1);
        let mut p = Paragraph::new(Line::from(spans));
        if focused {
            p = p.style(Style::default().add_modifier(Modifier::BOLD));
        }
        f.render_widget(p, rect);
        out.hit(rect, Hit::Item(hit, i));
    }
    offset
}

fn split_header(block_inner: Rect) -> (Rect, Rect) {
    let parts = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(block_inner);
    (parts[0], parts[1])
}

fn services_table(f: &mut Frame, area: Rect, core: &Core, tab: &SystemTab, out: &mut FrameOut) -> usize {
    let t = &core.theme;
    let block = panel(t, core.sym, "Services");
    let inner = block.inner(area);
    f.render_widget(block, area);
    let body = core.value(&ResourceId::Services);
    let svcs = services(core);
    if svcs.is_empty() {
        let text = match body.and_then(|b| b.get("systemd_available")).and_then(Value::as_bool) {
            Some(false) => "systemd is not reachable on this node".to_string(),
            Some(true) => "No agent services found".to_string(),
            None => loading(core, &ResourceId::Services),
        };
        f.render_widget(Paragraph::new(Span::styled(text, t.dim())), inner);
        return 0;
    }
    let (head, rest) = split_header(inner);
    f.render_widget(
        Paragraph::new(Span::styled(format!("  {:<24} {:<12} {:<11} {:>9}", "Service", "Active", "State", "Memory"), t.dim())),
        head,
    );
    let rows: Vec<Line<'static>> = svcs
        .iter()
        .map(|v| {
            let state = s(v, "state");
            let (glyph, word, style) = match (v.get("active").and_then(Value::as_bool), state) {
                (Some(true), _) => (core.sym.ok, "active", Style::default().fg(t.success())),
                (_, "failed") => (core.sym.err, "failed", Style::default().fg(t.error())),
                (_, "activating" | "reloading") => (core.sym.warn, state, Style::default().fg(t.warning())),
                _ => (core.sym.info, if state.is_empty() { "inactive" } else { state }, t.dim()),
            };
            let mem = num(v, "memory_mb").map_or_else(|| "—".to_string(), |m| format!("{m:.1} MB"));
            Line::from(vec![
                Span::styled(format!("{:<24} ", truncate(s(v, "name"), 24)), t.text()),
                Span::styled(format!("{glyph} {:<10} ", truncate(word, 10)), style),
                Span::styled(format!("{:<11} ", truncate(s(v, "sub_state"), 11)), t.dim()),
                Span::styled(format!("{mem:>9}"), t.text()),
            ])
        })
        .collect();
    let i = Pane::Services as usize;
    let sel = (tab.pane == Pane::Services).then_some((tab.sel[i], tab.offsets[i]));
    list(f, rest, core, rows, sel, "svc", out)
}

fn peripherals_table(f: &mut Frame, area: Rect, core: &Core, tab: &SystemTab, out: &mut FrameOut) -> usize {
    let t = &core.theme;
    let block = panel(t, core.sym, "Peripherals");
    let inner = block.inner(area);
    f.render_widget(block, area);
    let items = peripherals(core);
    if items.is_empty() {
        let text = if core.value(&ResourceId::Peripherals).is_some() {
            "No peripherals registered".to_string()
        } else {
            loading(core, &ResourceId::Peripherals)
        };
        f.render_widget(Paragraph::new(Span::styled(text, t.dim())), inner);
        return 0;
    }
    let name_w = (inner.width as usize).saturating_sub(30).clamp(8, 30);
    let rows: Vec<Line<'static>> = items
        .iter()
        .map(|p| {
            let connected = p.get("connected").and_then(Value::as_bool).unwrap_or_else(|| {
                matches!(s(p, "state"), "connected" | "ready" | "active")
            });
            let state = [s(p, "state"), s(p, "status")].into_iter().find(|x| !x.is_empty());
            let (glyph, word, style) = if connected {
                (core.sym.ok, "connected", Style::default().fg(t.success()))
            } else {
                match state.unwrap_or("unknown") {
                    w @ ("error" | "failed") => (core.sym.err, w, Style::default().fg(t.error())),
                    w @ ("missing" | "disconnected") => (core.sym.warn, w, Style::default().fg(t.warning())),
                    w => (core.sym.info, w, t.dim()),
                }
            };
            Line::from(vec![
                Span::styled(format!("{:<name_w$} ", truncate(&peripheral_name(p), name_w)), t.text()),
                Span::styled(format!("{glyph} {:<13} ", truncate(word, 13)), style),
                Span::styled(truncate(s(p, "transport"), 10), t.dim()),
            ])
        })
        .collect();
    let i = Pane::Peripherals as usize;
    let sel = (tab.pane == Pane::Peripherals).then_some((tab.sel[i], tab.offsets[i]));
    list(f, inner, core, rows, sel, "per", out)
}

fn hardware(f: &mut Frame, area: Rect, core: &Core, tab: &SystemTab, out: &mut FrameOut) -> usize {
    let t = &core.theme;
    let mut title = "Hardware check".to_string();
    if tab.rechecking {
        title.push_str(&format!(" {} re-checking…", core.sym.spinner_frame(core.frame as usize)));
    } else if let Some(last) = core.value(&ResourceId::HardwareCheck).map(|v| s(v, "last_run")).filter(|l| !l.is_empty()) {
        title.push_str(&format!(" · last run {last}"));
    }
    let block = panel(t, core.sym, &title);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let items = hw_items(core);
    if items.is_empty() {
        let text = if core.value(&ResourceId::HardwareCheck).is_some() {
            "No hardware items reported · c re-checks".to_string()
        } else {
            loading(core, &ResourceId::HardwareCheck)
        };
        f.render_widget(Paragraph::new(Span::styled(text, t.dim())), inner);
        return 0;
    }
    let detail_w = (inner.width as usize).saturating_sub(40);
    let rows: Vec<Line<'static>> = items
        .iter()
        .map(|h| {
            let state = s(h, "state");
            let (glyph, style) = match state {
                "ok" => (core.sym.ok, Style::default().fg(t.success())),
                "warning" => (core.sym.warn, Style::default().fg(t.warning())),
                "missing" => (core.sym.err, Style::default().fg(t.error())),
                "checking" => (core.sym.spinner_frame(core.frame as usize), t.dim()),
                _ => (core.sym.info, t.dim()),
            };
            let required = if h.get("required").and_then(Value::as_bool) == Some(true) { "" } else { " (optional)" };
            let detail = [s(h, "detail"), s(h, "fix_hint")]
                .into_iter()
                .filter(|x| !x.is_empty())
                .collect::<Vec<_>>()
                .join(" · ");
            Line::from(vec![
                Span::styled(format!("{glyph} {:<8} ", truncate(state, 8)), style),
                Span::styled(format!("{:<26} ", truncate(&format!("{}{required}", s(h, "label")), 26)), t.text()),
                Span::styled(truncate(&detail, detail_w), t.dim()),
            ])
        })
        .collect();
    let i = Pane::Hardware as usize;
    let sel = (tab.pane == Pane::Hardware).then_some((tab.sel[i], tab.offsets[i]));
    list(f, inner, core, rows, sel, "hw", out)
}
