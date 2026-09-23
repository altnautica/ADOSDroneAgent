//! Pair-drone detail page reachable from the overflow menu.
//!
//! Mirrors the drone-tile drilldown for the WFB radio pairing surface. Two
//! render paths:
//!
//! * **Paired** — when the paired-drone record carries a device id. Show the
//!   device id, the key-fingerprint short form, the paired-at relative time plus
//!   a short absolute clock, and a destructive "Unpair" button bottom-right.
//! * **Unpaired** — show a NOT PAIRED banner and the WFB auto-pair state: armed
//!   (a powered, unpaired drone in radio range pairs on its own), disarmed, or
//!   unknown when the pair read did not answer.
//!
//! Radio pairing uses no code. The six-character code the operator types into
//! Mission Control claims the node in the cloud, a different flow, so it lives
//! on the dashboard and drone surfaces, never here.
//!
//! The paired identity and the auto-pair flag both come from
//! [`PageContext::paired_drone`].

use crate::graphics::fonts::{FontFace, LoadedFont};
use crate::graphics::palette::Palette;
use crate::graphics::primitives::{fill_rect, text, Canvas};
use crate::pages::{
    blank_panel, AgentRequest, Chrome, HitAction, HitZone, Page, PageContext, PanelAction,
};
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

/// The Unpair button's custom key: `DELETE /api/v1/ground-station/wfb/pair`.
const UNPAIR_KEY: &str = "pair.unpair";

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

    fn chrome(&self) -> Chrome {
        Chrome::FullScreen
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
        }
        zones
    }

    fn on_custom(&self, key: &str, ctx: &PageContext) -> Option<PanelAction> {
        (key == UNPAIR_KEY && is_paired(ctx)).then(|| {
            PanelAction::Agent(AgentRequest {
                method: "DELETE",
                path: "/api/v1/ground-station/wfb/pair",
                body: None,
                label: "Unpair".to_string(),
            })
        })
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

/// The auto-pair row value and the operator hint under it for an unpaired
/// station, from the WFB auto-pair flag alone.
fn auto_pair_lines(auto_pair_enabled: Option<bool>) -> (&'static str, &'static str) {
    match auto_pair_enabled {
        Some(true) => ("ON", "Power on an unpaired drone within radio range."),
        Some(false) => ("OFF", "Turn auto-pair on or pair from Mission Control."),
        None => ("--", "Radio pairing state unavailable."),
    }
}

/// Paint the unpaired body: NOT PAIRED banner + the WFB auto-pair state.
fn render_unpaired(canvas: &mut Canvas, palette: &Palette, ctx: &PageContext) {
    let msg_font = LoadedFont::new(FontFace::SansBold, 13);
    text(
        canvas,
        &msg_font,
        "NOT PAIRED",
        16,
        HEADER_H + 8,
        palette.text_secondary,
    );

    let (state, hint) = auto_pair_lines(ctx.paired_drone.auto_pair_enabled);
    let label = LoadedFont::new(FontFace::SansBold, 11);
    let value_font = LoadedFont::new(FontFace::MonoBold, 22);
    let hint_font = LoadedFont::new(FontFace::SansRegular, 12);
    let cy = HEADER_H + 36;
    text(canvas, &label, "AUTO-PAIR", 16, cy, palette.text_tertiary);
    text(
        canvas,
        &value_font,
        state,
        16,
        cy + 14,
        palette.text_primary,
    );
    text(
        canvas,
        &hint_font,
        hint,
        16,
        cy + 48,
        palette.text_secondary,
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
        assert_eq!(zones.len(), 1, "unpaired: no control without an endpoint");
        assert_eq!(zones[0].action, HitAction::Back);
    }

    #[test]
    fn unpaired_page_never_shows_the_cloud_claim_code() {
        // The claim code belongs to the Mission Control claim flow; radio
        // pairing uses none, so its presence must not change a single pixel.
        let page = PairDroneDetailPage;
        let mut without = PageContext::default();
        without.paired_drone.auto_pair_enabled = Some(true);
        let mut with = without.clone();
        with.pairing.code = Some("ABC123".to_string());
        with.cloud.pair_code = Some("ABC123".to_string());
        assert_eq!(
            page.render(&with, &DARK).as_rgb888(),
            page.render(&without, &DARK).as_rgb888()
        );
        assert_eq!(page.hit_zones(&with).len(), 1);
    }

    #[test]
    fn auto_pair_state_reads_only_the_radio_flag() {
        assert_eq!(auto_pair_lines(Some(true)).0, "ON");
        assert_eq!(auto_pair_lines(Some(false)).0, "OFF");
        assert_eq!(auto_pair_lines(None).0, "--");
        let page = PairDroneDetailPage;
        let mut on = PageContext::default();
        on.paired_drone.auto_pair_enabled = Some(true);
        let mut off = on.clone();
        off.paired_drone.auto_pair_enabled = Some(false);
        assert_ne!(
            page.render(&on, &DARK).as_rgb888(),
            page.render(&off, &DARK).as_rgb888()
        );
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
        // The tap is the station-wide unpair route.
        let Some(PanelAction::Agent(req)) = page.on_custom("pair.unpair", &ctx) else {
            panic!("unpair must reach the agent");
        };
        assert_eq!(
            (req.method, req.path),
            ("DELETE", "/api/v1/ground-station/wfb/pair")
        );
        // Unpaired, the key does nothing.
        assert!(page
            .on_custom("pair.unpair", &PageContext::default())
            .is_none());
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
