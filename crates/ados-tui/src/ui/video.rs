//! Tab 4, Video: encoder and radio settings, cameras, glass-to-glass latency,
//! and the viewer URLs (copyable; no video is drawn in the terminal).

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use serde_json::Value;

use crate::app::{Core, FrameOut, Hit, Tab};
use crate::keymap::{Command, Ctx};
use crate::model::ring::Ring;
use crate::poll::{ResourceId, Sub};
use crate::ui::widgets::{kv_text, panel, render_cells, rows, truncate, Cell};

/// Latency samples kept: 100 reads at 3 s = 5 minutes.
const LATENCY_HISTORY: usize = 100;

pub struct VideoTab {
    latency: Ring,
    sel: usize,
}

impl Default for VideoTab {
    fn default() -> Self {
        Self {
            latency: Ring::new(LATENCY_HISTORY),
            sel: 0,
        }
    }
}

fn s(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| match x {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    })
}

fn dash(v: Option<String>) -> String {
    v.unwrap_or_else(|| "—".into())
}

/// An agent-relative path (`/whep`) made absolute against the target base;
/// an absolute URL passes through.
pub fn absolute(base: &str, url: &str) -> String {
    if url.contains("://") {
        url.to_string()
    } else {
        format!(
            "{}/{}",
            base.trim_end_matches('/'),
            url.trim_start_matches('/')
        )
    }
}

/// The viewer URLs, `(label, url)`, from the setup status `video` block and
/// the advertised video access URLs.
pub fn viewer_urls(core: &Core) -> Vec<(String, String)> {
    let base = core.target.base.as_str();
    let mut out: Vec<(String, String)> = Vec::new();
    if let Some(video) = core
        .value(&ResourceId::SetupStatus)
        .and_then(|v| v.get("video"))
    {
        for (key, label) in [
            ("whep_url", "WHEP"),
            ("hls_url", "HLS"),
            ("public_whep_url", "Public WHEP"),
            ("public_hls_url", "Public HLS"),
        ] {
            if let Some(url) = video
                .get(key)
                .and_then(Value::as_str)
                .filter(|u| !u.is_empty())
            {
                out.push((label.into(), absolute(base, url)));
            }
        }
    }
    if let Some(d) = core.dash() {
        for a in d.access_urls.iter().filter(|a| a.kind == "video") {
            let viewer = a.url.clone();
            let whep = format!("{}/whep", viewer.trim_end_matches('/'));
            out.push((format!("{} page", a.label), viewer));
            out.push((format!("{} WHEP", a.label), whep));
        }
    }
    out.dedup_by(|a, b| a.1 == b.1);
    out
}

impl VideoTab {
    fn encoder(&self, f: &mut Frame, area: Rect, core: &Core) {
        let t = &core.theme;
        let block = panel(t, core.sym, "Encoder");
        let inner = block.inner(area);
        f.render_widget(block, area);
        let Some(cfg) = core.value(&ResourceId::VideoConfig) else {
            return note(f, inner, core, &ResourceId::VideoConfig);
        };
        let e = cfg.get("encoder").cloned().unwrap_or(Value::Null);
        let size = match (s(&e, "width"), s(&e, "height")) {
            (Some(w), Some(h)) => format!("{w}×{h}"),
            _ => "—".into(),
        };
        let lines = vec![
            kv_text(t, "Codec", 9, dash(s(&e, "codec")).to_uppercase()),
            kv_text(t, "Size", 9, size),
            kv_text(
                t,
                "Frame rate",
                9,
                dash(s(&e, "fps").map(|f| format!("{f} fps"))),
            ),
            kv_text(
                t,
                "Bitrate",
                9,
                dash(s(&e, "bitrate_kbps").map(|b| format!("{b} kbit/s"))),
            ),
            kv_text(
                t,
                "Pipeline",
                9,
                core.dash()
                    .map(|d| d.video_state.clone())
                    .unwrap_or_else(|| "—".into()),
            ),
        ];
        f.render_widget(Paragraph::new(lines), inner);
    }

