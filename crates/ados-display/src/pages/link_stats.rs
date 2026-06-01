//! Link Stats page — live radio + decoder + system metrics.
//!
//! A top-level tab and a read-only watch surface that catches the failure modes
//! operators actually hit on the bench: a radio alive but not transmitting, a
//! decoder reaching PLAYING with fps stuck at zero, mediamtx "ready" with the
//! inbound rate flat, an over-temp SoC. Three vertical bands fill the content
//! region:
//!
//! 1. **RADIO LINK** (top, 100 px) — state pill, channel, RSSI, bitrate,
//!    packets/loss, FEC, and a 60-second RSSI sparkline. Source:
//!    [`PageContext::link`].
//! 2. **DECODER + STREAM** (middle, 76 px) — decoder kind, fps, glass-to-glass
//!    latency, mediamtx ready + inbound rate, recording badge. Source:
//!    [`PageContext::video`].
//! 3. **SYSTEM** (bottom) — CPU% / RAM% / temp / disk. Source:
//!    [`PageContext::health`].
//!
//! Values read green when in spec, amber when degraded, red when broken, via the
//! threshold tiers below. The content region is the 480x244 frame below the top
//! status bar, so every page-local y derives from [`CONTENT_Y`].

use embedded_graphics::pixelcolor::Rgb888;

use crate::graphics::fonts::{FontFace, LoadedFont};
use crate::graphics::palette::Palette;
use crate::graphics::primitives::{fill_circle, fill_rect, line, text, Canvas};
use crate::graphics::sparkline::draw_sparkline;
use crate::pages::{blank_panel, HitZone, Page, PageContext, CONTENT_H, CONTENT_Y, PANEL_W};
use crate::widgets::{bottom_bar_zones, draw_bottom_bar, draw_top_bar};

/// Content-region width.
const PAGE_W: i32 = PANEL_W as i32;
/// Content-region height.
const PAGE_H: i32 = CONTENT_H as i32;
/// Panel-global y of the content region's top edge.
const OY: i32 = CONTENT_Y as i32;

/// LINK band height.
const LINK_H: i32 = 100;
/// DECODER + STREAM band height.
const DEC_H: i32 = 76;
/// SYSTEM band height (the remainder).
const SYS_H: i32 = PAGE_H - LINK_H - DEC_H;

/// Threshold tiers, picked to match the bench operating envelope.
const RSSI_OK_DBM: f64 = -65.0;
const RSSI_WARN_DBM: f64 = -80.0;
const FPS_OK: f64 = 25.0;
const FPS_WARN: f64 = 15.0;
const TEMP_OK_C: f64 = 65.0;
const TEMP_WARN_C: f64 = 75.0;
const LOSS_OK: f64 = 1.0;
const LOSS_WARN: f64 = 5.0;
const MEM_WARN: f64 = 80.0;
const MEM_CRIT: f64 = 90.0;

/// Whether a higher or a lower measured value is the good direction.
#[derive(Clone, Copy)]
enum Better {
    Higher,
    Lower,
}

/// Map a value to a green / amber / red color against two cut points. A `None`
/// value renders in the muted secondary tone.
fn color_for(value: Option<f64>, ok: f64, warn: f64, palette: &Palette, better: Better) -> Rgb888 {
    let v = match value {
        Some(v) => v,
        None => return palette.text_secondary,
    };
    match better {
        Better::Higher => {
            if v >= ok {
                palette.status_success
            } else if v >= warn {
                palette.status_warning
            } else {
                palette.status_error
            }
        }
        Better::Lower => {
            if v <= ok {
                palette.status_success
            } else if v <= warn {
                palette.status_warning
            } else {
                palette.status_error
            }
        }
    }
}

/// The live link/decoder/system watch surface, registered as `link_stats`.
pub struct LinkStatsPage;

impl Page for LinkStatsPage {
    fn id(&self) -> &'static str {
        "link_stats"
    }

    fn refresh_hz(&self) -> f32 {
        2.0
    }

    fn render(&self, ctx: &PageContext, palette: &Palette) -> Canvas {
        let mut canvas = blank_panel(palette);
        draw_top_bar(
            &mut canvas,
            palette,
            &ctx.hostname,
            ctx.role.current.as_deref().unwrap_or("unset"),
            ctx.system.cpu_pct,
            ctx.system.ram_used_mb,
            ctx.system.ram_total_mb,
            ctx.system.temp_c,
            &ctx.clock,
        );

        draw_link_band(&mut canvas, palette, ctx, OY, LINK_H);
        draw_dec_band(&mut canvas, palette, ctx, OY + LINK_H, DEC_H);
        draw_sys_band(&mut canvas, palette, ctx, OY + LINK_H + DEC_H, SYS_H);

        draw_bottom_bar(&mut canvas, palette, self.id());
        canvas
    }

    fn hit_zones(&self, _ctx: &PageContext) -> Vec<HitZone> {
        bottom_bar_zones()
    }
}

