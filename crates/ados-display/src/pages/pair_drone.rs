//! Pair-drone detail page reachable from the overflow menu.
//!
//! Mirrors the drone-tile drilldown for the WFB radio pairing surface. Two
//! render paths:
//!
//! * **Paired** — when the paired-drone record carries a device id. Show the
//!   device id, the key-fingerprint short form, the paired-at relative time plus
//!   a short absolute clock, and a destructive "Unpair" button bottom-right.
//! * **Unpaired** — show a NOT PAIRED banner, the local pairing code, a QR of
//!   the pair URL on the right half, and an "Open pairing" accent button. While
//!   a pairing window is open the button is replaced by a countdown pill.
//!
//! The paired identity comes from [`PageContext::paired_drone`]; the unpaired
//! code/URL come from [`PageContext::pairing`] and [`PageContext::cloud`]; the
//! pairing-window countdown comes from [`PageContext::pairing`].

use crate::graphics::fonts::{FontFace, LoadedFont};
use crate::graphics::palette::Palette;
use crate::graphics::primitives::{fill_rect, fill_rect_outline, text, Canvas};
use crate::graphics::qr::render_qr;
use crate::pages::{blank_panel, HitAction, HitZone, Page, PageContext};
use crate::widgets::{draw_detail_header, DETAIL_HEADER_H};

/// Layout reference width of the detail-modal surface.
const PAGE_W: i32 = 480;
/// Layout reference height of the detail-modal surface.
const PAGE_H: i32 = 320;
/// Header band height shared by every detail modal.
const HEADER_H: i32 = DETAIL_HEADER_H;

/// Action-button geometry — bottom-right corner.
const BTN_W: i32 = 180;
const BTN_H: i32 = 40;
const BTN_RIGHT_PAD: i32 = 12;
const BTN_BOTTOM_PAD: i32 = 12;

/// Custom hit-zone keys the navigator routes to REST calls.
const UNPAIR_KEY: &str = "pair.unpair";
const OPEN_WINDOW_KEY: &str = "pair.open_window";

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

/// Format a unix timestamp as a short `HH:MM:SS` clock, or `--` on miss. The
/// hour-of-day derives from the seconds-since-epoch modulo a 24-hour day.
fn format_short_clock(timestamp: Option<f64>) -> String {
    match timestamp {
        Some(ts) if ts > 0.0 => {
            let total = ts as i64;
            let secs_of_day = total.rem_euclid(86400);
            let h = secs_of_day / 3600;
            let m = (secs_of_day % 3600) / 60;
            let s = secs_of_day % 60;
            format!("{h:02}:{m:02}:{s:02}")
        }
        _ => "--".to_string(),
    }
}

/// Build the fingerprint short form: head 12 chars, ellipsis, tail 4 chars when
/// the fingerprint is long; otherwise the full string, or `--` when absent.
fn fingerprint_short(fingerprint: &str) -> String {
    if fingerprint.is_empty() {
        return "--".to_string();
    }
    let count = fingerprint.chars().count();
    if count > 20 {
        let head: String = fingerprint.chars().take(12).collect();
        let tail: String = fingerprint.chars().skip(count.saturating_sub(4)).collect();
        format!("{head}\u{2026}{tail}")
    } else {
        fingerprint.to_string()
    }
}

/// True when the paired-drone record names a non-empty device id.
fn is_paired(ctx: &PageContext) -> bool {
    ctx.paired_drone
        .device_id
        .as_deref()
        .map(|s| !s.is_empty())
        .unwrap_or(false)
}

/// The pair-drone detail view, registered as `details.pair_drone`.
pub struct PairDroneDetailPage;

impl Page for PairDroneDetailPage {
    fn id(&self) -> &'static str {
        "details.pair_drone"
    }

    fn refresh_hz(&self) -> f32 {
        2.0
    }

    fn render(&self, ctx: &PageContext, palette: &Palette) -> Canvas {
        let mut canvas = blank_panel(palette);
        draw_detail_header(&mut canvas, palette, "Pair drone");

        if is_paired(ctx) {
            render_paired(&mut canvas, palette, ctx);
        } else {
            render_unpaired(&mut canvas, palette, ctx);
        }
        canvas
    }

    fn hit_zones(&self, ctx: &PageContext) -> Vec<HitZone> {
        let mut zones = vec![HitZone::new(8, 8, 40, 32, HitAction::Back)];
        let btn_x = PAGE_W - BTN_W - BTN_RIGHT_PAD;
        let btn_y = PAGE_H - BTN_H - BTN_BOTTOM_PAD;
        if is_paired(ctx) {
            zones.push(HitZone::new(
                btn_x,
                btn_y,
                BTN_W,
                BTN_H,
                HitAction::Custom(UNPAIR_KEY.to_string()),
            ));
        } else if !ctx.pairing.window_active {
            zones.push(HitZone::new(
                btn_x,
                btn_y,
                BTN_W,
                BTN_H,
                HitAction::Custom(OPEN_WINDOW_KEY.to_string()),
            ));
        }
        zones
    }
}

