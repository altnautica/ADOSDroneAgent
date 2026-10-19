//! Storage page: capture root, capture volume free / total with a bar meter,
//! capture budget use, state chip, sessions pending upload and write speed.
//!
//! Values come from `/api/v1/storage`; an absent route reads "not present".

use embedded_graphics::pixelcolor::Rgb888;

use crate::graphics::bar_meter::{draw_bar, DEFAULT_GAP};
use crate::graphics::palette::Palette;
use crate::graphics::primitives::Canvas;
use crate::graphics::thresholds::DISK_PCT;
use crate::pages::sensing_ctx::{
    fmt_num, fmt_str, paint_absent, paint_frame, paint_label, paint_status, paint_value, row_y,
    DASH, VALUE_X,
};
use crate::pages::{blank_panel, Chrome, HitZone, Page, PageContext};

/// Bytes per GiB.
const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
/// Bar meter geometry.
const BAR_W: u32 = 200;
const BAR_H: u32 = 12;
const BAR_SEGMENTS: u32 = 10;

/// The storage surface, registered as `storage`.
#[derive(Debug, Default, Clone, Copy)]
pub struct StoragePage;

/// Colour for the capture state chip.
pub(crate) fn state_color(palette: &Palette, state: Option<&str>) -> Rgb888 {
    match state {
        Some("ok") => palette.status_success,
        Some("near_full") => palette.status_warning,
        Some("full") => palette.status_error,
        _ => palette.text_tertiary,
    }
}

impl Page for StoragePage {
    fn id(&self) -> &'static str {
        "storage"
    }

    fn chrome(&self) -> Chrome {
        Chrome::Tabbed
    }

    fn refresh_hz(&self) -> f32 {
        0.5
    }

    fn render(&self, ctx: &PageContext, palette: &Palette) -> Canvas {
        let mut canvas = blank_panel(palette);
        paint_frame(&mut canvas, ctx, palette, "STORAGE");
        let Some(s) = ctx.sensing.storage.as_ref() else {
            paint_absent(&mut canvas, palette, "Capture storage");
            return canvas;
        };
        let c = &mut canvas;
        let text = palette.text_primary;

        paint_label(c, palette, 0, "ROOT");
        paint_value(c, 0, VALUE_X, &fmt_str(s.capture_root.as_deref()), text);

        paint_label(c, palette, 1, "FREE");
        let free = format!(
            "{} / {} GiB",
            fmt_num(s.free_bytes.map(|b| b / GIB), 1),
            fmt_num(s.total_bytes.map(|b| b / GIB), 1)
        );
        paint_value(c, 1, VALUE_X, &free, text);
        let used_pct = match (s.free_bytes, s.total_bytes) {
            (Some(f), Some(t)) if t > 0.0 => Some((t - f) / t * 100.0),
            _ => None,
        };
        draw_bar(
            c,
            VALUE_X,
            row_y(2) + 2,
            BAR_W,
            BAR_H,
            used_pct.map(|p| p / 100.0),
            BAR_SEGMENTS,
            palette.grade(used_pct, DISK_PCT),
            palette.bg_tertiary,
            DEFAULT_GAP,
        );

        paint_label(c, palette, 3, "BUDGET");
        let pct = s.budget_use_pct();
        paint_status(
            c,
            3,
            VALUE_X,
            &format!("{} %", fmt_num(pct, 0)),
            palette.grade(pct, DISK_PCT),
        );

        paint_label(c, palette, 4, "STATE");
        let state = s.state.as_deref();
        paint_status(c, 4, VALUE_X, &fmt_str(state), state_color(palette, state));

        paint_label(c, palette, 5, "PENDING");
        let pending = s
            .pending_upload()
            .map_or_else(|| DASH.to_string(), |n| format!("{n} sessions"));
        paint_value(c, 5, VALUE_X, &pending, text);

        paint_label(c, palette, 6, "WRITE");
        paint_value(
            c,
            6,
            VALUE_X,
            &format!("{} MB/s", fmt_num(s.write_mbps, 0)),
            text,
        );
        canvas
    }

    fn hit_zones(&self, _ctx: &PageContext) -> Vec<HitZone> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graphics::palette::DARK;
    use crate::pages::sensing_ctx::test_util::{
        assert_renders_and_leaves_tab_band, full_ctx, row_has_color,
    };

    #[test]
    fn renders_full_and_empty_without_touching_the_tab_band() {
        assert_eq!(StoragePage.id(), "storage");
        assert_renders_and_leaves_tab_band(&StoragePage);
    }

    #[test]
    fn state_chip_follows_capture_state() {
        let mut ctx = full_ctx(1000.0);
        let c = StoragePage.render(&ctx, &DARK);
        assert!(row_has_color(&c, 4, VALUE_X, DARK.status_success));
        if let Some(s) = ctx.sensing.storage.as_mut() {
            s.state = Some("full".into());
        }
        let c = StoragePage.render(&ctx, &DARK);
        assert!(row_has_color(&c, 4, VALUE_X, DARK.status_error));
        assert_eq!(state_color(&DARK, Some("near_full")), DARK.status_warning);
    }

    #[test]
    fn pending_is_sessions_minus_uploaded() {
        let ctx = full_ctx(1000.0);
        let s = ctx.sensing.storage.as_ref().map(|s| s.pending_upload());
        assert_eq!(s, Some(Some(3)));
    }
}
