//! Contract F — the vision-tap wire format.
//!
//! The frozen seam between the video pipeline (the writer, `ados-video`) and the
//! vision engine (the reader, `ados-vision`). A tap carries decoded rawvideo
//! frames from the encode pipeline to the engine over a Unix socket: the engine
//! *connects* as a client, the writer *serves* the socket and streams
//! `[header][pixels]` per frame.
//!
//! **Framing (v2):** a 24-byte little-endian header, then `byte_len` raw pixel
//! bytes:
//!
//! ```text
//! magic:u32=0x41445654 ("ADVT") | format:u8 | _pad:u8 | width:u16 | height:u16 | _pad:u16 | byte_len:u32 | ts_mono_ns:u64
//! ```
//!
//! `ts_mono_ns` is host CLOCK_MONOTONIC ns stamped by the writer at frame arrival.
//! Self-describing so a consumer can size a ring without a side channel. This
//! module is the single definition both sides build against — freeze it here,
//! change both sides in the same version bump (the same frozen-wire-contract
//! discipline the mavlink, state, and plugin sockets already follow, now
//! extended to the vision seam).

use thiserror::Error;
use tokio::io::AsyncWriteExt;

use crate::framebus::FrameFormat;

/// The tap wire-contract version. Bump on any framing change; both the writer
/// and the reader must move together.
pub const TAP_CONTRACT_VERSION: u32 = 2;

/// Header magic — ASCII "ADVT", little-endian.
pub const TAP_MAGIC: u32 = 0x4144_5654;
/// Fixed header length in bytes.
pub const TAP_HEADER_LEN: usize = 24;
/// Cap a single tapped frame so a corrupt header cannot drive an unbounded
/// allocation. 4K RGB24 is ~25 MiB; 64 MiB is a generous ceiling.
pub const TAP_MAX_FRAME: usize = 64 * 1024 * 1024;

/// A malformed tap header.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum TapHeaderError {
    /// The 4-byte magic did not match [`TAP_MAGIC`].
    #[error("bad tap frame magic {0:#x}")]
    BadMagic(u32),
    /// The format tag was not one of the known [`FrameFormat`] values.
    #[error("bad tap format tag {0}")]
    BadFormatTag(u8),
    /// The declared payload length exceeds [`TAP_MAX_FRAME`].
    #[error("tap frame of {0} bytes exceeds the {TAP_MAX_FRAME}-byte cap")]
    FrameTooLarge(usize),
}

/// A decoded tap frame header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TapHeader {
    pub format: FrameFormat,
    pub width: u32,
    pub height: u32,
    pub byte_len: usize,
    /// Host CLOCK_MONOTONIC ns at frame arrival on the writer side.
    pub ts_mono_ns: u64,
}

/// The pixel-format wire tag for a [`FrameFormat`].
pub fn format_tag(f: FrameFormat) -> u8 {
    match f {
        FrameFormat::Rgb24 => 0,
        FrameFormat::Nv12 => 1,
        FrameFormat::Yuv420p => 2,
        FrameFormat::Gray8 => 3,
        FrameFormat::Gray16 => 4,
        FrameFormat::BayerRggb8 => 5,
        FrameFormat::BayerRggb16 => 6,
    }
}

/// The [`FrameFormat`] for a wire tag, or `None` for an unknown tag.
pub fn format_from_tag(t: u8) -> Option<FrameFormat> {
    match t {
        0 => Some(FrameFormat::Rgb24),
        1 => Some(FrameFormat::Nv12),
        2 => Some(FrameFormat::Yuv420p),
        3 => Some(FrameFormat::Gray8),
        4 => Some(FrameFormat::Gray16),
        5 => Some(FrameFormat::BayerRggb8),
        6 => Some(FrameFormat::BayerRggb16),
        _ => None,
    }
}

/// Encode the tap frame header. The writer prepends this to each frame's pixels.
pub fn encode_tap_header(
    format: FrameFormat,
    width: u32,
    height: u32,
    byte_len: u32,
    ts_mono_ns: u64,
) -> [u8; TAP_HEADER_LEN] {
    let mut h = [0u8; TAP_HEADER_LEN];
    h[0..4].copy_from_slice(&TAP_MAGIC.to_le_bytes());
    h[4] = format_tag(format);
    h[6..8].copy_from_slice(&(width as u16).to_le_bytes());
    h[8..10].copy_from_slice(&(height as u16).to_le_bytes());
    h[12..16].copy_from_slice(&byte_len.to_le_bytes());
    h[16..24].copy_from_slice(&ts_mono_ns.to_le_bytes());
    h
}