/// Paint the paired body: identity rows + a destructive Unpair button.
fn render_paired(canvas: &mut Canvas, palette: &Palette, ctx: &PageContext) {
    let mono = LoadedFont::new(FontFace::MonoRegular, 12);
    let label = LoadedFont::new(FontFace::SansBold, 11);

    let device_id = ctx.paired_drone.device_id.clone().unwrap_or_default();
    let mut cy = HEADER_H + 12;
    text(canvas, &label, "DEVICE ID", 16, cy, palette.text_tertiary);
    text(canvas, &mono, &device_id, 16, cy + 14, palette.text_primary);
    cy += 36;

    let fingerprint = ctx.paired_drone.key_fingerprint.clone().unwrap_or_default();
    let short = fingerprint_short(&fingerprint);
    text(canvas, &label, "KEY", 16, cy, palette.text_tertiary);
    text(canvas, &mono, &short, 16, cy + 14, palette.text_secondary);
    cy += 36;

    let rel = format_relative(ctx.paired_drone.paired_at_seconds);
    let absolute = format_short_clock(ctx.paired_drone.paired_at);
    text(canvas, &label, "PAIRED", 16, cy, palette.text_tertiary);
    text(
        canvas,
        &mono,
        &format!("{rel}  ({absolute})"),
        16,
        cy + 14,
        palette.text_secondary,
    );

    // Unpair button — bottom-right, destructive fill.
    let btn_x = PAGE_W - BTN_W - BTN_RIGHT_PAD;
    let btn_y = PAGE_H - BTN_H - BTN_BOTTOM_PAD;
    fill_rect(
        canvas,
        btn_x,
        btn_y,
        btn_x + BTN_W - 1,
        btn_y + BTN_H - 1,
        palette.status_error,
    );
    let btn_label = "Unpair";
    let btn_font = LoadedFont::new(FontFace::SansBold, 14);
    let (bw, bh) = btn_font.text_size(btn_label);
    text(
        canvas,
        &btn_font,
        btn_label,
        btn_x + (BTN_W - bw as i32) / 2,
        btn_y + (BTN_H - bh as i32) / 2 - 1,
        palette.text_primary,
    );
}

