//! The frame path between the page composers and the LCD framebuffer.
//!
//! The display daemon's page UI ticks at [`TICK_GRANULARITY`], asks the
//! [`crate::navigator`] for the active page, and has the page composer in
//! [`crate::pages`] paint a full-panel [`crate::graphics::primitives::Canvas`].
//! [`FramePresenter`] then hashes that canvas, skips it when the off-thread
//! [`ados_display_driver::fb_writer`] already holds the same frame, and
//! otherwise fits it to the panel resolution and packs it for the panel's bit
//! depth via [`ados_display_driver::pack`] before handing it to the writer
//! (latest-wins, duplicate-skip).
//!
//! A panel whose resolution matches the canvas is packed straight from the
//! canvas bytes; any other size is scaled and letterboxed first.

use std::path::PathBuf;
use std::time::Duration;

use crate::graphics::primitives::Canvas;
use ados_display_driver::fb_writer::{hash_bytes, FbWriter, Frame};
use ados_display_driver::pack::pack_for_bpp;

/// Render-loop tick granularity. The render period derives from the active
/// page's refresh rate; this is the resolution the loop wakes at to decide
/// whether a render is due.
pub const TICK_GRANULARITY: Duration = Duration::from_millis(100);

/// Pack a finished page canvas into the panel's framebuffer bytes for `bpp`.
///
/// Reads the canvas as tightly-packed RGB888 and dispatches to the matching
/// [`ados_display_driver::pack`] packer (16 bpp -> RGB565 LE, 24 bpp -> RGB888
/// passthrough, 32 bpp -> xRGB32). Returns `None` for an unsupported depth so
/// the caller can drop the frame, matching the renderer's bpp gate.
pub fn pack_frame(canvas: &Canvas, bpp: u32) -> Option<Vec<u8>> {
    pack_for_bpp(canvas.as_rgb888(), bpp)
}

/// How the native canvas is placed onto a differently-sized panel: the uniform
/// scale factor and the top-left offset of the scaled image within the panel.
///
/// This is RENDER-side placement geometry: [`scale_letterbox`] uses it to blit
/// the scaled canvas. It is NOT how touch is mapped: the touch layer maps raw
/// ADC counts to LCD pixels through the [`crate::touch_input`] affine
/// calibration, fit against the panel's real geometry independently of this
/// letterbox, so there is no panel-to-canvas inverse of this transform anywhere
/// on the touch path.
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
/// size no longer has to match the authoring size. Bilinear sampling, placed by
/// [`LetterboxTransform::fit`].
///
/// Returns the RGB888 buffer (`xres*yres*3` bytes, ready for [`pack_for_bpp`]).
/// When the panel already matches the canvas exactly the result is a copy of
/// `src`.
pub fn scale_letterbox(src: &[u8], src_w: u32, src_h: u32, xres: u32, yres: u32) -> Vec<u8> {
    if xres == src_w && yres == src_h {
        return src.to_vec();
    }
    if src_w == 0 || src_h == 0 || xres == 0 || yres == 0 {
        return vec![0u8; (xres as usize) * (yres as usize) * 3];
    }

    let LetterboxTransform {
        scale,
        offset_x,
        offset_y,
        scaled_w,
        scaled_h,
    } = LetterboxTransform::fit(src_w, src_h, xres, yres);

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
    out
}

/// Pack the native canvas for a `xres`x`yres` panel at `bpp`. The entry point
/// the render loop uses: on an exactly-matching panel it is [`pack_frame`]
/// (packed straight from the canvas bytes, no intermediate copy); on any other
/// size it scales and letterboxes the canvas first, filling the surplus with
/// black. Returns the packed bytes (length `xres*yres*bpp/8`), or `None` on an
/// unsupported depth.
pub fn pack_frame_fitted(canvas: &Canvas, bpp: u32, xres: u32, yres: u32) -> Option<Vec<u8>> {
    if xres == canvas.width() && yres == canvas.height() {
        return pack_frame(canvas, bpp);
    }
    let rgb = scale_letterbox(
        canvas.as_rgb888(),
        canvas.width(),
        canvas.height(),
        xres,
        yres,
    );
    pack_for_bpp(&rgb, bpp)
}

/// Presents finished canvases through the off-thread writer without redoing
/// work for an unchanged frame.
///
/// The canvas is hashed once. When the writer already holds that frame (written
/// or pending) the bilinear scale and pack are skipped entirely; a snapshot is
/// requested only when the canvas differs from the last snapshot, and the PNG
/// encode happens on the writer thread.
pub struct FramePresenter {
    bpp: u32,
    xres: u32,
    yres: u32,
    last_snapshot_hash: Option<u64>,
    packs: u64,
    snapshots: u64,
}

