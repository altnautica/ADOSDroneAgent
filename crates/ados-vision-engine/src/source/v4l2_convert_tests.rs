//! Format negotiation and buffer conversion against a fake capture node that
//! behaves like a UVC driver.

use std::io;

use ados_protocol::vision_status::SourceError;

use super::super::convert::{
    candidates, ConvertError, Converter, FailureRun, SoftJpeg, Wire, CONVERT_FAILURE_LIMIT, JPEG,
    MJPG, YUYV,
};
use super::super::negotiate::{negotiate, FormatNode};
use super::super::pixels;
use super::super::v4l2_sys as sys;
use super::*;

/// A 16x16 baseline JFIF (4:2:2), four solid 8x8 quadrants: red, green,
/// blue, gray, as an MJPEG camera delivers a frame.
const QUADRANTS: &[u8] = include_bytes!("../../tests/fixtures/quadrants-16x16.jpg");
const QUADRANT_RGB: [((usize, usize), [u8; 3]); 4] = [
    ((3, 3), [200, 30, 30]),
    ((12, 3), [30, 200, 30]),
    ((3, 12), [30, 30, 200]),
    ((12, 12), [128, 128, 128]),
];
const H264: u32 = u32::from_le_bytes(*b"H264");
const RGB3: u32 = u32::from_le_bytes(*b"RGB3");
/// The row padding the fake driver adds to raw formats.
const PAD: u32 = 16;

/// A capture node offering each format at the listed sizes. Like a UVC
/// driver, `S_FMT` snaps an unknown format to the first and an unknown size
/// to the format's first.
struct FakeNode {
    formats: Vec<(u32, Vec<(u32, u32)>)>,
    set: Vec<u32>,
}

impl FakeNode {
    fn new(formats: &[(u32, &[(u32, u32)])]) -> Self {
        Self {
            formats: formats.iter().map(|(c, s)| (*c, s.to_vec())).collect(),
            set: Vec::new(),
        }
    }
}

impl FormatNode for FakeNode {
    fn offered(&mut self, _buf_type: u32) -> io::Result<Vec<u32>> {
        Ok(self.formats.iter().map(|f| f.0).collect())
    }

    fn set_format(&mut self, f: &mut sys::Format) -> io::Result<()> {
        let pix = f.pix();
        let (code, sizes) = self
            .formats
            .iter()
            .find(|(c, _)| *c == pix.pixelformat)
            .unwrap_or(&self.formats[0]);
        let (width, height) = if sizes.contains(&(pix.width, pix.height)) {
            (pix.width, pix.height)
        } else {
            sizes[0]
        };
        let bytesperline = match *code {
            MJPG => 0,
            YUYV => width * 2 + PAD,
            _ => width * 3,
        };
        self.set.push(*code);
        *f = sys::Format::single(sys::PixFormat {
            width,
            height,
            pixelformat: *code,
            bytesperline,
            ..pix
        });
        Ok(())
    }
}

fn settings(format: FrameFormat, width: u32, height: u32) -> V4l2Settings {
    V4l2Settings {
        device: "/dev/video9".into(),
        width,
        height,
        fps: 30,
        format,
        exposure_us: None,
        gain: None,
    }
}

fn open(node: &mut FakeNode, s: &V4l2Settings) -> io::Result<(Wire, Converter)> {
    let (layout, wire) = negotiate(
        node,
        sys::BUF_TYPE_VIDEO_CAPTURE,
        s,
        "/dev/video9 (uvcvideo)",
    )?;
    let conv = Converter::new(
        wire,
        s.format,
        s.width,
        s.height,
        layout.stride,
        Box::<SoftJpeg>::default(),
    );
    Ok((wire, conv))
}

/// BT.601 full-range RGB as limited-range Y, Cb, Cr.
fn limited_ycbcr(rgb: [u8; 3]) -> [f64; 3] {
    let [r, g, b] = rgb.map(f64::from);
    let y = 0.299 * r + 0.587 * g + 0.114 * b;
    let cb = 128.0 + (b - y) * 0.564;
    let cr = 128.0 + (r - y) * 0.713;
    [
        16.0 + y * 219.0 / 255.0,
        128.0 + (cb - 128.0) * 224.0 / 255.0,
        128.0 + (cr - 128.0) * 224.0 / 255.0,
    ]
}

fn near(got: u8, want: f64, tol: f64, what: &str) {
    assert!(
        (f64::from(got) - want).abs() <= tol,
        "{what}: got {got}, want {want:.1}"
    );
}

