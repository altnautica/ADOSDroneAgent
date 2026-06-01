//! The in-memory RGB888 canvas + the small draw primitives the pages use.
//!
//! [`Canvas`] is a tightly-packed RGB888 pixel buffer (3 bytes per pixel,
//! row-major, top-left origin) that the pages paint into. It implements
//! `embedded_graphics`' `DrawTarget` so the shipped shape primitives (rect,
//! line, circle) draw onto it directly, and exposes [`Canvas::as_rgb888`] so
//! [`crate::pack`] can convert the finished frame to the panel's bit depth.
//!
//! The text path rasterizes a [`super::fonts::LoadedFont`] glyph-by-glyph and
//! alpha-blends each covered pixel over the existing canvas content, so
//! anti-aliased edges read against tile backgrounds the way the prior renderer
//! produced them. Fills, hairlines, and bordered boxes match the page math:
//! a filled box is a rectangle plus an optional 1 px outline, exactly the
//! `ImageDraw.rectangle(..., fill=, outline=, width=1)` the pages issued.

use embedded_graphics::pixelcolor::Rgb888;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{
    Circle, Line, PrimitiveStyle, PrimitiveStyleBuilder, Rectangle,
};

use super::fonts::LoadedFont;

/// A row-major RGB888 pixel buffer the pages paint onto.
pub struct Canvas {
    width: u32,
    height: u32,
    buf: Vec<u8>,
}

impl Canvas {
    /// Allocate a `width` x `height` canvas filled with `fill`.
    pub fn new(width: u32, height: u32, fill: Rgb888) -> Self {
        let mut canvas = Self {
            width,
            height,
            buf: vec![0u8; (width * height * 3) as usize],
        };
        canvas.clear_color(fill);
        canvas
    }

    /// Canvas width in pixels.
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Canvas height in pixels.
    pub fn height(&self) -> u32 {
        self.height
    }

    /// The finished frame as tightly-packed RGB888 bytes, ready for
    /// [`crate::pack::pack_for_bpp`].
    pub fn as_rgb888(&self) -> &[u8] {
        &self.buf
    }

    /// Paint every pixel `color`.
    pub fn clear_color(&mut self, color: Rgb888) {
        for px in self.buf.chunks_exact_mut(3) {
            px[0] = color.r();
            px[1] = color.g();
            px[2] = color.b();
        }
    }

    /// Read one pixel. Out-of-bounds reads return black.
    pub fn pixel(&self, x: i32, y: i32) -> Rgb888 {
        if x < 0 || y < 0 || x as u32 >= self.width || y as u32 >= self.height {
            return Rgb888::BLACK;
        }
        let i = ((y as u32 * self.width + x as u32) * 3) as usize;
        Rgb888::new(self.buf[i], self.buf[i + 1], self.buf[i + 2])
    }

    /// Write one pixel. Out-of-bounds writes are dropped (clip to canvas).
    pub fn put_pixel(&mut self, x: i32, y: i32, color: Rgb888) {
        if x < 0 || y < 0 || x as u32 >= self.width || y as u32 >= self.height {
            return;
        }
        let i = ((y as u32 * self.width + x as u32) * 3) as usize;
        self.buf[i] = color.r();
        self.buf[i + 1] = color.g();
        self.buf[i + 2] = color.b();
    }

    /// Blend `color` over the existing pixel at `coverage` (0..=1). Used by the
    /// text rasterizer for anti-aliased glyph edges.
    pub fn blend_pixel(&mut self, x: i32, y: i32, color: Rgb888, coverage: f32) {
        if coverage <= 0.0 {
            return;
        }
        if coverage >= 1.0 {
            self.put_pixel(x, y, color);
            return;
        }
        let bg = self.pixel(x, y);
        let mix = |fg: u8, bg: u8| -> u8 {
            (fg as f32 * coverage + bg as f32 * (1.0 - coverage)).round() as u8
        };
        let blended = Rgb888::new(
            mix(color.r(), bg.r()),
            mix(color.g(), bg.g()),
            mix(color.b(), bg.b()),
        );
        self.put_pixel(x, y, blended);
    }
}

