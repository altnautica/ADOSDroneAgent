//! Video page — live H.264 preview from the local feed.
//!
//! The top-level Video tab. It reserves the upper 480x176 region of the content
//! area for the decoded video frame and the lower 480x68 strip for a permanent
//! metrics row. A REC chip lives over the top-left of the video plane and a
//! camera-switch chip over the top-right (the chip is hidden when only one
//! camera is enumerated). When no decoded frame is present the video plane shows
//! a centered "waiting for stream" card rather than a black hole.
//!
//! The metrics strip is a 4-column, 2-row grid: latency, RSSI, bitrate, and TX
//! power on the top row; FEC drops, FPS, channel, and MCS on the bottom row.
//! Each cell is a small caps label over a mono value. The values come straight
//! from [`PageContext::video`] and [`PageContext::link`]; the live-decode plane
//! is a later concern but the chrome, metrics, and chips are painted here.

use crate::graphics::fonts::{FontFace, LoadedFont};
use crate::graphics::palette::Palette;
use crate::graphics::primitives::{fill_circle, fill_rect, fill_rect_outline, line, text, Canvas};
use crate::pages::{
    blank_panel, HitAction, HitZone, LinkCtx, Page, PageContext, VideoCtx, CONTENT_H, CONTENT_Y,
    PANEL_W,
};
use crate::widgets::{bottom_bar_zones, draw_bottom_bar, draw_top_bar};

/// Height of the decoded-frame plane at the top of the content region.
const VIDEO_H: i32 = 176;
/// Height of the metrics strip below the video plane (content height minus the
/// video plane: `244 - 176 = 68`).
const METRICS_H: i32 = CONTENT_H as i32 - VIDEO_H;

/// REC / camera chip dimensions.
const CHIP_W: i32 = 80;
const CHIP_H: i32 = 32;

/// The live video preview, registered as `video`.
pub struct VideoPage;

impl Page for VideoPage {
    fn id(&self) -> &'static str {
        "video"
    }

    fn refresh_hz(&self) -> f32 {
        20.0
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
        render_content(&mut canvas, palette, ctx);
        draw_bottom_bar(&mut canvas, palette, self.id());
        canvas
    }

    fn hit_zones(&self, ctx: &PageContext) -> Vec<HitZone> {
        // The REC button and the camera chip take dispatch precedence, so they
        // come first. The surface zone covers the rest of the video plane and
        // toggles the detail overlay; the metrics strip absorbs taps so they
        // do not leak into a navigation gesture. The bottom-bar tab zones close
        // out the list.
        let mut zones = vec![HitZone::new(
            8,
            8,
            CHIP_W,
            CHIP_H,
            HitAction::Custom("video.rec_button".to_string()),
        )];
        if ctx.video.camera_count > 1 {
            zones.push(HitZone::new(
                PANEL_W as i32 - 88,
                8,
                CHIP_W,
                CHIP_H,
                HitAction::Custom("video.cam_chip".to_string()),
            ));
        }
        zones.push(HitZone::new(
            0,
            0,
            PANEL_W as i32,
            VIDEO_H,
            HitAction::Custom("video.surface".to_string()),
        ));
        zones.push(HitZone::new(
            0,
            VIDEO_H,
            PANEL_W as i32,
            METRICS_H,
            HitAction::Custom("video.metrics_strip".to_string()),
        ));
        zones.extend(bottom_bar_zones());
        zones
    }
}