#[test]
fn an_mjpeg_only_camera_opens_and_decodes_to_rgb24() {
    let mut node = FakeNode::new(&[(MJPG, &[(16, 16)])]);
    let (wire, mut conv) = open(&mut node, &settings(FrameFormat::Rgb24, 16, 16)).unwrap();
    assert_eq!((wire, node.set.as_slice()), (Wire::Mjpeg, &[MJPG][..]));
    let mut out = vec![0u8; FrameFormat::Rgb24.frame_bytes(16, 16)];
    conv.convert(QUADRANTS, &mut out).unwrap();
    for ((x, y), rgb) in QUADRANT_RGB {
        let px = &out[(y * 16 + x) * 3..][..3];
        for c in 0..3 {
            near(
                px[c],
                f64::from(rgb[c]),
                4.0,
                &format!("rgb ({x},{y})[{c}]"),
            );
        }
    }
}

#[test]
fn an_mjpeg_only_camera_decodes_to_limited_range_nv12_yuv420p_and_gray() {
    for format in [FrameFormat::Nv12, FrameFormat::Yuv420p] {
        let mut node = FakeNode::new(&[(MJPG, &[(16, 16)])]);
        let (_, mut conv) = open(&mut node, &settings(format, 16, 16)).unwrap();
        let mut out = vec![0u8; format.frame_bytes(16, 16)];
        conv.convert(QUADRANTS, &mut out).unwrap();
        let (luma, chroma) = out.split_at(256);
        for ((x, y), rgb) in QUADRANT_RGB {
            let [yy, cb, cr] = limited_ycbcr(rgb);
            near(luma[y * 16 + x], yy, 3.0, "luma");
            let (u, v) = if format == FrameFormat::Nv12 {
                let i = (y / 2) * 16 + (x / 2) * 2;
                (chroma[i], chroma[i + 1])
            } else {
                let i = (y / 2) * 8 + x / 2;
                (chroma[i], chroma[64 + i])
            };
            near(u, cb, 4.0, "cb");
            near(v, cr, 4.0, "cr");
        }
    }
    let mut node = FakeNode::new(&[(MJPG, &[(16, 16)])]);
    let (_, mut conv) = open(&mut node, &settings(FrameFormat::Gray8, 16, 16)).unwrap();
    let mut out = vec![0u8; 256];
    conv.convert(QUADRANTS, &mut out).unwrap();
    near(
        out[3 * 16 + 3],
        0.299 * 200.0 + 0.587 * 30.0 + 0.114 * 30.0,
        3.0,
        "gray",
    );
}

/// Two rows of a 4x2 YUYV frame: black, white, then two BT.601 reds, the
/// second row's chroma shifted so the vertical mean is checked.
fn yuyv_frame(stride: usize) -> Vec<u8> {
    let rows: [[u8; 8]; 2] = [
        [16, 128, 235, 128, 81, 90, 81, 240],
        [16, 130, 235, 126, 81, 92, 81, 238],
    ];
    let mut buf = vec![0xAA; stride * 2];
    for (r, row) in rows.iter().enumerate() {
        buf[r * stride..r * stride + 8].copy_from_slice(row);
    }
    buf
}

#[test]
fn a_yuyv_only_camera_opens_and_converts_to_rgb24_and_nv12() {
    let stride = (4 * 2 + PAD) as usize;
    let mut node = FakeNode::new(&[(YUYV, &[(4, 2)])]);
    let (wire, mut conv) = open(&mut node, &settings(FrameFormat::Rgb24, 4, 2)).unwrap();
    assert_eq!(wire, Wire::Yuyv);
    let mut rgb = vec![0u8; 4 * 2 * 3];
    conv.convert(&yuyv_frame(stride), &mut rgb).unwrap();
    assert_eq!(&rgb[..12], &[0, 0, 0, 255, 255, 255, 255, 0, 0, 255, 0, 0]);

    let mut node = FakeNode::new(&[(YUYV, &[(4, 2)])]);
    let (_, mut conv) = open(&mut node, &settings(FrameFormat::Nv12, 4, 2)).unwrap();
    let mut nv12 = vec![0u8; FrameFormat::Nv12.frame_bytes(4, 2)];
    conv.convert(&yuyv_frame(stride), &mut nv12).unwrap();
    // Luma as delivered, row padding dropped; chroma the mean of both rows.
    assert_eq!(&nv12[..8], &[16, 235, 81, 81, 16, 235, 81, 81]);
    assert_eq!(&nv12[8..], &[129, 127, 91, 239]);

    let mut node = FakeNode::new(&[(YUYV, &[(4, 2)])]);
    let (_, mut conv) = open(&mut node, &settings(FrameFormat::Yuv420p, 4, 2)).unwrap();
    let mut i420 = vec![0u8; 12];
    conv.convert(&yuyv_frame(stride), &mut i420).unwrap();
    assert_eq!(&i420[8..], &[129, 91, 127, 239]);

    // A buffer one byte short of the last row is not a frame.
    let short = &yuyv_frame(stride)[..stride + 7];
    assert_eq!(conv.convert(short, &mut i420), Err(ConvertError::Short));
}

