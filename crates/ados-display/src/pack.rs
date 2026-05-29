//! Pixel packers: RGB888 source -> the panel's bit depth.
//!
//! Ports the bpp branches of `_compose_and_pack` / `_pack_rgb565` in
//! `renderers/framebuffer.py`:
//!
//! * 16 bpp -> RGB565 little-endian.
//! * 24 bpp -> RGB888 passthrough (the canvas bytes as-is).
//! * 32 bpp -> xRGB32 laid out B, G, R, 0 per pixel (the fbtft xRGB order).
//!
//! Input is a tightly-packed RGB888 byte slice (3 bytes per pixel), matching
//! PIL's `image.convert("RGB").tobytes()`. Pure math, fully testable.

/// Pack RGB888 source bytes to RGB565 little-endian (2 bytes per pixel). The
/// channel masks match the Python `((r & 0xF8) << 8) | ((g & 0xFC) << 3) | (b
/// >> 3)`, written little-endian.
pub fn pack_rgb565(rgb: &[u8]) -> Vec<u8> {
    let px = rgb.len() / 3;
    let mut out = vec![0u8; px * 2];
    for i in 0..px {
        let r = rgb[i * 3] as u16;
        let g = rgb[i * 3 + 1] as u16;
        let b = rgb[i * 3 + 2] as u16;
        let v = ((r & 0xF8) << 8) | ((g & 0xFC) << 3) | (b >> 3);
        out[i * 2] = (v & 0xFF) as u8;
        out[i * 2 + 1] = (v >> 8) as u8;
    }
    out
}

/// 24 bpp passthrough: RGB888 source bytes are the panel buffer as-is.
pub fn pack_rgb888(rgb: &[u8]) -> Vec<u8> {
    rgb.to_vec()
}

/// Pack RGB888 source to 32-bit xRGB laid out B, G, R, 0 per pixel — the order
/// the fbtft 32 bpp branch writes (`out[..0]=B, out[..1]=G, out[..2]=R,
/// out[..3]=0`).
pub fn pack_xrgb32(rgb: &[u8]) -> Vec<u8> {
    let px = rgb.len() / 3;
    let mut out = vec![0u8; px * 4];
    for i in 0..px {
        let r = rgb[i * 3];
        let g = rgb[i * 3 + 1];
        let b = rgb[i * 3 + 2];
        out[i * 4] = b;
        out[i * 4 + 1] = g;
        out[i * 4 + 2] = r;
        out[i * 4 + 3] = 0;
    }
    out
}

/// Pack RGB888 source to the buffer for `bpp` (16/24/32). Returns `None` for an
/// unsupported depth so the caller can drop the frame, matching the renderer's
/// bpp gate.
pub fn pack_for_bpp(rgb: &[u8], bpp: u32) -> Option<Vec<u8>> {
    match bpp {
        16 => Some(pack_rgb565(rgb)),
        24 => Some(pack_rgb888(rgb)),
        32 => Some(pack_xrgb32(rgb)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgb565_known_colors() {
        // Pure red: r=255 -> 0xF800, little-endian -> [0x00, 0xF8].
        assert_eq!(pack_rgb565(&[255, 0, 0]), vec![0x00, 0xF8]);
        // Pure green: g=255 -> 0x07E0 -> [0xE0, 0x07].
        assert_eq!(pack_rgb565(&[0, 255, 0]), vec![0xE0, 0x07]);
        // Pure blue: b=255 -> 0x001F -> [0x1F, 0x00].
        assert_eq!(pack_rgb565(&[0, 0, 255]), vec![0x1F, 0x00]);
        // Black and white.
        assert_eq!(pack_rgb565(&[0, 0, 0]), vec![0x00, 0x00]);
        assert_eq!(pack_rgb565(&[255, 255, 255]), vec![0xFF, 0xFF]);
    }

    #[test]
    fn rgb565_truncation_matches_python_masks() {
        // r=0xFF, g=0xFF, b=0xFF after masking: (0xF8<<8)|(0xFC<<3)|(0xFF>>3)
        // = 0xF800 | 0x07E0 | 0x1F = 0xFFFF.
        assert_eq!(pack_rgb565(&[0xFF, 0xFF, 0xFF]), vec![0xFF, 0xFF]);
        // r=0x12 g=0x34 b=0x56 -> ((0x12&0xF8)<<8)|((0x34&0xFC)<<3)|(0x56>>3)
        // = (0x10<<8)|(0x34<<3)|(0x0A) = 0x1000 | 0x1A0 | 0x0A = 0x11AA.
        assert_eq!(pack_rgb565(&[0x12, 0x34, 0x56]), vec![0xAA, 0x11]);
    }

    #[test]
    fn rgb565_length_is_two_bytes_per_pixel() {
        let src = vec![0u8; 3 * 100];
        assert_eq!(pack_rgb565(&src).len(), 200);
    }

    #[test]
    fn rgb888_passthrough() {
        let src = vec![1, 2, 3, 4, 5, 6];
        assert_eq!(pack_rgb888(&src), src);
    }

    #[test]
    fn xrgb32_bgr0_order() {
        // One pixel R=0x11 G=0x22 B=0x33 -> [B, G, R, 0].
        assert_eq!(
            pack_xrgb32(&[0x11, 0x22, 0x33]),
            vec![0x33, 0x22, 0x11, 0x00]
        );
        // Two pixels: layout repeats.
        let two = pack_xrgb32(&[0x11, 0x22, 0x33, 0x44, 0x55, 0x66]);
        assert_eq!(two, vec![0x33, 0x22, 0x11, 0x00, 0x66, 0x55, 0x44, 0x00]);
    }

    #[test]
    fn xrgb32_length_is_four_bytes_per_pixel() {
        let src = vec![0u8; 3 * 50];
        assert_eq!(pack_xrgb32(&src).len(), 200);
    }

    #[test]
    fn pack_for_bpp_dispatch() {
        let src = vec![0x12, 0x34, 0x56];
        assert_eq!(pack_for_bpp(&src, 16).unwrap(), pack_rgb565(&src));
        assert_eq!(pack_for_bpp(&src, 24).unwrap(), src);
        assert_eq!(pack_for_bpp(&src, 32).unwrap(), pack_xrgb32(&src));
        assert!(pack_for_bpp(&src, 8).is_none());
        assert!(pack_for_bpp(&src, 15).is_none());
    }
}
