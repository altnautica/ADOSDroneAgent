//! Drone detail page.
//!
//! The drilldown from the dashboard's drone tile. Two render paths:
//!
//! * **Paired** (`paired_drone.device_id` is present) — show the device id,
//!   key-fingerprint short form, paired-at relative time, and a 2-column grid
//!   covering vehicle / mode / armed / battery / GPS on the left and a battery
//!   graphic plus a 60-second battery sparkline on the right.
//! * **Unpaired** — show a NOT PAIRED banner, the pairing code, a QR of the
//!   pair URL, and an "Open pairing window" button.
//!
//! Live FC telemetry comes from [`PageContext::fc`]; the paired-drone identity
//! comes from [`PageContext::paired_drone`]; the unpaired code/URL come from
//! [`PageContext::cloud`] and [`PageContext::pairing`].

use crate::graphics::fonts::{FontFace, LoadedFont};
use crate::graphics::palette::Palette;
use crate::graphics::primitives::{fill_rect, fill_rect_outline, text, Canvas};
use crate::graphics::qr::render_qr;
use crate::graphics::sparkline::draw_sparkline;
use crate::pages::{blank_panel, HitAction, HitZone, Page, PageContext};
use crate::widgets::{draw_detail_header, DETAIL_HEADER_H};

/// Layout reference width of the detail-modal surface.
const PAGE_W: i32 = 480;
/// Header band height shared by every detail modal.
const HEADER_H: i32 = DETAIL_HEADER_H;

/// Format an elapsed-seconds count as a short relative-time string.
fn format_relative(seconds: Option<f64>) -> String {
    match seconds {
        Some(s) if s >= 0.0 => {
            if s < 60.0 {
                format!("{}s ago", s as i64)
            } else if s < 3600.0 {
                format!("{}m ago", (s / 60.0) as i64)
            } else if s < 86400.0 {
                format!("{}h ago", (s / 3600.0) as i64)
            } else {
                format!("{}d ago", (s / 86400.0) as i64)
            }
        }
        _ => "--".to_string(),
    }
}

/// The drone detail view, registered as `details.drone`.
pub struct DroneDetailPage;

impl Page for DroneDetailPage {
    fn id(&self) -> &'static str {
        "details.drone"
    }

    fn refresh_hz(&self) -> f32 {
        2.0
    }

    fn render(&self, ctx: &PageContext, palette: &Palette) -> Canvas {
        let mut canvas = blank_panel(palette);
        draw_detail_header(&mut canvas, palette, "Drone");

        let paired = ctx
            .paired_drone
            .device_id
            .as_deref()
            .map(|s| !s.is_empty())
            .unwrap_or(false);

        if paired {
            render_paired(&mut canvas, palette, ctx);
        } else {
            render_unpaired(&mut canvas, palette, ctx);
        }
        canvas
    }

    fn hit_zones(&self, ctx: &PageContext) -> Vec<HitZone> {
        let mut zones = vec![HitZone::new(8, 8, 40, 32, HitAction::Back)];
        let paired = ctx
            .paired_drone
            .device_id
            .as_deref()
            .map(|s| !s.is_empty())
            .unwrap_or(false);
        if !paired {
            zones.push(HitZone::new(
                140,
                188,
                200,
                40,
                HitAction::Custom("drone.open_pairing".to_string()),
            ));
        }
        zones
    }
}

