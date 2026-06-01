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
}