impl Dimensions for Canvas {
    fn bounding_box(&self) -> Rectangle {
        Rectangle::new(Point::zero(), Size::new(self.width, self.height))
    }
}

impl DrawTarget for Canvas {
    type Color = Rgb888;
    type Error = core::convert::Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        for Pixel(point, color) in pixels {
            self.put_pixel(point.x, point.y, color);
        }
        Ok(())
    }
}

/// Fill the inclusive rectangle `(x0, y0)..=(x1, y1)` with `color`.
///
/// The pages address rectangles by their inclusive far corner
/// (`ImageDraw.rectangle((x, y, x + w - 1, y + h - 1))`); this helper takes the
/// same inclusive corners so call-site math is unchanged.
pub fn fill_rect(canvas: &mut Canvas, x0: i32, y0: i32, x1: i32, y1: i32, color: Rgb888) {
    let (lx, rx) = if x0 <= x1 { (x0, x1) } else { (x1, x0) };
    let (ty, by) = if y0 <= y1 { (y0, y1) } else { (y1, y0) };
    let w = (rx - lx + 1).max(0) as u32;
    let h = (by - ty + 1).max(0) as u32;
    if w == 0 || h == 0 {
        return;
    }
    let _ = Rectangle::new(Point::new(lx, ty), Size::new(w, h))
        .into_styled(PrimitiveStyle::with_fill(color))
        .draw(canvas);
}

/// Fill the inclusive rectangle and stroke a 1 px `border` outline, matching
/// the pages' bordered-box fill (`fill=` plus `outline=`, `width=1`).
pub fn fill_rect_outline(
    canvas: &mut Canvas,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    fill: Rgb888,
    border: Rgb888,
) {
    let (lx, rx) = if x0 <= x1 { (x0, x1) } else { (x1, x0) };
    let (ty, by) = if y0 <= y1 { (y0, y1) } else { (y1, y0) };
    let w = (rx - lx + 1).max(0) as u32;
    let h = (by - ty + 1).max(0) as u32;
    if w == 0 || h == 0 {
        return;
    }
    let style = PrimitiveStyleBuilder::new()
        .fill_color(fill)
        .stroke_color(border)
        .stroke_width(1)
        .build();
    let _ = Rectangle::new(Point::new(lx, ty), Size::new(w, h))
        .into_styled(style)
        .draw(canvas);
}

/// Draw a 1 px line from `(x0, y0)` to `(x1, y1)`.
pub fn line(canvas: &mut Canvas, x0: i32, y0: i32, x1: i32, y1: i32, color: Rgb888) {
    let _ = Line::new(Point::new(x0, y0), Point::new(x1, y1))
        .into_styled(PrimitiveStyle::with_stroke(color, 1))
        .draw(canvas);
}

/// Draw a filled circle of `radius` centered at `(cx, cy)`. An optional 1 px
/// `outline` strokes the edge (the pages use a hairline so a status dot pops
/// against a dark tile).
pub fn fill_circle(
    canvas: &mut Canvas,
    cx: i32,
    cy: i32,
    radius: i32,
    color: Rgb888,
    outline: Option<Rgb888>,
) {
    if radius < 0 {
        return;
    }
    let diameter = (radius * 2 + 1).max(1) as u32;
    let top_left = Point::new(cx - radius, cy - radius);
    let style = match outline {
        Some(o) => PrimitiveStyleBuilder::new()
            .fill_color(color)
            .stroke_color(o)
            .stroke_width(1)
            .build(),
        None => PrimitiveStyle::with_fill(color),
    };
    let _ = Circle::new(top_left, diameter)
        .into_styled(style)
        .draw(canvas);
}

