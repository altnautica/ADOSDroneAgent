//! The page render loop that drives the LCD off the device state.
//!
//! On each tick it reads the latest agent state, asks the [`crate::navigator`]
//! for the active page, has the page composer in [`crate::pages`] paint a
//! full-panel [`crate::graphics::primitives::Canvas`], packs it for the panel's
//! bit depth via [`crate::pack`], and hands the frame to the off-thread
//! [`crate::fb_writer`] (latest-wins, duplicate-skip). This replaces the prior
//! page-render service while reusing the byte-level write path this crate
//! already owns.
//!
//! This module carries the pure frame-packing seam the loop is built on. The
//! tick loop itself is wired in the integration stage that follows; keeping the
//! seam here lets the page composers and the panel write path develop and test
//! independently.

use crate::graphics::primitives::Canvas;
use crate::pack::pack_for_bpp;

/// Pack a finished page canvas into the panel's framebuffer bytes for `bpp`.
///
/// Reads the canvas as tightly-packed RGB888 and dispatches to the matching
/// [`crate::pack`] packer (16 bpp -> RGB565 LE, 24 bpp -> RGB888 passthrough,
/// 32 bpp -> xRGB32). Returns `None` for an unsupported depth so the caller can
/// drop the frame, matching the renderer's bpp gate.
pub fn pack_frame(canvas: &Canvas, bpp: u32) -> Option<Vec<u8>> {
    pack_for_bpp(canvas.as_rgb888(), bpp)
}

/// How the native canvas was placed onto a differently-sized panel: the uniform
/// scale factor and the top-left offset of the scaled image within the panel.
///
/// This is RENDER-side placement geometry — [`scale_letterbox`] uses it to blit
/// the scaled canvas, and it is returned so the placement can be asserted in
/// tests. It is NOT how touch is mapped: the touch layer maps raw ADC counts to
/// LCD pixels through the [`crate::touch_input`] affine calibration, fit against
/// the panel's real geometry independently of this letterbox, so there is no
/// panel-to-canvas inverse of this transform anywhere on the touch path.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LetterboxTransform {
    pub scale: f32,
    pub offset_x: u32,
    pub offset_y: u32,
    pub scaled_w: u32,
    pub scaled_h: u32,
}

impl LetterboxTransform {
    /// The identity placement (panel == native canvas): no scale, no offset.
    pub fn identity(w: u32, h: u32) -> Self {
        Self {
            scale: 1.0,
            offset_x: 0,
            offset_y: 0,
            scaled_w: w,
            scaled_h: h,
        }
    }

    /// The placement of a `src_w`x`src_h` canvas fitted (aspect-preserving,
    /// centered) onto a `xres`x`yres` panel. Pure geometry — the same math
    /// [`scale_letterbox`] uses to place pixels.
    pub fn fit(src_w: u32, src_h: u32, xres: u32, yres: u32) -> Self {
        if (xres == src_w && yres == src_h) || src_w == 0 || src_h == 0 || xres == 0 || yres == 0 {
            return Self::identity(src_w, src_h);
        }
        let scale = (xres as f32 / src_w as f32).min(yres as f32 / src_h as f32);
        let scaled_w = ((src_w as f32 * scale).round() as u32).clamp(1, xres);
        let scaled_h = ((src_h as f32 * scale).round() as u32).clamp(1, yres);
        Self {
            scale,
            offset_x: (xres - scaled_w) / 2,
            offset_y: (yres - scaled_h) / 2,
            scaled_w,
            scaled_h,
        }
    }
}

/// Scale the native `src_w`x`src_h` RGB888 canvas to fit a `xres`x`yres` panel,
/// preserving aspect ratio, centered, with black letterbox bars — so the panel
/// size no longer has to match the authoring size. Bilinear sampling.
///
/// Returns the packed-target RGB888 buffer (`xres*yres*3` bytes, ready for
/// [`pack_for_bpp`]) and the [`LetterboxTransform`] that placed it. When the
/// panel already matches the canvas exactly, this is a cheap identity copy so
/// the common case pays nothing.
pub fn scale_letterbox(
    src: &[u8],
    src_w: u32,
    src_h: u32,
    xres: u32,
    yres: u32,
) -> (Vec<u8>, LetterboxTransform) {
    if xres == src_w && yres == src_h {
        return (src.to_vec(), LetterboxTransform::identity(src_w, src_h));
    }
    if src_w == 0 || src_h == 0 || xres == 0 || yres == 0 {
        return (
            vec![0u8; (xres as usize) * (yres as usize) * 3],
            LetterboxTransform::identity(src_w, src_h),
        );
    }

    let xf = LetterboxTransform::fit(src_w, src_h, xres, yres);
    let LetterboxTransform {
        scale,
        offset_x,
        offset_y,
        scaled_w,
        scaled_h,
    } = xf;

    let mut out = vec![0u8; (xres as usize) * (yres as usize) * 3]; // black bars
    let sw = src_w as i32;
    let sh = src_h as i32;
    for dy in 0..scaled_h {
        // Pixel-center source coordinate.
        let sy = ((dy as f32 + 0.5) / scale) - 0.5;
        let y0 = sy.floor();
        let fy = sy - y0;
        let y0 = y0 as i32;
        for dx in 0..scaled_w {
            let sx = ((dx as f32 + 0.5) / scale) - 0.5;
            let x0 = sx.floor();
            let fx = sx - x0;
            let x0 = x0 as i32;
            let mut rgb = [0u8; 3];
            for (c, ch) in rgb.iter_mut().enumerate() {
                let s = |x: i32, y: i32| -> f32 {
                    let xc = x.clamp(0, sw - 1);
                    let yc = y.clamp(0, sh - 1);
                    src[((yc * sw + xc) as usize) * 3 + c] as f32
                };
                let top = s(x0, y0) * (1.0 - fx) + s(x0 + 1, y0) * fx;
                let bot = s(x0, y0 + 1) * (1.0 - fx) + s(x0 + 1, y0 + 1) * fx;
                *ch = (top * (1.0 - fy) + bot * fy).round().clamp(0.0, 255.0) as u8;
            }
            let ox = (offset_x + dx) as usize;
            let oy = (offset_y + dy) as usize;
            let di = (oy * xres as usize + ox) * 3;
            out[di..di + 3].copy_from_slice(&rgb);
        }
    }
    (out, xf)
}

