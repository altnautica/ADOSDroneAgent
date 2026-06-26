//! Keyframe image encoding: turn a raw ring frame (the normalized format the
//! vision engine wrote into shared memory) into a compressed JPEG for the
//! keyframe envelope. A reconstructor ingests compressed images, never raw
//! planes, and the full-resolution raw frame is far too large to ship, so the
//! capture service encodes only the frames it selects as keyframes (a low rate).

use ados_protocol::framebus::FrameFormat;
use image::codecs::jpeg::JpegEncoder;
use image::ExtendedColorType;

/// JPEG quality (0..100) for keyframes. High enough that feature matching and
/// photometric reconstruction stay clean, low enough that the keyframe fits a
/// LAN-bulk or relayed transfer.
const JPEG_QUALITY: u8 = 90;

/// Encode one raw ring frame as JPEG bytes. The frame is converted to packed
/// RGB8 first (a no-op for `Rgb24`, a BT.601 YUV→RGB conversion for the 4:2:0
/// formats), then JPEG-encoded.
pub fn encode_keyframe_jpeg(
    width: u32,
    height: u32,
    format: FrameFormat,
    data: &[u8],
) -> anyhow::Result<Vec<u8>> {
    let rgb = to_rgb8(width, height, format, data)?;
    let mut out = Vec::new();
    let mut enc = JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY);
    enc.encode(&rgb, width, height, ExtendedColorType::Rgb8)
        .map_err(|e| anyhow::anyhow!("jpeg encode {width}x{height}: {e}"))?;
    Ok(out)
}

/// Convert a raw ring frame to packed RGB8 (`width*height*3` bytes).
fn to_rgb8(width: u32, height: u32, format: FrameFormat, data: &[u8]) -> anyhow::Result<Vec<u8>> {
    let expected = format.frame_bytes(width, height);
    if data.len() < expected {
        anyhow::bail!(
            "frame too small: got {} bytes, {width}x{height} {format:?} needs {expected}",
            data.len()
        );
    }
    // The 4:2:0 chroma subsampling assumes even dimensions; an odd width/height
    // would index past the half-resolution chroma plane and panic, so reject it
    // here (the caller degrades a malformed frame to a dropped frame, not a crash).
    if matches!(format, FrameFormat::Nv12 | FrameFormat::Yuv420p)
        && (!width.is_multiple_of(2) || !height.is_multiple_of(2))
    {
        anyhow::bail!("4:2:0 frame requires even dimensions, got {width}x{height}");
    }
    let w = width as usize;
    let h = height as usize;
    match format {
        FrameFormat::Rgb24 => Ok(data[..w * h * 3].to_vec()),
        FrameFormat::Nv12 => Ok(nv12_to_rgb8(w, h, data)),
        FrameFormat::Yuv420p => Ok(yuv420p_to_rgb8(w, h, data)),
    }
}

/// BT.601 full-range YUV→RGB for one pixel. `clamp` keeps each channel in 0..=255.
#[inline]
fn yuv_to_rgb(y: u8, u: u8, v: u8) -> [u8; 3] {
    let yf = y as f32;
    let uf = u as f32 - 128.0;
    let vf = v as f32 - 128.0;
    let r = yf + 1.402 * vf;
    let g = yf - 0.344_136 * uf - 0.714_136 * vf;
    let b = yf + 1.772 * uf;
    [
        r.clamp(0.0, 255.0) as u8,
        g.clamp(0.0, 255.0) as u8,
        b.clamp(0.0, 255.0) as u8,
    ]
}

/// NV12: a full-resolution Y plane (`w*h`) followed by an interleaved
/// `U,V,U,V,...` chroma plane (`w*h/2`) at half resolution in each axis.
fn nv12_to_rgb8(w: usize, h: usize, data: &[u8]) -> Vec<u8> {
    let y_plane = &data[..w * h];
    let uv_plane = &data[w * h..w * h + (w * h / 2)];
    let mut rgb = vec![0u8; w * h * 3];
    for row in 0..h {
        for col in 0..w {
            let y = y_plane[row * w + col];
            // One chroma sample covers a 2x2 luma block; the interleaved pair
            // sits at (row/2, col/2) with stride `w` (two bytes per chroma column).
            let c_index = (row / 2) * w + (col / 2) * 2;
            let u = uv_plane[c_index];
            let v = uv_plane[c_index + 1];
            let px = (row * w + col) * 3;
            rgb[px..px + 3].copy_from_slice(&yuv_to_rgb(y, u, v));
        }
    }
    rgb
}