    fn radio(&self, f: &mut Frame, area: Rect, core: &Core) {
        let t = &core.theme;
        let block = panel(t, core.sym, "Radio");
        let inner = block.inner(area);
        f.render_widget(block, area);
        let Some(cfg) = core.value(&ResourceId::VideoConfig) else {
            return note(f, inner, core, &ResourceId::VideoConfig);
        };
        let r = cfg.get("radio").cloned().unwrap_or(Value::Null);
        let a = cfg.get("adaptive").cloned().unwrap_or(Value::Null);
        let mut lines = vec![
            kv_text(
                t,
                "Channel",
                9,
                format!("{} · {}", dash(s(&r, "channel")), dash(s(&r, "band"))),
            ),
            kv_text(t, "Preset", 9, dash(s(&r, "preset"))),
            kv_text(
                t,
                "MCS / FEC",
                9,
                format!(
                    "MCS {} · FEC {}/{}",
                    dash(s(&r, "mcs_index")),
                    dash(s(&r, "fec_k")),
                    dash(s(&r, "fec_n"))
                ),
            ),
            kv_text(
                t,
                "TX power",
                9,
                dash(s(&r, "tx_power_dbm").map(|p| format!("{p} dBm"))),
            ),
        ];
        if let Some(rec) = s(&a, "recommended_bitrate_kbps") {
            lines.push(kv_text(
                t,
                "Adaptive",
                9,
                format!("recommends {rec} kbit/s"),
            ));
        }
        f.render_widget(Paragraph::new(lines), inner);
    }

    fn latency(&self, f: &mut Frame, area: Rect, core: &Core) {
        let t = &core.theme;
        let block = panel(t, core.sym, "Latency");
        let inner = block.inner(area);
        f.render_widget(block, area);
        let Some(l) = core.value(&ResourceId::VideoLatency) else {
            return note(f, inner, core, &ResourceId::VideoLatency);
        };
        let num = |k: &str| l.get(k).and_then(Value::as_f64);
        let source = l
            .get("source")
            .and_then(Value::as_str)
            .unwrap_or("—")
            .to_string();
        let mut cells = vec![
            Cell::Line(kv_text(
                t,
                "EWMA",
                9,
                num("ewma_ms")
                    .map(|v| format!("{v:.0} ms"))
                    .unwrap_or_else(|| "—".into()),
            )),
            Cell::Line(kv_text(
                t,
                "Last",
                9,
                num("latency_ms")
                    .map(|v| format!("{v:.0} ms"))
                    .unwrap_or_else(|| "—".into()),
            )),
            Cell::Line(kv_text(t, "Source", 9, source)),
        ];
        if let Some(n) = num("samples") {
            cells.push(Cell::Line(kv_text(t, "Samples", 9, format!("{n:.0}"))));
        }
        cells.push(Cell::Spark {
            data: self.latency.spark(1.0),
            max: None,
            color: t.accent(),
        });
        render_cells(f, t, inner, cells);
    }

    fn cameras(&self, f: &mut Frame, area: Rect, core: &Core) {
        let t = &core.theme;
        let block = panel(t, core.sym, "Cameras");
        let inner = block.inner(area);
        f.render_widget(block, area);
        let Some(v) = core.value(&ResourceId::VideoCameras) else {
            return note(f, inner, core, &ResourceId::VideoCameras);
        };
        let list = v
            .get("cameras")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        if list.is_empty() {
            f.render_widget(
                Paragraph::new(Span::styled("No cameras detected", t.dim())),
                inner,
            );
            return;
        }
        let mut lines = vec![Line::from(Span::styled(
            format!("{:<16} {:<8} {:<32} {}", "Device", "Type", "Label", "Size"),
            t.dim(),
        ))];
        for c in list {
            let size = match (s(c, "width"), s(c, "height")) {
                (Some(w), Some(h)) if w != "0" => format!("{w}×{h}"),
                _ => "—".into(),
            };
            lines.push(Line::from(Span::styled(
                format!(
                    "{:<16} {:<8} {:<32} {size}",
                    truncate(&dash(s(c, "device_path")), 16),
                    truncate(&dash(s(c, "type")), 8),
                    truncate(&dash(s(c, "label")), 32)
                ),
                t.text(),
            )));
        }
        f.render_widget(Paragraph::new(lines), inner);
    }

