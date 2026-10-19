//! Rotation and flip filter insertion on the ffmpeg and GStreamer builders.

use super::*;

// --- orientation filters, ffmpeg builder --------------------------

#[test]
fn ffmpeg_rotation_and_flip_filter_insertion() {
    // Every rotation value + hflip/vflip yields the exact `-vf` chain. An
    // unrotated, unflipped camera emits NO -vf (covered by the existing
    // fixtures, which pass untouched). 90=transpose=1, 180=transpose=2,
    // transpose=2, 270=transpose=2; then hflip/vflip append.
    let ff = |rot: u32, h: bool, v: bool| {
        build(
            &params_cfg(
                EncoderKind::Ffmpeg,
                1280,
                720,
                30,
                4000,
                "auto",
                rot,
                h,
                v,
                0,
            ),
            "/dev/video1",
            RTSP_OUT,
            &usb_mjpeg(),
            &rockchip(),
            false,
        )
    };
    assert_eq!(ff(90, false, false), expected("ffmpeg_rot90"));
    assert_eq!(ff(180, false, false), expected("ffmpeg_rot180"));
    assert_eq!(ff(270, false, false), expected("ffmpeg_rot270"));
    assert_eq!(ff(0, true, false), expected("ffmpeg_hflip"));
    assert_eq!(ff(0, false, true), expected("ffmpeg_vflip"));
    assert_eq!(ff(180, true, false), expected("ffmpeg_rot180_hflip"));
    for (rot, want) in [
        (90, "transpose=1"),
        (180, "transpose=2,transpose=2"),
        (270, "transpose=2"),
    ] {
        let vf = ff(rot, false, false);
        let fi = vf.iter().position(|t| t == "-vf").unwrap();
        assert_eq!(vf[fi + 1], want);
    }
    let flip = ff(0, true, true);
    let fi = flip.iter().position(|t| t == "-vf").unwrap();
    assert_eq!(flip[fi + 1], "hflip,vflip");
}

// --- orientation filters, gstreamer builder -----------------------

#[test]
fn gst_x264_rotation_and_flip_filter_insertion() {
    // videoflip element chain on the software x264 gstreamer path, with the
    // legacy (non-OMX) capture prefix otherwise unchanged.
    let g = |rot: u32, h: bool, v: bool| {
        build(
            &params_cfg(
                EncoderKind::Gstreamer,
                1280,
                720,
                30,
                4000,
                "auto",
                rot,
                h,
                v,
                0,
            ),
            "/dev/video1",
            RTSP_OUT,
            &usb_mjpeg(),
            &non_rk_sw(),
            false,
        )
    };
    assert_eq!(g(90, false, false), expected("gst_x264_rot90"));
    assert_eq!(g(180, false, false), expected("gst_x264_rot180"));
    assert_eq!(g(270, false, false), expected("gst_x264_rot270"));
    assert_eq!(g(0, true, false), expected("gst_x264_hflip"));
    assert_eq!(g(0, false, true), expected("gst_x264_vflip"));
    for (rot, method) in [
        (90, "method=clockwise"),
        (180, "method=rotate-180"),
        (270, "method=counterclockwise"),
    ] {
        let tok = g(rot, false, false).to_vec();
        // The GStreamer pipeline is token-split on spaces, so `videoflip
        // method=…` lands as two tokens; check the method token is present
        // and is preceded by a `videoflip` element token.
        assert!(tok.iter().any(|t| t == method), "missing {method}");
        assert!(
            tok.iter()
                .position(|t| t == method)
                .map(|i| i >= 1 && tok[i - 1] == "videoflip")
                .unwrap_or(false),
            "{method} not preceded by a videoflip element"
        );
    }
    let flip = g(0, true, true);
    assert!(flip.iter().any(|t| t == "method=horizontal-flip"));
    assert!(flip.iter().any(|t| t == "method=vertical-flip"));
}
