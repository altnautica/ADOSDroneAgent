//! Filled-circle status dot.
//!
//! A small filled disc whose only signal is its color, used for the header role
//! indicator and for in-tile mesh / uplink status. The three-color status
//! semantics map cleanly to success / warning / error, so no shape variation is
//! needed. A 1 px hairline outline in the background color makes the dot pop on
//! a dark tile.
//!
//! The default radius (7, a 14 px disc) matches the in-tile dot size; the
//! header role indicator passes a larger radius and the broadcasting pulse a
//! smaller one.

use embedded_graphics::pixelcolor::Rgb888;

use super::primitives::{fill_circle, Canvas};

/// Default in-tile dot radius — a 14 px disc.
pub const DEFAULT_RADIUS: i32 = 7;

/// Draw a filled disc of `radius` centered at `(cx, cy)` in `color`, with a
/// 1 px `outline` hairline (pass the background color so the dot pops on a dark
/// tile).
pub fn draw_dot(
    canvas: &mut Canvas,
    cx: i32,
    cy: i32,
    color: Rgb888,
    radius: i32,
    outline: Rgb888,
) {
    fill_circle(canvas, cx, cy, radius, color, Some(outline));
}

#[cfg(test)]
mod tests {
    use embedded_graphics::prelude::RgbColor;

    use super::*;

    #[test]
    fn dot_paints_its_center() {
        let mut c = Canvas::new(20, 20, Rgb888::BLACK);
        let color = Rgb888::new(0x22, 0xC5, 0x5E);
        draw_dot(&mut c, 10, 10, color, DEFAULT_RADIUS, Rgb888::BLACK);
        assert_eq!(c.pixel(10, 10), color);
    }

    #[test]
    fn dot_stays_within_its_radius() {
        let mut c = Canvas::new(40, 40, Rgb888::BLACK);
        let color = Rgb888::new(0xEF, 0x44, 0x44);
        draw_dot(&mut c, 20, 20, color, 5, Rgb888::BLACK);
        // A pixel two radii away from center is untouched.
        assert_eq!(c.pixel(20 + 12, 20), Rgb888::BLACK);
    }
}
