//! Pixel conversions from what a UVC camera delivers (packed YUYV, decoded
//! JPEG) to the frame-bus formats.
//!
//! A UVC camera's YUYV is BT.601 limited range; a JFIF JPEG decodes to BT.601
//! full-range YCbCr. The frame bus carries RGB and gray full range, and NV12
//! and YUV 4:2:0 limited range (untagged YUV, as the encoders and ffmpeg read
//! it). Chroma is halved vertically (YUYV) or both ways (4:4:4 JPEG output)
//! by averaging. Every function returns `false`, writing nothing, when `src`
//! is too short for the frame; `out` is the exact frame-bus frame length.

fn clamp8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

/// A limited-range BT.601 sample as full-range RGB.
pub fn rgb_from_limited(y: u8, u: u8, v: u8) -> [u8; 3] {
    let c = 298 * (i32::from(y) - 16);
    let d = i32::from(u) - 128;
    let e = i32::from(v) - 128;
    [
        clamp8((c + 409 * e + 128) >> 8),
        clamp8((c - 100 * d - 208 * e + 128) >> 8),
        clamp8((c + 516 * d + 128) >> 8),
    ]
}

/// Limited-range luma as full-range gray.
pub fn gray_from_limited(y: u8) -> u8 {
    clamp8(((i32::from(y) - 16) * 255 + 109) / 219)
}

/// Full-range luma as limited range.
pub fn luma_to_limited(y: u8) -> u8 {
    (16 + (u32::from(y) * 219 + 127) / 255) as u8
}

/// A full-range chroma value as limited range.
pub fn chroma_to_limited(c: u8) -> u8 {
    ((u32::from(c) * 224 + 128 * 31 + 127) / 255) as u8
}

/// Whether `src` holds `h` rows of `row_len` bytes at `stride`.
fn fits(src: &[u8], stride: usize, row_len: usize, h: usize) -> bool {
    h > 0 && stride >= row_len && src.len() >= stride * (h - 1) + row_len
}

/// Row `r` of a frame [`fits`] has checked.
fn line(src: &[u8], stride: usize, row_len: usize, r: usize) -> &[u8] {
    &src[r * stride..r * stride + row_len]
}

/// Store one chroma sample of a 4:2:0 frame's chroma planes (everything
/// after its `w * h` luma): interleaved UV (NV12), or U then V (YUV 4:2:0).
fn write_chroma(chroma: &mut [u8], w: usize, nv12: bool, x: usize, y: usize, u: u8, v: u8) {
    let cw = w / 2;
    if nv12 {
        let i = y * w + x * 2;
        chroma[i] = u;
        chroma[i + 1] = v;
    } else {
        let quarter = chroma.len() / 2;
        chroma[y * cw + x] = u;
        chroma[quarter + y * cw + x] = v;
    }
}

/// YUYV to packed RGB.
pub fn yuyv_to_rgb24(src: &[u8], stride: usize, w: usize, h: usize, out: &mut [u8]) -> bool {
    if !fits(src, stride, w * 2, h) {
        return false;
    }
    for (r, dst) in out.chunks_exact_mut(w * 3).take(h).enumerate() {
        let line = line(src, stride, w * 2, r);
        let (pairs, _) = line.as_chunks::<4>();
        let (out_pairs, _) = dst.as_chunks_mut::<6>();
        for (px, rgb) in pairs.iter().zip(out_pairs.iter_mut()) {
            let (u, v) = (px[1], px[3]);
            rgb[..3].copy_from_slice(&rgb_from_limited(px[0], u, v));
            rgb[3..].copy_from_slice(&rgb_from_limited(px[2], u, v));
        }
    }
    true
}

/// YUYV to 8-bit gray.
pub fn yuyv_to_gray8(src: &[u8], stride: usize, w: usize, h: usize, out: &mut [u8]) -> bool {
    if !fits(src, stride, w * 2, h) {
        return false;
    }
    for (r, dst) in out.chunks_exact_mut(w).take(h).enumerate() {
        let line = line(src, stride, w * 2, r);
        for (yuyv, g) in line.as_chunks::<2>().0.iter().zip(dst.iter_mut()) {
            *g = gray_from_limited(yuyv[0]);
        }
    }
    true
}

/// YUYV (4:2:2) to NV12 or YUV 4:2:0: the luma as is, each chroma sample the
/// mean of the two rows sharing it. `w` and `h` even.
pub fn yuyv_to_420(
    src: &[u8],
    stride: usize,
    w: usize,
    h: usize,
    nv12: bool,
    out: &mut [u8],
) -> bool {
    if !fits(src, stride, w * 2, h) {
        return false;
    }
    let (luma, chroma) = out.split_at_mut(w * h);
    for (r, dst) in luma.chunks_exact_mut(w).enumerate() {
        let line = line(src, stride, w * 2, r);
        for (yuyv, y) in line.as_chunks::<2>().0.iter().zip(dst.iter_mut()) {
            *y = yuyv[0];
        }
    }
    for cy in 0..h / 2 {
        let a = line(src, stride, w * 2, cy * 2);
        let b = line(src, stride, w * 2, cy * 2 + 1);
        for cx in 0..w / 2 {
            let i = cx * 4;
            let mean = |o: usize| (u16::from(a[i + o]) + u16::from(b[i + o])).div_ceil(2) as u8;
            write_chroma(chroma, w, nv12, cx, cy, mean(1), mean(3));
        }
    }
    true
}

/// Full-range interleaved YCbCr 4:4:4 (a decoded JPEG) to limited-range NV12
/// or YUV 4:2:0, each chroma sample the mean of its 2x2 block. `w`, `h` even.
pub fn ycbcr444_to_420(src: &[u8], w: usize, h: usize, nv12: bool, out: &mut [u8]) -> bool {
    if src.len() < w * h * 3 {
        return false;
    }
    let (luma, chroma) = out.split_at_mut(w * h);
    for (px, y) in src.as_chunks::<3>().0.iter().zip(luma.iter_mut()) {
        *y = luma_to_limited(px[0]);
    }
    let row = w * 3;
    for cy in 0..h / 2 {
        for cx in 0..w / 2 {
            let at = |dy: usize, dx: usize, c: usize| {
                u16::from(src[(cy * 2 + dy) * row + (cx * 2 + dx) * 3 + c])
            };
            let mean =
                |c: usize| ((at(0, 0, c) + at(0, 1, c) + at(1, 0, c) + at(1, 1, c) + 2) / 4) as u8;
            let (u, v) = (chroma_to_limited(mean(1)), chroma_to_limited(mean(2)));
            write_chroma(chroma, w, nv12, cx, cy, u, v);
        }
    }
    true
}
