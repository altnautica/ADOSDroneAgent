//! What a captured camera is streamed in, and the conversion of each buffer
//! to the frame-bus format.
//!
//! A camera that offers the configured bus format is streamed in it and its
//! buffers are copied. A UVC camera offers MJPEG and YUYV instead: it is
//! streamed in one of them and every buffer is decoded or converted to the
//! bus format (RGB24, NV12, YUV 4:2:0 or GRAY8). Large frames prefer MJPEG,
//! whose compressed size keeps a 720p stream inside a shared USB 2 hub;
//! smaller ones prefer YUYV, which costs no decode.
//!
//! JPEG decoding goes through [`JpegDecode`]. The software decoder
//! (`zune-jpeg`) is the one in use: none of the supported boards exposes a
//! V4L2 memory-to-memory JPEG decoder node. A hardware decoder slots in here
//! as another [`JpegDecode`]. A Motion JPEG frame that leaves out its Huffman
//! tables is given the standard ones, and a frame that does not end in its
//! end-of-image marker (cut short) is refused rather than decoded with a grey
//! band where its missing rows would be.

use std::io;
use std::time::Duration;

use ados_protocol::framebus::FrameFormat;
use zune_jpeg::zune_core::bytestream::ZCursor;
use zune_jpeg::zune_core::colorspace::ColorSpace;
use zune_jpeg::zune_core::options::DecoderOptions;
use zune_jpeg::JpegDecoder;

use super::pixels;
use super::{copy_rows, fourcc, row_bytes};

/// `V4L2_PIX_FMT_MJPEG`, `V4L2_PIX_FMT_JPEG` and `V4L2_PIX_FMT_YUYV`.
pub const MJPG: u32 = u32::from_le_bytes(*b"MJPG");
pub const JPEG: u32 = u32::from_le_bytes(*b"JPEG");
pub const YUYV: u32 = u32::from_le_bytes(*b"YUYV");

/// Frames at least this large (720p) prefer MJPEG to YUYV.
const MJPEG_PREFERRED_PIXELS: u64 = 1280 * 720;

/// How the camera's buffers reach the bus format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wire {
    /// The camera delivers the bus format: rows are copied.
    Direct,
    /// Motion JPEG, decoded.
    Mjpeg,
    /// Packed YUYV 4:2:2, converted.
    Yuyv,
}

/// The fourcc a format code reads as (`"MJPG"`).
pub fn fourcc_name(code: u32) -> String {
    String::from_utf8_lossy(&code.to_le_bytes())
        .trim_end()
        .to_string()
}

/// Whether frames of `wire` can be turned into `target` here.
fn converts(wire: Wire, target: FrameFormat) -> bool {
    wire == Wire::Direct
        || matches!(
            target,
            FrameFormat::Rgb24 | FrameFormat::Nv12 | FrameFormat::Yuv420p | FrameFormat::Gray8
        )
}

/// The formats to try, best first, from those the camera `offered`: the bus
/// format itself, then MJPEG and YUYV (MJPEG first from 720p up). 4:2:0
/// targets need even dimensions to be converted into.
pub fn candidates(
    target: FrameFormat,
    width: u32,
    height: u32,
    offered: &[u32],
) -> Vec<(u32, Wire)> {
    let direct = u32::from_le_bytes(fourcc(target));
    let mut out = Vec::new();
    if offered.contains(&direct) {
        out.push((direct, Wire::Direct));
    }
    let halves = matches!(target, FrameFormat::Nv12 | FrameFormat::Yuv420p);
    if halves && (!width.is_multiple_of(2) || !height.is_multiple_of(2)) {
        return out;
    }
    let mjpeg = [MJPG, JPEG].into_iter().find(|c| offered.contains(c));
    let yuyv = offered.contains(&YUYV).then_some(YUYV);
    let big = u64::from(width) * u64::from(height) >= MJPEG_PREFERRED_PIXELS;
    let order = if big {
        [(mjpeg, Wire::Mjpeg), (yuyv, Wire::Yuyv)]
    } else {
        [(yuyv, Wire::Yuyv), (mjpeg, Wire::Mjpeg)]
    };
    for (code, wire) in order {
        if let Some(code) = code.filter(|_| converts(wire, target)) {
            out.push((code, wire));
        }
    }
    out
}

/// What a JPEG is decoded to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JpegOut {
    Rgb,
    /// Full-range interleaved YCbCr 4:4:4.
    YCbCr,
    Luma,
}

