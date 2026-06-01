//! Embedded TrueType faces + glyph rasterizer for the LCD pages.
//!
//! The page layout was tuned against the DejaVu family at fixed pixel sizes
//! (the title caps at 11 px, the headline numbers at 32 px, the clock at
//! 14 px, and so on). To keep a render pixel-identical the same four faces are
//! embedded here and rasterized with the same pixel-size convention: a TrueType
//! pixel size, not a point size at a DPI.
//!
//! Four faces are bundled, matching the names the pages ask for:
//!
//! * [`FontFace::SansRegular`] / [`FontFace::SansBold`] — proportional body and
//!   bold display text (hostname, role label, tile titles).
//! * [`FontFace::MonoRegular`] / [`FontFace::MonoBold`] — fixed-width numeric
//!   and metric text (clock, RSSI value, footer counters).
//!
//! Rasterization draws each glyph's coverage onto the canvas via a per-pixel
//! callback, alpha-blending the text color over whatever is already painted.
//! Metrics (`text_width` / `text_height`) report the same tight bounding box
//! the page math expects so right-anchored and centered text lands correctly.

use ab_glyph::{Font, FontRef, Glyph, ScaleFont};

/// The four bundled DejaVu faces. These are the faces the page layout was
/// measured against; the embedded bytes are the upstream DejaVu TrueType files.
const SANS_REGULAR_TTF: &[u8] = include_bytes!("../../assets/fonts/DejaVuSans.ttf");
const SANS_BOLD_TTF: &[u8] = include_bytes!("../../assets/fonts/DejaVuSans-Bold.ttf");
const MONO_REGULAR_TTF: &[u8] = include_bytes!("../../assets/fonts/DejaVuSansMono.ttf");
const MONO_BOLD_TTF: &[u8] = include_bytes!("../../assets/fonts/DejaVuSansMono-Bold.ttf");

/// Which embedded face to render with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FontFace {
    SansRegular,
    SansBold,
    MonoRegular,
    MonoBold,
}

impl FontFace {
    /// The raw TrueType bytes for this face.
    fn ttf(self) -> &'static [u8] {
        match self {
            FontFace::SansRegular => SANS_REGULAR_TTF,
            FontFace::SansBold => SANS_BOLD_TTF,
            FontFace::MonoRegular => MONO_REGULAR_TTF,
            FontFace::MonoBold => MONO_BOLD_TTF,
        }
    }
}

/// A face parsed and held at a fixed pixel size, ready to rasterize. Build one
/// per (face, size) pair; the page layer caches these the way the prior
/// renderer cached its font handles.
pub struct LoadedFont {
    font: FontRef<'static>,
    px: f32,
}

impl LoadedFont {
    /// Parse `face` and pin it at `px` pixels. The pixel size is the TrueType
    /// em size in device pixels, matching the page layout's size convention.
    pub fn new(face: FontFace, px: u32) -> Self {
        // The bundled bytes are valid TrueType; a parse failure would be a
        // build-time corruption of the embedded asset, which the test suite
        // catches. Treat it as unrecoverable rather than silently substituting
        // a different face that would shift every metric.
        let font = FontRef::try_from_slice(face.ttf()).expect("embedded DejaVu face must parse");
        Self {
            font,
            px: px as f32,
        }
    }

    /// The line height (ascent + descent + line gap) in pixels for this size.
    pub fn line_height(&self) -> u32 {
        let sf = self.font.as_scaled(self.px);
        (sf.ascent() - sf.descent() + sf.line_gap()).ceil().max(0.0) as u32
    }

    /// Pixel advance of `text` — the sum of per-glyph horizontal advances with
    /// kerning. This is the layout width used to right-anchor and center text.
    pub fn text_advance(&self, text: &str) -> u32 {
        let sf = self.font.as_scaled(self.px);
        let mut width = 0.0_f32;
        let mut prev: Option<ab_glyph::GlyphId> = None;
        for ch in text.chars() {
            let id = self.font.glyph_id(ch);
            if let Some(p) = prev {
                width += sf.kern(p, id);
            }
            width += sf.h_advance(id);
            prev = Some(id);
        }
        width.ceil().max(0.0) as u32
    }

    /// Tight bounding-box width and height of `text`, matching the prior
    /// renderer's `getbbox`-derived `text_size`: the span of inked pixels, not
    /// the advance. Empty text and whitespace-only text measure to a height
    /// from the font ascent so vertical layout stays stable.
    pub fn text_size(&self, text: &str) -> (u32, u32) {
        let sf = self.font.as_scaled(self.px);
        let mut min_x = f32::MAX;
        let mut max_x = f32::MIN;
        let mut min_y = f32::MAX;
        let mut max_y = f32::MIN;
        let mut caret = 0.0_f32;
        let mut prev: Option<ab_glyph::GlyphId> = None;
        let mut inked = false;
        for ch in text.chars() {
            let id = self.font.glyph_id(ch);
            if let Some(p) = prev {
                caret += sf.kern(p, id);
            }
            let glyph: Glyph = id.with_scale_and_position(self.px, ab_glyph::point(caret, 0.0));
            if let Some(outline) = self.font.outline_glyph(glyph) {
                let bb = outline.px_bounds();
                min_x = min_x.min(bb.min.x);
                max_x = max_x.max(bb.max.x);
                min_y = min_y.min(bb.min.y);
                max_y = max_y.max(bb.max.y);
                inked = true;
            }
            caret += sf.h_advance(id);
            prev = Some(id);
        }
        if !inked {
            // No inked glyphs (empty / whitespace): width is the advance, the
            // height the ascent, so a blank measures sensibly.
            return (self.text_advance(text), sf.ascent().ceil().max(0.0) as u32);
        }
        let w = (max_x - min_x).ceil().max(0.0) as u32;
        let h = (max_y - min_y).ceil().max(0.0) as u32;
        (w, h)
    }