/// Paint the video plane, metrics strip, and overlay chips into the content
/// region. Page-local coordinates are shifted down by [`CONTENT_Y`] for the
/// panel-global paint, matching the dashboard's content offset.
fn render_content(canvas: &mut Canvas, palette: &Palette, ctx: &PageContext) {
    let oy = CONTENT_Y as i32;
    // Clear the content region so a redraw never keeps a previous page's pixels.
    fill_rect(
        canvas,
        0,
        oy,
        PANEL_W as i32 - 1,
        oy + CONTENT_H as i32 - 1,
        palette.bg_primary,
    );

    let video = &ctx.video;
    // The video plane reads as a dedicated band with a centered status-message
    // card when no decoded frame is present, so the operator never sees a black
    // hole; the metrics row dims its colors while the plane has no live frame.
    let dim = !video.active;
    draw_video_plane(canvas, palette, oy, video);
    draw_metrics(canvas, palette, oy, video, &ctx.link, dim);
    draw_rec_button(canvas, palette, 8, oy + 8, video.recording);
    draw_camera_chip(
        canvas,
        palette,
        PANEL_W as i32 - 88,
        oy + 8,
        video.camera_label.as_deref().unwrap_or("CAM 1"),
        video.camera_count.max(1),
    );
}

/// Paint the decoded-frame plane: a status card with a centered message that
/// names whether the pipeline is waiting for a stream or unavailable.
fn draw_video_plane(canvas: &mut Canvas, palette: &Palette, oy: i32, video: &VideoCtx) {
    fill_rect(
        canvas,
        0,
        oy,
        PANEL_W as i32 - 1,
        oy + VIDEO_H - 1,
        palette.bg_secondary,
    );
    // The message tracks whether the pipeline ever came up: a ready mediamtx
    // path that has not yet handed a frame reads as "waiting for stream"; a
    // pipeline that never bound reads as "unavailable".
    let message = if video.mediamtx_ready || video.active {
        "Video link not available — waiting for stream"
    } else {
        "Video pipeline unavailable"
    };
    let font = LoadedFont::new(FontFace::SansRegular, 14);
    let (tw, th) = font.text_size(message);
    let tx = (PANEL_W as i32 - tw as i32) / 2;
    let ty = oy + (VIDEO_H - th as i32) / 2;
    text(canvas, &font, message, tx, ty, palette.text_secondary);
}

/// Paint the 4x2 metrics grid in the strip below the video plane.
fn draw_metrics(
    canvas: &mut Canvas,
    palette: &Palette,
    oy: i32,
    video: &VideoCtx,
    link: &LinkCtx,
    dim: bool,
) {
    let strip_top = oy + VIDEO_H;
    // Background plate so the metrics row reads as a separate band.
    fill_rect(
        canvas,
        0,
        strip_top,
        PANEL_W as i32 - 1,
        oy + CONTENT_H as i32 - 1,
        palette.bg_secondary,
    );
    // Top divider so the eye separates picture from data.
    line(
        canvas,
        0,
        strip_top,
        PANEL_W as i32 - 1,
        strip_top,
        palette.border_default,
    );

    let label_font = LoadedFont::new(FontFace::SansBold, 9);
    let value_font = LoadedFont::new(FontFace::MonoRegular, 11);
    let col_w = PANEL_W as i32 / 4;

    // FEC drops are derived from the recovered / lost counters the same way the
    // metrics formatter does: lost / (recovered + lost).
    let fec = match (link.fec_lost, link.fec_recovered) {
        (Some(lost), Some(rec)) => Some((lost, rec + lost)),
        _ => None,
    };

    let cells: [[(&str, String); 4]; 2] = [
        [
            ("LATENCY", format_latency(video.latency_ms)),
            ("RSSI", format_rssi(link.rssi_dbm)),
            ("BITRATE", format_bitrate(video.bitrate_kbps)),
            ("TX", format_tx_power(link.tx_power_dbm)),
        ],
        [
            ("FEC DROPS", format_drops(fec)),
            ("FPS", format_fps(video.fps)),
            ("CHANNEL", format_channel(link.channel)),
            ("MCS", format_mcs(link.mcs_index)),
        ],
    ];

    let label_color = if dim {
        palette.text_tertiary
    } else {
        palette.text_secondary
    };
    let value_color = if dim {
        palette.text_secondary
    } else {
        palette.text_primary
    };

    for (r_idx, row) in cells.iter().enumerate() {
        let ry = strip_top + 6 + r_idx as i32 * 30;
        for (c_idx, (label, value)) in row.iter().enumerate() {
            let lx = c_idx as i32 * col_w + 8;
            text(canvas, &label_font, label, lx, ry, label_color);
            text(canvas, &value_font, value, lx, ry + 11, value_color);
        }
    }
}