/// A JPEG decoder: one frame into `out`, which holds exactly a `width` x
/// `height` image in `space`. A frame of another size is an error.
pub trait JpegDecode: Send {
    fn decode(
        &mut self,
        jpeg: &[u8],
        width: u32,
        height: u32,
        space: JpegOut,
        out: &mut [u8],
    ) -> Result<(), String>;
}

/// The software JPEG decoder.
#[derive(Debug, Default)]
pub struct SoftJpeg {
    /// A frame with the standard Huffman tables put in, reused frame to frame.
    with_tables: Vec<u8>,
}

impl JpegDecode for SoftJpeg {
    fn decode(
        &mut self,
        jpeg: &[u8],
        width: u32,
        height: u32,
        space: JpegOut,
        out: &mut [u8],
    ) -> Result<(), String> {
        // Motion JPEG may leave out its Huffman tables, which then are the
        // standard ones; the decoder fills them in only for an AVI1-marked
        // frame, so they are put in here.
        let jpeg = match missing_tables_at(jpeg) {
            Some(sos) => {
                self.with_tables.clear();
                self.with_tables.extend_from_slice(&jpeg[..sos]);
                self.with_tables.extend_from_slice(&STANDARD_DHT);
                self.with_tables.extend_from_slice(&jpeg[sos..]);
                &self.with_tables[..]
            }
            None => jpeg,
        };
        let space = match space {
            JpegOut::Rgb => ColorSpace::RGB,
            JpegOut::YCbCr => ColorSpace::YCbCr,
            JpegOut::Luma => ColorSpace::Luma,
        };
        let options = DecoderOptions::default().jpeg_set_out_colorspace(space);
        let mut d = JpegDecoder::new_with_options(ZCursor::new(jpeg), options);
        d.decode_headers().map_err(|e| e.to_string())?;
        let (w, h) = d.dimensions().ok_or("no dimensions")?;
        if (w, h) != (width as usize, height as usize) {
            return Err(format!("a {w}x{h} JPEG in a {width}x{height} stream"));
        }
        d.decode_into(out).map_err(|e| e.to_string())
    }
}

const SOI: u8 = 0xD8;
const EOI: u8 = 0xD9;
const SOS: u8 = 0xDA;
const DHT: u8 = 0xC4;

/// Where the Huffman tables belong in a JPEG that carries none: the offset of
/// its SOS marker, when no DHT segment comes before it. `None` for a JPEG
/// with tables, or one whose headers do not parse (the decoder reports that).
fn missing_tables_at(jpeg: &[u8]) -> Option<usize> {
    if !jpeg.starts_with(&[0xFF, SOI]) {
        return None;
    }
    let mut at = 2;
    loop {
        if *jpeg.get(at)? != 0xFF {
            return None;
        }
        // Any 0xFF fill bytes before the marker code.
        let mut code = at + 1;
        while *jpeg.get(code)? == 0xFF {
            code += 1;
        }
        match jpeg[code] {
            DHT | SOI | EOI => return None,
            SOS => return Some(at),
            // Markers without a length.
            0x01 | 0xD0..=0xD7 => at = code + 1,
            _ => {
                let hi = *jpeg.get(code + 1)?;
                let lo = *jpeg.get(code + 2)?;
                let len = usize::from(u16::from_be_bytes([hi, lo]));
                if len < 2 {
                    return None;
                }
                at = code + 1 + len;
            }
        }
    }
}

