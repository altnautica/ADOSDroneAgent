//! Diagnostics detail page reachable from the overflow menu.
//!
//! A read-only system-info drilldown with three sections stacked under the
//! detail header band:
//!
//! 1. **System metrics** — CPU% / RAM% / temp / uptime in a four-column grid,
//!    each headline number paired with a 60-second sparkline (CPU and temp pull
//!    their trend from [`PageContext::system`]; uptime has no trend).
//! 2. **Identity** — board name + agent version on one line, device id on the
//!    next, primary IP + MAC on the third.
//! 3. **Recent agent logs** — the tail of the agent journal, oldest first,
//!    tinted by severity (error red, warning amber, info muted), clipped to the
//!    log band and offset by the page's scroll position.
//!
//! The fields the composer reads are gathered in [`PageContext::system`],
//! [`PageContext::device`], and [`PageContext::diagnostics`]. The page is a
//! pure composer over that context; the scroll position is supplied by the
//! navigator via `diagnostics.log_scroll_offset`.

use embedded_graphics::pixelcolor::Rgb888;

use crate::graphics::fonts::{FontFace, LoadedFont};
use crate::graphics::palette::Palette;
use crate::graphics::primitives::{line, text, Canvas};
use crate::graphics::sparkline::draw_sparkline;
use crate::pages::{blank_panel, HitAction, HitZone, Page, PageContext};
use crate::widgets::{draw_detail_header, DETAIL_HEADER_H};

/// Layout reference width of the detail-modal surface.
const PAGE_W: i32 = 480;
/// Layout reference height of the detail-modal surface (480x244 content frame).
const PAGE_H: i32 = 244;
/// Header band height shared by every detail modal.
const HEADER_H: i32 = DETAIL_HEADER_H;

/// Fixed height of the system-metrics section.
const METRICS_H: i32 = 56;
/// Fixed height of the identity section.
const IDENTITY_H: i32 = 60;

/// Per-log-row height in pixels.
const LOG_ROW_H: i32 = 12;
/// Left padding of the log lines.
const LOG_LEFT_PAD: i32 = 12;

/// Severity tier of a single log line.
enum LogLevel {
    Error,
    Warning,
    Info,
}

/// Classify a log line into a severity tier by keyword sniffing.
///
/// The journal tail strips the level prefix, so the tier is derived from the
/// text: anything mentioning an error / traceback / exception / critical /
/// failure is red, anything mentioning a warning is amber, everything else is
/// the muted secondary tone.
fn classify_log_level(line: &str) -> LogLevel {
    let low = line.to_ascii_lowercase();
    if low.contains("error")
        || low.contains("traceback")
        || low.contains("exception")
        || low.contains("critical")
        || low.contains("failed")
    {
        LogLevel::Error
    } else if low.contains("warn") {
        LogLevel::Warning
    } else {
        LogLevel::Info
    }
}

/// Format an uptime in seconds as a compact `Ns` / `Nm Ns` / `Nh Nm` / `Nd Nh`
/// string, or `--` when the value is absent or negative.
fn format_uptime(seconds: Option<f64>) -> String {
    let s = match seconds {
        Some(v) if v >= 0.0 => v as i64,
        _ => return "--".to_string(),
    };
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m {}s", s / 60, s % 60)
    } else if s < 86_400 {
        let h = s / 3600;
        let rem = s % 3600;
        format!("{h}h {}m", rem / 60)
    } else {
        let d = s / 86_400;
        let rem = s % 86_400;
        format!("{d}d {}h", rem / 3600)
    }
}

/// The diagnostics detail view, registered as `details.diagnostics`.
pub struct DiagnosticsDetailPage;

impl Page for DiagnosticsDetailPage {
    fn id(&self) -> &'static str {
        "details.diagnostics"
    }

    fn refresh_hz(&self) -> f32 {
        1.0
    }

    fn render(&self, ctx: &PageContext, palette: &Palette) -> Canvas {
        let mut canvas = blank_panel(palette);
        draw_detail_header(&mut canvas, palette, "Diagnostics");

        render_metrics_section(&mut canvas, palette, ctx);
        render_identity_section(&mut canvas, palette, ctx);
        render_log_section(&mut canvas, palette, ctx);
        canvas
    }

    fn hit_zones(&self, _ctx: &PageContext) -> Vec<HitZone> {
        let log_top = HEADER_H + METRICS_H + IDENTITY_H;
        vec![
            HitZone::new(8, 8, 40, 32, HitAction::Back),
            HitZone::new(
                0,
                log_top,
                PAGE_W,
                PAGE_H - log_top,
                HitAction::Custom("diagnostics.log_scroll".to_string()),
            ),
        ]
    }
}