/// Draw `text` with its top-left at `(x, y)` in `color`, anti-aliased over the
/// existing canvas content.
pub fn text(canvas: &mut Canvas, font: &LoadedFont, text: &str, x: i32, y: i32, color: Rgb888) {
    font.draw_text(text, x, y, |px, py, coverage| {
        canvas.blend_pixel(px, py, color, coverage);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_canvas_is_filled() {
        let c = Canvas::new(4, 2, Rgb888::new(0x10, 0x20, 0x30));
        assert_eq!(c.as_rgb888().len(), 4 * 2 * 3);
        assert_eq!(c.pixel(0, 0), Rgb888::new(0x10, 0x20, 0x30));
        assert_eq!(c.pixel(3, 1), Rgb888::new(0x10, 0x20, 0x30));
    }

    #[test]
    fn out_of_bounds_writes_are_clipped() {
        let mut c = Canvas::new(2, 2, Rgb888::BLACK);
        c.put_pixel(-1, 0, Rgb888::WHITE);
        c.put_pixel(0, 5, Rgb888::WHITE);
        c.put_pixel(2, 2, Rgb888::WHITE);
        // Nothing changed; every pixel still black.
        for y in 0..2 {
            for x in 0..2 {
                assert_eq!(c.pixel(x, y), Rgb888::BLACK);
            }
        }
    }

    #[test]
    fn fill_rect_uses_inclusive_corners() {
        let mut c = Canvas::new(5, 5, Rgb888::BLACK);
        // (1,1)..=(3,3) is a 3x3 block.
        fill_rect(&mut c, 1, 1, 3, 3, Rgb888::WHITE);
        assert_eq!(c.pixel(1, 1), Rgb888::WHITE);
        assert_eq!(c.pixel(3, 3), Rgb888::WHITE);
        assert_eq!(c.pixel(0, 0), Rgb888::BLACK);
        assert_eq!(c.pixel(4, 4), Rgb888::BLACK);
    }

    #[test]
    fn fill_rect_outline_strokes_border() {
        let mut c = Canvas::new(6, 6, Rgb888::BLACK);
        let fill = Rgb888::new(0x0A, 0x0A, 0x0A);
        let border = Rgb888::new(0x2A, 0x2A, 0x2A);
        fill_rect_outline(&mut c, 0, 0, 5, 5, fill, border);
        // Corner is the border, center is the fill.
        assert_eq!(c.pixel(0, 0), border);
        assert_eq!(c.pixel(3, 3), fill);
    }

    #[test]
    fn horizontal_line_paints_a_row() {
        let mut c = Canvas::new(5, 3, Rgb888::BLACK);
        line(&mut c, 0, 1, 4, 1, Rgb888::WHITE);
        for x in 0..5 {
            assert_eq!(c.pixel(x, 1), Rgb888::WHITE);
        }
        assert_eq!(c.pixel(0, 0), Rgb888::BLACK);
    }

    #[test]
    fn fill_circle_paints_the_center() {
        let mut c = Canvas::new(15, 15, Rgb888::BLACK);
        fill_circle(&mut c, 7, 7, 5, Rgb888::WHITE, None);
        assert_eq!(c.pixel(7, 7), Rgb888::WHITE);
        // A corner well outside the disc stays black.
        assert_eq!(c.pixel(0, 0), Rgb888::BLACK);
    }

    #[test]
    fn blend_half_coverage_is_a_midpoint() {
        let mut c = Canvas::new(1, 1, Rgb888::new(0, 0, 0));
        c.blend_pixel(0, 0, Rgb888::new(0xFF, 0xFF, 0xFF), 0.5);
        let p = c.pixel(0, 0);
        // 255 * 0.5 rounds to 128.
        assert_eq!(p, Rgb888::new(128, 128, 128));
    }

    #[test]
    fn text_inks_pixels_inside_canvas() {
        let mut c = Canvas::new(64, 32, Rgb888::BLACK);
        let font = LoadedFont::new(super::super::fonts::FontFace::SansBold, 20);
        text(&mut c, &font, "Hi", 2, 2, Rgb888::WHITE);
        // At least one pixel got lightened from pure black.
        let mut any = false;
        for y in 0..32 {
            for x in 0..64 {
                if c.pixel(x, y) != Rgb888::BLACK {
                    any = true;
                }
            }
        }
        assert!(any, "text should have inked at least one pixel");
    }
}
