//! Link tab tables: the LoRa lane and beacons, and the ground station's
//! adapters, receiver and mesh. Cumulative counters show as `Δ` since the
//! previous poll.

use std::time::{SystemTime, UNIX_EPOCH};

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use serde_json::Value;

use super::or_dash;
use crate::app::Core;
use crate::model::link::{tq_bar, Counters};
use crate::poll::ResourceId;
use crate::ui::widgets::{panel, truncate};

fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("—")
}

fn n(v: &Value, key: &str) -> Option<f64> {
    v.get(key).and_then(Value::as_f64)
}

fn items<'a>(v: Option<&'a Value>, key: &str) -> &'a [Value] {
    v.and_then(|v| v.get(key))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// `12 s ago` for an epoch-ms timestamp.
fn ago(ms: Option<i64>) -> String {
    or_dash(ms, |ms| {
        format!("{} s ago", ((now_ms() - ms) / 1000).max(0))
    })
}

fn delta(c: &Counters, name: &str) -> String {
    or_dash(c.get(name), |d| format!("+{d}"))
}

/// The agent's error code for a failed read, e.g. `E_NOT_IN_MESH`.
fn error_code(core: &Core, id: &ResourceId) -> Option<String> {
    let body = core.slot(id)?.error_body.as_ref()?;
    body.pointer("/detail/error/code")
        .or_else(|| body.get("error"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// A panel body: `lines` or a dim note.
fn body(
    f: &mut Frame,
    area: Rect,
    core: &Core,
    title: &str,
    lines: Result<Vec<Line<'static>>, String>,
) {
    let t = &core.theme;
    let block = panel(t, core.sym, title);
    let inner = block.inner(area);
    f.render_widget(block, area);
    match lines {
        Ok(lines) => f.render_widget(Paragraph::new(lines), inner),
        Err(note) => f.render_widget(Paragraph::new(Span::styled(note, t.dim())), inner),
    }
}

/// The current value of a resource, or why there is none.
fn current<'a>(core: &'a Core, id: &ResourceId) -> Result<&'a Value, String> {
    match core.slot(id) {
        Some(slot) if slot.error.is_some() => Err(slot.error.clone().unwrap_or_default()),
        Some(slot) => slot.value.as_ref().ok_or_else(|| "Loading…".to_string()),
        None => Err("Loading…".into()),
    }
}

pub(super) fn ground(f: &mut Frame, area: Rect, core: &Core, c: &Counters) {
    let rows = Layout::vertical([Constraint::Percentage(55), Constraint::Min(4)]).split(area);
    let cols = Layout::horizontal([Constraint::Ratio(1, 2); 2]).split(rows[0]);
    adapters(f, cols[0], core, c);
    receiver(f, cols[1], core, c);
    mesh(f, rows[1], core);
}

fn adapters(f: &mut Frame, area: Rect, core: &Core, c: &Counters) {
    let t = &core.theme;
    let lines = current(core, &ResourceId::Wfb).map(|w| {
        let list = items(Some(w), "adapters");
        let mut out = vec![Line::from(Span::styled(
            format!(
                "{:<7} {:<5} {:>9} {:>7} {:>6} {:>6}",
                "iface", "role", "rssi", "pkts Δ", "lost Δ", "fec Δ"
            ),
            t.dim(),
        ))];
        for a in list {
            let iface = s(a, "iface");
            let rssi = a
                .get("rssi_dbm")
                .and_then(Value::as_array)
                .map(|ch| {
                    ch.iter()
                        .filter_map(Value::as_i64)
                        .map(|v| v.to_string())
                        .collect::<Vec<_>>()
                        .join("/")
                })
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "—".into());
            out.push(Line::from(Span::styled(
                format!(
                    "{:<7} {:<5} {:>9} {:>7} {:>6} {:>6}",
                    truncate(iface, 7),
                    truncate(s(a, "role"), 5),
                    truncate(&rssi, 9),
                    delta(c, &format!("adapter:{iface}:packets")),
                    delta(c, &format!("adapter:{iface}:lost")),
                    delta(c, &format!("adapter:{iface}:fec_rec")),
                ),
                t.text(),
            )));
        }
        if list.is_empty() {
            out.push(Line::from(Span::styled(
                "no receive adapters reported",
                t.dim(),
            )));
        }
        if let Some(d) = w.get("diversity").filter(|d| d.is_object()) {
            out.push(Line::from(Span::styled(
                format!(
                    "diversity {} rx · combined loss {} · best single {}",
                    d.get("rx_adapters").and_then(Value::as_u64).unwrap_or(0),
                    or_dash(n(d, "combined_loss_pct"), |v| format!("{v:.1} %")),
                    or_dash(n(d, "best_single_loss_pct"), |v| format!("{v:.1} %"))
                ),
                t.dim(),
            )));
        }
        out
    });
    body(f, area, core, "Adapters", lines);
}