#[test]
fn the_bus_format_wins_then_mjpeg_from_720p_up_else_yuyv() {
    let all = [YUYV, MJPG, RGB3];
    assert_eq!(
        candidates(FrameFormat::Rgb24, 640, 480, &all)[0],
        (RGB3, Wire::Direct)
    );
    let uvc = [YUYV, MJPG];
    assert_eq!(
        candidates(FrameFormat::Rgb24, 1280, 720, &uvc),
        vec![(MJPG, Wire::Mjpeg), (YUYV, Wire::Yuyv)]
    );
    assert_eq!(
        candidates(FrameFormat::Nv12, 640, 480, &uvc),
        vec![(YUYV, Wire::Yuyv), (MJPG, Wire::Mjpeg)]
    );
    // `JPEG` is taken as MJPEG; a 16-bit or Bayer bus is never converted.
    assert_eq!(
        candidates(FrameFormat::Gray8, 1920, 1080, &[JPEG]),
        vec![(JPEG, Wire::Mjpeg)]
    );
    assert!(candidates(FrameFormat::Gray16, 1280, 720, &uvc).is_empty());
    // 4:2:0 needs an even size to be converted into.
    assert!(candidates(FrameFormat::Nv12, 641, 480, &uvc).is_empty());

    // Both offered at 720p: MJPEG is the one set on the node.
    let mut node = FakeNode::new(&[(YUYV, &[(1280, 720)]), (MJPG, &[(1280, 720)])]);
    let (wire, _) = open(&mut node, &settings(FrameFormat::Rgb24, 1280, 720)).unwrap();
    assert_eq!((wire, node.set.as_slice()), (Wire::Mjpeg, &[MJPG][..]));
}

#[test]
fn a_camera_with_no_usable_format_is_refused_with_the_reason() {
    let mut node = FakeNode::new(&[(H264, &[(1280, 720)])]);
    let err = open(&mut node, &settings(FrameFormat::Rgb24, 1280, 720))
        .err()
        .unwrap();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    assert_eq!(
        err.to_string(),
        "/dev/video9 (uvcvideo) offers [H264]; rgb24 frames need RGB3 from the camera, or \
         MJPG/YUYV to convert to rgb24, nv12, yuv420p or gray8 (4:2:0 at an even size)"
    );
    assert!(node.set.is_empty());

    // Offered, but not at the configured size: every attempt is named.
    let mut node = FakeNode::new(&[(MJPG, &[(640, 480)]), (YUYV, &[(320, 240)])]);
    let err = open(&mut node, &settings(FrameFormat::Nv12, 1280, 720))
        .err()
        .unwrap();
    assert_eq!(
        err.to_string(),
        "/dev/video9 (uvcvideo) offers [MJPG, YUYV] but none at 1280x720 for nv12 frames: \
         MJPG gave 640x480 MJPG; YUYV gave 320x240 YUYV"
    );
}

#[test]
fn a_corrupt_or_mis_sized_jpeg_is_a_decode_error() {
    let mut node = FakeNode::new(&[(MJPG, &[(16, 16), (32, 32)])]);
    let (_, mut conv) = open(&mut node, &settings(FrameFormat::Rgb24, 32, 32)).unwrap();
    let mut out = vec![0u8; 32 * 32 * 3];
    // A 16x16 picture in a 32x32 stream.
    assert!(matches!(
        conv.convert(QUADRANTS, &mut out),
        Err(ConvertError::Decode(_))
    ));
    // Garbage.
    assert!(matches!(
        conv.convert(&[0xff, 0xd8, 0x00], &mut out),
        Err(ConvertError::Decode(_))
    ));
}

/// `jpeg` with its DHT segments taken out, as a UVC camera that relies on the
/// standard Huffman tables sends a frame.
fn without_huffman_tables(jpeg: &[u8]) -> Vec<u8> {
    let mut out = jpeg[..2].to_vec();
    let mut at = 2;
    while jpeg[at + 1] != 0xDA {
        let len = usize::from(u16::from_be_bytes([jpeg[at + 2], jpeg[at + 3]]));
        if jpeg[at + 1] != 0xC4 {
            out.extend_from_slice(&jpeg[at..at + 2 + len]);
        }
        at += 2 + len;
    }
    out.extend_from_slice(&jpeg[at..]);
    out
}

