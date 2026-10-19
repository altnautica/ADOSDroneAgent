//! Encoder backend selection and encoder labelling.

use super::*;
use std::path::Path;

/// Pick the encoder backend for a camera, given which binaries are present.
///
/// CSI → rpicam-vid (fallback ffmpeg). USB/IP → ffmpeg (the Rockchip
/// `mpph264enc` VPU path is disabled, see below; fallback gstreamer). The
/// binary-presence flags are taken as inputs to keep this pure.
///
/// # Why `mpph264enc` is disabled, and exactly how to retest it
///
/// The reason on record is "emits corrupt frames". That is not evidence of a
/// broken VPU — the RK3588 VPU is the single largest latency win available in
/// this fleet — it is the signature of a known upstream rkmpp *rate-control*
/// defect: `rc-mode=cbr` does not honour the requested bitrate on RK3588
/// (rockchip-linux/mpp#429), and the bitstream that falls out of it trips
/// `h264parse` with "NAL unit of length 0" (rockchip-linux/mpp#177) — i.e.
/// exactly "corrupt frames".
///
/// Both prior attempts almost certainly ran in CBR without knowing it:
/// `MppEncRcMode` is `VBR=0, CBR=1, FIXQP=2`, so the `rc-mode=1` this builder
/// historically emitted *was* CBR, despite the comment beside it claiming VBR.
///
/// Retest procedure — run exactly this. Do not improvise, and do not retry the
/// blind `rc-mode=1` configuration a third time:
///
/// 1. Pin `rc-mode=vbr` **by name**, never by number. The numeric enum has
///    already been misread once and cost two retest cycles.
/// 2. Set `bps` and `bps-max` explicitly, both sized to the radio's link
///    budget. Leaving either at 0 makes the element auto-derive
///    `bps = width * height / 8 * fps` and `bps-max = bps * 17/16`, so the
///    peak silently escapes the link budget.
/// 3. Feed the encoder NV12. Any other input format forces a conversion it
///    does not want.
/// 4. **No RGA anywhere in the path** — no `rgaconvert`, and no RGA-backed
///    `rotation` / `width` / `height` property on the element. Rockchip's own
///    GStreamer user guide (RK-YH-YF-921) states RGA is abnormal on RK3588 and
///    recommends against using it, so an RGA in the pipeline invalidates the
///    result whichever way it comes out.
/// 5. Run on a BSP 6.1 or 5.10 kernel. Mainline carries no rkmpp VPU path, so
///    a mainline-kernel run tests nothing.
///
/// Only a sustained clean run under *that* configuration earns
/// `EncoderKind::Gstreamer` for USB/IP on a board with `mpph264enc`. Until
/// then the disable stays in force.
pub fn detect_encoder_for_camera(
    camera_type: CameraType,
    has_rpicam: bool,
    has_ffmpeg: bool,
    has_gst_launch: bool,
) -> Option<EncoderKind> {
    match camera_type {
        CameraType::Csi => {
            if has_rpicam {
                Some(EncoderKind::RpicamVid)
            } else if has_ffmpeg {
                Some(EncoderKind::Ffmpeg)
            } else {
                None
            }
        }
        CameraType::Usb | CameraType::Ip | CameraType::Framebus => {
            // mpph264enc (Rockchip VPU) is disabled for USB/IP — fall back to
            // ffmpeg libx264, then gstreamer x264enc. See this function's doc
            // comment for the exact retest procedure before re-enabling it. A
            // frame-bus source takes the VPU in `resolve_kind` instead.
            if has_ffmpeg {
                Some(EncoderKind::Ffmpeg)
            } else if has_gst_launch {
                Some(EncoderKind::Gstreamer)
            } else {
                None
            }
        }
    }
}

/// Probe `program` on PATH (best-effort `which`). Used by callers that want to
/// drive [`detect_encoder_for_camera`] from the live environment.
pub fn binary_present(program: &str) -> bool {
    if let Ok(path) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = Path::new(&dir).join(program);
            if candidate.is_file() {
                return true;
            }
        }
    }
    false
}

/// The codec elements the label is derived from, hardware first. An argv can
/// only ever carry one of them, so the order is a tie-break that never fires
/// in practice; it is fixed so the emitted value is deterministic.
pub(super) const ENCODER_ELEMENTS: [&str; 8] = [
    "h264_v4l2m2m",
    "hevc_v4l2m2m",
    "mpph264enc",
    "mpph265enc",
    "omxh264videoenc",
    "x264enc",
    "libx264",
    "libx265",
];

/// The on-wire encoder identity for the `camera-state.json` sidecar.
///
/// [`EncoderKind`] cannot answer this: `Ffmpeg` covers both the hardware
/// `h264_v4l2m2m` path and the software `libx264` fallback, so a label derived
/// from the kind tells an operator nothing about whether the node fell back to
/// software — which is the one question the sidecar field exists to answer. The
/// codec element appears only in the built command, so that is what is scanned.
///
/// Values: `rpicam-vid`, and `<family>-<element>` for `ffmpeg` / `gstreamer`
/// (e.g. `ffmpeg-h264_v4l2m2m`, `gstreamer-x264enc`), falling back to
/// `<family>-unknown` when no known element is present.
pub fn encoder_label(kind: EncoderKind, cmd: &[String]) -> String {
    if kind == EncoderKind::RpicamVid {
        return "rpicam-vid".to_string();
    }
    let family = match kind {
        EncoderKind::Gstreamer => "gstreamer",
        _ => "ffmpeg",
    };
    for token in cmd {
        // The rpicam / gstreamer forms are `bash -c "<pipeline>"`,
        // which carries the whole command in one token, so argv position alone
        // cannot order the elements: match as a substring and take the earliest
        // hit within the token.
        let hit = ENCODER_ELEMENTS
            .iter()
            .filter_map(|el| token.find(el).map(|at| (at, *el)))
            .min();
        if let Some((_, element)) = hit {
            return format!("{family}-{element}");
        }
    }
    format!("{family}-unknown")
}

/// Whether [`encoder_label`] names a hardware encoder. Published as its own
/// sidecar field so no consumer has to sniff the label string to find out that
/// a node is burning its CPU on the software fallback.
pub fn encoder_is_hardware(label: &str) -> bool {
    label == "rpicam-vid"
        || label.contains("v4l2m2m")
        || label.contains("mpp")
        || label.contains("omxh264videoenc")
}