fn receiver(f: &mut Frame, area: Rect, core: &Core, c: &Counters) {
    let t = &core.theme;
    let lines = match current(core, &ResourceId::ReceiverCombined) {
        Err(_)
            if error_code(core, &ResourceId::ReceiverCombined).as_deref()
                == Some("E_WRONG_ROLE") =>
        {
            Err("Receiver tables show on the receiver role".into())
        }
        Err(e) => Err(e),
        Ok(v) if v.get("stale").and_then(Value::as_bool) == Some(true) => {
            Err("No current receiver state".into())
        }
        Ok(v) => {
            let up = match v.get("up").and_then(Value::as_bool) {
                Some(true) => Span::styled(
                    format!("{} up", core.sym.ok),
                    Style::default().fg(t.success()),
                ),
                _ => Span::styled(
                    format!("{} down", core.sym.err),
                    Style::default().fg(t.error()),
                ),
            };
            let mut out = vec![
                Line::from(vec![
                    Span::styled("Combined ", t.dim()),
                    up,
                    Span::styled(
                        format!(
                            " · {}",
                            or_dash(n(v, "output_kbps"), |k| format!("{k:.0} kbit/s"))
                        ),
                        t.text(),
                    ),
                ]),
                Line::from(Span::styled(
                    format!(
                        "fragments {} · FEC repaired {}",
                        delta(c, "combined:fragments_after_dedup"),
                        delta(c, "combined:fec_repaired"),
                    ),
                    t.text(),
                )),
                Line::from(Span::styled(
                    format!("{:<18} {:>8} {:>10}", "relay", "frags Δ", "seen"),
                    t.dim(),
                )),
            ];
            let relays = core.value(&ResourceId::ReceiverRelays);
            for r in items(relays, "relays") {
                let mac = s(r, "mac");
                out.push(Line::from(Span::styled(
                    format!(
                        "{:<18} {:>8} {:>10}",
                        mac,
                        delta(c, &format!("relay:{mac}")),
                        ago(r.get("last_seen_ms").and_then(Value::as_i64))
                    ),
                    t.text(),
                )));
            }
            if items(relays, "relays").is_empty() {
                out.push(Line::from(Span::styled("no relays heard", t.dim())));
            }
            Ok(out)
        }
    };
    body(f, area, core, "Receiver", lines);
}

fn mesh(f: &mut Frame, area: Rect, core: &Core) {
    let t = &core.theme;
    let lines = match current(core, &ResourceId::Mesh) {
        Err(_) if error_code(core, &ResourceId::Mesh).as_deref() == Some("E_NOT_IN_MESH") => {
            Err("Not in a mesh (direct role)".into())
        }
        Err(e) => Err(e),
        Ok(v) if v.as_object().is_some_and(|o| o.is_empty()) => Err("No current mesh state".into()),
        Ok(v) => {
            let up = v.get("up").and_then(Value::as_bool) == Some(true);
            let mut out = vec![Line::from(vec![
                Span::styled(format!("{} ", s(v, "role")), t.bright()),
                Span::styled(format!("{} ", s(v, "mesh_id")), t.dim()),
                if up {
                    Span::styled(
                        format!("{} up", core.sym.ok),
                        Style::default().fg(t.success()),
                    )
                } else {
                    Span::styled(
                        format!("{} down", core.sym.err),
                        Style::default().fg(t.error()),
                    )
                },
            ])];
            if v.get("partition").and_then(Value::as_bool) == Some(true) {
                out.push(Line::from(Span::styled(
                    format!("{} mesh partitioned", core.sym.warn),
                    Style::default().fg(t.warning()),
                )));
            }
            let (full, empty) = if core.sym.name == "ascii" {
                ("#", ".")
            } else {
                ("█", "░")
            };
            for nb in items(Some(v), "neighbors") {
                let tq = nb.get("tq").and_then(Value::as_i64).unwrap_or(0);
                out.push(Line::from(vec![
                    Span::styled(format!("{} ", s(nb, "mac")), t.text()),
                    Span::styled(tq_bar(tq, full, empty), t.accented()),
                    Span::styled(format!(" {tq:>3}"), t.bright()),
                ]));
            }
            if items(Some(v), "neighbors").is_empty() {
                out.push(Line::from(Span::styled("no neighbours", t.dim())));
            }
            Ok(out)
        }
    };
    body(f, area, core, "Mesh neighbours (TQ)", lines);
}