/// Paint the unpaired body: NOT PAIRED banner + code + QR + button/countdown.
fn render_unpaired(canvas: &mut Canvas, palette: &Palette, ctx: &PageContext) {
    let code = ctx
        .pairing
        .code
        .clone()
        .or_else(|| ctx.cloud.pairing_code.clone())
        .or_else(|| ctx.cloud.pair_code.clone())
        .unwrap_or_default();

    let msg_font = LoadedFont::new(FontFace::SansBold, 13);
    text(
        canvas,
        &msg_font,
        "NOT PAIRED",
        16,
        HEADER_H + 8,
        palette.text_secondary,
    );

    let code_font = LoadedFont::new(FontFace::MonoBold, 22);
    let code_text = if code.is_empty() {
        "------".to_string()
    } else {
        code.clone()
    };
    text(
        canvas,
        &code_font,
        &code_text,
        16,
        HEADER_H + 28,
        palette.text_primary,
    );

    // 100 px QR pinned to the right half, mirroring the paste anchor.
    if !code.is_empty() {
        let qr_payload = ctx
            .pairing
            .pair_url
            .clone()
            .or_else(|| ctx.cloud.pair_url.clone())
            .unwrap_or_else(|| format!("altnautica.com/command?pair={code}"));
        if let Some(qr) = render_qr(&qr_payload, 100, 2) {
            let qr_x = PAGE_W - qr.size as i32 - 24;
            let qr_y = HEADER_H + 8;
            for py in 0..qr.size {
                for px in 0..qr.size {
                    if qr.is_dark(px, py) {
                        canvas.put_pixel(qr_x + px as i32, qr_y + py as i32, palette.text_primary);
                    }
                }
            }
        }
    }

    let btn_x = PAGE_W - BTN_W - BTN_RIGHT_PAD;
    let btn_y = PAGE_H - BTN_H - BTN_BOTTOM_PAD;

    if ctx.pairing.window_active {
        // Countdown pill where the button would normally sit.
        let secs = ctx
            .pairing
            .window_remaining_seconds
            .map(|v| v.max(0.0) as i64)
            .unwrap_or(0);
        let mins = secs / 60;
        let rem = secs % 60;
        let countdown = format!("Open \u{b7} {mins}:{rem:02} left");
        for ring in 0..2 {
            fill_rect_outline(
                canvas,
                btn_x + ring,
                btn_y + ring,
                btn_x + BTN_W - 1 - ring,
                btn_y + BTN_H - 1 - ring,
                palette.bg_secondary,
                palette.accent_primary,
            );
        }
        let cf = LoadedFont::new(FontFace::SansBold, 13);
        let (cw, ch) = cf.text_size(&countdown);
        text(
            canvas,
            &cf,
            &countdown,
            btn_x + (BTN_W - cw as i32) / 2,
            btn_y + (BTN_H - ch as i32) / 2 - 1,
            palette.accent_primary,
        );
        return;
    }

    // Idle — the call-to-action button.
    fill_rect(
        canvas,
        btn_x,
        btn_y,
        btn_x + BTN_W - 1,
        btn_y + BTN_H - 1,
        palette.accent_primary,
    );
    let btn_label = "Open pairing";
    let btn_font = LoadedFont::new(FontFace::SansBold, 14);
    let (bw, bh) = btn_font.text_size(btn_label);
    text(
        canvas,
        &btn_font,
        btn_label,
        btn_x + (BTN_W - bw as i32) / 2,
        btn_y + (BTN_H - bh as i32) / 2 - 1,
        palette.text_primary,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graphics::palette::DARK;
    use crate::pages::PANEL_W;

    #[test]
    fn pair_drone_renders_with_back_zone() {
        let page = PairDroneDetailPage;
        let ctx = PageContext::default();
        let c = page.render(&ctx, &DARK);
        assert_eq!(c.width(), PANEL_W);
        let zones = page.hit_zones(&ctx);
        assert_eq!(zones.len(), 2);
        assert_eq!(zones[0].action, HitAction::Back);
    }

    #[test]
    fn unpaired_idle_exposes_open_window_zone() {
        let page = PairDroneDetailPage;
        let mut ctx = PageContext::default();
        ctx.pairing.code = Some("ABC123".to_string());
        let c = page.render(&ctx, &DARK);
        assert_eq!(c.width(), PANEL_W);
        let zones = page.hit_zones(&ctx);
        assert_eq!(zones.len(), 2);
        assert_eq!(
            zones[1].action,
            HitAction::Custom("pair.open_window".to_string())
        );
    }

    #[test]
    fn open_window_active_drops_the_button_zone() {
        let page = PairDroneDetailPage;
        let mut ctx = PageContext::default();
        ctx.pairing.code = Some("ABC123".to_string());
        ctx.pairing.window_active = true;
        ctx.pairing.window_remaining_seconds = Some(95.0);
        let c = page.render(&ctx, &DARK);
        assert_eq!(c.width(), PANEL_W);
        let zones = page.hit_zones(&ctx);
        assert_eq!(zones.len(), 1);
        assert_eq!(zones[0].action, HitAction::Back);
    }

    #[test]
    fn paired_exposes_unpair_zone() {
        let page = PairDroneDetailPage;
        let mut ctx = PageContext::default();
        ctx.paired_drone.device_id = Some("ados-58c27faf".to_string());
        ctx.paired_drone.key_fingerprint = Some("0123456789abcdef0011aabbcc".to_string());
        ctx.paired_drone.paired_at_seconds = Some(125.0);
        ctx.paired_drone.paired_at = Some(1_700_000_000.0);
        let c = page.render(&ctx, &DARK);
        assert_eq!(c.width(), PANEL_W);
        let zones = page.hit_zones(&ctx);
        assert_eq!(zones.len(), 2);
        assert_eq!(
            zones[1].action,
            HitAction::Custom("pair.unpair".to_string())
        );
    }

    #[test]
    fn fingerprint_short_form_buckets() {
        assert_eq!(fingerprint_short(""), "--");
        assert_eq!(fingerprint_short("0123456789ab"), "0123456789ab");
        // Long fingerprint collapses to head 12 + ellipsis + tail 4.
        assert_eq!(
            fingerprint_short("0123456789abcdef0011aabbcc"),
            "0123456789ab\u{2026}bbcc"
        );
    }

    #[test]
    fn short_clock_formats_seconds_of_day() {
        // 01:00:00 past a day boundary.
        assert_eq!(format_short_clock(Some(90000.0)), "01:00:00");
        assert_eq!(format_short_clock(None), "--");
        assert_eq!(format_short_clock(Some(0.0)), "--");
    }

    #[test]
    fn relative_time_buckets() {
        assert_eq!(format_relative(None), "--");
        assert_eq!(format_relative(Some(12.0)), "12s ago");
        assert_eq!(format_relative(Some(125.0)), "2m ago");
        assert_eq!(format_relative(Some(7200.0)), "2h ago");
        assert_eq!(format_relative(Some(172800.0)), "2d ago");
    }
}
