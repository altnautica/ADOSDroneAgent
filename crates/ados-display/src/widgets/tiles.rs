//! The bordered content tile, the headline big number and the caps field label.

use embedded_graphics::pixelcolor::Rgb888;

use crate::graphics::fonts::{FontFace, LoadedFont};
use crate::graphics::palette::Palette;
use crate::graphics::primitives::{fill_rect_outline, line, text, Canvas};

/// Height reserved for a tile's caps title row.
pub const TILE_TITLE_BAR_H: i32 = 18;
/// Horizontal padding inside a tile's title row.
pub const TILE_TITLE_PAD_X: i32 = 8;

/// The inner body box a tile reserves below its title bar:
/// `(body_x, body_y, body_w, body_h)`.
pub type TileBody = (i32, i32, i32, i32);
/// Paint a bordered content tile at `(x, y, w, h)` and return its inner body box.
///
/// The tile is a 1 px-bordered box filled with `bg_secondary`, an uppercase
/// muted title in the top `TILE_TITLE_BAR_H` px, an optional right-aligned mono
/// caption, and a 1 px separator under the title. The returned body box is the
/// region below the separator with 8 px side padding.
#[allow(clippy::too_many_arguments)]
pub fn draw_tile(
    canvas: &mut Canvas,
    palette: &Palette,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    title: &str,
    title_right: &str,
) -> TileBody {
    fill_rect_outline(
        canvas,
        x,
        y,
        x + w - 1,
        y + h - 1,
        palette.bg_secondary,
        palette.border_default,
    );
    let title_font = LoadedFont::new(FontFace::SansBold, 11);
    let upper = title.to_ascii_uppercase();
    text(
        canvas,
        &title_font,
        &upper,
        x + TILE_TITLE_PAD_X,
        y + 4,
        palette.text_secondary,
    );
    if !title_right.is_empty() {
        let right_font = LoadedFont::new(FontFace::MonoRegular, 11);
        let rw = right_font.text_size(title_right).0 as i32;
        text(
            canvas,
            &right_font,
            title_right,
            x + w - TILE_TITLE_PAD_X - rw,
            y + 4,
            palette.text_tertiary,
        );
    }
    let sep_y = y + TILE_TITLE_BAR_H;
    line(
        canvas,
        x + 1,
        sep_y,
        x + w - 2,
        sep_y,
        palette.border_default,
    );

    let body_x = x + 8;
    let body_y = sep_y + 2;
    let body_w = w - 16;
    let body_h = h - TILE_TITLE_BAR_H - 10;
    (body_x, body_y, body_w, body_h)
}

/// Paint a headline number with an optional unit suffix and return the total
/// width painted.
///
/// The value renders in Mono Bold at `size`; the unit renders smaller (one
/// third the size, floored at 10 px) baseline-aligned just to the right. An
/// empty unit string skips the suffix.
#[allow(clippy::too_many_arguments)]
pub fn draw_big_number(
    canvas: &mut Canvas,
    x: i32,
    y: i32,
    value_text: &str,
    color: Rgb888,
    size: u32,
    unit: &str,
    unit_color: Rgb888,
) -> i32 {
    let value_font = LoadedFont::new(FontFace::MonoBold, size);
    text(canvas, &value_font, value_text, x, y, color);
    let (value_w, value_h) = value_font.text_size(value_text);
    let mut total_w = value_w as i32;

    if !unit.is_empty() {
        let unit_px = (size / 3).max(10);
        let unit_font = LoadedFont::new(FontFace::SansBold, unit_px);
        let (unit_w, unit_h) = unit_font.text_size(unit);
        let unit_x = x + value_w as i32 + 4;
        let unit_y = y + value_h as i32 - unit_h as i32 - 2;
        text(canvas, &unit_font, unit, unit_x, unit_y, unit_color);
        total_w += 4 + unit_w as i32;
    }
    total_w
}

/// Paint a small caps label in muted tertiary text — the column / field label
/// the detail grids share.
pub fn draw_label(canvas: &mut Canvas, palette: &Palette, x: i32, y: i32, label: &str, size: u32) {
    let font = LoadedFont::new(FontFace::SansBold, size);
    text(
        canvas,
        &font,
        &label.to_ascii_uppercase(),
        x,
        y,
        palette.text_tertiary,
    );
}