pub(super) fn lora(f: &mut Frame, area: Rect, core: &Core, c: &Counters) {
    let cols = Layout::horizontal([Constraint::Length(50), Constraint::Min(30)]).split(area);
    let t = &core.theme;
    let lines = current(core, &ResourceId::Lora).map(|v| {
        let mut out = Vec::new();
        let region = v.get("region").filter(|r| r.is_object());
        out.push(Line::from(vec![
            Span::styled("Region   ", t.dim()),
            Span::styled(
                region
                    .map(|r| {
                        let band = r
                            .get("band_mhz")
                            .and_then(Value::as_array)
                            .map(|b| {
                                b.iter()
                                    .filter_map(Value::as_f64)
                                    .map(|x| format!("{x}"))
                                    .collect::<Vec<_>>()
                                    .join("–")
                            })
                            .unwrap_or_default();
                        format!("{} {} {band} MHz", s(r, "code"), s(r, "name"))
                    })
                    .unwrap_or_else(|| "not set".into()),
                t.bright(),
            ),
        ]));
        if let Some(ch) = v.get("channel").filter(|c| c.is_object()) {
            out.push(Line::from(vec![
                Span::styled("Channel  ", t.dim()),
                Span::styled(
                    format!(
                        "{} · {} kHz · SF{} · {}/{} mW",
                        or_dash(n(ch, "freq_mhz"), |f| format!("{f:.4} MHz")),
                        or_dash(n(ch, "bw_khz"), |b| format!("{b:.0}")),
                        or_dash(n(ch, "sf"), |s| format!("{s:.0}")),
                        or_dash(n(ch, "erp_mw"), |e| format!("{e:.0}")),
                        or_dash(n(ch, "max_erp_mw"), |e| format!("{e:.0}"))
                    ),
                    t.text(),
                ),
            ]));
        }
        let allowed = v.get("tx_allowed").and_then(Value::as_bool) == Some(true);
        out.push(Line::from(vec![
            Span::styled("TX       ", t.dim()),
            if allowed {
                Span::styled(
                    format!("{} allowed", core.sym.ok),
                    Style::default().fg(t.success()),
                )
            } else {
                Span::styled(
                    format!(
                        "{} blocked: {}",
                        core.sym.err,
                        s(v, "blocked_reason").replace('_', " ")
                    ),
                    Style::default().fg(t.error()),
                )
            },
        ]));
        let rules = v
            .pointer("/governor/rules")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        for r in rules.iter().filter(|r| s(r, "kind") == "duty_cycle") {
            out.push(Line::from(vec![
                Span::styled("Duty use ", t.dim()),
                Span::styled(
                    format!(
                        "{} of {} ({})",
                        or_dash(n(r, "used_pct"), |u| format!("{u:.2} %")),
                        or_dash(n(r, "limit_pct"), |l| format!("{l} %")),
                        s(r, "sub_band")
                    ),
                    t.text(),
                ),
            ]));
        }
        out.push(Line::from(vec![
            Span::styled("Frames   ", t.dim()),
            Span::styled(
                format!(
                    "tx {} · refused {} · rx {} · crc err {}",
                    delta(c, "lora:tx"),
                    delta(c, "lora:refused"),
                    delta(c, "lora:rx"),
                    delta(c, "lora:crc")
                ),
                t.text(),
            ),
        ]));
        out
    });
    body(f, cols[0], core, "LoRa", lines);

    let beacons = current(core, &ResourceId::LoraBeacons).map(|v| {
        let list = items(Some(v), "beacons");
        let mut out = vec![Line::from(Span::styled(
            format!(
                "{:>6} {:<10} {:>5} {:>9} {:>7} {:>10}  Position",
                "node", "mode", "batt", "rssi", "snr", "heard"
            ),
            t.dim(),
        ))];
        for b in list {
            let pos = match (n(b, "lat_deg"), n(b, "lon_deg")) {
                (Some(la), Some(lo)) => format!("{la:.5}, {lo:.5}"),
                _ => "—".into(),
            };
            out.push(Line::from(Span::styled(
                format!(
                    "{:>6} {:<10} {:>5} {:>9} {:>7} {:>10}  {pos}",
                    or_dash(n(b, "node_id"), |x| format!("{x:.0}")),
                    truncate(b.get("mode").and_then(Value::as_str).unwrap_or("—"), 10),
                    or_dash(n(b, "battery_pct"), |x| format!("{x:.0}%")),
                    or_dash(n(b, "rssi_dbm"), |x| format!("{x:.0} dBm")),
                    or_dash(n(b, "snr_db"), |x| format!("{x:.1}")),
                    ago(b.get("received_at_ms").and_then(Value::as_i64)),
                ),
                t.text(),
            )));
        }
        if list.is_empty() {
            out.push(Line::from(Span::styled("no beacons heard", t.dim())));
        }
        out
    });
    body(f, cols[1], core, "LoRa beacons", beacons);
}
