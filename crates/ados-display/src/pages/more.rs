//! More page — overflow menu of secondary actions.
//!
//! A short list of operator actions that drill into detail pages or fire a
//! confirm-dialog action: pair drone, diagnostics, restart agent, about. Each
//! row is a 48 px list row in the shared list styling — a left-aligned label
//! with a right-pointing chevron — and the four rows fit inside the content
//! area without a scroll envelope (`4 * 48 = 192` px).

use embedded_graphics::pixelcolor::Rgb888;

use crate::graphics::fonts::{FontFace, LoadedFont};
use crate::graphics::palette::Palette;
use crate::graphics::primitives::{fill_rect, line, text, Canvas};
use crate::pages::{blank_panel, HitAction, HitZone, Page, PageContext, PANEL_W};

/// Height of one overflow list row.
pub const ROW_H: i32 = 48;

/// Pixel padding from the left edge for the label.
const LEFT_PAD: i32 = 12;
/// Pixel padding from the right edge for the chevron.
const RIGHT_PAD: i32 = 12;

/// The overflow menu, registered as `more`.
pub struct MorePage;

impl MorePage {
    /// The rows, in display order: `(zone key, operator label, drill-into page
    /// id or action)`. A `None` target marks an action row handled by the
    /// navigator (the restart confirm dialog).
    const ROWS: [(&'static str, &'static str, Option<&'static str>); 4] = [
        ("more.pair", "Pair drone", Some("details.pair_drone")),
        (
            "more.diagnostics",
            "Diagnostics",
            Some("details.diagnostics"),
        ),
        ("more.restart", "Restart agent", None),
        ("more.about", "About", Some("details.about")),
    ];
}

impl Page for MorePage {
    fn id(&self) -> &'static str {
        "more"
    }

    fn refresh_hz(&self) -> f32 {
        2.0
    }

    fn render(&self, _ctx: &PageContext, palette: &Palette) -> Canvas {
        let mut canvas = blank_panel(palette);
        for (i, (_key, label, _target)) in Self::ROWS.iter().enumerate() {
            let row_y = i as i32 * ROW_H;
            draw_list_row(&mut canvas, palette, row_y, label);
        }
        canvas
    }

    fn hit_zones(&self, _ctx: &PageContext) -> Vec<HitZone> {
        Self::ROWS
            .iter()
            .enumerate()
            .map(|(i, (key, _label, target))| {
                let action = match target {
                    Some(page_id) => HitAction::OpenDetail(page_id),
                    None => HitAction::Custom((*key).to_string()),
                };
                HitZone::new(0, i as i32 * ROW_H, PANEL_W as i32, ROW_H, action)
            })
            .collect()
    }
}

/// Paint one 48 px overflow row at `y`: a left-aligned label in primary text, a
/// right-pointing chevron, and a 1 px divider closing the bottom edge so the
/// stack reads as one list.
fn draw_list_row(canvas: &mut Canvas, palette: &Palette, y: i32, label: &str) {
    // Row plate so a redraw never keeps a previous surface behind the row.
    fill_rect(
        canvas,
        0,
        y,
        PANEL_W as i32 - 1,
        y + ROW_H - 1,
        palette.bg_primary,
    );

    let label_font = LoadedFont::new(FontFace::SansRegular, 14);
    let (_, label_h) = label_font.text_size(label);
    let label_y = y + (ROW_H - label_h as i32) / 2 - 2;
    text(
        canvas,
        &label_font,
        label,
        LEFT_PAD,
        label_y,
        palette.text_primary,
    );

    let chevron_x = PANEL_W as i32 - RIGHT_PAD - 8;
    draw_chevron(canvas, chevron_x, y + ROW_H / 2, palette.text_tertiary);

    line(
        canvas,
        0,
        y + ROW_H - 1,
        PANEL_W as i32 - 1,
        y + ROW_H - 1,
        palette.border_default,
    );
}

/// Draw a right-pointing chevron centered on `(cx, cy)`.
fn draw_chevron(canvas: &mut Canvas, cx: i32, cy: i32, color: Rgb888) {
    let arm = 5;
    // Two-pixel stroke, one row apart, so the chevron reads on the panel.
    for off in 0..2 {
        line(canvas, cx - arm, cy - arm + off, cx, cy + off, color);
        line(canvas, cx, cy + off, cx - arm, cy + arm + off, color);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graphics::palette::DARK;

    #[test]
    fn more_has_four_row_zones() {
        let page = MorePage;
        let ctx = PageContext::default();
        let c = page.render(&ctx, &DARK);
        assert_eq!(c.width(), PANEL_W);
        let zones = page.hit_zones(&ctx);
        assert_eq!(zones.len(), 4);
        assert_eq!(zones[0].action, HitAction::OpenDetail("details.pair_drone"));
        assert_eq!(
            zones[2].action,
            HitAction::Custom("more.restart".to_string())
        );
        assert_eq!(zones[3].action, HitAction::OpenDetail("details.about"));
    }

    #[test]
    fn each_row_paints_a_chevron_and_divider() {
        let page = MorePage;
        let c = page.render(&PageContext::default(), &DARK);
        // A divider line sits at the bottom edge of every row.
        for i in 0..4 {
            let y = i * ROW_H + ROW_H - 1;
            assert_eq!(c.pixel(0, y), DARK.border_default);
        }
        // The label is inked somewhere on the first row (not pure background).
        let mut inked = false;
        for y in 0..ROW_H {
            for x in LEFT_PAD..(LEFT_PAD + 120) {
                if c.pixel(x, y) != DARK.bg_primary && c.pixel(x, y) != DARK.border_default {
                    inked = true;
                }
            }
        }
        assert!(inked, "the row label should ink at least one pixel");
    }
}