impl FramePresenter {
    pub fn new(bpp: u32, xres: u32, yres: u32) -> Self {
        Self {
            bpp,
            xres,
            yres,
            last_snapshot_hash: None,
            packs: 0,
            snapshots: 0,
        }
    }

    /// Present `canvas`; when `snapshot_to` is set (the snapshot cadence is
    /// due) also queue a PNG of it there unless that exact canvas was the last
    /// snapshot.
    pub fn present(&mut self, writer: &FbWriter, canvas: &Canvas, snapshot_to: Option<PathBuf>) {
        let raw = canvas.as_rgb888();
        let hash = hash_bytes(raw);
        if !writer.is_last(hash) {
            match pack_frame_fitted(canvas, self.bpp, self.xres, self.yres) {
                Some(packed) => {
                    self.packs += 1;
                    writer.present(Frame::with_hash(packed, hash));
                }
                None => {
                    tracing::warn!(bpp = self.bpp, "unsupported panel bit depth; frame dropped")
                }
            }
        }
        if let Some(path) = snapshot_to {
            if self.last_snapshot_hash != Some(hash) {
                let rgb888 = raw.to_vec();
                let (width, height) = (canvas.width(), canvas.height());
                writer.request_snapshot(Box::new(move || {
                    if let Err(e) =
                        crate::sidecar::write_snapshot_png(&path, &rgb888, width, height)
                    {
                        tracing::debug!(error = %e, "lcd-snapshot write failed");
                    }
                }));
                self.last_snapshot_hash = Some(hash);
                self.snapshots += 1;
            }
        }
    }

    /// Frames scaled and packed so far.
    pub fn packs(&self) -> u64 {
        self.packs
    }

    /// Snapshot PNGs requested so far.
    pub fn snapshots(&self) -> u64 {
        self.snapshots
    }
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
    fn scale_letterbox_identity_is_a_copy() {
        let src = vec![7u8; 4 * 3 * 3]; // 4x3 RGB
        assert_eq!(scale_letterbox(&src, 4, 3, 4, 3), src);
        assert_eq!(
            LetterboxTransform::fit(4, 3, 4, 3),
            LetterboxTransform::identity(4, 3)
        );
    }

    #[test]
    fn scale_letterbox_centers_with_black_bars_and_preserves_aspect() {
        // A 2x2 white canvas onto a 6x4 panel: scale = min(3, 2) = 2, so the
        // image is 4x4 centered -> x-bars of 1px each side, no y-bars.
        let src = vec![255u8; 2 * 2 * 3];
        let out = scale_letterbox(&src, 2, 2, 6, 4);
        let xf = LetterboxTransform::fit(2, 2, 6, 4);
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
    fn pack_frame_fitted_matches_pack_frame_on_a_matching_panel() {
        let canvas = blank_panel(&DARK);
        let (w, h) = (canvas.width(), canvas.height());
        assert_eq!(
            pack_frame_fitted(&canvas, 16, w, h),
            pack_frame(&canvas, 16)
        );
        // A larger panel is letterboxed to its own size.
        let packed = pack_frame_fitted(&canvas, 16, w * 2, h * 2).unwrap();
        assert_eq!(packed.len(), (w * 2 * h * 2 * 2) as usize);
    }

    struct CountingSink(std::sync::Arc<std::sync::atomic::AtomicUsize>);

    impl ados_display_driver::fb_writer::FrameSink for CountingSink {
        fn write_frame(&mut self, _buf: &[u8]) -> std::io::Result<()> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
    }

    #[test]
    fn static_page_packs_once_png_once() {
        let writes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut writer = FbWriter::spawn(CountingSink(writes.clone()));
        let mut presenter = FramePresenter::new(16, 480, 320);
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("lcd-snapshot.png");
        let canvas = blank_panel(&DARK);

        // Twenty renders of an unchanging page, each with the snapshot due.
        for _ in 0..20 {
            presenter.present(&writer, &canvas, Some(png.clone()));
        }
        assert_eq!(presenter.packs(), 1, "an unchanged frame is packed once");
        assert_eq!(
            presenter.snapshots(),
            1,
            "an unchanged frame is snapshotted once"
        );

        writer.cleanup();
        assert_eq!(writes.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(png.exists(), "the writer thread encoded the snapshot");
    }
}