/// Paint the four-column system-metrics grid: CPU / RAM / TEMP / UPTIME, each a
/// caps label over a headline value, with a 60-second sparkline under CPU and
/// TEMP (RAM and UPTIME carry no trend in the context).
fn render_metrics_section(canvas: &mut Canvas, palette: &Palette, ctx: &PageContext) {
    let section_y = HEADER_H + 4;
    let sys = &ctx.system;

    let cpu_text = match sys.cpu_pct {
        Some(v) => format!("{}%", v as i64),
        None => "--".to_string(),
    };
    let ram_pct = match (sys.ram_used_mb, sys.ram_total_mb) {
        (Some(used), Some(total)) if total > 0.0 => Some((100.0 * used / total).round() as i64),
        _ => None,
    };
    let ram_text = match ram_pct {
        Some(v) => format!("{v}%"),
        None => "--".to_string(),
    };
    let temp_text = match sys.temp_c {
        Some(v) => format!("{}°", v as i64),
        None => "--".to_string(),
    };
    let uptime_text = format_uptime(sys.uptime_seconds);

    let col_w = (PAGE_W - 16) / 4;
    let label_font = LoadedFont::new(FontFace::SansBold, 10);
    let value_font = LoadedFont::new(FontFace::SansBold, 16);

    let columns = [
        MetricColumn {
            label: "CPU",
            value: cpu_text.as_str(),
            spark: Some(&sys.cpu_history),
            color: palette.accent_primary,
            y_max: Some(100.0),
        },
        MetricColumn {
            label: "RAM",
            value: ram_text.as_str(),
            spark: None,
            color: palette.accent_primary,
            y_max: Some(100.0),
        },
        MetricColumn {
            label: "TEMP",
            value: temp_text.as_str(),
            spark: Some(&sys.temp_history),
            color: palette.status_warning,
            y_max: None,
        },
        MetricColumn {
            label: "UPTIME",
            value: uptime_text.as_str(),
            spark: None,
            color: palette.text_secondary,
            y_max: None,
        },
    ];

    for (i, col) in columns.iter().enumerate() {
        let cx = 8 + i as i32 * col_w;
        text(
            canvas,
            &label_font,
            col.label,
            cx,
            section_y,
            palette.text_tertiary,
        );
        text(
            canvas,
            &value_font,
            col.value,
            cx,
            section_y + 14,
            palette.text_primary,
        );
        if let Some(samples) = col.spark {
            if samples.iter().filter(|s| s.is_some()).count() >= 2 {
                draw_sparkline(
                    canvas,
                    cx,
                    section_y + 38,
                    (col_w - 12).max(1) as u32,
                    14,
                    samples,
                    col.color,
                    Some(0.0),
                    col.y_max,
                );
            }
        }
    }
}

/// One column of the system-metrics grid: a caps label, a headline value, and
/// an optional 60-second sparkline pinned to `y_max` (`None` auto-scales).
struct MetricColumn<'a> {
    label: &'a str,
    value: &'a str,
    spark: Option<&'a [Option<f64>]>,
    color: Rgb888,
    y_max: Option<f64>,
}

/// Paint the three identity lines: board + agent version, device id, IP + MAC.
fn render_identity_section(canvas: &mut Canvas, palette: &Palette, ctx: &PageContext) {
    let section_y = HEADER_H + METRICS_H;
    let dev = &ctx.device;

    let board = dev.board_name.as_deref().unwrap_or("--");
    let version = dev
        .version
        .as_deref()
        .or(ctx.system.agent_version.as_deref())
        .unwrap_or("--");
    let device_id = dev.device_id.as_deref().unwrap_or("--");
    let ip = dev.primary_ip.as_deref().unwrap_or("--");
    let mac = dev
        .primary_mac
        .as_deref()
        .or(dev.mac_eth0.as_deref())
        .or(dev.mac_wlan0.as_deref())
        .unwrap_or("--");

    let bold = LoadedFont::new(FontFace::SansBold, 12);
    let mono = LoadedFont::new(FontFace::MonoRegular, 11);

    let mut cy = section_y + 4;
    text(
        canvas,
        &bold,
        &format!("{board}  ·  agent {version}"),
        12,
        cy,
        palette.text_primary,
    );
    cy += 18;
    text(
        canvas,
        &mono,
        &format!("id {device_id}"),
        12,
        cy,
        palette.text_secondary,
    );
    cy += 18;
    text(
        canvas,
        &mono,
        &format!("ip {ip}  ·  mac {mac}"),
        12,
        cy,
        palette.text_secondary,
    );
}

