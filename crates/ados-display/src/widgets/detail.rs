//! Detail-page header band, its back-chevron zone, and the action ack line.

use crate::graphics::fonts::{FontFace, LoadedFont};
use crate::graphics::palette::Palette;
use crate::graphics::primitives::{fill_rect, line, text, Canvas};
use crate::pages::{HitAction, HitZone, PANEL_W};

/// Height of a detail page's header band.
pub const DETAIL_HEADER_H: i32 = 40;

/// Paint a detail page's header band: a left-pointing back chevron, a centered
/// uppercase title (Sans Bold 14), and a 1 px divider under the band. Returns
/// the back-chevron hit zone, in panel coordinates (a detail page is
/// [`crate::pages::Chrome::FullScreen`]).
///
/// The chevron is two 2 px diagonal strokes meeting at a left-pointing apex
/// inside a 40x32 box at `(8, 8)` — the same geometry every detail page shares.
pub fn draw_detail_header(canvas: &mut Canvas, palette: &Palette, title: &str) -> HitZone {
    let (bx, by, bw, bh) = (8, 8, 40, 32);
    let cx = bx + bw / 2;
    let cy = by + bh / 2;
    let arm = 8;
    let color = palette.text_primary;
    // Two-pixel chevron: stroke the line twice, one row apart.
    for off in 0..2 {
        line(canvas, cx - arm, cy + off, cx + arm, cy - arm + off, color);
        line(canvas, cx - arm, cy + off, cx + arm, cy + arm + off, color);
    }

    let font = LoadedFont::new(FontFace::SansBold, 14);
    let upper = title.to_ascii_uppercase();
    let tw = font.text_size(&upper).0 as i32;
    text(
        canvas,
        &font,
        &upper,
        (PANEL_W as i32 - tw) / 2,
        12,
        palette.text_primary,
    );

    line(
        canvas,
        0,
        DETAIL_HEADER_H,
        PANEL_W as i32 - 1,
        DETAIL_HEADER_H,
        palette.border_default,
    );

    HitZone::new(bx, by, bw, bh, HitAction::Back)
}

/// The detail-page back-chevron hit zone, without painting, in panel
/// coordinates. Detail page `hit_zones` implementations call this so the zone
/// stays in sync with the one [`draw_detail_header`] paints.
pub fn detail_back_zone() -> HitZone {
    HitZone::new(8, 8, 40, 32, HitAction::Back)
}

/// Height of the acknowledgement line a panel action paints.
pub const ACK_LINE_H: i32 = 24;

/// Paint the one-line outcome of a panel action across the bottom of the panel:
/// the success tint for an acknowledgement, the error tint for a failure.
pub fn draw_ack_line(canvas: &mut Canvas, palette: &Palette, message: &str, ok: bool) {
    let h = canvas.height() as i32;
    let y0 = h - ACK_LINE_H;
    let bg = if ok {
        palette.status_success
    } else {
        palette.status_error
    };
    fill_rect(canvas, 0, y0, PANEL_W as i32 - 1, h - 1, bg);
    let font = LoadedFont::new(FontFace::SansBold, 12);
    let (_, th) = font.text_size(message);
    text(
        canvas,
        &font,
        message,
        8,
        y0 + (ACK_LINE_H - th as i32) / 2 - 1,
        palette.bg_primary,
    );
}
