//! Rendering for the Ground tab.

use std::time::{SystemTime, UNIX_EPOCH};

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use serde_json::Value;

use super::{arr, text, GroundTab, Pane};
use crate::app::{Core, FrameOut, Hit};
use crate::poll::ResourceId;
use crate::ui::widgets::{chip, panel, rows, scroll_offset, truncate};

fn num(v: &Value, key: &str) -> Option<f64> {
    v.get(key).and_then(Value::as_f64)
}

fn opt(v: Option<f64>, fmt: impl Fn(f64) -> String) -> String {
    v.map(fmt).unwrap_or_else(|| "—".into())
}

/// The slot's failure text, when its last read failed and nothing good was
/// ever read.
fn failed(core: &Core, id: &ResourceId) -> Option<String> {
    let slot = core.slot(id)?;
    slot.value.is_none().then(|| slot.error.clone()).flatten()
}

pub(super) fn render(tab: &GroundTab, f: &mut Frame, area: Rect, core: &Core, out: &mut FrameOut) {
    let receivers = arr(core, &ResourceId::Gnss, "receivers").len() as u16;
    let gnss_h = (receivers.max(1) + 5).min(area.height / 2);
    let parts = Layout::vertical([
        Constraint::Length(gnss_h),
        Constraint::Min(6),
        Constraint::Length(3),
    ])
    .split(area);
    gnss(f, parts[0], core);
    let cols = Layout::horizontal([
        Constraint::Percentage(30),
        Constraint::Percentage(40),
        Constraint::Percentage(30),
    ])
    .split(parts[1]);
    gamepads(tab, f, cols[0], core, out);
    bluetooth(tab, f, cols[1], core, out);
    invites(tab, f, cols[2], core, out);
    display(f, parts[2], core);
}