/// Paint the scrollable agent-log pane below a divider + section label,
/// clipping rows to the log band and tinting each by severity.
fn render_log_section(canvas: &mut Canvas, palette: &Palette, ctx: &PageContext) {
    let section_y = HEADER_H + METRICS_H + IDENTITY_H;
    line(
        canvas,
        0,
        section_y,
        PAGE_W - 1,
        section_y,
        palette.border_default,
    );

    let label_font = LoadedFont::new(FontFace::SansBold, 10);
    text(
        canvas,
        &label_font,
        "AGENT LOGS",
        12,
        section_y + 2,
        palette.text_tertiary,
    );

    let lines = &ctx.diagnostics.agent_logs;
    let line_font = LoadedFont::new(FontFace::MonoRegular, 9);
    let log_band_top = section_y + 16;
    let log_band_bottom = PAGE_H - 1;
    let max_visible = ((log_band_bottom - log_band_top) / LOG_ROW_H).max(0);

    let offset = ctx.diagnostics.log_scroll_offset.max(0) as i32;
    let first_line = (offset / LOG_ROW_H).max(0) as usize;
    let sub_pixel = offset % LOG_ROW_H;

    for i in 0..=max_visible {
        let idx = first_line + i as usize;
        if idx >= lines.len() {
            break;
        }
        let row_y = log_band_top + i * LOG_ROW_H - sub_pixel;
        if row_y >= log_band_bottom {
            break;
        }
        let raw = &lines[idx];
        let color = match classify_log_level(raw) {
            LogLevel::Error => palette.status_error,
            LogLevel::Warning => palette.status_warning,
            LogLevel::Info => palette.text_secondary,
        };
        // Truncate to ~80 chars at mono 9 so a long line cannot spill past the
        // panel edge; append an ellipsis to mark the cut.
        let truncated = if raw.chars().count() <= 80 {
            raw.clone()
        } else {
            let head: String = raw.chars().take(79).collect();
            format!("{head}…")
        };
        text(canvas, &line_font, &truncated, LOG_LEFT_PAD, row_y, color);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graphics::palette::DARK;
    use crate::pages::PANEL_W;

    fn ctx_with_diag() -> PageContext {
        let mut ctx = PageContext::default();
        ctx.system.cpu_pct = Some(34.0);
        ctx.system.ram_used_mb = Some(1200.0);
        ctx.system.ram_total_mb = Some(4096.0);
        ctx.system.temp_c = Some(52.0);
        ctx.system.uptime_seconds = Some(3.0 * 3600.0 + 25.0 * 60.0);
        ctx.system.cpu_history = (0..60).map(|i| Some(20.0 + (i % 10) as f64)).collect();
        ctx.system.temp_history = (0..60).map(|i| Some(48.0 + (i % 5) as f64)).collect();
        ctx.device.board_name = Some("Reference SBC".to_string());
        ctx.device.version = Some("0.49.41".to_string());
        ctx.device.device_id = Some("ados-58c27faf".to_string());
        ctx.device.primary_ip = Some("192.168.200.178".to_string());
        ctx.device.primary_mac = Some("dc:a6:32:01:02:03".to_string());
        ctx.diagnostics.agent_logs = vec![
            "supervisor started".to_string(),
            "WARNING radio bind retry".to_string(),
            "ERROR wfb_tx exited".to_string(),
        ];
        ctx
    }

    #[test]
    fn diagnostics_renders_with_back_and_scroll_zones() {
        let page = DiagnosticsDetailPage;
        let ctx = ctx_with_diag();
        let c = page.render(&ctx, &DARK);
        assert_eq!(c.width(), PANEL_W);
        let zones = page.hit_zones(&ctx);
        assert_eq!(zones.len(), 2);
        assert_eq!(zones[0].action, HitAction::Back);
        assert_eq!(
            zones[1].action,
            HitAction::Custom("diagnostics.log_scroll".to_string())
        );
    }

    #[test]
    fn empty_context_still_renders_full_panel() {
        let page = DiagnosticsDetailPage;
        let ctx = PageContext::default();
        let c = page.render(&ctx, &DARK);
        assert_eq!(c.width(), PANEL_W);
    }

    #[test]
    fn uptime_formats_across_ranges() {
        assert_eq!(format_uptime(None), "--");
        assert_eq!(format_uptime(Some(-1.0)), "--");
        assert_eq!(format_uptime(Some(42.0)), "42s");
        assert_eq!(format_uptime(Some(125.0)), "2m 5s");
        assert_eq!(format_uptime(Some(3.0 * 3600.0 + 1500.0)), "3h 25m");
        assert_eq!(format_uptime(Some(2.0 * 86_400.0 + 4.0 * 3600.0)), "2d 4h");
    }

    #[test]
    fn log_level_classification() {
        assert!(matches!(classify_log_level("ERROR boom"), LogLevel::Error));
        assert!(matches!(
            classify_log_level("a traceback here"),
            LogLevel::Error
        ));
        assert!(matches!(
            classify_log_level("WARNING low"),
            LogLevel::Warning
        ));
        assert!(matches!(classify_log_level("steady state"), LogLevel::Info));
    }
}