#[test]
fn an_mjpeg_frame_without_huffman_tables_decodes_with_the_standard_ones() {
    // The fixture's tables are the standard ones, so the bare frame is the
    // same picture.
    let bare = without_huffman_tables(QUADRANTS);
    assert!(bare.len() < QUADRANTS.len());
    assert!(!bare.windows(2).any(|w| w == [0xFF, 0xC4]));
    let mut node = FakeNode::new(&[(MJPG, &[(16, 16)])]);
    for format in [FrameFormat::Rgb24, FrameFormat::Nv12, FrameFormat::Gray8] {
        let (_, mut conv) = open(&mut node, &settings(format, 16, 16)).unwrap();
        let n = format.frame_bytes(16, 16);
        let mut want = vec![0u8; n];
        conv.convert(QUADRANTS, &mut want).unwrap();
        // Twice: the second goes through the warm table buffer.
        for _ in 0..2 {
            let mut got = vec![0u8; n];
            let r = conv.convert(&bare, &mut got);
            assert_eq!(r, Ok(()), "{format:?}");
            assert_eq!(got, want, "{format:?}");
        }
        // A frame with its tables still decodes after one without.
        let mut again = vec![0u8; n];
        conv.convert(QUADRANTS, &mut again).unwrap();
        assert_eq!(again, want, "{format:?}");
    }
}

#[test]
fn a_frame_cut_short_is_a_decode_error_and_zero_padding_is_not() {
    let mut node = FakeNode::new(&[(MJPG, &[(16, 16)])]);
    let (_, mut conv) = open(&mut node, &settings(FrameFormat::Rgb24, 16, 16)).unwrap();
    let mut want = vec![0u8; 16 * 16 * 3];
    conv.convert(QUADRANTS, &mut want).unwrap();
    // The driver's bytesused may run past the end-of-image marker in zeros.
    let mut padded = QUADRANTS.to_vec();
    padded.extend_from_slice(&[0; 64]);
    let mut got = vec![0u8; 16 * 16 * 3];
    conv.convert(&padded, &mut got).unwrap();
    assert_eq!(got, want);
    // A frame the camera did not finish: the scan stops part way and the
    // end-of-image marker never comes, padded or not.
    let cut = &QUADRANTS[..QUADRANTS.len() - 40];
    let mut cut_padded = cut.to_vec();
    cut_padded.extend_from_slice(&[0; 64]);
    for frame in [cut, &cut_padded[..]] {
        let r = conv.convert(frame, &mut got);
        assert!(matches!(r, Err(ConvertError::Decode(_))), "{r:?}");
    }
}

#[test]
fn frames_that_never_decode_end_the_capture_with_a_decode_error_not_silence() {
    const S: u64 = 1_000_000_000;
    let bad = ConvertError::Decode("no end-of-image marker".into());
    let mut run = FailureRun::default();
    assert!(run.failed(10 * S, &bad).is_none());
    assert!(run.failed(12 * S, &bad).is_none());
    // A good frame ends the run.
    run.converted();
    assert!(run.failed(13 * S, &bad).is_none());
    assert!(run.failed(15 * S, &bad).is_none());
    let Some(e) = run.failed(16 * S, &bad) else {
        panic!("3 s of frames that do not decode");
    };
    assert_eq!(e.kind(), io::ErrorKind::InvalidData);
    // The reason reaches the status sidecar before the engine's silence
    // timeout could, and does not read as silence, which recovery answers
    // with a USB rebind.
    assert!(CONVERT_FAILURE_LIMIT < crate::work_status::INPUT_SILENCE_TIMEOUT);
    let reported = SourceError {
        camera: "front".into(),
        error: format!("{:#}", anyhow::Error::from(e)),
    };
    assert!(!reported.is_silence());
    assert!(reported.error.contains("no end-of-image marker"));

    // Buffers that keep arriving short are a source error too.
    let mut run = FailureRun::default();
    assert!(run.failed(0, &ConvertError::Short).is_none());
    assert!(run.failed(3 * S, &ConvertError::Short).is_some());
}

#[test]
fn range_conversions_hit_their_endpoints() {
    assert_eq!(pixels::rgb_from_limited(16, 128, 128), [0, 0, 0]);
    assert_eq!(pixels::rgb_from_limited(235, 128, 128), [255, 255, 255]);
    assert_eq!(
        (
            pixels::gray_from_limited(16),
            pixels::gray_from_limited(235)
        ),
        (0, 255)
    );
    assert_eq!(
        (pixels::luma_to_limited(0), pixels::luma_to_limited(255)),
        (16, 235)
    );
    assert_eq!([0, 128, 255].map(pixels::chroma_to_limited), [16, 128, 240]);
}