    fn urls(
        &self,
        f: &mut Frame,
        area: Rect,
        core: &Core,
        urls: &[(String, String)],
        out: &mut FrameOut,
    ) {
        let t = &core.theme;
        let block = panel(t, core.sym, "Viewer URLs");
        let inner = block.inner(area);
        f.render_widget(block, area);
        if urls.is_empty() {
            let state = core
                .dash()
                .map(|d| d.video_state.clone())
                .unwrap_or_default();
            let msg = format!(
                "No viewer URL: the video pipeline is {}",
                if state.is_empty() {
                    "not reported"
                } else {
                    &state
                }
            );
            f.render_widget(Paragraph::new(Span::styled(msg, t.dim())), inner);
            return;
        }
        let width = urls
            .iter()
            .map(|(l, _)| l.chars().count())
            .max()
            .unwrap_or(0)
            .min(24);
        for ((i, (label, url)), rect) in urls.iter().enumerate().zip(rows(inner)) {
            let sel = i == self.sel.min(urls.len() - 1);
            let marker = if sel { core.sym.row_marker } else { " " };
            let style = if sel { t.accented() } else { t.text() };
            let line = Line::from(vec![
                Span::styled(format!("{marker} "), t.accented()),
                Span::styled(format!("{:<width$} ", truncate(label, width)), t.dim()),
                Span::styled(url.clone(), style),
            ]);
            f.render_widget(Paragraph::new(line), rect);
            out.hit(rect, Hit::Item("url", i));
        }
    }
}

/// A dim note for a resource with no value: its error, or loading.
fn note(f: &mut Frame, area: Rect, core: &Core, id: &ResourceId) {
    let text = core
        .slot(id)
        .and_then(|s| s.error.clone())
        .unwrap_or_else(|| "Loading…".into());
    f.render_widget(Paragraph::new(Span::styled(text, core.theme.dim())), area);
}

impl Tab for VideoTab {
    fn ctx(&self, _core: &Core) -> Vec<Ctx> {
        vec![Ctx::List]
    }

    fn subs(&self, _core: &Core) -> Vec<Sub> {
        vec![
            Sub::every_ms(ResourceId::VideoConfig, "/api/video/config", 15_000),
            Sub::every_ms(ResourceId::VideoCameras, "/api/video/cameras", 15_000),
            Sub::every_ms(ResourceId::VideoLatency, "/api/video/latency", 3000),
            Sub::every_ms(ResourceId::SetupStatus, "/api/v1/setup/status", 15_000),
        ]
    }

    fn on_resource(&mut self, core: &mut Core, id: &ResourceId) {
        if *id != ResourceId::VideoLatency {
            return;
        }
        let ewma = core
            .slot(id)
            .filter(|s| s.error.is_none())
            .and_then(|s| s.value.as_ref())
            .and_then(|v| v.get("ewma_ms"))
            .and_then(Value::as_f64);
        self.latency.push(ewma);
    }

    fn on_command(&mut self, core: &mut Core, cmd: Command) -> bool {
        let n = viewer_urls(core).len();
        match cmd {
            Command::Up => self.sel = self.sel.saturating_sub(1),
            Command::Down => self.sel = (self.sel + 1).min(n.saturating_sub(1)),
            Command::Top => self.sel = 0,
            Command::Bottom => self.sel = n.saturating_sub(1),
            Command::Copy => match viewer_urls(core)
                .into_iter()
                .nth(self.sel.min(n.saturating_sub(1)))
            {
                Some((_, url)) => core.copy(&url),
                None => core.toast(crate::toast::ToastKind::Info, "No viewer URL to copy"),
            },
            _ => return false,
        }
        core.dirty = true;
        true
    }

    fn on_click(&mut self, core: &mut Core, hit: &Hit) {
        if let Hit::Item("url", i) = hit {
            self.sel = *i;
            core.dirty = true;
        }
    }

    fn render(&mut self, f: &mut Frame, area: Rect, core: &Core, out: &mut FrameOut) {
        let urls = viewer_urls(core);
        let url_h = (urls.len().max(1) as u16 + 2).min(10);
        let parts = Layout::vertical([
            Constraint::Length(9),
            Constraint::Min(4),
            Constraint::Length(url_h),
        ])
        .split(area);
        let top = Layout::horizontal([Constraint::Ratio(1, 3); 3]).split(parts[0]);
        self.encoder(f, top[0], core);
        self.radio(f, top[1], core);
        self.latency(f, top[2], core);
        self.cameras(f, parts[1], core);
        self.urls(f, parts[2], core, &urls, out);
    }

    fn context_line(&self, core: &Core) -> Option<String> {
        let urls = viewer_urls(core);
        let (label, _) = urls.get(self.sel.min(urls.len().saturating_sub(1)))?;
        let mut s = format!("y copies the {label} URL");
        if label.contains("WHEP") {
            s.push_str(" · open it in a WebRTC (WHEP) player");
        }
        Some(s)
    }
}

#[cfg(test)]
mod tests;