/// Planar YUV 4:2:0: a Y plane (`w*h`), then a U plane and a V plane each at
/// quarter resolution (`w*h/4`).
fn yuv420p_to_rgb8(w: usize, h: usize, data: &[u8]) -> Vec<u8> {
    let y_plane = &data[..w * h];
    let u_plane = &data[w * h..w * h + (w * h / 4)];
    let v_plane = &data[w * h + (w * h / 4)..w * h + (w * h / 2)];
    let cw = w / 2;
    let mut rgb = vec![0u8; w * h * 3];
    for row in 0..h {
        for col in 0..w {
            let y = y_plane[row * w + col];
            let c_index = (row / 2) * cw + (col / 2);
            let u = u_plane[c_index];
            let v = v_plane[c_index];
            let px = (row * w + col) * 3;
            rgb[px..px + 3].copy_from_slice(&yuv_to_rgb(y, u, v));
        }
    }
    rgb
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_rgb24_to_decodable_jpeg() {
        let (w, h) = (16u32, 16u32);
        let mut rgb = vec![0u8; (w * h * 3) as usize];
        for (i, px) in rgb.chunks_mut(3).enumerate() {
            px[0] = (i % 256) as u8;
            px[1] = 128;
            px[2] = (255 - i % 256) as u8;
        }
        let jpeg = encode_keyframe_jpeg(w, h, FrameFormat::Rgb24, &rgb).unwrap();
        // It is a JPEG (SOI marker) and decodes back to the right dimensions.
        assert_eq!(&jpeg[..2], &[0xFF, 0xD8]);
        let decoded = image::load_from_memory(&jpeg).unwrap();
        assert_eq!(decoded.width(), w);
        assert_eq!(decoded.height(), h);
    }

    #[test]
    fn converts_nv12_and_yuv420p_grey_to_grey_rgb() {
        // A mid-grey frame: Y=128, chroma neutral (U=V=128) → R≈G≈B≈128.
        let (w, h) = (4usize, 4usize);
        let mut nv12 = vec![128u8; w * h + w * h / 2];
        // chroma neutral
        for b in nv12[w * h..].iter_mut() {
            *b = 128;
        }
        let rgb = nv12_to_rgb8(w, h, &nv12);
        for px in rgb.chunks(3) {
            assert!(
                px.iter().all(|&c| (c as i32 - 128).abs() <= 2),
                "neutral grey"
            );
        }

        let mut yuv = vec![128u8; w * h + w * h / 2];
        for b in yuv[w * h..].iter_mut() {
            *b = 128;
        }
        let rgb2 = yuv420p_to_rgb8(w, h, &yuv);
        for px in rgb2.chunks(3) {
            assert!(
                px.iter().all(|&c| (c as i32 - 128).abs() <= 2),
                "neutral grey"
            );
        }
    }

    #[test]
    fn rejects_a_short_frame() {
        let err = encode_keyframe_jpeg(64, 64, FrameFormat::Rgb24, &[0u8; 10]);
        assert!(err.is_err());
    }

    #[test]
    fn rejects_odd_dimensions_for_420_instead_of_panicking() {
        // An odd-dimension 4:2:0 frame would index past the chroma plane; it must
        // return Err (the caller drops the frame), never panic. Size the buffer so
        // the length check passes and the even-dimension check is what rejects it.
        let nv12 = vec![128u8; FrameFormat::Nv12.frame_bytes(3, 3)];
        assert!(encode_keyframe_jpeg(3, 3, FrameFormat::Nv12, &nv12).is_err());
        let yuv = vec![128u8; FrameFormat::Yuv420p.frame_bytes(5, 4)];
        assert!(encode_keyframe_jpeg(5, 4, FrameFormat::Yuv420p, &yuv).is_err());
        // RGB has no chroma subsampling, so odd dimensions are fine.
        let rgb = vec![0u8; FrameFormat::Rgb24.frame_bytes(3, 3)];
        assert!(encode_keyframe_jpeg(3, 3, FrameFormat::Rgb24, &rgb).is_ok());
    }

    #[test]
    fn red_yuv_round_trips_to_red_ish() {
        // A pure-red pixel in YUV (Y~76,U~84,V~255) decodes to a red-dominant RGB.
        let rgb = yuv_to_rgb(76, 84, 255);
        assert!(rgb[0] > 200 && rgb[1] < 80 && rgb[2] < 80, "got {rgb:?}");
    }
}