/// One DHT segment holding the standard Huffman tables of JPEG Annex K.3
/// (tables K.3 to K.6), the ones a Motion JPEG frame without tables implies.
#[rustfmt::skip]
const STANDARD_DHT: [u8; 420] = [
    0xFF, 0xC4, 0x01, 0xA2,
    // DC luminance (K.3)
    0x00, 0x00, 0x01, 0x05, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06,
    0x07, 0x08, 0x09, 0x0A, 0x0B,
    // AC luminance (K.5)
    0x10, 0x00, 0x02, 0x01, 0x03, 0x03, 0x02, 0x04, 0x03, 0x05, 0x05, 0x04,
    0x04, 0x00, 0x00, 0x01, 0x7D, 0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05,
    0x12, 0x21, 0x31, 0x41, 0x06, 0x13, 0x51, 0x61, 0x07, 0x22, 0x71, 0x14,
    0x32, 0x81, 0x91, 0xA1, 0x08, 0x23, 0x42, 0xB1, 0xC1, 0x15, 0x52, 0xD1,
    0xF0, 0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0A, 0x16, 0x17, 0x18, 0x19,
    0x1A, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2A, 0x34, 0x35, 0x36, 0x37, 0x38,
    0x39, 0x3A, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4A, 0x53, 0x54,
    0x55, 0x56, 0x57, 0x58, 0x59, 0x5A, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68,
    0x69, 0x6A, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7A, 0x83, 0x84,
    0x85, 0x86, 0x87, 0x88, 0x89, 0x8A, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97,
    0x98, 0x99, 0x9A, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7, 0xA8, 0xA9, 0xAA,
    0xB2, 0xB3, 0xB4, 0xB5, 0xB6, 0xB7, 0xB8, 0xB9, 0xBA, 0xC2, 0xC3, 0xC4,
    0xC5, 0xC6, 0xC7, 0xC8, 0xC9, 0xCA, 0xD2, 0xD3, 0xD4, 0xD5, 0xD6, 0xD7,
    0xD8, 0xD9, 0xDA, 0xE1, 0xE2, 0xE3, 0xE4, 0xE5, 0xE6, 0xE7, 0xE8, 0xE9,
    0xEA, 0xF1, 0xF2, 0xF3, 0xF4, 0xF5, 0xF6, 0xF7, 0xF8, 0xF9, 0xFA,
    // DC chrominance (K.4)
    0x01, 0x00, 0x03, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06,
    0x07, 0x08, 0x09, 0x0A, 0x0B,
    // AC chrominance (K.6)
    0x11, 0x00, 0x02, 0x01, 0x02, 0x04, 0x04, 0x03, 0x04, 0x07, 0x05, 0x04,
    0x04, 0x00, 0x01, 0x02, 0x77, 0x00, 0x01, 0x02, 0x03, 0x11, 0x04, 0x05,
    0x21, 0x31, 0x06, 0x12, 0x41, 0x51, 0x07, 0x61, 0x71, 0x13, 0x22, 0x32,
    0x81, 0x08, 0x14, 0x42, 0x91, 0xA1, 0xB1, 0xC1, 0x09, 0x23, 0x33, 0x52,
    0xF0, 0x15, 0x62, 0x72, 0xD1, 0x0A, 0x16, 0x24, 0x34, 0xE1, 0x25, 0xF1,
    0x17, 0x18, 0x19, 0x1A, 0x26, 0x27, 0x28, 0x29, 0x2A, 0x35, 0x36, 0x37,
    0x38, 0x39, 0x3A, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4A, 0x53,
    0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5A, 0x63, 0x64, 0x65, 0x66, 0x67,
    0x68, 0x69, 0x6A, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7A, 0x82,
    0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8A, 0x92, 0x93, 0x94, 0x95,
    0x96, 0x97, 0x98, 0x99, 0x9A, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7, 0xA8,
    0xA9, 0xAA, 0xB2, 0xB3, 0xB4, 0xB5, 0xB6, 0xB7, 0xB8, 0xB9, 0xBA, 0xC2,
    0xC3, 0xC4, 0xC5, 0xC6, 0xC7, 0xC8, 0xC9, 0xCA, 0xD2, 0xD3, 0xD4, 0xD5,
    0xD6, 0xD7, 0xD8, 0xD9, 0xDA, 0xE2, 0xE3, 0xE4, 0xE5, 0xE6, 0xE7, 0xE8,
    0xE9, 0xEA, 0xF2, 0xF3, 0xF4, 0xF5, 0xF6, 0xF7, 0xF8, 0xF9, 0xFA,
];

/// The JPEG in a buffer: through its end-of-image marker, past any zero
/// padding the driver leaves after it. `None` when the marker is not there,
/// as in a frame the camera cut short: the decoder would fill the missing
/// rows with grey and report success.
fn jpeg_body(src: &[u8]) -> Option<&[u8]> {
    let end = src.iter().rposition(|&b| b != 0)? + 1;
    let body = &src[..end];
    body.ends_with(&[0xFF, EOI]).then_some(body)
}

/// Why a frame without its end-of-image marker is refused.
const CUT_SHORT: &str = "no end-of-image marker: the frame was cut short";

/// The JPEG decoder for a new capture.
pub fn jpeg_decoder() -> Box<dyn JpegDecode> {
    Box::<SoftJpeg>::default()
}

/// Why a buffer did not become a frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConvertError {
    /// The buffer is shorter than the negotiated frame.
    Short,
    /// The compressed frame did not decode (a truncated or corrupt JPEG).
    Decode(String),
}