/// Decode a tap frame header.
pub fn decode_tap_header(h: &[u8; TAP_HEADER_LEN]) -> Result<TapHeader, TapHeaderError> {
    let magic = u32::from_le_bytes(h[0..4].try_into().unwrap());
    if magic != TAP_MAGIC {
        return Err(TapHeaderError::BadMagic(magic));
    }
    let format = format_from_tag(h[4]).ok_or(TapHeaderError::BadFormatTag(h[4]))?;
    let width = u16::from_le_bytes(h[6..8].try_into().unwrap()) as u32;
    let height = u16::from_le_bytes(h[8..10].try_into().unwrap()) as u32;
    let byte_len = u32::from_le_bytes(h[12..16].try_into().unwrap()) as usize;
    if byte_len > TAP_MAX_FRAME {
        return Err(TapHeaderError::FrameTooLarge(byte_len));
    }
    let ts_mono_ns = u64::from_le_bytes(h[16..24].try_into().unwrap());
    Ok(TapHeader {
        format,
        width,
        height,
        byte_len,
        ts_mono_ns,
    })
}

/// Write one `[header][pixels]` tap frame to `w`. The shared writer the video
/// pipeline shim and any producer use so the framing matches the reader exactly.
pub async fn write_tap_frame<W>(
    w: &mut W,
    format: FrameFormat,
    width: u32,
    height: u32,
    pixels: &[u8],
    ts_mono_ns: u64,
) -> std::io::Result<()>
where
    W: AsyncWriteExt + Unpin,
{
    let header = encode_tap_header(format, width, height, pixels.len() as u32, ts_mono_ns);
    w.write_all(&header).await?;
    if !pixels.is_empty() {
        w.write_all(pixels).await?;
    }
    w.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hand-written tap contract version must match the registry entry so a
    /// future drift is a build-time test failure, not a silent mis-parse.
    #[test]
    fn version_matches_registry() {
        assert_eq!(
            TAP_CONTRACT_VERSION as u16,
            crate::contracts::contract_version("tap.frame").unwrap()
        );
    }

    #[test]
    fn header_round_trips() {
        for (fmt, w, h, ts) in [
            (FrameFormat::Rgb24, 640u32, 480u32, 0u64),
            (FrameFormat::Nv12, 1280, 720, 1),
            (FrameFormat::Yuv420p, 1920, 1080, u64::MAX),
            (FrameFormat::Gray8, 640, 480, 123_456_789_012),
            (FrameFormat::Gray16, 640, 480, 7),
            (FrameFormat::BayerRggb8, 640, 480, 8),
            (FrameFormat::BayerRggb16, 640, 480, 9),
        ] {
            let len = fmt.frame_bytes(w, h);
            let hdr = encode_tap_header(fmt, w, h, len as u32, ts);
            let d = decode_tap_header(&hdr).unwrap();
            assert_eq!(
                d,
                TapHeader {
                    format: fmt,
                    width: w,
                    height: h,
                    byte_len: len,
                    ts_mono_ns: ts,
                }
            );
        }
    }

    #[test]
    fn header_layout_is_pinned() {
        // A wire-shape regression tripwire: the exact bytes for a known frame.
        let hdr = encode_tap_header(FrameFormat::Rgb24, 8, 4, 96, 0x0102_0304_0506_0708);
        assert_eq!(&hdr[0..4], b"TVDA"); // 0x41445654 little-endian
        assert_eq!(hdr[4], 0); // rgb24
        assert_eq!(u16::from_le_bytes([hdr[6], hdr[7]]), 8);
        assert_eq!(u16::from_le_bytes([hdr[8], hdr[9]]), 4);
        assert_eq!(u32::from_le_bytes([hdr[12], hdr[13], hdr[14], hdr[15]]), 96);
        assert_eq!(
            u64::from_le_bytes(hdr[16..24].try_into().unwrap()),
            0x0102_0304_0506_0708
        );
    }

    #[test]
    fn decode_rejects_bad_magic_tag_and_oversize() {
        let mut hdr = encode_tap_header(FrameFormat::Rgb24, 8, 4, 96, 0);
        let good = hdr;
        hdr[0] ^= 0xFF;
        assert!(matches!(
            decode_tap_header(&hdr),
            Err(TapHeaderError::BadMagic(_))
        ));
        let mut bad_tag = good;
        bad_tag[4] = 9;
        assert_eq!(
            decode_tap_header(&bad_tag),
            Err(TapHeaderError::BadFormatTag(9))
        );
        let mut big = good;
        big[12..16].copy_from_slice(&((TAP_MAX_FRAME as u32) + 1).to_le_bytes());
        assert!(matches!(
            decode_tap_header(&big),
            Err(TapHeaderError::FrameTooLarge(_))
        ));
    }
}
