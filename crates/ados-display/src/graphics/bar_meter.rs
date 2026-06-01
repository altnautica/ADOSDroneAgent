//! Segmented horizontal bar meter.
//!
//! Renders `segments` filled-or-empty chips left to right. The fill fraction is
//! a measured value over a cap, clamped to 0..1 so a momentary overshoot does
//! not break the layout. The discrete chips (five by default) read faster from
//! across the bench than a continuous gradient fill would on a panel with no
//! anti-aliasing on the fill itself.
//!
//! Geometry matches the prior renderer: chip width is
//! `(w - (segments - 1) * gap) / segments`, chips are stepped by
//! `chip_w + gap`, and a chip is filled when its index is below
//! `round(fraction * segments)`.

use embedded_graphics::pixelcolor::Rgb888;

use super::primitives::{fill_rect, Canvas};

/// Default chip count for a bar meter.
pub const DEFAULT_SEGMENTS: u32 = 5;
/// Default inter-chip gap in pixels.
pub const DEFAULT_GAP: i32 = 2;

/// Paint a chipped bar at `(x, y, w, h)`.
///
/// `fraction` of `None` or below zero yields an all-empty bar; at or above one
/// yields all-filled. `fill_color` paints filled chips, `empty_color` the rest.
#[allow(clippy::too_many_arguments)]
pub fn draw_bar(
    canvas: &mut Canvas,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    fraction: Option<f64>,
    segments: u32,
    fill_color: Rgb888,
    empty_color: Rgb888,
    gap: i32,
) {
    if segments == 0 || w == 0 || h == 0 {
        return;
    }
    let frac = fraction.unwrap_or(0.0).clamp(0.0, 1.0);
    let filled_count = (frac * segments as f64).round() as u32;

    let chip_w = (w as f64 - ((segments - 1) as i32 * gap) as f64) / segments as f64;
    for i in 0..segments {
        let cx = x + ((i as f64) * (chip_w + gap as f64)).round() as i32;
        let c_w = (chip_w.round() as i32).max(1);
        let color = if i < filled_count {
            fill_color
        } else {
            empty_color
        };
        fill_rect(canvas, cx, y, cx + c_w - 1, y + h as i32 - 1, color);
    }
}

#[cfg(test)]
mod tests {
    use embedded_graphics::prelude::RgbColor;

    use super::*;

    fn chip_color_at(c: &Canvas, x: i32, y: i32) -> Rgb888 {
        c.pixel(x, y)
    }

    #[test]
    fn none_fraction_is_all_empty() {
        let mut c = Canvas::new(40, 8, Rgb888::BLACK);
        let fill = Rgb888::new(0x22, 0xC5, 0x5E);
        let empty = Rgb888::new(0x2A, 0x2A, 0x2A);
        draw_bar(&mut c, 0, 0, 40, 8, None, 5, fill, empty, 2);
        assert_eq!(chip_color_at(&c, 0, 0), empty);
    }

    #[test]
    fn full_fraction_fills_first_chip() {
        let mut c = Canvas::new(40, 8, Rgb888::BLACK);
        let fill = Rgb888::new(0x22, 0xC5, 0x5E);
        let empty = Rgb888::new(0x2A, 0x2A, 0x2A);
        draw_bar(&mut c, 0, 0, 40, 8, Some(1.0), 5, fill, empty, 2);
        assert_eq!(chip_color_at(&c, 0, 0), fill);
    }

    #[test]
    fn overshoot_clamps_to_full() {
        let mut c = Canvas::new(40, 8, Rgb888::BLACK);
        let fill = Rgb888::new(0x22, 0xC5, 0x5E);
        let empty = Rgb888::new(0x2A, 0x2A, 0x2A);
        draw_bar(&mut c, 0, 0, 40, 8, Some(5.0), 5, fill, empty, 2);
        // The last chip's left edge is filled.
        let chip_w: f64 = (40.0 - 4.0 * 2.0) / 5.0;
        let last_cx = (4.0 * (chip_w + 2.0)).round() as i32;
        assert_eq!(chip_color_at(&c, last_cx, 0), fill);
    }

    #[test]
    fn half_fraction_fills_lower_half() {
        let mut c = Canvas::new(50, 8, Rgb888::BLACK);
        let fill = Rgb888::new(0x22, 0xC5, 0x5E);
        let empty = Rgb888::new(0x2A, 0x2A, 0x2A);
        // 0.5 * 5 = 2.5 -> rounds to 3 filled chips (banker-free round-half-up).
        draw_bar(&mut c, 0, 0, 50, 8, Some(0.5), 5, fill, empty, 2);
        let chip_w: f64 = (50.0 - 4.0 * 2.0) / 5.0;
        // Chip index 2 (third) filled, index 3 (fourth) empty.
        let cx2 = (2.0 * (chip_w + 2.0)).round() as i32;
        let cx3 = (3.0 * (chip_w + 2.0)).round() as i32;
        assert_eq!(chip_color_at(&c, cx2, 0), fill);
        assert_eq!(chip_color_at(&c, cx3, 0), empty);
    }
}