/// Paint the RADIO LINK band: state pill, channel, RSSI + bitrate headlines,
/// packets / loss, FEC counters, and the 60-second RSSI sparkline.
fn draw_link_band(canvas: &mut Canvas, palette: &Palette, ctx: &PageContext, y: i32, h: i32) {
    let link = &ctx.link;
    fill_rect(canvas, 0, y, PAGE_W - 1, y + h - 1, palette.bg_secondary);
    line(
        canvas,
        0,
        y + h - 1,
        PAGE_W - 1,
        y + h - 1,
        palette.border_default,
    );

    let title_f = LoadedFont::new(FontFace::SansBold, 11);
    text(
        canvas,
        &title_f,
        "RADIO LINK",
        12,
        y + 6,
        palette.text_tertiary,
    );

    let state = link
        .state
        .clone()
        .unwrap_or_else(|| "—".to_string())
        .to_ascii_uppercase();
    let state_color = match state.as_str() {
        "CONNECTED" => palette.status_success,
        "CONNECTING" | "DEGRADED" | "AUTO_PAIRING" | "BINDING" => palette.status_warning,
        "DISCONNECTED" | "UNPAIRED" => palette.status_error,
        _ => palette.text_secondary,
    };
    fill_circle(canvas, 93, y + 12, 3, state_color, None);
    text(canvas, &title_f, &state, 102, y + 6, palette.text_primary);

    let ch_text = match link.channel {
        Some(ch) if ch > 0 => format!("ch {ch}"),
        _ => "ch —".to_string(),
    };
    text(
        canvas,
        &title_f,
        &ch_text,
        PAGE_W - 70,
        y + 6,
        palette.text_secondary,
    );

    // Headline row: RSSI + bitrate.
    let big_f = LoadedFont::new(FontFace::MonoBold, 24);
    let small_f = LoadedFont::new(FontFace::SansRegular, 10);

    let rssi_text = match link.rssi_dbm {
        Some(v) => format!("{}", v as i64),
        None => "—".to_string(),
    };
    let rssi_color = color_for(
        link.rssi_dbm,
        RSSI_OK_DBM,
        RSSI_WARN_DBM,
        palette,
        Better::Higher,
    );
    text(canvas, &big_f, &rssi_text, 12, y + 26, rssi_color);
    text(
        canvas,
        &small_f,
        "RSSI dBm",
        12,
        y + 56,
        palette.text_secondary,
    );

    let bitrate_text = match link.bitrate_kbps {
        Some(kbps) => format!("{:.1}", kbps / 1000.0),
        None => "—".to_string(),
    };
    text(
        canvas,
        &big_f,
        &bitrate_text,
        100,
        y + 26,
        palette.text_primary,
    );
    text(
        canvas,
        &small_f,
        "Mbps",
        100,
        y + 56,
        palette.text_secondary,
    );

    // Packets + loss block.
    let med_f = LoadedFont::new(FontFace::SansRegular, 11);
    let pkt_label = match link.packets_received {
        Some(n) => format!("pkts {n}"),
        None => "pkts —".to_string(),
    };
    let lost_label = match link.packets_lost {
        Some(n) => format!("lost {n}"),
        None => "lost —".to_string(),
    };
    let loss_label = match link.loss_percent {
        Some(loss) => format!("({loss:.1}%)"),
        None => "(—)".to_string(),
    };
    let loss_color = color_for(
        link.loss_percent,
        LOSS_OK,
        LOSS_WARN,
        palette,
        Better::Lower,
    );
    text(
        canvas,
        &med_f,
        &pkt_label,
        200,
        y + 30,
        palette.text_primary,
    );
    text(
        canvas,
        &med_f,
        &lost_label,
        200,
        y + 46,
        palette.text_secondary,
    );
    text(canvas, &med_f, &loss_label, 280, y + 46, loss_color);

    // FEC counters.
    let fec_ok = match link.fec_recovered {
        Some(n) => format!("FEC ok {n}"),
        None => "FEC ok —".to_string(),
    };
    let fec_bad = match link.fec_lost {
        Some(n) => format!("FEC bad {n}"),
        None => "FEC bad —".to_string(),
    };
    let fec_bad_color = match link.fec_lost {
        Some(n) if n > 0 => palette.status_error,
        _ => palette.text_secondary,
    };
    text(canvas, &med_f, &fec_ok, 350, y + 30, palette.text_secondary);
    text(canvas, &med_f, &fec_bad, 350, y + 46, fec_bad_color);

    // 60-second RSSI sparkline along the bottom of the band.
    if link.rssi_history.iter().any(|v| v.is_some()) {
        draw_sparkline(
            canvas,
            12,
            y + 70,
            (PAGE_W - 24) as u32,
            24,
            &link.rssi_history,
            palette.accent_primary,
            Some(-90.0),
            Some(-30.0),
        );
    }
}