/// Paint the paired body: identity rows + a 2-column telemetry grid.
fn render_paired(canvas: &mut Canvas, palette: &Palette, ctx: &PageContext) {
    let device_id = ctx.paired_drone.device_id.clone().unwrap_or_default();
    let fingerprint = ctx.paired_drone.key_fingerprint.clone().unwrap_or_default();

    // Identity rows y=48..96.
    let mono = LoadedFont::new(FontFace::MonoRegular, 11);
    text(
        canvas,
        &mono,
        &format!("id  {device_id}"),
        12,
        HEADER_H + 8,
        palette.text_primary,
    );
    let short = if !fingerprint.is_empty() {
        let head: String = fingerprint.chars().take(16).collect();
        if fingerprint.chars().count() > 16 {
            format!("{head}...")
        } else {
            head
        }
    } else {
        "--".to_string()
    };
    text(
        canvas,
        &mono,
        &format!("key {short}"),
        12,
        HEADER_H + 24,
        palette.text_secondary,
    );
    let ago_str = format_relative(ctx.paired_drone.paired_at_seconds);
    text(
        canvas,
        &mono,
        &format!("paired {ago_str}"),
        12,
        HEADER_H + 40,
        palette.text_tertiary,
    );

    // 2-column grid y=104..236.
    let col_left_x = 12;
    let col_right_x = 256;
    let grid_y = HEADER_H + 60;

    let fc = &ctx.fc;
    let vehicle = fc.vehicle.clone().unwrap_or_else(|| "--".to_string());
    let mode = fc.mode.clone().unwrap_or_else(|| "--".to_string());
    let armed = fc.armed;
    let bat_v = fc.battery_voltage;
    let bat_pct = fc.battery_remaining;
    let gps_fix = fc.gps_fix_type;
    let gps_sats = fc.gps_satellites_visible;

    let body_font = LoadedFont::new(FontFace::MonoRegular, 12);
    let body_label = LoadedFont::new(FontFace::SansBold, 10);

    text(
        canvas,
        &body_label,
        "VEHICLE",
        col_left_x,
        grid_y,
        palette.text_tertiary,
    );
    text(
        canvas,
        &body_font,
        &vehicle.to_ascii_uppercase(),
        col_left_x,
        grid_y + 14,
        palette.text_primary,
    );
    text(
        canvas,
        &body_font,
        &format!("mode  {mode}"),
        col_left_x,
        grid_y + 30,
        palette.text_secondary,
    );
    let (arm_label, arm_color) = if armed {
        ("ARMED", palette.status_success)
    } else {
        ("DISARMED", palette.text_secondary)
    };
    text(
        canvas,
        &body_font,
        arm_label,
        col_left_x,
        grid_y + 46,
        arm_color,
    );
    let bat_text = match (bat_v, bat_pct) {
        (Some(v), Some(p)) => format!("bat {v:.1}V  {}%", p as i64),
        _ => "bat --".to_string(),
    };
    text(
        canvas,
        &body_font,
        &bat_text,
        col_left_x,
        grid_y + 62,
        palette.text_secondary,
    );
    let gps_text = match (gps_fix, gps_sats) {
        (Some(fix), Some(sats)) => format!("gps {fix} · {sats} sats"),
        _ => "gps --".to_string(),
    };
    text(
        canvas,
        &body_font,
        &gps_text,
        col_left_x,
        grid_y + 78,
        palette.text_secondary,
    );

    // Right column: 60x60 battery box + cap nub + fill.
    let bat_x0 = col_right_x;
    let bat_y0 = grid_y;
    let bat_w = 60;
    let bat_h = 60;
    // 2 px outlined box: stroke the border twice, one ring in.
    for ring in 0..2 {
        fill_rect_outline(
            canvas,
            bat_x0 + ring,
            bat_y0 + ring,
            bat_x0 + bat_w - 1 - ring,
            bat_y0 + bat_h - 1 - ring,
            palette.bg_primary,
            palette.border_strong,
        );
    }
    // Battery cap nub on the right edge.
    fill_rect(
        canvas,
        bat_x0 + bat_w,
        bat_y0 + bat_h / 4,
        bat_x0 + bat_w + 6,
        bat_y0 + 3 * bat_h / 4,
        palette.border_strong,
    );
    if let Some(p) = bat_pct {
        let pct = (p as i64).clamp(0, 100);
        let fill_w = ((bat_w - 4) * pct as i32) / 100;
        let color = if pct >= 50 {
            palette.status_success
        } else if pct >= 20 {
            palette.status_warning
        } else {
            palette.status_error
        };
        if fill_w > 0 {
            fill_rect(
                canvas,
                bat_x0 + 2,
                bat_y0 + 2,
                bat_x0 + 2 + fill_w,
                bat_y0 + bat_h - 3,
                color,
            );
        }
    }

    // 60s battery sparkline to the right of the box, pinned 0..100.
    let spark_x = col_right_x + bat_w + 16;
    let spark_y = grid_y + 4;
    let spark_w = PAGE_W - spark_x - 12;
    let spark_h = bat_h - 8;
    if !fc.battery_history.is_empty() && spark_w > 0 {
        draw_sparkline(
            canvas,
            spark_x,
            spark_y,
            spark_w as u32,
            spark_h as u32,
            &fc.battery_history,
            palette.accent_primary,
            Some(0.0),
            Some(100.0),
        );
    }
}

