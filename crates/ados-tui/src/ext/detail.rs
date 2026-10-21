//! The detail document of one extension: identity, permissions, MCP tools,
//! service readiness and the latest published state per topic, as plain lines
//! the Extensions tab scrolls. Also the catalog entry card.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;

use super::state::{self, CatalogEntry, Install, StatusClass};
use crate::app::{Core, Slot};
use crate::poll::ResourceId;
use crate::theme::Theme;

/// The colour and glyph of an install status.
pub fn status_style(core: &Core, status: &str) -> (Color, &'static str) {
    let s = core.sym;
    match state::status_class(status) {
        StatusClass::Live => (core.theme.success(), s.ok),
        StatusClass::Off => (core.theme.muted(), s.info),
        StatusClass::Failed => (core.theme.error(), s.err),
        StatusClass::Pending => (core.theme.warning(), s.warn),
    }
}

/// The colour of a risk word.
pub fn risk_color(t: &Theme, risk: &str) -> Color {
    match risk {
        "critical" | "high" => t.error(),
        "medium" => t.warning(),
        "low" => t.success(),
        _ => t.muted(),
    }
}

fn heading(t: &Theme, text: &str) -> Line<'static> {
    Line::from(Span::styled(
        text.to_string(),
        t.text().add_modifier(Modifier::UNDERLINED | Modifier::BOLD),
    ))
}

