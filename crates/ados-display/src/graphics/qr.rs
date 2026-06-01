//! QR-code rasterizer for short payloads (pair URL / short code).
//!
//! Encodes a payload to a QR matrix at medium error correction (the same level
//! the prior renderer used), then rasterizes it into a [`QrImage`] of square
//! modules with a quiet-zone border. The module pixel size is chosen so the
//! whole code lands near a requested target size while staying an integer
//! multiple of a module, so the squares stay crisp (the equivalent of the prior
//! renderer's NEAREST resize).
//!
//! The matrix is dark-on-light by QR convention; the result is returned as a
//! per-pixel boolean grid (`true` = dark module) so the page can paint it in
//! whichever foreground / background the dashboard theme calls for. An encode
//! failure (payload too long for any version) returns `None` so the page can
//! fall back to text.

use qrcode::types::QrError;
use qrcode::{EcLevel, QrCode};

/// A rasterized QR code: a square grid of `size` x `size` pixels where `true`
/// marks a dark pixel. Quiet-zone border pixels are `false` (light).
pub struct QrImage {
    /// Side length in pixels (modules * module_px + 2 * border * module_px).
    pub size: u32,
    /// Pixel size of one QR module.
    pub module_px: u32,
    /// Row-major dark/light grid, `size * size` entries.
    pub pixels: Vec<bool>,
}

impl QrImage {
    /// Read one pixel; out-of-bounds reads are light (`false`).
    pub fn is_dark(&self, x: u32, y: u32) -> bool {
        if x >= self.size || y >= self.size {
            return false;
        }
        self.pixels[(y * self.size + x) as usize]
    }
}

/// Encode `text` and rasterize it near `target_px` square with a
/// `border_modules`-wide quiet zone.
///
/// The module pixel size is `max(1, target_px / total_modules)`, so the code is
/// the largest integer-module render that fits the target. Returns `None` when
/// the payload cannot be encoded at medium error correction.
pub fn render_qr(text: &str, target_px: u32, border_modules: u32) -> Option<QrImage> {
    let code = match QrCode::with_error_correction_level(text.as_bytes(), EcLevel::M) {
        Ok(c) => c,
        Err(QrError::DataTooLong) => return None,
        Err(_) => return None,
    };
    let width = code.width() as u32;
    let total_modules = width + 2 * border_modules;
    if total_modules == 0 {
        return None;
    }
    let module_px = (target_px / total_modules).max(1);
    let size = total_modules * module_px;

    let colors = code.to_colors();
    let mut pixels = vec![false; (size * size) as usize];
    for my in 0..width {
        for mx in 0..width {
            let dark = colors[(my * width + mx) as usize] == qrcode::Color::Dark;
            if !dark {
                continue;
            }
            // Paint the module's block, offset past the quiet-zone border.
            let px0 = (mx + border_modules) * module_px;
            let py0 = (my + border_modules) * module_px;
            for dy in 0..module_px {
                for dx in 0..module_px {
                    let x = px0 + dx;
                    let y = py0 + dy;
                    pixels[(y * size + x) as usize] = true;
                }
            }
        }
    }
    Some(QrImage {
        size,
        module_px,
        pixels,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_a_short_payload() {
        let qr = render_qr("https://ados.local/pair", 96, 2).expect("short payload encodes");
        assert!(qr.size > 0);
        assert!(qr.module_px >= 1);
        // The quiet-zone border is light all around.
        assert!(!qr.is_dark(0, 0));
    }

    #[test]
    fn render_is_square_and_integer_module() {
        let qr = render_qr("ABC123", 100, 2).expect("encodes");
        assert_eq!(qr.pixels.len(), (qr.size * qr.size) as usize);
        // size must be an exact multiple of module_px.
        assert_eq!(qr.size % qr.module_px, 0);
    }

    #[test]
    fn has_dark_modules() {
        let qr = render_qr("payload", 96, 2).expect("encodes");
        let dark = qr.pixels.iter().filter(|p| **p).count();
        assert!(dark > 0, "a real QR has dark modules");
    }

    #[test]
    fn out_of_bounds_is_light() {
        let qr = render_qr("x", 64, 2).expect("encodes");
        assert!(!qr.is_dark(qr.size, qr.size));
        assert!(!qr.is_dark(qr.size + 100, 0));
    }
}