    /// Rasterize `text` with its top-left at `(x, y)`. For each covered device
    /// pixel `emit(px, py, coverage)` is called with `coverage` in `0.0..=1.0`;
    /// the caller blends the text color by that coverage. The baseline is placed
    /// at the font ascent below `y` so the visible glyphs sit inside the
    /// `(x, y, advance, line_height)` box the page reserved, matching the prior
    /// renderer's top-left text origin.
    pub fn draw_text<F: FnMut(i32, i32, f32)>(&self, text: &str, x: i32, y: i32, mut emit: F) {
        let sf = self.font.as_scaled(self.px);
        let ascent = sf.ascent();
        let mut caret = 0.0_f32;
        let mut prev: Option<ab_glyph::GlyphId> = None;
        for ch in text.chars() {
            let id = self.font.glyph_id(ch);
            if let Some(p) = prev {
                caret += sf.kern(p, id);
            }
            let glyph: Glyph = id.with_scale_and_position(self.px, ab_glyph::point(caret, ascent));
            if let Some(outline) = self.font.outline_glyph(glyph) {
                let bounds = outline.px_bounds();
                let ox = x + bounds.min.x as i32;
                let oy = y + bounds.min.y as i32;
                outline.draw(|gx, gy, coverage| {
                    if coverage > 0.0 {
                        emit(ox + gx as i32, oy + gy as i32, coverage);
                    }
                });
            }
            caret += sf.h_advance(id);
            prev = Some(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_face_parses() {
        for face in [
            FontFace::SansRegular,
            FontFace::SansBold,
            FontFace::MonoRegular,
            FontFace::MonoBold,
        ] {
            let f = LoadedFont::new(face, 16);
            assert!(f.line_height() > 0);
        }
    }

    #[test]
    fn larger_size_is_wider() {
        let small = LoadedFont::new(FontFace::SansBold, 12);
        let large = LoadedFont::new(FontFace::SansBold, 32);
        let sw = small.text_advance("groundnode");
        let lw = large.text_advance("groundnode");
        assert!(lw > sw, "32px advance {lw} should exceed 12px advance {sw}");
    }

    #[test]
    fn mono_advances_are_uniform() {
        // A monospaced face advances every glyph by the same per-glyph step.
        // `text_advance` sums the raw f32 advances and ceils once at the end,
        // so the *incremental* advance of each added glyph is the invariant to
        // assert, not a `4 * ceil(one)` identity (that would double-count the
        // rounding the single trailing ceil already folds in).
        let f = LoadedFont::new(FontFace::MonoRegular, 14);
        let a1 = f.text_advance("0") as i64;
        let a2 = f.text_advance("00") as i64;
        let a3 = f.text_advance("000") as i64;
        let a4 = f.text_advance("0000") as i64;
        // The advance grows by a constant step (within 1 px of the rounding
        // wobble the trailing ceil introduces).
        let step = a2 - a1;
        assert!((a3 - a2 - step).abs() <= 1);
        assert!((a4 - a3 - step).abs() <= 1);
        // And the wider string is strictly wider than the narrower one.
        assert!(a4 > a1);
    }

    #[test]
    fn empty_text_has_zero_advance_nonzero_height() {
        let f = LoadedFont::new(FontFace::SansRegular, 16);
        assert_eq!(f.text_advance(""), 0);
        let (w, h) = f.text_size("");
        assert_eq!(w, 0);
        assert!(h > 0);
    }

    #[test]
    fn draw_text_emits_covered_pixels() {
        let f = LoadedFont::new(FontFace::SansBold, 24);
        let mut count = 0u32;
        f.draw_text("A", 0, 0, |_x, _y, cov| {
            assert!((0.0..=1.0).contains(&cov));
            count += 1;
        });
        assert!(count > 0, "a glyph should cover at least one pixel");
    }

    #[test]
    fn drawn_glyphs_fall_inside_the_reserved_box() {
        // The baseline sits at the ascent, so an ASCII letter inks below y=0
        // and within the advance width — it must not spill above the box top.
        let px = 20u32;
        let f = LoadedFont::new(FontFace::SansRegular, px);
        let mut min_y = i32::MAX;
        let mut max_y = i32::MIN;
        f.draw_text("Mg", 0, 0, |_x, y, _cov| {
            min_y = min_y.min(y);
            max_y = max_y.max(y);
        });
        assert!(
            min_y >= 0,
            "glyph inked above the reserved box top: {min_y}"
        );
        assert!((max_y as u32) <= f.line_height() + px);
    }
}