fn gnss(f: &mut Frame, area: Rect, core: &Core) {
    let t = &core.theme;
    let block = panel(t, core.sym, "GNSS");
    let inner = block.inner(area);
    f.render_widget(block, area);
    let dim = |s: String| Line::from(Span::styled(s, t.dim()));
    if let Some(err) = failed(core, &ResourceId::Gnss) {
        f.render_widget(Paragraph::new(dim(err)), inner);
        return;
    }
    let Some(g) = core.value(&ResourceId::Gnss) else {
        f.render_widget(Paragraph::new(dim("Loading…".into())), inner);
        return;
    };
    if g.get("present").and_then(Value::as_bool) != Some(true) {
        f.render_widget(
            Paragraph::new(dim("No GNSS receiver reporting".into())),
            inner,
        );
        return;
    }
    let mut lines = vec![Line::from(Span::styled(
        format!(
            "{:<8} {:<10} {:<14} {:<10} {:>4} {:>5} {:<6} {:<4} Position",
            "ID", "Source", "Model", "Fix", "Sats", "HDOP", "RTK", "PPS"
        ),
        t.dim(),
    ))];
    for r in arr(core, &ResourceId::Gnss, "receivers") {
        let fix = text(r, "fix");
        let pos = match (num(r, "lat_deg"), num(r, "lon_deg")) {
            (Some(la), Some(lo)) => format!("{la:.6}, {lo:.6}"),
            _ => "—".into(),
        };
        let fix_color = match fix {
            "3d" | "dgps" | "rtk_float" | "rtk_fixed" => t.success(),
            "2d" => t.warning(),
            _ => t.error(),
        };
        let pps = match r.get("pps_locked").and_then(Value::as_bool) {
            Some(true) => "lock",
            _ => "—",
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!(
                    "{:<8} {:<10} {:<14} ",
                    truncate(text(r, "id"), 8),
                    truncate(text(r, "source"), 10),
                    truncate(r.get("model").and_then(Value::as_str).unwrap_or("—"), 14)
                ),
                t.text(),
            ),
            Span::styled(
                format!("{:<10} ", fix.replace('_', " ")),
                Style::default().fg(fix_color),
            ),
            Span::styled(
                format!(
                    "{:>4} {:>5} {:<6} {:<4} {pos}",
                    r.get("sats")
                        .and_then(Value::as_u64)
                        .map(|n| n.to_string())
                        .unwrap_or_else(|| "—".into()),
                    opt(num(r, "hdop"), |h| format!("{h:.1}")),
                    text(r, "rtk"),
                    pps
                ),
                t.text(),
            ),
        ]));
    }
    if let Some(b) = g.get("base").filter(|b| b.is_object()) {
        let state = text(b, "state");
        let color = match state {
            "fixed" => t.success(),
            "surveying" => t.info(),
            "failed" => t.error(),
            _ => t.muted(),
        };
        let mut spans = vec![
            Span::styled("Base     ", t.dim()),
            Span::styled(
                format!("{} · {state}", text(b, "mode")),
                Style::default().fg(color),
            ),
        ];
        let mut push = |s: String| spans.push(Span::styled(format!(" · {s}"), t.text()));
        if state == "surveying" {
            push(format!(
                "{}/{} s",
                b.get("elapsed_s").and_then(Value::as_u64).unwrap_or(0),
                b.get("min_dur_s").and_then(Value::as_u64).unwrap_or(0)
            ));
        }
        push(format!(
            "accuracy {} (target {})",
            opt(num(b, "accuracy_m"), |a| format!("{a:.2} m")),
            opt(num(b, "target_accuracy_m"), |a| format!("{a:.2} m"))
        ));
        if let Some(fx) = b.get("fixed").filter(|x| x.is_object()) {
            push(format!(
                "at {}, {}",
                opt(num(fx, "lat_deg"), |v| format!("{v:.6}")),
                opt(num(fx, "lon_deg"), |v| format!("{v:.6}"))
            ));
        }
        if let Some(rtcm) = b.get("rtcm_out") {
            push(format!(
                "RTCM {} msg/s → {} targets",
                opt(num(rtcm, "msgs_per_s"), |v| format!("{v:.1}")),
                rtcm.get("targets").and_then(Value::as_u64).unwrap_or(0)
            ));
        }
        lines.push(Line::from(spans));
    }
    if let Some(h) = g
        .get("heading")
        .filter(|h| h.get("available").and_then(Value::as_bool) == Some(true))
    {
        lines.push(Line::from(vec![
            Span::styled("Heading  ", t.dim()),
            Span::styled(
                format!(
                    "{} ± {} · baseline {}",
                    opt(num(h, "deg"), |v| format!("{v:.1}°")),
                    opt(num(h, "accuracy_deg"), |v| format!("{v:.1}°")),
                    opt(num(h, "baseline_m"), |v| format!("{v:.2} m"))
                ),
                t.text(),
            ),
        ]));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

/// A list panel: title (accented when focused), then rows with the `›`
/// marker on the selection. Rows are recorded as mouse hits.
#[allow(clippy::too_many_arguments)]
fn list(
    f: &mut Frame,
    area: Rect,
    core: &Core,
    out: &mut FrameOut,
    title: String,
    hit: &'static str,
    focused: bool,
    selected: usize,
    items: Vec<Line<'static>>,
    empty: String,
) {
    let t = &core.theme;
    let mut block = panel(t, core.sym, &title);
    if focused {
        block = block.border_style(Style::default().fg(t.accent()));
    }
    let inner = block.inner(area);
    f.render_widget(block, area);
    if items.is_empty() {
        f.render_widget(Paragraph::new(Span::styled(empty, t.dim())), inner);
        return;
    }
    let offset = scroll_offset(selected, 0, inner.height as usize);
    for ((i, line), rect) in items.into_iter().enumerate().skip(offset).zip(rows(inner)) {
        let sel = i == selected;
        let marker = if sel && focused {
            core.sym.row_marker
        } else {
            " "
        };
        let mut spans = vec![Span::styled(format!("{marker} "), t.accented())];
        spans.extend(line.spans);
        let style = if sel && focused {
            t.accented()
        } else {
            Style::default()
        };
        f.render_widget(Paragraph::new(Line::from(spans)).style(style), rect);
        out.hit(rect, Hit::Item(hit, i));
    }
}

fn gamepads(tab: &GroundTab, f: &mut Frame, area: Rect, core: &Core, out: &mut FrameOut) {
    let t = &core.theme;
    let primary = core
        .value(&ResourceId::Gamepads)
        .and_then(|v| v.get("primary_id"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let items = arr(core, &ResourceId::Gamepads, "devices")
        .iter()
        .map(|d| {
            let id = text(d, "device_id");
            let mut spans = vec![Span::styled(text(d, "name").to_string(), t.text())];
            if id == primary {
                spans.push(Span::raw(" "));
                spans.push(chip(t, "PRIMARY", t.accent()));
            }
            Line::from(spans)
        })
        .collect();
    let empty =
        failed(core, &ResourceId::Gamepads).unwrap_or_else(|| "No gamepads attached".into());
    let sel = tab.selected(core, Pane::Gamepads).unwrap_or(0);
    list(
        f,
        area,
        core,
        out,
        "Gamepads".into(),
        "gamepad",
        tab.pane == Pane::Gamepads,
        sel,
        items,
        empty,
    );
}

fn bluetooth(tab: &GroundTab, f: &mut Frame, area: Rect, core: &Core, out: &mut FrameOut) {
    let t = &core.theme;
    let spin = core.sym.spinner_frame(core.frame as usize);
    let title = if tab.scanning {
        format!("Bluetooth {spin} scanning…")
    } else if let Some(name) = &tab.pairing {
        format!("Bluetooth {spin} pairing {name}…")
    } else {
        "Bluetooth".into()
    };
    let items = tab
        .bt_rows(core)
        .into_iter()
        .map(|r| {
            let (word, color) = match (r.paired, r.connected) {
                (true, Some(true)) => (format!("{} connected", core.sym.ok), t.success()),
                (true, Some(false)) => ("paired".to_string(), t.muted()),
                (true, None) => ("paired".to_string(), t.muted()),
                (false, _) => ("found · Enter pairs".to_string(), t.info()),
            };
            Line::from(vec![
                Span::styled(format!("{} ", truncate(&r.name, 18)), t.text()),
                Span::styled(format!("{} ", r.mac), t.dim()),
                Span::styled(word, Style::default().fg(color)),
            ])
        })
        .collect();
    let empty = failed(core, &ResourceId::BtPaired)
        .unwrap_or_else(|| "No paired devices · s to scan".into());
    let sel = tab.selected(core, Pane::Bluetooth).unwrap_or(0);
    list(
        f,
        area,
        core,
        out,
        title,
        "bt",
        tab.pane == Pane::Bluetooth,
        sel,
        items,
        empty,
    );
}

fn invites(tab: &GroundTab, f: &mut Frame, area: Rect, core: &Core, out: &mut FrameOut) {
    let t = &core.theme;
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let items = arr(core, &ResourceId::Invites, "pending")
        .iter()
        .map(|inv| {
            let left = inv
                .get("expires_at_ms")
                .and_then(Value::as_i64)
                .map(|ms| format!(" {} s left", ((ms - now_ms) / 1000).max(0)))
                .unwrap_or_default();
            Line::from(vec![
                Span::styled(format!("{} ", truncate(text(inv, "label"), 14)), t.text()),
                Span::styled(text(inv, "phone_fingerprint").to_string(), t.accented()),
                Span::styled(left, t.dim()),
            ])
        })
        .collect();
    let empty =
        failed(core, &ResourceId::Invites).unwrap_or_else(|| "No pending phone invites".into());
    let sel = tab.selected(core, Pane::Invites).unwrap_or(0);
    list(
        f,
        area,
        core,
        out,
        "Phone receivers".into(),
        "invite",
        tab.pane == Pane::Invites,
        sel,
        items,
        empty,
    );
}

fn display(f: &mut Frame, area: Rect, core: &Core) {
    let t = &core.theme;
    let block = panel(t, core.sym, "Display");
    let inner = block.inner(area);
    f.render_widget(block, area);
    let line = match (
        core.value(&ResourceId::GsDisplay),
        failed(core, &ResourceId::GsDisplay),
    ) {
        (Some(d), _) => {
            let kiosk = match d.get("kiosk_enabled").and_then(Value::as_bool) {
                Some(true) => "on",
                Some(false) => "off",
                None => "—",
            };
            let url = d
                .get("kiosk_target_url")
                .and_then(Value::as_str)
                .unwrap_or("default");
            Line::from(vec![
                Span::styled("Resolution ", t.dim()),
                Span::styled(
                    d.get("resolution")
                        .and_then(Value::as_str)
                        .unwrap_or("—")
                        .to_string(),
                    t.text(),
                ),
                Span::styled("  Kiosk ", t.dim()),
                Span::styled(kiosk, t.text()),
                Span::styled("  Target ", t.dim()),
                Span::styled(url.to_string(), t.text()),
            ])
        }
        (None, Some(err)) => Line::from(Span::styled(err, t.dim())),
        (None, None) => Line::from(Span::styled("Loading…", t.dim())),
    };
    f.render_widget(Paragraph::new(line), inner);
}