/// Paint the DECODER + STREAM band: decoder kind, fps, latency on the left;
/// mediamtx ready + inbound rate on the right; a recording badge when active.
fn draw_dec_band(canvas: &mut Canvas, palette: &Palette, ctx: &PageContext, y: i32, h: i32) {
    let video = &ctx.video;
    fill_rect(canvas, 0, y, PAGE_W - 1, y + h - 1, palette.bg_primary);
    line(
        canvas,
        0,
        y + h - 1,
        PAGE_W - 1,
        y + h - 1,
        palette.border_default,
    );

    let title_f = LoadedFont::new(FontFace::SansBold, 11);
    text(
        canvas,
        &title_f,
        "DECODER",
        12,
        y + 6,
        palette.text_tertiary,
    );
    text(
        canvas,
        &title_f,
        "STREAM",
        240,
        y + 6,
        palette.text_tertiary,
    );

    let med_f = LoadedFont::new(FontFace::SansRegular, 11);
    let mono_f = LoadedFont::new(FontFace::MonoBold, 14);

    let decoder = video.decoder.clone().unwrap_or_else(|| "—".to_string());
    text(canvas, &med_f, &decoder, 12, y + 24, palette.text_primary);

    let fps_text = match video.fps {
        Some(v) => format!("{v:.1} fps"),
        None => "— fps".to_string(),
    };
    let fps_color = color_for(video.fps, FPS_OK, FPS_WARN, palette, Better::Higher);
    text(canvas, &mono_f, &fps_text, 12, y + 42, fps_color);

    let (latency_text, latency_color) = match video.latency_ms {
        Some(ms) => {
            let color = if ms <= 80.0 {
                palette.status_success
            } else if ms <= 150.0 {
                palette.status_warning
            } else {
                palette.status_error
            };
            (format!("{} ms", ms.round() as i64), color)
        }
        None => ("— ms".to_string(), palette.text_secondary),
    };
    text(canvas, &mono_f, &latency_text, 110, y + 42, latency_color);

    if !video.active {
        text(
            canvas,
            &med_f,
            "(tap inactive)",
            200,
            y + 46,
            palette.text_secondary,
        );
    }

    // Stream column: mediamtx ready + inbound rate.
    let ready = video.mediamtx_ready;
    let ready_text = if ready { "ready" } else { "not-ready" };
    let ready_color = if ready {
        palette.status_success
    } else {
        palette.status_error
    };
    fill_circle(canvas, 243, y + 30, 3, ready_color, None);
    text(
        canvas,
        &med_f,
        &format!("mediamtx {ready_text}"),
        252,
        y + 24,
        palette.text_primary,
    );
    let rate_text = match video.mediamtx_inbound_kbps {
        Some(kbps) => format!("{:.2} Mbps in", kbps / 1000.0),
        None => "— Mbps in".to_string(),
    };
    text(
        canvas,
        &mono_f,
        &rate_text,
        240,
        y + 42,
        palette.text_primary,
    );

    if video.recording {
        fill_circle(canvas, PAGE_W - 87, y + 30, 3, palette.status_error, None);
        text(
            canvas,
            &med_f,
            "REC",
            PAGE_W - 76,
            y + 24,
            palette.status_error,
        );
    }
}