fn kv(t: &Theme, label: &str, value: Span<'static>) -> Line<'static> {
    Line::from(vec![Span::styled(format!("  {label:<12}"), t.dim()), value])
}

fn s(v: &Value, path: &str) -> String {
    crate::app::at(v, path)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Why a resource has no body yet: the failure text, or "Reading…".
fn missing(t: &Theme, slot: Option<&Slot>) -> Line<'static> {
    match slot.and_then(|s| s.error.clone()) {
        Some(e) => Line::from(Span::styled(
            format!("  {e}"),
            Style::default().fg(t.warning()),
        )),
        None => Line::from(Span::styled("  Reading…", t.dim())),
    }
}

/// The installed plugin's detail lines.
pub fn installed(core: &Core, install: &Install) -> Vec<Line<'static>> {
    let t = &core.theme;
    let id = &install.id;
    let detail = core.value(&ResourceId::PluginDetail(id.clone()));
    let mut out = Vec::new();
    let name = detail
        .map(|d| s(d, "manifest.name"))
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| id.clone());
    out.push(Line::from(vec![
        Span::styled(name, t.bright()),
        Span::styled(format!("  {id}"), t.dim()),
    ]));
    let (color, glyph) = status_style(core, &install.status);
    out.push(kv(
        t,
        "Status",
        Span::styled(
            format!("{glyph} {}", install.status),
            Style::default().fg(color),
        ),
    ));
    out.push(kv(
        t,
        "Version",
        Span::styled(install.version.clone(), t.text()),
    ));
    let signer = install.signer.clone().unwrap_or_else(|| "unsigned".into());
    out.push(kv(t, "Signer", Span::styled(signer, t.text())));
    if let Some(d) = detail {
        let risk = s(d, "manifest.risk");
        if !risk.is_empty() {
            out.push(kv(
                t,
                "Risk",
                Span::styled(risk.clone(), Style::default().fg(risk_color(t, &risk))),
            ));
        }
    }
    if let Some(reason) = &install.failure_reason {
        out.push(kv(
            t,
            "Failure",
            Span::styled(
                format!("{} {reason}", core.sym.err),
                Style::default().fg(t.error()),
            ),
        ));
    }
    if install.revocation_pending {
        out.push(kv(
            t,
            "Revocation",
            Span::styled(
                format!("{} pending restart", core.sym.warn),
                Style::default().fg(t.warning()),
            ),
        ));
    }

    out.push(Line::default());
    out.push(heading(t, "Permissions"));
    match detail {
        Some(d) => {
            let perms = state::perms(d.get("manifest").unwrap_or(&Value::Null));
            if perms.is_empty() {
                out.push(Line::from(Span::styled("  none declared", t.dim())));
            }
            for p in perms {
                let granted = install.granted(&p.id);
                let (gc, gw) = if granted {
                    (t.success(), format!("{} granted", core.sym.ok))
                } else {
                    (t.warning(), format!("{} not granted", core.sym.warn))
                };
                out.push(Line::from(vec![
                    Span::styled(format!("  {:<30}", p.id), t.text()),
                    Span::styled(
                        format!("{:<10}", if p.required { "required" } else { "optional" }),
                        t.dim(),
                    ),
                    Span::styled(gw, Style::default().fg(gc)),
                ]));
            }
        }
        None => out.push(missing(t, core.slot(&ResourceId::PluginDetail(id.clone())))),
    }

    if let Some(tools) = detail
        .and_then(|d| crate::app::at(d, "manifest.mcp.tools"))
        .and_then(Value::as_array)
        .filter(|l| !l.is_empty())
    {
        out.push(Line::default());
        out.push(heading(t, "MCP tools"));
        for tool in tools {
            let safety = tool
                .get("safety_class")
                .or_else(|| tool.get("safety"))
                .and_then(Value::as_str)
                .unwrap_or("—");
            out.push(Line::from(vec![
                Span::styled(format!("  {:<30}", s(tool, "name")), t.text()),
                Span::styled(safety.to_string(), t.dim()),
            ]));
        }
    }

    out.push(Line::default());
    out.push(heading(t, "Services"));
    let ready_id = ResourceId::PluginReadiness(id.clone());
    match core
        .value(&ready_id)
        .and_then(|v| v.get("services"))
        .and_then(Value::as_array)
    {
        Some(list) if list.is_empty() => {
            out.push(Line::from(Span::styled("  none declared", t.dim())))
        }
        Some(list) => {
            for svc in list {
                let ready = svc.get("ready").and_then(Value::as_bool).unwrap_or(false);
                let (c, g, w) = if ready {
                    (t.success(), core.sym.ok, "ready")
                } else {
                    (t.error(), core.sym.err, "not ready")
                };
                let mut spans = vec![
                    Span::styled(format!("  {g} "), Style::default().fg(c)),
                    Span::styled(format!("{:<24}", s(svc, "name")), t.text()),
                    Span::styled(w.to_string(), Style::default().fg(c)),
                ];
                let reason = s(svc, "reason");
                if !reason.is_empty() {
                    spans.push(Span::styled(format!("  {reason}"), t.dim()));
                }
                out.push(Line::from(spans));
            }
        }
        None => out.push(missing(t, core.slot(&ready_id))),
    }

    out.push(Line::default());
    out.push(heading(t, "State"));
    let state_id = ResourceId::PluginState(id.clone());
    match (
        core.value(&state_id).and_then(Value::as_object),
        core.slot(&state_id),
    ) {
        (Some(topics), Some(slot)) if slot.error.is_none() => {
            if topics.is_empty() {
                out.push(Line::from(Span::styled("  nothing published", t.dim())));
            }
            for (topic, entry) in topics {
                let ts = entry
                    .get("ts_ms")
                    .and_then(Value::as_i64)
                    .map(|v| format!("  ts {v}"))
                    .unwrap_or_default();
                out.push(Line::from(vec![
                    Span::styled(format!("  {topic}"), t.accented()),
                    Span::styled(ts, t.dim()),
                ]));
                let payload = entry.get("payload").unwrap_or(entry);
                let pretty = serde_json::to_string_pretty(payload).unwrap_or_default();
                for l in pretty.lines() {
                    out.push(Line::from(Span::styled(format!("    {l}"), t.text())));
                }
            }
        }
        (_, Some(slot)) if slot.code == Some(404) => out.push(Line::from(Span::styled(
            "  No published state (not running, or nothing published yet)",
            t.dim(),
        ))),
        (_, slot) => out.push(missing(t, slot)),
    }
    out
}

/// The catalog entry card.
pub fn catalog(core: &Core, entry: &CatalogEntry) -> Vec<Line<'static>> {
    let t = &core.theme;
    let mut out = vec![
        Line::from(vec![
            Span::styled(entry.name.clone(), t.bright()),
            Span::styled(format!("  {}", entry.id), t.dim()),
        ]),
        kv(t, "Version", Span::styled(entry.version.clone(), t.text())),
        kv(
            t,
            "Risk",
            Span::styled(
                entry.risk.clone(),
                Style::default().fg(risk_color(t, &entry.risk)),
            ),
        ),
    ];
    if !entry.category.is_empty() {
        out.push(kv(
            t,
            "Category",
            Span::styled(entry.category.clone(), t.text()),
        ));
    }
    out.push(kv(
        t,
        "Archive",
        Span::styled(entry.download_url.clone(), t.dim()),
    ));
    out.push(kv(t, "sha256", Span::styled(entry.sha256.clone(), t.dim())));
    out.push(Line::default());
    out.push(Line::from(Span::styled(
        entry.description.clone(),
        t.text(),
    )));
    out.push(Line::default());
    out.push(Line::from(Span::styled(
        "Enter installs: the agent downloads and verifies the pinned archive, then asks for permissions.",
        t.dim(),
    )));
    out
}