/// Pack the native canvas, scaled+letterboxed to a `xres`x`yres` panel, for
/// `bpp`. The scale-to-fit entry point the render loop uses instead of packing
/// the raw canvas: on an exactly-matching panel it is `pack_frame`, on any other
/// size it fits the canvas and fills the surplus with black. Returns the packed
/// bytes (length `xres*yres*bpp/8`) plus the transform for the touch layer, or
/// `None` on an unsupported depth.
pub fn pack_frame_fitted(
    canvas: &Canvas,
    bpp: u32,
    xres: u32,
    yres: u32,
) -> Option<(Vec<u8>, LetterboxTransform)> {
    let (rgb, xf) = scale_letterbox(
        canvas.as_rgb888(),
        canvas.width(),
        canvas.height(),
        xres,
        yres,
    );
    pack_for_bpp(&rgb, bpp).map(|packed| (packed, xf))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graphics::palette::DARK;
    use crate::pages::blank_panel;

    #[test]
    fn pack_frame_matches_bit_depth() {
        let canvas = blank_panel(&DARK);
        let pixels = (canvas.width() * canvas.height()) as usize;
        // 16 bpp -> 2 bytes per pixel.
        assert_eq!(pack_frame(&canvas, 16).unwrap().len(), pixels * 2);
        // 24 bpp -> 3 bytes per pixel (passthrough).
        assert_eq!(pack_frame(&canvas, 24).unwrap().len(), pixels * 3);
        // 32 bpp -> 4 bytes per pixel.
        assert_eq!(pack_frame(&canvas, 32).unwrap().len(), pixels * 4);
        // Unsupported depth drops the frame.
        assert!(pack_frame(&canvas, 8).is_none());
    }

    #[test]
    fn scale_letterbox_identity_is_a_cheap_copy() {
        let src = vec![7u8; 4 * 3 * 3]; // 4x3 RGB
        let (out, xf) = scale_letterbox(&src, 4, 3, 4, 3);
        assert_eq!(out, src);
        assert_eq!(xf, LetterboxTransform::identity(4, 3));
    }

    #[test]
    fn scale_letterbox_centers_with_black_bars_and_preserves_aspect() {
        // A 2x2 white canvas onto a 6x4 panel: scale = min(3, 2) = 2, so the
        // image is 4x4 centered -> x-bars of 1px each side, no y-bars.
        let src = vec![255u8; 2 * 2 * 3];
        let (out, xf) = scale_letterbox(&src, 2, 2, 6, 4);
        assert_eq!(out.len(), 6 * 4 * 3);
        assert_eq!(xf.scale, 2.0);
        assert_eq!((xf.scaled_w, xf.scaled_h), (4, 4));
        assert_eq!((xf.offset_x, xf.offset_y), (1, 0));
        // The left bar column (x=0) is black; the image column (x=1) is white.
        let px = |x: usize, y: usize| -> [u8; 3] {
            let i = (y * 6 + x) * 3;
            [out[i], out[i + 1], out[i + 2]]
        };
        assert_eq!(px(0, 0), [0, 0, 0], "left letterbox bar is black");
        assert_eq!(px(1, 0), [255, 255, 255], "image region is white");
        assert_eq!(px(5, 0), [0, 0, 0], "right letterbox bar is black");
    }

    #[test]
    fn letterbox_transform_fit_matches_scale_letterbox() {
        // fit() is the pure-geometry twin of scale_letterbox's placement.
        let src = [255u8; 2 * 2 * 3];
        let (_out, xf) = scale_letterbox(&src, 2, 2, 6, 4);
        assert_eq!(xf, LetterboxTransform::fit(2, 2, 6, 4));
        assert_eq!(
            LetterboxTransform::fit(480, 320, 480, 320),
            LetterboxTransform::identity(480, 320)
        );
    }
}