/// Paint the SYSTEM band: a four-column CPU / RAM / TEMP / DISK grid from the
/// health sidecar, with RAM and temperature threshold-colored.
fn draw_sys_band(canvas: &mut Canvas, palette: &Palette, ctx: &PageContext, y: i32, h: i32) {
    let health = &ctx.health;
    fill_rect(canvas, 0, y, PAGE_W - 1, y + h - 1, palette.bg_secondary);

    let title_f = LoadedFont::new(FontFace::SansBold, 11);
    text(canvas, &title_f, "SYSTEM", 12, y + 6, palette.text_tertiary);

    let val_f = LoadedFont::new(FontFace::MonoBold, 14);
    let lab_f = LoadedFont::new(FontFace::SansRegular, 10);

    let ram_color = color_for(
        health.memory_percent,
        MEM_WARN - 1.0,
        MEM_CRIT - 1.0,
        palette,
        Better::Lower,
    );
    let temp_color = color_for(
        health.temperature,
        TEMP_OK_C,
        TEMP_WARN_C,
        palette,
        Better::Lower,
    );

    let cpu_text = match health.cpu_percent {
        Some(v) => format!("{}%", v as i64),
        None => "—".to_string(),
    };
    let ram_text = match health.memory_percent {
        Some(v) => format!("{}%", v as i64),
        None => "—".to_string(),
    };
    let temp_text = match health.temperature {
        Some(v) => format!("{}°C", v as i64),
        None => "—".to_string(),
    };
    let disk_text = match health.disk_percent {
        Some(v) => format!("{}%", v as i64),
        None => "—".to_string(),
    };

    let x0 = 12;
    let col_w = (PAGE_W - 24) / 4;
    let columns = [
        (cpu_text.as_str(), "CPU", palette.text_primary),
        (ram_text.as_str(), "RAM", ram_color),
        (temp_text.as_str(), "TEMP", temp_color),
        (disk_text.as_str(), "DISK", palette.text_primary),
    ];
    for (i, (value, label, color)) in columns.iter().enumerate() {
        let cx = x0 + i as i32 * col_w;
        text(canvas, &val_f, value, cx, y + 26, *color);
        text(canvas, &lab_f, label, cx, y + 48, palette.text_secondary);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graphics::palette::DARK;

    fn ctx_with_stats() -> PageContext {
        let mut ctx = PageContext::default();
        ctx.link.state = Some("connected".to_string());
        ctx.link.channel = Some(149);
        ctx.link.rssi_dbm = Some(-58.0);
        ctx.link.bitrate_kbps = Some(18000.0);
        ctx.link.packets_received = Some(120_000);
        ctx.link.packets_lost = Some(12);
        ctx.link.loss_percent = Some(0.4);
        ctx.link.fec_recovered = Some(7);
        ctx.link.fec_lost = Some(0);
        ctx.link.rssi_history = (0..60).map(|i| Some(-60.0 + (i % 8) as f64)).collect();
        ctx.video.decoder = Some("h264 v4l2m2m".to_string());
        ctx.video.active = true;
        ctx.video.fps = Some(30.0);
        ctx.video.latency_ms = Some(64.0);
        ctx.video.mediamtx_ready = true;
        ctx.video.mediamtx_inbound_kbps = Some(4200.0);
        ctx.video.recording = true;
        ctx.health.cpu_percent = Some(41.0);
        ctx.health.memory_percent = Some(55.0);
        ctx.health.disk_percent = Some(22.0);
        ctx.health.temperature = Some(52.0);
        ctx
    }

    #[test]
    fn link_stats_renders_and_carries_tab_zones() {
        let page = LinkStatsPage;
        let ctx = ctx_with_stats();
        let c = page.render(&ctx, &DARK);
        assert_eq!(c.width(), PANEL_W);
        assert_eq!(page.hit_zones(&ctx).len(), 5);
        assert_eq!(page.id(), "link_stats");
    }

    #[test]
    fn empty_context_renders_full_panel() {
        let page = LinkStatsPage;
        let ctx = PageContext::default();
        let c = page.render(&ctx, &DARK);
        assert_eq!(c.width(), PANEL_W);
    }

    #[test]
    fn color_tiers_follow_direction() {
        // Higher-is-better: above ok = success, between = warning, below = error.
        assert_eq!(
            color_for(
                Some(-50.0),
                RSSI_OK_DBM,
                RSSI_WARN_DBM,
                &DARK,
                Better::Higher
            ),
            DARK.status_success
        );
        assert_eq!(
            color_for(
                Some(-72.0),
                RSSI_OK_DBM,
                RSSI_WARN_DBM,
                &DARK,
                Better::Higher
            ),
            DARK.status_warning
        );
        assert_eq!(
            color_for(
                Some(-88.0),
                RSSI_OK_DBM,
                RSSI_WARN_DBM,
                &DARK,
                Better::Higher
            ),
            DARK.status_error
        );
        // Lower-is-better: at/below ok = success, above warn = error.
        assert_eq!(
            color_for(Some(0.5), LOSS_OK, LOSS_WARN, &DARK, Better::Lower),
            DARK.status_success
        );
        assert_eq!(
            color_for(Some(9.0), LOSS_OK, LOSS_WARN, &DARK, Better::Lower),
            DARK.status_error
        );
        // None renders muted.
        assert_eq!(
            color_for(None, FPS_OK, FPS_WARN, &DARK, Better::Higher),
            DARK.text_secondary
        );
    }
}