/// Paint the unpaired body: NOT PAIRED banner + pair code + QR + button.
fn render_unpaired(canvas: &mut Canvas, palette: &Palette, ctx: &PageContext) {
    let code = ctx
        .cloud
        .pairing_code
        .clone()
        .or_else(|| ctx.cloud.pair_code.clone())
        .or_else(|| ctx.pairing.code.clone())
        .unwrap_or_default();

    let msg_font = LoadedFont::new(FontFace::SansBold, 14);
    let msg = "NOT PAIRED";
    let mw = msg_font.text_size(msg).0 as i32;
    text(
        canvas,
        &msg_font,
        msg,
        (PAGE_W - mw) / 2,
        HEADER_H + 8,
        palette.text_secondary,
    );

    if !code.is_empty() {
        let code_font = LoadedFont::new(FontFace::MonoBold, 22);
        let cw = code_font.text_size(&code).0 as i32;
        text(
            canvas,
            &code_font,
            &code,
            (PAGE_W - cw) / 2,
            HEADER_H + 32,
            palette.text_primary,
        );
        let qr_payload = ctx
            .cloud
            .pair_url
            .clone()
            .unwrap_or_else(|| format!("altnautica.com/command?pair={code}"));
        if let Some(qr) = render_qr(&qr_payload, 100, 2) {
            let qr_x = (PAGE_W - qr.size as i32) / 2;
            let qr_y = HEADER_H + 60;
            // Dark-on-light QR convention: paint dark modules in primary text so
            // the code reads bright against the dark ground.
            for py in 0..qr.size {
                for px in 0..qr.size {
                    if qr.is_dark(px, py) {
                        canvas.put_pixel(qr_x + px as i32, qr_y + py as i32, palette.text_primary);
                    }
                }
            }
        }
    }

    // Open pairing window button: 200x40 centered at y=188.
    let btn_w = 200;
    let btn_h = 40;
    let btn_x = (PAGE_W - btn_w) / 2;
    let btn_y = 188;
    fill_rect_outline(
        canvas,
        btn_x,
        btn_y,
        btn_x + btn_w - 1,
        btn_y + btn_h - 1,
        palette.accent_primary,
        palette.text_primary,
    );
    let btn_label = "Open pairing window";
    let btn_font = LoadedFont::new(FontFace::SansBold, 12);
    let (bw, bh) = btn_font.text_size(btn_label);
    text(
        canvas,
        &btn_font,
        btn_label,
        btn_x + (btn_w - bw as i32) / 2,
        btn_y + (btn_h - bh as i32) / 2 - 1,
        palette.text_primary,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graphics::palette::DARK;
    use crate::pages::PANEL_W;

    #[test]
    fn unpaired_has_open_pairing_zone() {
        let page = DroneDetailPage;
        let mut ctx = PageContext::default();
        ctx.cloud.pairing_code = Some("ABC123".to_string());
        let c = page.render(&ctx, &DARK);
        assert_eq!(c.width(), PANEL_W);
        let zones = page.hit_zones(&ctx);
        assert_eq!(zones.len(), 2);
        assert_eq!(zones[0].action, HitAction::Back);
        assert_eq!(
            zones[1].action,
            HitAction::Custom("drone.open_pairing".to_string())
        );
    }

    #[test]
    fn paired_drops_the_open_pairing_zone() {
        let page = DroneDetailPage;
        let mut ctx = PageContext::default();
        ctx.paired_drone.device_id = Some("ados-58c27faf".to_string());
        ctx.paired_drone.key_fingerprint = Some("0123456789abcdef0011".to_string());
        ctx.paired_drone.paired_at_seconds = Some(125.0);
        ctx.fc.vehicle = Some("quad".to_string());
        ctx.fc.mode = Some("LOITER".to_string());
        ctx.fc.armed = true;
        ctx.fc.battery_voltage = Some(16.4);
        ctx.fc.battery_remaining = Some(72.0);
        ctx.fc.gps_fix_type = Some(3);
        ctx.fc.gps_satellites_visible = Some(14);
        ctx.fc.battery_history = (0..60).map(|i| Some(50.0 + (i % 20) as f64)).collect();
        let c = page.render(&ctx, &DARK);
        assert_eq!(c.width(), PANEL_W);
        let zones = page.hit_zones(&ctx);
        assert_eq!(zones.len(), 1);
        assert_eq!(zones[0].action, HitAction::Back);
    }

    #[test]
    fn relative_time_buckets() {
        assert_eq!(format_relative(None), "--");
        assert_eq!(format_relative(Some(-1.0)), "--");
        assert_eq!(format_relative(Some(12.0)), "12s ago");
        assert_eq!(format_relative(Some(125.0)), "2m ago");
        assert_eq!(format_relative(Some(7200.0)), "2h ago");
        assert_eq!(format_relative(Some(172800.0)), "2d ago");
    }
}