/// Paint the REC chip at `(x, y)`. Idle is an outlined chip in muted text;
/// recording is a filled error-red pill with a dot to the left of the label.
fn draw_rec_button(canvas: &mut Canvas, palette: &Palette, x: i32, y: i32, recording: bool) {
    let label = "REC";
    let label_font = LoadedFont::new(FontFace::SansBold, 12);
    let (text_w, text_h) = label_font.text_size(label);

    if recording {
        let bg = palette.status_error;
        let text_color = palette.text_primary;
        fill_rect_outline(canvas, x, y, x + CHIP_W - 1, y + CHIP_H - 1, bg, text_color);
        // Indicator dot on the left.
        let dot_radius = 5;
        let dot_cx = x + 16;
        let dot_cy = y + CHIP_H / 2;
        fill_circle(canvas, dot_cx, dot_cy, dot_radius, text_color, None);
        let tx = x + 32 + (CHIP_W - 32 - text_w as i32) / 2;
        let ty = y + (CHIP_H - text_h as i32) / 2 - 1;
        text(canvas, &label_font, label, tx, ty, text_color);
    } else {
        let bg = palette.bg_secondary;
        let outline = palette.text_secondary;
        fill_rect_outline(canvas, x, y, x + CHIP_W - 1, y + CHIP_H - 1, bg, outline);
        let tx = x + (CHIP_W - text_w as i32) / 2;
        let ty = y + (CHIP_H - text_h as i32) / 2 - 1;
        text(canvas, &label_font, label, tx, ty, outline);
    }
}

/// Paint the camera-switch chip at `(x, y)`. Hidden when only one camera is
/// present so the operator is not drawn into a no-op picker.
fn draw_camera_chip(
    canvas: &mut Canvas,
    palette: &Palette,
    x: i32,
    y: i32,
    label: &str,
    count: i64,
) {
    if count <= 1 {
        return;
    }
    let bg = palette.bg_secondary;
    let outline = palette.text_secondary;
    fill_rect_outline(canvas, x, y, x + CHIP_W - 1, y + CHIP_H - 1, bg, outline);

    let label_font = LoadedFont::new(FontFace::SansBold, 11);
    let badge_font = LoadedFont::new(FontFace::MonoRegular, 10);
    let badge = format!("·{count}");

    let (label_w, label_h) = label_font.text_size(label);
    let (badge_w, badge_h) = badge_font.text_size(&badge);
    let total_w = label_w as i32 + 4 + badge_w as i32;
    let start_x = x + (CHIP_W - total_w) / 2;
    let label_y = y + (CHIP_H - label_h as i32) / 2 - 1;
    text(
        canvas,
        &label_font,
        label,
        start_x,
        label_y,
        palette.text_primary,
    );
    let badge_x = start_x + label_w as i32 + 4;
    let badge_y = y + (CHIP_H - badge_h as i32) / 2 - 1;
    text(
        canvas,
        &badge_font,
        &badge,
        badge_x,
        badge_y,
        palette.accent_primary,
    );
}

// ── metric formatters ───────────────────────────────────────────────

fn format_latency(value: Option<f64>) -> String {
    match value {
        Some(v) => format!("{} ms", v as i64),
        None => "--".to_string(),
    }
}

fn format_rssi(value: Option<f64>) -> String {
    match value {
        Some(v) => format!("{} dBm", v as i64),
        None => "--".to_string(),
    }
}