/// How long a camera may deliver only buffers that fail to convert before its
/// capture ends with the reason. Well inside the engine's input silence
/// timeout, so a camera streaming frames this host cannot decode reads as a
/// source error, which recovery leaves alone, and never as a silent device,
/// which recovery answers with a USB rebind.
pub const CONVERT_FAILURE_LIMIT: Duration = Duration::from_secs(3);

/// The run of buffers that failed to convert since the last good frame.
#[derive(Debug, Default)]
pub struct FailureRun {
    since_ns: Option<u64>,
}

impl FailureRun {
    /// A buffer became a frame: the run is over.
    pub fn converted(&mut self) {
        self.since_ns = None;
    }

    /// A buffer that arrived at `now_ns` (host monotonic) failed with
    /// `error`. Once buffers have failed for [`CONVERT_FAILURE_LIMIT`] with
    /// none converted, the error the capture ends with.
    pub fn failed(&mut self, now_ns: u64, error: &ConvertError) -> Option<io::Error> {
        let since = *self.since_ns.get_or_insert(now_ns);
        let limit = CONVERT_FAILURE_LIMIT;
        if now_ns.saturating_sub(since) < limit.as_nanos() as u64 {
            return None;
        }
        let secs = limit.as_secs();
        let reason = match error {
            ConvertError::Short => {
                format!("frames arrive shorter than the negotiated frame for {secs} s")
            }
            ConvertError::Decode(e) => format!("frames arrive but none decoded for {secs} s: {e}"),
        };
        Some(io::Error::new(io::ErrorKind::InvalidData, reason))
    }
}

/// Turns one camera's buffers into frame-bus frames.
pub struct Converter {
    wire: Wire,
    target: FrameFormat,
    width: usize,
    height: usize,
    /// Bytes per row of the camera's buffer (padded), for raw formats.
    stride: usize,
    jpeg: Box<dyn JpegDecode>,
    /// Decoded 4:4:4 YCbCr, reused frame to frame (MJPEG to 4:2:0 only).
    scratch: Vec<u8>,
}

impl Converter {
    pub fn new(
        wire: Wire,
        target: FrameFormat,
        width: u32,
        height: u32,
        stride: u32,
        jpeg: Box<dyn JpegDecode>,
    ) -> Self {
        Self {
            wire,
            target,
            width: width as usize,
            height: height as usize,
            // A driver that reports no row pitch packs its rows.
            stride: match (stride, wire) {
                (0, Wire::Yuyv) => width as usize * 2,
                _ => stride as usize,
            },
            jpeg,
            scratch: Vec::new(),
        }
    }

    /// Fill `out` (exactly one bus frame) from the buffer bytes `src`.
    pub fn convert(&mut self, src: &[u8], out: &mut [u8]) -> Result<(), ConvertError> {
        let (w, h) = (self.width, self.height);
        let nv12 = self.target == FrameFormat::Nv12;
        let ok = match self.wire {
            Wire::Direct => {
                let row = row_bytes(self.target, self.width as u32);
                copy_rows(src, self.stride, row, out)
            }
            Wire::Yuyv => match self.target {
                FrameFormat::Rgb24 => pixels::yuyv_to_rgb24(src, self.stride, w, h, out),
                FrameFormat::Gray8 => pixels::yuyv_to_gray8(src, self.stride, w, h, out),
                _ => pixels::yuyv_to_420(src, self.stride, w, h, nv12, out),
            },
            Wire::Mjpeg => return self.decode_jpeg(src, out),
        };
        ok.then_some(()).ok_or(ConvertError::Short)
    }

    fn decode_jpeg(&mut self, src: &[u8], out: &mut [u8]) -> Result<(), ConvertError> {
        let (w, h) = (self.width as u32, self.height as u32);
        let Some(src) = jpeg_body(src) else {
            return Err(ConvertError::Decode(CUT_SHORT.into()));
        };
        let direct = match self.target {
            FrameFormat::Rgb24 => Some(JpegOut::Rgb),
            FrameFormat::Gray8 => Some(JpegOut::Luma),
            _ => None,
        };
        if let Some(space) = direct {
            return self
                .jpeg
                .decode(src, w, h, space, out)
                .map_err(ConvertError::Decode);
        }
        self.scratch.resize(self.width * self.height * 3, 0);
        self.jpeg
            .decode(src, w, h, JpegOut::YCbCr, &mut self.scratch)
            .map_err(ConvertError::Decode)?;
        let nv12 = self.target == FrameFormat::Nv12;
        pixels::ycbcr444_to_420(&self.scratch, self.width, self.height, nv12, out)
            .then_some(())
            .ok_or(ConvertError::Short)
    }
}