fn format_bitrate(value: Option<f64>) -> String {
    match value {
        Some(kbps) if kbps >= 1000.0 => format!("{:.1} Mbps", kbps / 1000.0),
        Some(kbps) => format!("{:.0} kbps", kbps),
        None => "--".to_string(),
    }
}

fn format_drops(value: Option<(i64, i64)>) -> String {
    match value {
        Some((lost, total)) => format!("{lost} / {total}"),
        None => "--".to_string(),
    }
}

fn format_channel(value: Option<i64>) -> String {
    match value {
        Some(v) if v > 0 => format!("ch{v}"),
        _ => "--".to_string(),
    }
}

fn format_mcs(value: Option<i64>) -> String {
    match value {
        Some(v) => format!("MCS{v}"),
        None => "--".to_string(),
    }
}

fn format_tx_power(value: Option<i64>) -> String {
    match value {
        Some(v) => format!("{v} dBm"),
        None => "--".to_string(),
    }
}

fn format_fps(value: Option<f64>) -> String {
    match value {
        Some(v) if v > 0.0 => format!("{:.1}", v),
        _ => "--".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graphics::palette::DARK;

    fn ctx_with_video(video: VideoCtx, link: LinkCtx) -> PageContext {
        PageContext {
            video,
            link,
            ..Default::default()
        }
    }

    #[test]
    fn video_renders_and_carries_tab_zones() {
        let page = VideoPage;
        let ctx = PageContext::default();
        let c = page.render(&ctx, &DARK);
        assert_eq!(c.width(), PANEL_W);
        // REC button + surface + metrics strip + five tabs = 8 (no cam chip
        // with a single camera).
        assert_eq!(page.hit_zones(&ctx).len(), 8);
        assert_eq!(page.id(), "video");
    }

    #[test]
    fn camera_chip_zone_appears_with_multiple_cameras() {
        let page = VideoPage;
        let ctx = ctx_with_video(
            VideoCtx {
                camera_count: 2,
                camera_label: Some("CAM 2".to_string()),
                ..Default::default()
            },
            LinkCtx::default(),
        );
        let zones = page.hit_zones(&ctx);
        // The cam chip zone joins the list when more than one camera exists.
        assert_eq!(zones.len(), 9);
        assert!(zones
            .iter()
            .any(|z| z.action == HitAction::Custom("video.cam_chip".to_string())));
    }

    #[test]
    fn metric_formatters_match_the_strip_copy() {
        assert_eq!(format_latency(Some(42.0)), "42 ms");
        assert_eq!(format_latency(None), "--");
        assert_eq!(format_rssi(Some(-71.0)), "-71 dBm");
        assert_eq!(format_bitrate(Some(8200.0)), "8.2 Mbps");
        assert_eq!(format_bitrate(Some(640.0)), "640 kbps");
        assert_eq!(format_drops(Some((3, 1247))), "3 / 1247");
        assert_eq!(format_drops(None), "--");
        assert_eq!(format_channel(Some(149)), "ch149");
        assert_eq!(format_channel(Some(0)), "--");
        assert_eq!(format_mcs(Some(1)), "MCS1");
        assert_eq!(format_tx_power(Some(10)), "10 dBm");
        assert_eq!(format_fps(Some(29.97)), "30.0");
        assert_eq!(format_fps(Some(0.0)), "--");
    }

    #[test]
    fn recording_chip_inks_the_error_color() {
        let page = VideoPage;
        let ctx = ctx_with_video(
            VideoCtx {
                recording: true,
                active: true,
                ..Default::default()
            },
            LinkCtx::default(),
        );
        let c = page.render(&ctx, &DARK);
        // Somewhere inside the REC chip the error red is painted when recording.
        let oy = CONTENT_Y as i32;
        let mut found = false;
        for y in (oy + 8)..(oy + 8 + CHIP_H) {
            for x in 8..(8 + CHIP_W) {
                if c.pixel(x, y) == DARK.status_error {
                    found = true;
                }
            }
        }
        assert!(found, "recording chip should paint the error color");
    }
}
