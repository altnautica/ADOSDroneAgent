use super::*;
use serde_json::Value;

use super::rpicam::{select_input_format, shell_quote};
use ados_protocol::hwcaps::{EncoderDevice, Probed};

/// The frozen argv vectors this builder must reproduce byte for byte.
///
/// This file IS the reference for the shipped argv. It is GENERATED from
/// [`fixture_cases`] — never hand-edited — by
///
/// ```text
/// cargo test -p ados-video --lib regenerate_encoder_fixtures -- --ignored --exact
/// ```
///
/// (on this workspace's macOS dev host, prefix
/// `DEVELOPER_DIR=/Library/Developer/CommandLineTools`).
///
/// [`fixture_table_matches_the_file`] fails whenever the two disagree, so a
/// hand-edit that does not correspond to a real builder output cannot
/// survive, and a deliberate argv change is one command away from a
/// reviewable diff. Every environment the cases use is fully pinned
/// (including the CPU count), so the output is identical on any host.
const FIXTURES: &str = include_str!("../../tests/encoder_fixtures.json");

const RTSP_OUT: &str = "rtsp://127.0.0.1:8554/main";
const UDP_OUT: &str = "udp://127.0.0.1:5600";

fn fixtures() -> Value {
    serde_json::from_str(FIXTURES).expect("encoder_fixtures.json parses")
}

fn expected(name: &str) -> Vec<String> {
    let v = fixtures();
    v.get(name)
        .unwrap_or_else(|| panic!("fixture {name:?} present"))
        .as_array()
        .unwrap_or_else(|| panic!("fixture {name:?} is an array"))
        .iter()
        .map(|x| x.as_str().expect("fixture token is a string").to_string())
        .collect()
}

fn csi() -> CameraInfo {
    CameraInfo {
        camera_type: CameraType::Csi,
        device_path: "/dev/video0".into(),
        capabilities: vec!["h264".into(), "mjpeg".into()],
    }
}
fn usb_mjpeg() -> CameraInfo {
    CameraInfo {
        camera_type: CameraType::Usb,
        device_path: "/dev/video1".into(),
        capabilities: vec!["mjpeg".into(), "yuyv".into()],
    }
}
fn usb_yuyv() -> CameraInfo {
    CameraInfo {
        camera_type: CameraType::Usb,
        device_path: "/dev/video2".into(),
        capabilities: vec!["yuyv".into()],
    }
}
fn ip_cam() -> CameraInfo {
    CameraInfo {
        camera_type: CameraType::Ip,
        device_path: "rtsp://10.0.0.9:554/live".into(),
        capabilities: vec!["rtsp".into()],
    }
}

use ados_protocol::hwcaps::{AbsenceReason, Evidence};

/// A probed-Present HW encoder device (a real V4L2 node passed trial-init).
fn hw_present() -> Probed<EncoderDevice> {
    Probed::present(
        EncoderDevice {
            node: "/dev/video11".into(),
            fourcc: *b"H264",
        },
        Evidence::TrialInit {
            node: "/dev/video11".into(),
            ms: 120,
        },
    )
}

/// The board advertised an encoder wrapper but no real device exists. This
/// is the live-bug case the probe-first HAL fixes: the builder must pick
/// software libx264, never the wrapper.
fn hw_node_missing() -> Probed<EncoderDevice> {
    Probed::absent(AbsenceReason::NodeMissing)
}

/// The CPU count every fixture environment is pinned to: four cores, which
/// is what the fleet's boards actually have (Pi 4 / CM4, CM5, RK3588) and
/// what the sliced-threads levers are gated on. Pinned rather than probed
/// so the generated argv is identical on any build host.
const FIXTURE_CPUS: u32 = 4;

fn rockchip() -> EncoderEnv {
    EncoderEnv {
        hw_h264: hw_node_missing(),
        has_mpph264enc: false,
        has_omxh264videoenc: false,
        has_rtspclientsink: true,
        encoder_api: "unknown".into(),
        pi5_class: false,
        cpu_threads: FIXTURE_CPUS,
    }
}
fn non_rk_sw() -> EncoderEnv {
    EncoderEnv {
        hw_h264: hw_node_missing(),
        has_mpph264enc: false,
        has_omxh264videoenc: false,
        has_rtspclientsink: true,
        encoder_api: "unknown".into(),
        pi5_class: false,
        cpu_threads: FIXTURE_CPUS,
    }
}
fn non_rk_hw() -> EncoderEnv {
    EncoderEnv {
        hw_h264: hw_present(),
        has_mpph264enc: false,
        has_omxh264videoenc: false,
        has_rtspclientsink: true,
        encoder_api: "unknown".into(),
        pi5_class: false,
        cpu_threads: FIXTURE_CPUS,
    }
}
fn rk_mpp() -> EncoderEnv {
    EncoderEnv {
        hw_h264: hw_node_missing(),
        has_mpph264enc: true,
        has_omxh264videoenc: false,
        has_rtspclientsink: true,
        encoder_api: "rkmpp".into(),
        pi5_class: false,
        cpu_threads: FIXTURE_CPUS,
    }
}
fn rk_mpp_noclient() -> EncoderEnv {
    EncoderEnv {
        has_rtspclientsink: false,
        ..rk_mpp()
    }
}
/// An Allwinner vendor board (Cedar OMX) with `omxh264videoenc` present.
fn allwinner_omx() -> EncoderEnv {
    EncoderEnv {
        hw_h264: hw_node_missing(),
        has_mpph264enc: false,
        has_omxh264videoenc: true,
        has_rtspclientsink: true,
        encoder_api: "vendor".into(),
        pi5_class: false,
        cpu_threads: FIXTURE_CPUS,
    }
}
/// A Pi-5-class board (BCM2712: Pi 5 / CM5) — no hardware H.264 encoder,
/// so `rpicam-vid` there is software x264 and needs `--low-latency`.
fn pi5() -> EncoderEnv {
    EncoderEnv {
        pi5_class: true,
        ..rockchip()
    }
}

/// A two-core board: below the sliced-threads gate, so the libx264 argv
/// must keep the conservative frame-threaded form.
fn dual_core() -> EncoderEnv {
    EncoderEnv {
        cpu_threads: 2,
        ..rockchip()
    }
}

fn params(kind: EncoderKind, w: u32, h: u32, fps: u32, kbps: u32) -> EncoderParams {
    EncoderParams {
        kind,
        codec: "h264".into(),
        width: w,
        height: h,
        fps,
        bitrate_kbps: kbps,
        encoder: "auto".into(),
        rotation: 0,
        hflip: false,
        vflip: false,
        keyframe_interval: 0,
        // The shipped default for a primary leg: the radio's RTP copy comes
        // out of this encoder rather than a second ffmpeg re-reading
        // mediamtx. Ignored by the rpicam / GStreamer arms.
        rtp_fanout: true,
        passthrough: false,
    }
}

/// Build params with a specific encode config (orientation / encoder
/// override / keyframe) on top of the given geometry.
#[allow(clippy::too_many_arguments)]
fn params_cfg(
    kind: EncoderKind,
    w: u32,
    h: u32,
    fps: u32,
    kbps: u32,
    encoder: &str,
    rotation: u32,
    hflip: bool,
    vflip: bool,
    keyframe_interval: u32,
) -> EncoderParams {
    EncoderParams {
        encoder: encoder.into(),
        rotation,
        hflip,
        vflip,
        keyframe_interval,
        ..params(kind, w, h, fps, kbps)
    }
}

/// Build (and optionally SEI-split) the way the capture script does. A
/// split renders as `encode ++ [SEI_STAGE_TOKEN] ++ publish`.
fn build(
    p: &EncoderParams,
    src: &str,
    out: &str,
    cam: &CameraInfo,
    env: &EncoderEnv,
    sei: bool,
) -> Vec<String> {
    let cmd = build_encoder_command(p, src, out, Some(cam), env).expect("builds");
    if sei {
        wrap_with_sei_inject(&cmd, out).map_or(cmd, |s| s.render())
    } else {
        cmd
    }
}

/// Every case the fixture file pins, as `(name, built argv)`.
///
/// This is the generator input AND the completeness guard: the file is
/// rendered from here, and [`fixture_table_matches_the_file`] asserts the
/// two are identical in both directions, so neither a stale fixture nor an
/// unpinned case can hide. Each entry is a real builder invocation; nothing
/// here is transcribed by hand.
fn fixture_cases() -> Vec<(&'static str, Vec<String>)> {
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
    let gst = |rot: u32, h: bool, v: bool| {
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
    let omx = |encoder: &str, rot: u32| {
        build(
            &params_cfg(
                EncoderKind::Gstreamer,
                1280,
                720,
                30,
                4000,
                encoder,
                rot,
                false,
                false,
                0,
            ),
            "/dev/video2",
            RTSP_OUT,
            &usb_yuyv(),
            &allwinner_omx(),
            false,
        )
    };
    let hd = |kind: EncoderKind| params(kind, 1280, 720, 30, 4000);

    vec![
        // --- CSI → rpicam (bash pipeline; the fan-out flag is ignored) ---
        (
            "csi_rpicam_rtsp_rk",
            build(
                &hd(EncoderKind::RpicamVid),
                "/dev/video0",
                RTSP_OUT,
                &csi(),
                &rockchip(),
                false,
            ),
        ),
        (
            "csi_rpicam_rtsp_rk_sei",
            build(
                &hd(EncoderKind::RpicamVid),
                "/dev/video0",
                RTSP_OUT,
                &csi(),
                &rockchip(),
                true,
            ),
        ),
        (
            "csi_rpicam_file",
            build(
                &params(EncoderKind::RpicamVid, 1920, 1080, 60, 8000),
                "/dev/video0",
                "/var/lib/ados/out.h264",
                &csi(),
                &rockchip(),
                false,
            ),
        ),
        // --- USB MJPEG → ffmpeg libx264 --------------------------------
        (
            "usb_mjpeg_ffmpeg_rtsp_rk",
            build(
                &hd(EncoderKind::Ffmpeg),
                "/dev/video1",
                RTSP_OUT,
                &usb_mjpeg(),
                &rockchip(),
                false,
            ),
        ),
        (
            "usb_mjpeg_ffmpeg_rtsp_rk_sei",
            build(
                &hd(EncoderKind::Ffmpeg),
                "/dev/video1",
                RTSP_OUT,
                &usb_mjpeg(),
                &rockchip(),
                true,
            ),
        ),
        // The vision raw-tap node (and any non-radio node): no second
        // output, so the plain single-destination RTSP form.
        (
            "usb_mjpeg_ffmpeg_rtsp_rk_no_fanout",
            build(
                &EncoderParams {
                    rtp_fanout: false,
                    ..hd(EncoderKind::Ffmpeg)
                },
                "/dev/video1",
                RTSP_OUT,
                &usb_mjpeg(),
                &rockchip(),
                false,
            ),
        ),
        // A two-core board: the sliced-threads levers must NOT appear.
        (
            "usb_mjpeg_ffmpeg_rtsp_dual_core",
            build(
                &hd(EncoderKind::Ffmpeg),
                "/dev/video1",
                RTSP_OUT,
                &usb_mjpeg(),
                &dual_core(),
                false,
            ),
        ),
        (
            "usb_mjpeg_ffmpeg_rtsp_rk_640x480_15",
            build(
                &params(EncoderKind::Ffmpeg, 640, 480, 15, 1500),
                "/dev/video1",
                RTSP_OUT,
                &usb_mjpeg(),
                &rockchip(),
                false,
            ),
        ),
        (
            "usb_mjpeg_ffmpeg_udp_rk",
            build(
                &hd(EncoderKind::Ffmpeg),
                "/dev/video1",
                UDP_OUT,
                &usb_mjpeg(),
                &rockchip(),
                false,
            ),
        ),
        (
            "usb_mjpeg_ffmpeg_udp_rk_sei",
            build(
                &hd(EncoderKind::Ffmpeg),
                "/dev/video1",
                UDP_OUT,
                &usb_mjpeg(),
                &rockchip(),
                true,
            ),
        ),
        (
            "usb_yuyv_ffmpeg_rtsp_rk",
            build(
                &hd(EncoderKind::Ffmpeg),
                "/dev/video2",
                RTSP_OUT,
                &usb_yuyv(),
                &rockchip(),
                false,
            ),
        ),
        // --- USB on a board with a probed HW encoder (h264_v4l2m2m) ----
        (
            "usb_mjpeg_ffmpeg_rtsp_pi_hw",
            build(
                &hd(EncoderKind::Ffmpeg),
                "/dev/video1",
                RTSP_OUT,
                &usb_mjpeg(),
                &non_rk_hw(),
                false,
            ),
        ),
        (
            "usb_mjpeg_ffmpeg_rtsp_pi_hw_sei",
            build(
                &hd(EncoderKind::Ffmpeg),
                "/dev/video1",
                RTSP_OUT,
                &usb_mjpeg(),
                &non_rk_hw(),
                true,
            ),
        ),
        (
            "usb_mjpeg_ffmpeg_rtsp_nonrk_sw",
            build(
                &hd(EncoderKind::Ffmpeg),
                "/dev/video1",
                RTSP_OUT,
                &usb_mjpeg(),
                &non_rk_sw(),
                false,
            ),
        ),
        (
            "ip_ffmpeg_rtsp_rk",
            build(
                &hd(EncoderKind::Ffmpeg),
                "rtsp://10.0.0.9:554/live",
                RTSP_OUT,
                &ip_cam(),
                &rockchip(),
                false,
            ),
        ),
        // --- GStreamer arms (bash pipeline / gst-launch) ---------------
        (
            "gst_usb_mjpeg_rtsp_rk_mpp",
            build(
                &hd(EncoderKind::Gstreamer),
                "/dev/video1",
                RTSP_OUT,
                &usb_mjpeg(),
                &rk_mpp(),
                false,
            ),
        ),
        (
            "gst_usb_mjpeg_rtsp_rk_mpp_noclient",
            build(
                &hd(EncoderKind::Gstreamer),
                "/dev/video1",
                RTSP_OUT,
                &usb_mjpeg(),
                &rk_mpp_noclient(),
                false,
            ),
        ),
        (
            "gst_usb_yuyv_rtsp_nonrk_x264",
            build(
                &hd(EncoderKind::Gstreamer),
                "/dev/video2",
                RTSP_OUT,
                &usb_yuyv(),
                &non_rk_sw(),
                false,
            ),
        ),
        (
            "gst_usb_mjpeg_file_rk_mpp",
            build(
                &hd(EncoderKind::Gstreamer),
                "/dev/video1",
                "/var/lib/ados/cap.h264",
                &usb_mjpeg(),
                &rk_mpp(),
                false,
            ),
        ),
        (
            "gst_usb_mjpeg_rtsp_rk_mpp_sei_skip",
            build(
                &hd(EncoderKind::Gstreamer),
                "/dev/video1",
                RTSP_OUT,
                &usb_mjpeg(),
                &rk_mpp(),
                true,
            ),
        ),
        // --- ffmpeg orientation / override matrix ----------------------
        ("ffmpeg_rot90", ff(90, false, false)),
        ("ffmpeg_rot180", ff(180, false, false)),
        ("ffmpeg_rot270", ff(270, false, false)),
        ("ffmpeg_hflip", ff(0, true, false)),
        ("ffmpeg_vflip", ff(0, false, true)),
        ("ffmpeg_rot180_hflip", ff(180, true, false)),
        (
            "ffmpeg_keyframe5",
            build(
                &params_cfg(
                    EncoderKind::Ffmpeg,
                    1280,
                    720,
                    30,
                    4000,
                    "auto",
                    0,
                    false,
                    false,
                    5,
                ),
                "/dev/video1",
                RTSP_OUT,
                &usb_mjpeg(),
                &rockchip(),
                false,
            ),
        ),
        (
            "ffmpeg_override_software",
            build(
                &params_cfg(
                    EncoderKind::Ffmpeg,
                    1280,
                    720,
                    30,
                    4000,
                    "software",
                    0,
                    false,
                    false,
                    0,
                ),
                "/dev/video1",
                RTSP_OUT,
                &usb_mjpeg(),
                &non_rk_hw(),
                false,
            ),
        ),
        (
            "ffmpeg_override_v4l2m2m",
            build(
                &params_cfg(
                    EncoderKind::Ffmpeg,
                    1280,
                    720,
                    30,
                    4000,
                    "v4l2m2m",
                    0,
                    false,
                    false,
                    0,
                ),
                "/dev/video1",
                RTSP_OUT,
                &usb_mjpeg(),
                &rockchip(),
                false,
            ),
        ),
        // --- Allwinner OMX + GStreamer orientation ---------------------
        ("gst_omx_argv", omx("auto", 0)),
        ("gst_omx_rot90", omx("auto", 90)),
        ("gst_omx_explicit", omx("omx", 0)),
        ("gst_omx_sw_override", omx("software", 0)),
        ("gst_x264_rot90", gst(90, false, false)),
        ("gst_x264_rot180", gst(180, false, false)),
        ("gst_x264_rot270", gst(270, false, false)),
        ("gst_x264_hflip", gst(0, true, false)),
        ("gst_x264_vflip", gst(0, false, true)),
    ]
}

/// The fixture file's path, for the regenerator.
fn fixture_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/encoder_fixtures.json")
}

/// The completeness guard in both directions: every case the builder
/// produces is pinned in the file, every key in the file corresponds to a
/// case, and the argv match byte for byte. A hand-edited fixture that does
/// not correspond to real builder output fails here.
#[test]
fn fixture_table_matches_the_file() {
    let file = fixtures();
    let file_obj = file.as_object().expect("fixtures is a JSON object");
    let cases = fixture_cases();

    let mut missing: Vec<&str> = Vec::new();
    for (name, argv) in &cases {
        match file_obj.get(*name) {
            None => missing.push(name),
            Some(_) => assert_eq!(
                &expected(name),
                argv,
                "fixture {name:?} is stale — regenerate with \
                 `cargo test -p ados-video --lib regenerate_encoder_fixtures -- --ignored --exact`"
            ),
        }
    }
    assert!(missing.is_empty(), "unpinned fixture case(s): {missing:?}");

    let case_names: Vec<&str> = cases.iter().map(|(n, _)| *n).collect();
    let orphans: Vec<&String> = file_obj
        .keys()
        .filter(|k| !case_names.contains(&k.as_str()))
        .collect();
    assert!(
        orphans.is_empty(),
        "fixture key(s) with no generating case: {orphans:?}"
    );
}

/// Rewrite `tests/encoder_fixtures.json` from [`fixture_cases`].
///
/// Ignored by default (it writes to the source tree). Run it deliberately
/// after an intentional argv change:
///
/// ```text
/// cargo test -p ados-video --lib regenerate_encoder_fixtures -- --ignored --exact
/// ```
///
/// then read the diff: it is the exact byte-level change every encode path
/// will run on the rig.
#[test]
#[ignore = "writes tests/encoder_fixtures.json; run explicitly to regenerate"]
fn regenerate_encoder_fixtures() {
    // Rendered in case order (not serde's map order) and in the file's
    // existing 2/4-space shape, so a regeneration diff shows only the argv
    // that actually changed instead of reshuffling every key.
    let cases = fixture_cases();
    let mut body = String::from("{\n");
    for (i, (name, argv)) in cases.iter().enumerate() {
        body.push_str(&format!("  {}: [\n", Value::from(*name)));
        for (j, token) in argv.iter().enumerate() {
            let comma = if j + 1 < argv.len() { "," } else { "" };
            body.push_str(&format!("    {}{comma}\n", Value::from(token.as_str())));
        }
        let comma = if i + 1 < cases.len() { "," } else { "" };
        body.push_str(&format!("  ]{comma}\n"));
    }
    body.push_str("}\n");
    let path = fixture_path();
    std::fs::write(&path, body).expect("fixture file is writable");
    eprintln!("regenerated {} ({} cases)", path.display(), cases.len());
}

// --- CSI → rpicam --------------------------------------------------

#[test]
fn csi_rpicam_rtsp_rk() {
    let got = build(
        &params(EncoderKind::RpicamVid, 1280, 720, 30, 4000),
        "/dev/video0",
        RTSP_OUT,
        &csi(),
        &rockchip(),
        false,
    );
    assert_eq!(got, expected("csi_rpicam_rtsp_rk"));
}

#[test]
fn csi_rpicam_rtsp_rk_sei() {
    let got = build(
        &params(EncoderKind::RpicamVid, 1280, 720, 30, 4000),
        "/dev/video0",
        RTSP_OUT,
        &csi(),
        &rockchip(),
        true,
    );
    assert_eq!(got, expected("csi_rpicam_rtsp_rk_sei"));
}

#[test]
fn csi_rpicam_file() {
    let got = build(
        &params(EncoderKind::RpicamVid, 1920, 1080, 60, 8000),
        "/dev/video0",
        "/var/lib/ados/out.h264",
        &csi(),
        &rockchip(),
        false,
    );
    assert_eq!(got, expected("csi_rpicam_file"));
}

/// The value that follows `flag` in an argv, whether the argv is a plain
/// vector or a `bash -c "<pipeline>"` triple. Fixture assertions pin whole
/// argv vectors; these contract tests pin a single flag, so they keep
/// holding when an unrelated flag moves.
fn flag_value(cmd: &[String], flag: &str) -> Option<String> {
    if cmd.first().map(String::as_str) == Some("bash") {
        let mut it = cmd[2].split(' ');
        while let Some(t) = it.next() {
            if t == flag {
                return it.next().map(str::to_string);
            }
        }
        return None;
    }
    cmd.iter()
        .position(|t| t == flag)
        .and_then(|i| cmd.get(i + 1))
        .cloned()
}

fn has_flag(cmd: &[String], flag: &str) -> bool {
    if cmd.first().map(String::as_str) == Some("bash") {
        return cmd[2].split(' ').any(|t| t == flag);
    }
    cmd.iter().any(|t| t == flag)
}

#[test]
fn rpicam_intra_follows_the_gop_contract_not_a_hardcoded_30() {
    // `keyframe_interval: 0` is the documented default and means "a 0.5s
    // GOP at the configured fps so radio FEC recovers fast". `--intra`
    // takes a frame count, so it must be exactly `gop_interval`. The old
    // hardcoded `--intra 30` was a 1.0s GOP at 30fps — double the
    // contracted worst-case packet-loss recovery time.
    //
    // Three fps values, so no single constant can satisfy all of them:
    // restoring `--intra 30` fails the 30fps and 15fps cases.
    for (fps, want_intra) in [(30u32, "15"), (60, "30"), (15, "7")] {
        let p = params(EncoderKind::RpicamVid, 1280, 720, fps, 4000);
        assert_eq!(p.keyframe_interval, 0, "default keyframe_interval");
        let got = build(&p, "/dev/video0", RTSP_OUT, &csi(), &rockchip(), false);
        assert_eq!(
            flag_value(&got, "--intra").as_deref(),
            Some(want_intra),
            "--intra at {fps}fps must equal gop_interval"
        );
    }
    // An explicit keyframe_interval still wins over the derived default.
    let p = params_cfg(
        EncoderKind::RpicamVid,
        1280,
        720,
        30,
        4000,
        "auto",
        0,
        false,
        false,
        20,
    );
    let got = build(&p, "/dev/video0", RTSP_OUT, &csi(), &rockchip(), false);
    assert_eq!(flag_value(&got, "--intra").as_deref(), Some("20"));
}

#[test]
fn rpicam_encodes_high_4_1_to_match_the_browser_player() {
    // The browser MSE player is pinned to `avc1.640029` = H.264 High 4.1,
    // and the ffmpeg libx264 path already encodes High 4.1. A CSI drone
    // publishing Constrained Baseline into that player is a silent,
    // permanent black screen on the receive side, so all three encode
    // paths must agree.
    for out in [RTSP_OUT, "/var/lib/ados/out.h264"] {
        let got = build(
            &params(EncoderKind::RpicamVid, 1280, 720, 30, 4000),
            "/dev/video0",
            out,
            &csi(),
            &rockchip(),
            false,
        );
        assert_eq!(flag_value(&got, "--profile").as_deref(), Some("high"));
        assert_eq!(flag_value(&got, "--level").as_deref(), Some("4.1"));
        assert!(!has_flag(&got, "baseline"), "no baseline profile anywhere");
    }
}

#[test]
fn rpicam_low_latency_only_on_pi5_class_boards() {
    // BCM2712 (Pi 5 / CM5) has no H.264 encode block, so rpicam runs
    // software x264 and buffers a constant 8-frame pipeline (~267ms at
    // 30fps) without `--low-latency`. A board with a real hardware encoder
    // must NOT get the flag (older rpicam-apps reject it outright, and it
    // buys nothing there).
    let p = params(EncoderKind::RpicamVid, 1280, 720, 30, 4000);
    let on_pi5 = build(&p, "/dev/video0", RTSP_OUT, &csi(), &pi5(), false);
    assert!(has_flag(&on_pi5, "--low-latency"));
    let elsewhere = build(&p, "/dev/video0", RTSP_OUT, &csi(), &rockchip(), false);
    assert!(!has_flag(&elsewhere, "--low-latency"));

    // The flag must survive the file-output form and the SEI splice too.
    let file = build(
        &p,
        "/dev/video0",
        "/var/lib/ados/out.h264",
        &csi(),
        &pi5(),
        false,
    );
    assert!(has_flag(&file, "--low-latency"));
    let sei = build(&p, "/dev/video0", RTSP_OUT, &csi(), &pi5(), true);
    assert!(has_flag(&sei, "--low-latency"));
}

#[test]
fn soc_compatible_classifies_pi5_class_silicon() {
    // Real `/proc/device-tree/compatible` contents, most-specific first.
    let pi5 = [
        "raspberrypi,5-model-b".to_string(),
        "brcm,bcm2712".to_string(),
    ];
    let cm5 = [
        "raspberrypi,5-compute-module".to_string(),
        "brcm,bcm2712".to_string(),
    ];
    assert!(soc_is_pi5_class(&pi5));
    assert!(soc_is_pi5_class(&cm5));
    // Either entry alone is enough (a downstream DT may carry only one).
    assert!(soc_is_pi5_class(&["brcm,bcm2712".to_string()]));

    // Pi 4 / CM4 are BCM2711 and DO have a hardware encoder — they must not
    // classify as Pi-5-class or they would be handed `--low-latency`.
    assert!(!soc_is_pi5_class(&[
        "raspberrypi,4-model-b".to_string(),
        "brcm,bcm2711".to_string(),
    ]));
    assert!(!soc_is_pi5_class(&[
        "raspberrypi,4-compute-module".to_string(),
        "brcm,bcm2711".to_string(),
    ]));
    assert!(!soc_is_pi5_class(&[
        "radxa,rock-5c".to_string(),
        "rockchip,rk3588s".to_string(),
    ]));
    assert!(!soc_is_pi5_class(&[]));
}

// --- USB MJPEG → ffmpeg libx264 (Rockchip) -------------------------

#[test]
fn usb_mjpeg_ffmpeg_rtsp_rk() {
    let got = build(
        &params(EncoderKind::Ffmpeg, 1280, 720, 30, 4000),
        "/dev/video1",
        RTSP_OUT,
        &usb_mjpeg(),
        &rockchip(),
        false,
    );
    assert_eq!(got, expected("usb_mjpeg_ffmpeg_rtsp_rk"));
}

#[test]
fn usb_mjpeg_ffmpeg_rtsp_rk_sei() {
    let got = build(
        &params(EncoderKind::Ffmpeg, 1280, 720, 30, 4000),
        "/dev/video1",
        RTSP_OUT,
        &usb_mjpeg(),
        &rockchip(),
        true,
    );
    assert_eq!(got, expected("usb_mjpeg_ffmpeg_rtsp_rk_sei"));
}

#[test]
fn usb_mjpeg_ffmpeg_rtsp_rk_640x480_15() {
    let got = build(
        &params(EncoderKind::Ffmpeg, 640, 480, 15, 1500),
        "/dev/video1",
        RTSP_OUT,
        &usb_mjpeg(),
        &rockchip(),
        false,
    );
    assert_eq!(got, expected("usb_mjpeg_ffmpeg_rtsp_rk_640x480_15"));
}

#[test]
fn usb_mjpeg_ffmpeg_udp_rk() {
    let got = build(
        &params(EncoderKind::Ffmpeg, 1280, 720, 30, 4000),
        "/dev/video1",
        UDP_OUT,
        &usb_mjpeg(),
        &rockchip(),
        false,
    );
    assert_eq!(got, expected("usb_mjpeg_ffmpeg_udp_rk"));
}

#[test]
fn usb_mjpeg_ffmpeg_udp_rk_sei() {
    let got = build(
        &params(EncoderKind::Ffmpeg, 1280, 720, 30, 4000),
        "/dev/video1",
        UDP_OUT,
        &usb_mjpeg(),
        &rockchip(),
        true,
    );
    assert_eq!(got, expected("usb_mjpeg_ffmpeg_udp_rk_sei"));
}

// --- USB YUYV → ffmpeg libx264 -------------------------------------

#[test]
fn usb_yuyv_ffmpeg_rtsp_rk() {
    let got = build(
        &params(EncoderKind::Ffmpeg, 1280, 720, 30, 4000),
        "/dev/video2",
        RTSP_OUT,
        &usb_yuyv(),
        &rockchip(),
        false,
    );
    assert_eq!(got, expected("usb_yuyv_ffmpeg_rtsp_rk"));
}

// --- USB on non-Rockchip with HW encoder ---------------------------

#[test]
fn usb_mjpeg_ffmpeg_rtsp_pi_hw() {
    let got = build(
        &params(EncoderKind::Ffmpeg, 1280, 720, 30, 4000),
        "/dev/video1",
        RTSP_OUT,
        &usb_mjpeg(),
        &non_rk_hw(),
        false,
    );
    assert_eq!(got, expected("usb_mjpeg_ffmpeg_rtsp_pi_hw"));
}

#[test]
fn usb_mjpeg_ffmpeg_rtsp_pi_hw_sei() {
    let got = build(
        &params(EncoderKind::Ffmpeg, 1280, 720, 30, 4000),
        "/dev/video1",
        RTSP_OUT,
        &usb_mjpeg(),
        &non_rk_hw(),
        true,
    );
    assert_eq!(got, expected("usb_mjpeg_ffmpeg_rtsp_pi_hw_sei"));
}

// --- USB on non-Rockchip software ----------------------------------

#[test]
fn usb_mjpeg_ffmpeg_rtsp_nonrk_sw() {
    let got = build(
        &params(EncoderKind::Ffmpeg, 1280, 720, 30, 4000),
        "/dev/video1",
        RTSP_OUT,
        &usb_mjpeg(),
        &non_rk_sw(),
        false,
    );
    assert_eq!(got, expected("usb_mjpeg_ffmpeg_rtsp_nonrk_sw"));
}

// --- IP camera → ffmpeg --------------------------------------------

#[test]
fn ip_ffmpeg_rtsp_rk() {
    let got = build(
        &params(EncoderKind::Ffmpeg, 1280, 720, 30, 4000),
        "rtsp://10.0.0.9:554/live",
        RTSP_OUT,
        &ip_cam(),
        &rockchip(),
        false,
    );
    assert_eq!(got, expected("ip_ffmpeg_rtsp_rk"));
}

// --- GStreamer paths -----------------------------------------------

#[test]
fn gst_usb_mjpeg_rtsp_rk_mpp() {
    let got = build(
        &params(EncoderKind::Gstreamer, 1280, 720, 30, 4000),
        "/dev/video1",
        RTSP_OUT,
        &usb_mjpeg(),
        &rk_mpp(),
        false,
    );
    assert_eq!(got, expected("gst_usb_mjpeg_rtsp_rk_mpp"));
}

#[test]
fn gst_usb_mjpeg_rtsp_rk_mpp_noclient() {
    let got = build(
        &params(EncoderKind::Gstreamer, 1280, 720, 30, 4000),
        "/dev/video1",
        RTSP_OUT,
        &usb_mjpeg(),
        &rk_mpp_noclient(),
        false,
    );
    assert_eq!(got, expected("gst_usb_mjpeg_rtsp_rk_mpp_noclient"));
}

#[test]
fn gst_usb_yuyv_rtsp_nonrk_x264() {
    let got = build(
        &params(EncoderKind::Gstreamer, 1280, 720, 30, 4000),
        "/dev/video2",
        RTSP_OUT,
        &usb_yuyv(),
        &non_rk_sw(),
        false,
    );
    assert_eq!(got, expected("gst_usb_yuyv_rtsp_nonrk_x264"));
}

#[test]
fn gst_usb_mjpeg_file_rk_mpp() {
    let got = build(
        &params(EncoderKind::Gstreamer, 1280, 720, 30, 4000),
        "/dev/video1",
        "/var/lib/ados/cap.h264",
        &usb_mjpeg(),
        &rk_mpp(),
        false,
    );
    assert_eq!(got, expected("gst_usb_mjpeg_file_rk_mpp"));
}

#[test]
fn gst_usb_mjpeg_rtsp_rk_mpp_sei_skip() {
    // SEI wrap must leave a GStreamer command unchanged (case 3).
    let got = build(
        &params(EncoderKind::Gstreamer, 1280, 720, 30, 4000),
        "/dev/video1",
        RTSP_OUT,
        &usb_mjpeg(),
        &rk_mpp(),
        true,
    );
    assert_eq!(got, expected("gst_usb_mjpeg_rtsp_rk_mpp_sei_skip"));
}

// --- probe-first HW encoder selection ------------------------------

#[test]
fn probe_absent_node_selects_libx264() {
    // The live bug: a board advertises the h264_v4l2m2m wrapper but ships
    // no real V4L2 encoder device. With the probe reporting NodeMissing the
    // builder MUST fall back to software libx264 — never the wrapper, which
    // would make ffmpeg exit at init and stream zero bytes.
    let env = rockchip();
    let got = build(
        &params(EncoderKind::Ffmpeg, 1280, 720, 30, 4000),
        "/dev/video1",
        RTSP_OUT,
        &usb_mjpeg(),
        &env,
        false,
    );
    let ci = got.iter().position(|t| t == "-c:v").unwrap();
    assert_eq!(got[ci + 1], "libx264");
    assert!(!got.iter().any(|t| t == "h264_v4l2m2m"));
}

#[test]
fn probe_present_device_selects_h264_v4l2m2m() {
    // A real V4L2 M2M node enumerated H.264 and passed the bounded
    // trial-init → the builder uses the hardware encoder.
    let env = EncoderEnv {
        hw_h264: hw_present(),
        ..rockchip()
    };
    let got = build(
        &params(EncoderKind::Ffmpeg, 1280, 720, 30, 4000),
        "/dev/video1",
        RTSP_OUT,
        &usb_mjpeg(),
        &env,
        false,
    );
    let ci = got.iter().position(|t| t == "-c:v").unwrap();
    assert_eq!(got[ci + 1], "h264_v4l2m2m");
    assert!(!got.iter().any(|t| t == "libx264"));
}

// --- builder-logic unit tests (not fixture-driven) -----------------

#[test]
fn validate_source_rejects_disallowed_chars() {
    assert!(matches!(
        validate_source("rm; rf"),
        Err(EncoderError::InvalidSource(_))
    ));
    assert!(matches!(
        validate_source("a$(whoami)"),
        Err(EncoderError::InvalidSource(_))
    ));
    assert_eq!(validate_source("-").unwrap(), "-");
    assert_eq!(validate_source("/dev/video0").unwrap(), "/dev/video0");
    assert_eq!(
        validate_source("rtsp://127.0.0.1:8554/main").unwrap(),
        "rtsp://127.0.0.1:8554/main"
    );
}

#[test]
fn build_rejects_bad_source() {
    let p = params(EncoderKind::Ffmpeg, 1280, 720, 30, 4000);
    let err = build_encoder_command(&p, "bad source", RTSP_OUT, None, &rockchip());
    assert!(matches!(err, Err(EncoderError::InvalidSource(_))));
}

#[test]
fn gop_floors_at_one() {
    // fps=1 → fps/2 = 0 → max(.,1) = 1.
    let p = params(EncoderKind::Ffmpeg, 320, 240, 1, 500);
    let got = build_encoder_command(&p, "/dev/video1", RTSP_OUT, Some(&usb_mjpeg()), &rockchip())
        .unwrap();
    let gi = got.iter().position(|t| t == "-g").unwrap();
    assert_eq!(got[gi + 1], "1");
}

#[test]
fn detect_encoder_matches_python_matrix() {
    // CSI → rpicam, fallback ffmpeg.
    assert_eq!(
        detect_encoder_for_camera(CameraType::Csi, true, true, true),
        Some(EncoderKind::RpicamVid)
    );
    assert_eq!(
        detect_encoder_for_camera(CameraType::Csi, false, true, true),
        Some(EncoderKind::Ffmpeg)
    );
    assert_eq!(
        detect_encoder_for_camera(CameraType::Csi, false, false, true),
        None
    );
    // USB / IP → ffmpeg, fallback gstreamer (mpph264enc disabled).
    assert_eq!(
        detect_encoder_for_camera(CameraType::Usb, true, true, true),
        Some(EncoderKind::Ffmpeg)
    );
    assert_eq!(
        detect_encoder_for_camera(CameraType::Usb, true, false, true),
        Some(EncoderKind::Gstreamer)
    );
    assert_eq!(
        detect_encoder_for_camera(CameraType::Ip, false, false, false),
        None
    );
}

#[test]
fn encoder_label_names_the_codec_element_the_command_actually_runs() {
    // The sidecar field exists so an operator can tell a node that fell back
    // to software from one on its hardware encoder. EncoderKind::Ffmpeg
    // covers both, so the label is read off the built argv.
    let sw = build(
        &params(EncoderKind::Ffmpeg, 1280, 720, 30, 4000),
        "/dev/video1",
        RTSP_OUT,
        &usb_mjpeg(),
        &non_rk_sw(),
        false,
    );
    assert!(sw.iter().any(|a| a == "libx264"), "fixture sanity");
    assert_eq!(encoder_label(EncoderKind::Ffmpeg, &sw), "ffmpeg-libx264");
    assert!(!encoder_is_hardware("ffmpeg-libx264"));

    let hw = build(
        &params(EncoderKind::Ffmpeg, 1280, 720, 30, 4000),
        "/dev/video1",
        RTSP_OUT,
        &usb_mjpeg(),
        &non_rk_hw(),
        false,
    );
    assert_eq!(
        encoder_label(EncoderKind::Ffmpeg, &hw),
        "ffmpeg-h264_v4l2m2m"
    );
    assert!(encoder_is_hardware("ffmpeg-h264_v4l2m2m"));

    // CSI: the backend answers before the argv is looked at, because the
    // rpicam pipeline hands its elementary stream to a `-c copy` ffmpeg and
    // carries no codec element of its own.
    let csi_cmd = build(
        &params(EncoderKind::RpicamVid, 1280, 720, 30, 4000),
        "/dev/video0",
        RTSP_OUT,
        &csi(),
        &rockchip(),
        false,
    );
    assert_eq!(
        encoder_label(EncoderKind::RpicamVid, &csi_cmd),
        "rpicam-vid"
    );
    assert!(encoder_is_hardware("rpicam-vid"));

    // GStreamer, both halves of the Rockchip split.
    let gst_hw = build(
        &params(EncoderKind::Gstreamer, 1280, 720, 30, 4000),
        "/dev/video1",
        RTSP_OUT,
        &usb_mjpeg(),
        &rk_mpp(),
        false,
    );
    assert_eq!(
        encoder_label(EncoderKind::Gstreamer, &gst_hw),
        "gstreamer-mpph264enc"
    );
    assert!(encoder_is_hardware("gstreamer-mpph264enc"));

    let gst_sw = build(
        &params(EncoderKind::Gstreamer, 1280, 720, 30, 4000),
        "/dev/video1",
        RTSP_OUT,
        &usb_mjpeg(),
        &non_rk_sw(),
        false,
    );
    assert_eq!(
        encoder_label(EncoderKind::Gstreamer, &gst_sw),
        "gstreamer-x264enc"
    );
    assert!(!encoder_is_hardware("gstreamer-x264enc"));

    // A command with no recognised element is labelled unknown rather than
    // guessed at.
    assert_eq!(
        encoder_label(
            EncoderKind::Ffmpeg,
            &["ffmpeg".to_string(), "-c".into(), "copy".into()]
        ),
        "ffmpeg-unknown"
    );
    assert!(!encoder_is_hardware("ffmpeg-unknown"));
}

#[test]
fn select_input_format_priority() {
    assert_eq!(select_input_format(Some(&usb_mjpeg())), Some("mjpeg"));
    assert_eq!(select_input_format(Some(&usb_yuyv())), Some("yuyv"));
    assert_eq!(select_input_format(None), None);
    let unknown = CameraInfo {
        camera_type: CameraType::Usb,
        device_path: "/dev/video9".into(),
        capabilities: vec!["nv12".into()],
    };
    assert_eq!(select_input_format(Some(&unknown)), None);
}

#[test]
fn shell_quote_matches_python_minimal_quote() {
    assert_eq!(shell_quote("plain"), "plain");
    assert_eq!(shell_quote("/dev/video0"), "/dev/video0");
    assert_eq!(shell_quote(""), "''");
    assert_eq!(shell_quote("a b"), "'a b'");
    assert_eq!(shell_quote("it's"), "'it'\\''s'");
    assert_eq!(shell_quote("a|b"), "'a|b'");
}

fn has_pair(cmd: &[String], flag: &str, value: &str) -> bool {
    cmd.windows(2).any(|w| w[0] == flag && w[1] == value)
}

// --- capture queue depth --------------------------------------------

#[test]
fn capture_queue_is_sized_by_input_format() {
    // MJPEG keeps the deep queue: its frames are small.
    assert_eq!(input_queue_depth(Some("mjpeg"), 1920, 1080), 128);
    // Raw YUYV at 720p: 32 MiB / 1.84 MB = 18 frames, not 128 (~235 MB).
    assert_eq!(input_queue_depth(Some("yuyv"), 1280, 720), 18);
    // Large raw frames hit the floor; tiny ones hit the ceiling.
    assert_eq!(input_queue_depth(Some("yuyv"), 3840, 2160), 8);
    assert_eq!(input_queue_depth(Some("yuyv"), 320, 240), 128);
    // An unknown format is sized as raw.
    assert_eq!(input_queue_depth(None, 1280, 720), 18);
    // A zero-sized request cannot divide by zero.
    assert_eq!(input_queue_depth(None, 0, 0), 128);

    let yuyv = build_encoder_command(
        &params(EncoderKind::Ffmpeg, 1280, 720, 30, 4000),
        "/dev/video1",
        RTSP_OUT,
        Some(&usb_yuyv()),
        &rockchip(),
    )
    .unwrap();
    assert!(has_pair(&yuyv, "-thread_queue_size", "18"));
    let mjpeg = build_encoder_command(
        &params(EncoderKind::Ffmpeg, 1280, 720, 30, 4000),
        "/dev/video1",
        RTSP_OUT,
        Some(&usb_mjpeg()),
        &rockchip(),
    )
    .unwrap();
    assert!(has_pair(&mjpeg, "-thread_queue_size", "128"));
}

// --- network passthrough --------------------------------------------

#[test]
fn passthrough_argv() {
    let src = "rtsp://192.168.1.50:554/stream1";
    let p = EncoderParams {
        passthrough: true,
        rotation: 90,
        ..params(EncoderKind::Ffmpeg, 1280, 720, 30, 4000)
    };
    let cmd = build_encoder_command(&p, src, RTSP_OUT, None, &non_rk_hw()).unwrap();
    // Copied, never decoded or re-encoded: no encoder, no rate cap, no
    // filter, no bitrate.
    assert!(has_pair(&cmd, "-c:v", "copy"));
    assert!(has_pair(&cmd, "-bsf:v", "dump_extra=freq=keyframe"));
    for absent in ["-r", "-vf", "-b:v", "-g", "-pix_fmt"] {
        assert!(!cmd.iter().any(|t| t == absent), "{absent} in {cmd:?}");
    }
    assert!(!cmd.iter().any(|t| t == "libx264" || t == "h264_v4l2m2m"));
    // Input side is the network source over TCP; output stage unchanged.
    assert!(has_pair(&cmd, "-rtsp_transport", "tcp"));
    assert!(has_pair(&cmd, "-i", src));
    assert!(
        cmd.last().unwrap().contains(RTSP_OUT),
        "fan-out keeps the publish URI"
    );

    // A local device ignores the flag and still encodes.
    let local = build_encoder_command(&p, "/dev/video1", RTSP_OUT, Some(&usb_mjpeg()), &rockchip())
        .unwrap();
    assert!(!has_pair(&local, "-c:v", "copy"));
    // And without the flag a network source is re-encoded as before.
    let off = EncoderParams {
        passthrough: false,
        ..p
    };
    let encoded = build_encoder_command(&off, src, RTSP_OUT, None, &rockchip()).unwrap();
    assert!(has_pair(&encoded, "-c:v", "libx264"));
}

// --- wire-codec gate ----------------------------------------------

#[test]
fn unsupported_codecs_are_refused_on_every_builder_arm() {
    // h265 / hevc / mjpeg parse clean out of `video.camera.codec` and used
    // to map onto libx265 / mjpeg, which fell through BOTH tuning blocks —
    // no `-g` and no in-band parameter-set bitstream filter — while the
    // radio leg still framed the payload as RTP type 96 (H.264). Publishing
    // that is worse than refusing it, so no argv is produced at all.
    for codec in ["h265", "hevc", "mjpeg", "av1", "H264", ""] {
        for (kind, source, camera, env) in [
            (EncoderKind::Ffmpeg, "/dev/video1", usb_mjpeg(), rockchip()),
            (EncoderKind::RpicamVid, "/dev/video0", csi(), rockchip()),
            (EncoderKind::Gstreamer, "/dev/video1", usb_mjpeg(), rk_mpp()),
        ] {
            let err = build_encoder_command(
                &EncoderParams {
                    codec: codec.into(),
                    ..params(kind, 1280, 720, 30, 4000)
                },
                source,
                RTSP_OUT,
                Some(&camera),
                &env,
            )
            .expect_err("an untunable codec must not produce an argv");
            assert_eq!(err, EncoderError::UnsupportedCodec(codec.to_string()));
        }
    }
}

#[test]
fn unsupported_codec_error_names_the_framing_reason() {
    // The operator reads this text in the journal and on the camera card,
    // so it has to say WHY, not just "unsupported".
    let text = EncoderError::UnsupportedCodec("h265".into()).to_string();
    for needle in ["h265", "-g", "SPS/PPS", "payload type", "h264"] {
        assert!(text.contains(needle), "{needle:?} missing from: {text}");
    }
}

#[test]
fn h264_still_builds_on_every_arm() {
    // The gate must not be a blanket refusal: the shipped codec builds.
    for (kind, source, camera, env) in [
        (EncoderKind::Ffmpeg, "/dev/video1", usb_mjpeg(), rockchip()),
        (EncoderKind::RpicamVid, "/dev/video0", csi(), rockchip()),
        (EncoderKind::Gstreamer, "/dev/video1", usb_mjpeg(), rk_mpp()),
    ] {
        let cmd = build_encoder_command(
            &params(kind, 1280, 720, 30, 4000),
            source,
            RTSP_OUT,
            Some(&camera),
            &env,
        )
        .expect("h264 builds");
        assert!(!cmd.is_empty());
    }
}

// --- Allwinner OMX hardware encode (A733) ------------------------

#[test]
fn gst_omx_selects_omxh264videoenc_with_nv12_and_low_latency() {
    // On an Allwinner vendor board with omxh264videoenc present, an
    // auto default must use the OMX hardware encoder: CBR constant bitrate
    // (bps = kbps*1000), NV12 into the encoder, the frame-dropping queue,
    // and `h264parse config-interval=1` so radio FEC recovery resyncs fast.
    let got = build(
        &params(EncoderKind::Gstreamer, 1280, 720, 30, 4000),
        "/dev/video2",
        RTSP_OUT,
        &usb_yuyv(),
        &allwinner_omx(),
        false,
    );
    assert_eq!(got, expected("gst_omx_argv"));
    assert!(got.iter().any(|a| a == "omxh264videoenc"));
    assert!(encoder_is_hardware("gstreamer-omxh264videoenc"));
}

#[test]
fn gst_omx_rotation90_inserts_videoflip() {
    let got = build(
        &params_cfg(
            EncoderKind::Gstreamer,
            1280,
            720,
            30,
            4000,
            "auto",
            90,
            false,
            false,
            0,
        ),
        "/dev/video2",
        RTSP_OUT,
        &usb_yuyv(),
        &allwinner_omx(),
        false,
    );
    assert_eq!(got, expected("gst_omx_rot90"));
}

#[test]
fn gst_explicit_omx_and_software_override() {
    // Explicit encoder: omx → OMX (same argv as auto on the vendor board);
    // software → never the HW branch, always desktop x264enc even with the
    // OMX element present (software names the codec, keeps the GStreamer
    // family).
    let omx = build(
        &params_cfg(
            EncoderKind::Gstreamer,
            1280,
            720,
            30,
            4000,
            "omx",
            0,
            false,
            false,
            0,
        ),
        "/dev/video2",
        RTSP_OUT,
        &usb_yuyv(),
        &allwinner_omx(),
        false,
    );
    assert_eq!(omx, expected("gst_omx_explicit"));
    assert_eq!(
        omx,
        expected("gst_omx_argv"),
        "explicit omx == auto on vendor"
    );

    let sw = build(
        &params_cfg(
            EncoderKind::Gstreamer,
            1280,
            720,
            30,
            4000,
            "software",
            0,
            false,
            false,
            0,
        ),
        "/dev/video2",
        RTSP_OUT,
        &usb_yuyv(),
        &allwinner_omx(),
        false,
    );
    assert_eq!(sw, expected("gst_omx_sw_override"));
    assert!(!sw.iter().any(|a| a == "omxh264videoenc"));
    assert!(sw.iter().any(|a| a == "x264enc"));
    assert!(!encoder_is_hardware("gstreamer-x264enc"));
}

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

// --- encoder override selection -----------------------------------

#[test]
fn ffmpeg_override_software_and_v4l2m2m() {
    // software → libx264 even when a real HW device is present.
    let sw = build(
        &params_cfg(
            EncoderKind::Ffmpeg,
            1280,
            720,
            30,
            4000,
            "software",
            0,
            false,
            false,
            0,
        ),
        "/dev/video1",
        RTSP_OUT,
        &usb_mjpeg(),
        &non_rk_hw(),
        false,
    );
    assert_eq!(sw, expected("ffmpeg_override_software"));
    assert!(sw.iter().any(|a| a == "libx264"));
    assert!(!encoder_is_hardware("ffmpeg-libx264"));

    // v4l2m2m → force the HW M2M wrapper even when the probe found no device
    // (an operator explicitly opting into the wrapper takes the risk).
    let hw = build(
        &params_cfg(
            EncoderKind::Ffmpeg,
            1280,
            720,
            30,
            4000,
            "v4l2m2m",
            0,
            false,
            false,
            0,
        ),
        "/dev/video1",
        RTSP_OUT,
        &usb_mjpeg(),
        &rockchip(),
        false,
    );
    assert_eq!(hw, expected("ffmpeg_override_v4l2m2m"));
    assert!(hw.iter().any(|a| a == "h264_v4l2m2m"));
    assert!(encoder_is_hardware("ffmpeg-h264_v4l2m2m"));
}

#[test]
fn auto_default_stays_ffmpeg_off_vendor_boards() {
    // A non-Allwinner board (encoder_api unknown, no omxh264videoenc) with
    // the auto default must NOT be hijacked into the OMX gstreamer path —
    // it stays on ffmpeg libx264 (byte-identical legacy behavior).
    let p = params(EncoderKind::Ffmpeg, 1280, 720, 30, 4000);
    let got = build_encoder_command(&p, "/dev/video1", RTSP_OUT, Some(&usb_mjpeg()), &rockchip())
        .unwrap();
    assert!(got.iter().any(|a| a == "libx264"));
    assert!(!got.iter().any(|a| a == "omxh264videoenc"));
}

#[test]
fn ffmpeg_keyframe_interval_overrides_the_gop() {
    // keyframe_interval=5 → -g 5 (instead of the 0.5 s default of 15).
    let got = build(
        &params_cfg(
            EncoderKind::Ffmpeg,
            1280,
            720,
            30,
            4000,
            "auto",
            0,
            false,
            false,
            5,
        ),
        "/dev/video1",
        RTSP_OUT,
        &usb_mjpeg(),
        &rockchip(),
        false,
    );
    assert_eq!(got, expected("ffmpeg_keyframe5"));
    let gi = got.iter().position(|t| t == "-g").unwrap();
    assert_eq!(got[gi + 1], "5");
    // Default (0) keeps the existing 0.5 s GOP: fps30 → 15 (see the legacy
    // fixtures, which still pass with -g 15).
    assert_eq!(
        gop_interval(&params(EncoderKind::Ffmpeg, 1280, 720, 30, 4000)),
        15
    );
    assert_eq!(
        gop_interval(&params_cfg(
            EncoderKind::Ffmpeg,
            1280,
            720,
            30,
            4000,
            "auto",
            0,
            false,
            false,
            5
        )),
        5
    );
}

#[test]
fn gstreamer_keyframe_interval_reaches_the_omx_gop() {
    // keyframe_interval=5 → interval-intraframes=5 on the Allwinner OMX path.
    let got = build(
        &params_cfg(
            EncoderKind::Gstreamer,
            1280,
            720,
            30,
            4000,
            "auto",
            0,
            false,
            false,
            5,
        ),
        "/dev/video1",
        RTSP_OUT,
        &usb_yuyv(),
        &allwinner_omx(),
        false,
    );
    let tok = got.to_vec();
    assert!(
        tok.iter().any(|t| t == "interval-intraframes=5"),
        "missing interval-intraframes=5"
    );
}

#[test]
fn omx_encoder_label_and_hardware_flag() {
    // The on-wire label for the OMX branch is gstreamer-omxh264videoenc and
    // it must read encoder_hw=true (the camera_state sidecar field).
    let got = build(
        &params(EncoderKind::Gstreamer, 1280, 720, 30, 4000),
        "/dev/video2",
        RTSP_OUT,
        &usb_yuyv(),
        &allwinner_omx(),
        false,
    );
    assert_eq!(
        encoder_label(EncoderKind::Gstreamer, &got),
        "gstreamer-omxh264videoenc"
    );
    assert!(encoder_is_hardware("gstreamer-omxh264videoenc"));
    assert!(!encoder_is_hardware("gstreamer-x264enc"));
}

#[test]
fn v4l2_capture_queue_is_deep_enough_to_absorb_a_scheduler_hiccup() {
    // `-thread_queue_size` is queue DEPTH in packets between the kernel's
    // V4L2 buffers and ffmpeg's demuxer thread. It only adds latency while
    // occupied; overrunning it does not delay frames, it DROPS them. The
    // old depth of 4 overran on any hiccup past ~130ms at 30fps.
    for env in [rockchip(), non_rk_hw()] {
        let got = build(
            &params(EncoderKind::Ffmpeg, 1280, 720, 30, 4000),
            "/dev/video1",
            RTSP_OUT,
            &usb_mjpeg(),
            &env,
            false,
        );
        let depth: u32 = flag_value(&got, "-thread_queue_size")
            .expect("v4l2 input carries -thread_queue_size")
            .parse()
            .expect("numeric queue depth");
        assert!(
            depth >= 64,
            "capture queue depth {depth} drops frames on a loaded SBC"
        );
    }
    // A network source has no v4l2 input stage, so it must not grow one.
    let ip = build(
        &params(EncoderKind::Ffmpeg, 1280, 720, 30, 4000),
        "rtsp://10.0.0.9:554/live",
        RTSP_OUT,
        &ip_cam(),
        &rockchip(),
        false,
    );
    assert!(!has_flag(&ip, "-thread_queue_size"));
}

#[test]
fn h264_v4l2m2m_carries_an_explicit_quantizer_bound() {
    // ffmpeg's v4l2_m2m_enc.c writes V4L2_CID_MPEG_VIDEO_H264_MIN_QP
    // unconditionally and substitutes 0 when `-qmin` is unset, clobbering
    // the `bcm2835-codec` firmware default of 20 and licensing an unbounded
    // bit spend on a scene change. Omitting `-qmin` is therefore NOT
    // "leaving the driver alone".
    let got = build(
        &params(EncoderKind::Ffmpeg, 1280, 720, 30, 4000),
        "/dev/video1",
        RTSP_OUT,
        &usb_mjpeg(),
        &non_rk_hw(),
        false,
    );
    assert!(got.iter().any(|t| t == "h264_v4l2m2m"), "HW path selected");
    let qmin: u32 = flag_value(&got, "-qmin")
        .expect("h264_v4l2m2m carries -qmin")
        .parse()
        .expect("numeric qmin");
    assert_eq!(qmin, V4L2M2M_MIN_QP);
    assert!(qmin > 0, "qmin 0 is ffmpeg's unbounded default");
    assert_eq!(flag_value(&got, "-qmax").as_deref(), Some("51"));

    // `-maxrate` / `-bufsize` must NOT be emitted here: v4l2_m2m_enc.c
    // never reads rc_max_rate / rc_buffer_size, so they would look like a
    // VBV and enforce nothing.
    assert!(!has_flag(&got, "-maxrate"));
    assert!(!has_flag(&got, "-bufsize"));
}

#[test]
fn mpph264enc_rate_control_is_pinned_to_the_link_budget() {
    // The VBV ceiling is the configured bitrate, not 1.5x it: surplus above
    // what the radio can carry does not become quality, it becomes queue
    // depth in the wfb_tx FEC block and then loss.
    for kbps in [4000u32, 12000] {
        let got = build(
            &params(EncoderKind::Gstreamer, 1280, 720, 30, kbps),
            "/dev/video1",
            RTSP_OUT,
            &usb_mjpeg(),
            &rk_mpp(),
            false,
        );
        let joined = got.join(" ");
        let bps = kbps * 1000;
        assert!(
            joined.contains(&format!("bps={bps} ")),
            "target bps: {joined}"
        );
        assert!(
            joined.contains(&format!("bps-max={bps} ")),
            "VBV ceiling must equal the link budget: {joined}"
        );
        // Spelled by name. MppEncRcMode is VBR=0, CBR=1, so `rc-mode=1`
        // was CBR — the mode with the known RK3588 rate-control defect —
        // while the comment beside it claimed VBR.
        assert!(joined.contains("rc-mode=vbr"), "rc-mode by name: {joined}");
        assert!(!joined.contains("rc-mode=1"), "no numeric rc-mode");
        // The first I-frame lands before the rate controller converges, so
        // bps-max cannot clamp it retroactively; qp-min must.
        assert!(joined.contains("qp-min=18"), "qp-min bound: {joined}");
    }
}

#[tokio::test]
async fn detect_async_matches_the_sync_probe() {
    // `detect` blocks on subprocess spawns and device ioctls; `detect_async`
    // is the same body moved onto a blocking pool, so the resolved env must
    // be identical. This also proves the async entry point is callable from
    // a runtime without the sync body panicking on it.
    let sync = EncoderEnv::detect();
    let async_env = EncoderEnv::detect_async().await;
    assert_eq!(sync.hw_h264.is_present(), async_env.hw_h264.is_present());
    assert_eq!(sync.has_mpph264enc, async_env.has_mpph264enc);
    assert_eq!(sync.has_omxh264videoenc, async_env.has_omxh264videoenc);
    assert_eq!(sync.has_rtspclientsink, async_env.has_rtspclientsink);
    assert_eq!(sync.encoder_api, async_env.encoder_api);
    assert_eq!(sync.pi5_class, async_env.pi5_class);
    assert_eq!(sync.cpu_threads, async_env.cpu_threads);
}

// --- latency contracts on the publish path -------------------------

#[test]
fn every_rtsp_publishing_ffmpeg_strips_the_muxer_delay() {
    // ffmpeg's muxer defaults are ~0.7 s of mux delay plus ~0.5 s of
    // preload. Every other ffmpeg in this tree strips them; the primary
    // encoder — the USB/IP default path — did not, so a USB-camera drone
    // shipped with up to 1.2 s of avoidable latency in front of every
    // other hop, invisible to the SEI probe because the probe reads the
    // same mediamtx path the delay sits in front of.
    //
    // `-muxdelay` / `-muxpreload` are ffmpeg CLI output options, NOT
    // AVFormatContext options, so they must appear at argv level even on
    // the tee form: inside a tee branch ffmpeg rejects them ("Unknown
    // option 'muxdelay'") and aborts the whole output.
    for (label, cmd) in [
        (
            "libx264 + fan-out",
            build(
                &params(EncoderKind::Ffmpeg, 1280, 720, 30, 4000),
                "/dev/video1",
                RTSP_OUT,
                &usb_mjpeg(),
                &rockchip(),
                false,
            ),
        ),
        (
            "libx264 plain",
            build(
                &EncoderParams {
                    rtp_fanout: false,
                    ..params(EncoderKind::Ffmpeg, 1280, 720, 30, 4000)
                },
                "/dev/video1",
                RTSP_OUT,
                &usb_mjpeg(),
                &rockchip(),
                false,
            ),
        ),
        (
            "h264_v4l2m2m",
            build(
                &params(EncoderKind::Ffmpeg, 1280, 720, 30, 4000),
                "/dev/video1",
                RTSP_OUT,
                &usb_mjpeg(),
                &non_rk_hw(),
                false,
            ),
        ),
        (
            "ip camera",
            build(
                &params(EncoderKind::Ffmpeg, 1280, 720, 30, 4000),
                "rtsp://10.0.0.9:554/live",
                RTSP_OUT,
                &ip_cam(),
                &rockchip(),
                false,
            ),
        ),
        (
            "rpicam bash pipeline",
            build(
                &params(EncoderKind::RpicamVid, 1280, 720, 30, 4000),
                "/dev/video0",
                RTSP_OUT,
                &csi(),
                &rockchip(),
                false,
            ),
        ),
        (
            "gstreamer → ffmpeg bridge",
            build(
                &params(EncoderKind::Gstreamer, 1280, 720, 30, 4000),
                "/dev/video1",
                RTSP_OUT,
                &usb_mjpeg(),
                &rk_mpp_noclient(),
                false,
            ),
        ),
        (
            "SEI-spliced publish stage",
            build(
                &params(EncoderKind::Ffmpeg, 1280, 720, 30, 4000),
                "/dev/video1",
                RTSP_OUT,
                &usb_mjpeg(),
                &rockchip(),
                true,
            ),
        ),
    ] {
        assert_eq!(
            flag_value(&cmd, "-muxdelay").as_deref(),
            Some("0"),
            "{label} must strip the mux delay"
        );
        assert_eq!(
            flag_value(&cmd, "-muxpreload").as_deref(),
            Some("0"),
            "{label} must strip the mux preload"
        );
    }
}

#[test]
fn every_encode_path_repeats_parameter_sets_per_idr() {
    // A decoder that loses sync mid-stream (a browser on the direct LAN
    // feed, or the ground ingest after an FEC-unrecoverable burst) can only
    // re-bootstrap from in-band SPS/PPS. The RTSP muxer's global-header
    // flag keeps libx264's and the M2M driver's parameter sets out-of-band,
    // so without an explicit lever they appear exactly once, at stream
    // start, and the picture freezes on the last decoded frame until the
    // page is reloaded.
    //
    // Per path: rpicam `--inline`, libx264/v4l2m2m `dump_extra=freq=
    // keyframe`, GStreamer `h264parse config-interval=1` (plus
    // `mpph264enc header-mode=1` at the encoder).
    let joined = |cmd: &[String]| cmd.join(" ");

    let rpicam = build(
        &params(EncoderKind::RpicamVid, 1280, 720, 30, 4000),
        "/dev/video0",
        RTSP_OUT,
        &csi(),
        &rockchip(),
        false,
    );
    assert!(has_flag(&rpicam, "--inline"), "rpicam repeats per IDR");

    for (label, env) in [("libx264", rockchip()), ("h264_v4l2m2m", non_rk_hw())] {
        let cmd = build(
            &params(EncoderKind::Ffmpeg, 1280, 720, 30, 4000),
            "/dev/video1",
            RTSP_OUT,
            &usb_mjpeg(),
            &env,
            false,
        );
        let bsf = flag_value(&cmd, "-bsf:v").expect("{label} carries a bitstream filter");
        assert!(
            bsf.contains("dump_extra=freq=keyframe"),
            "{label} must re-insert SPS/PPS per IDR, got {bsf:?}"
        );
        // The Annex-B conversion is only correct on the libx264 path; the
        // M2M encoder already emits Annex-B and applying it twice corrupts
        // the NAL boundaries.
        assert_eq!(
            bsf.contains("h264_mp4toannexb"),
            label == "libx264",
            "{label} mp4toannexb placement"
        );
    }

    for (label, env) in [
        ("x264enc software", non_rk_sw()),
        ("mpph264enc", rk_mpp()),
        ("omx", allwinner_omx()),
    ] {
        let cmd = build(
            &params(EncoderKind::Gstreamer, 1280, 720, 30, 4000),
            "/dev/video1",
            RTSP_OUT,
            &usb_mjpeg(),
            &env,
            false,
        );
        assert!(
            joined(&cmd).contains("h264parse config-interval=1"),
            "{label} must re-stamp SPS/PPS per IDR"
        );
    }
}

#[test]
fn the_radio_rtp_copy_comes_out_of_the_encoder_not_a_re_read() {
    // VID-02: one encode, two muxers. The RTP branch must carry the exact
    // framing contract the receiver's static SDP describes (payload type
    // 96, SSRC 0xCAFE, 1316-byte datagrams to 127.0.0.1:5600), and the RTSP
    // branch must still publish to mediamtx for WHEP.
    let cmd = build(
        &params(EncoderKind::Ffmpeg, 1280, 720, 30, 4000),
        "/dev/video1",
        RTSP_OUT,
        &usb_mjpeg(),
        &rockchip(),
        false,
    );
    assert_eq!(
        flag_value(&cmd, "-f").as_deref(),
        Some("v4l2"),
        "input side"
    );
    let spec = cmd.last().expect("the tee spec is the last token");
    assert_eq!(
        cmd[cmd.len() - 2],
        "tee",
        "the output muxer is the tee: {cmd:?}"
    );
    assert!(spec.contains(&format!(
        "[f=rtsp:rtsp_transport=tcp:max_delay=0:flush_packets=1]{RTSP_OUT}"
    )));
    assert!(spec.contains(&format!(
        "[f=rtp:payload_type={}:ssrc={}:max_delay=0:flush_packets=1]{}",
        crate::wfb_tee::WFB_TEE_PAYLOAD_TYPE,
        crate::wfb_tee::WFB_TEE_SSRC,
        crate::wfb_tee::rtp_destination_url(),
    )));
    // No `onfail=ignore`: a radio branch that cannot be opened must fail the
    // encoder (and be restarted) rather than silently serve LAN-only video.
    assert!(
        !spec.contains("onfail"),
        "shared fate, not a silent degrade"
    );
    // The tee needs the explicit map; without it ffmpeg refuses the output.
    assert!(cmd.windows(2).any(|w| w[0] == "-map" && w[1] == "0:v"));
    // And the orchestrator must be able to read the fact back off the argv.
    assert!(crate::wfb_tee::cmd_emits_wfb_rtp(&cmd));

    // The plain form (vision raw-tap / no radio) keeps the single RTSP
    // destination and must NOT read as emitting the radio copy.
    let plain = build(
        &EncoderParams {
            rtp_fanout: false,
            ..params(EncoderKind::Ffmpeg, 1280, 720, 30, 4000)
        },
        "/dev/video1",
        RTSP_OUT,
        &usb_mjpeg(),
        &rockchip(),
        false,
    );
    assert_eq!(plain.last().map(String::as_str), Some(RTSP_OUT));
    assert!(!crate::wfb_tee::cmd_emits_wfb_rtp(&plain));
    assert!(!plain.iter().any(|t| t == "tee"));

    // The bash-pipeline arms cannot carry a mapped second output, so they
    // keep the separate tap and must not claim otherwise.
    for (label, kind, cam, env) in [
        ("rpicam", EncoderKind::RpicamVid, csi(), rockchip()),
        (
            "gstreamer",
            EncoderKind::Gstreamer,
            usb_mjpeg(),
            rk_mpp_noclient(),
        ),
    ] {
        let cmd = build(
            &params(kind, 1280, 720, 30, 4000),
            "/dev/video0",
            RTSP_OUT,
            &cam,
            &env,
            false,
        );
        assert!(
            !crate::wfb_tee::cmd_emits_wfb_rtp(&cmd),
            "{label} must keep the separate tap"
        );
    }
}

#[test]
fn the_sei_splice_keeps_both_publish_branches() {
    // Turning the SEI probe on rebuilds the publish stage from scratch. If
    // that rebuild dropped the radio branch, enabling a measurement probe
    // would take the radio leg down — so the encode half must end at
    // stdout and the publish half must carry the identical tee spec.
    let cmd = build_encoder_command(
        &params(EncoderKind::Ffmpeg, 1280, 720, 30, 4000),
        "/dev/video1",
        RTSP_OUT,
        Some(&usb_mjpeg()),
        &rockchip(),
    )
    .unwrap();
    let split = wrap_with_sei_inject(&cmd, RTSP_OUT).expect("an ffmpeg publisher splits");
    let encode = split.encode.join(" ");
    let publish = split.publish.join(" ");
    // The encode half ends at stdout and carries no output-muxer leftovers.
    assert!(encode.ends_with("-f h264 -"));
    assert!(
        !encode.contains("-f tee"),
        "no muxer left on the encode half"
    );
    assert!(!encode.contains("-map 0:v"));
    assert!(!encode.contains("-rtsp_transport"));
    assert!(!encode.contains("-muxdelay"));
    // No Python stage anywhere: the injector runs in-process.
    assert!(!encode.contains("python") && !publish.contains("python"));
    // The publish half re-mounts BOTH destinations, through the same
    // output-stage builder the non-SEI path uses.
    assert_eq!(split.publish[0], "ffmpeg");
    assert!(publish.contains("-f h264 -i -"));
    assert!(publish.contains("-f tee"));
    assert!(publish.contains(RTSP_OUT));
    assert!(publish.contains(&crate::wfb_tee::rtp_destination_url()));
    assert!(crate::wfb_tee::cmd_emits_wfb_rtp(&split.render()));

    // A GStreamer command is not split.
    let gst = vec!["gst-launch-1.0".to_string(), "-e".to_string()];
    assert_eq!(wrap_with_sei_inject(&gst, RTSP_OUT), None);
}

#[test]
fn sliced_threading_is_gated_on_probed_cores() {
    // Frame-level threading holds `threads` frames in flight; sliced
    // threading splits one frame across the cores and emits it whole. On
    // Pi-5-class silicon x264 IS the encoder, so this is the largest
    // remaining lever in the encode hop — but only where there are cores to
    // split across. Two cores keep the conservative form.
    let four = build(
        &params(EncoderKind::Ffmpeg, 1280, 720, 30, 4000),
        "/dev/video1",
        RTSP_OUT,
        &usb_mjpeg(),
        &rockchip(),
        false,
    );
    assert_eq!(flag_value(&four, "-threads").as_deref(), Some("4"));
    let x264 = flag_value(&four, "-x264-params").expect("x264 params");
    assert!(x264.contains("sliced-threads=1"), "{x264}");
    assert!(x264.contains("slices=4"), "{x264}");

    let two = build(
        &params(EncoderKind::Ffmpeg, 1280, 720, 30, 4000),
        "/dev/video1",
        RTSP_OUT,
        &usb_mjpeg(),
        &dual_core(),
        false,
    );
    assert_eq!(flag_value(&two, "-threads").as_deref(), Some("2"));
    let x264_two = flag_value(&two, "-x264-params").expect("x264 params");
    assert!(x264_two.contains("sliced-threads=0"), "{x264_two}");
    assert!(!x264_two.contains("slices="), "{x264_two}");

    // Intra-refresh stays forbidden on every path: it removes true IDR NALs
    // and the ingest parser cannot bootstrap SPS/PPS from a refreshed
    // stream.
    for cmd in [&four, &two] {
        assert!(!cmd.join(" ").contains("intra-refresh"));
    }
}

/// No configured or derived GOP exceeds the ceiling, and none is zero.
#[test]
fn the_gop_never_leaves_its_bounds() {
    let long = params_cfg(
        EncoderKind::Ffmpeg,
        1280,
        720,
        30,
        4000,
        "auto",
        0,
        false,
        false,
        300,
    );
    assert_eq!(gop_interval(&long), MAX_GOP_FRAMES);
    let got = build(
        &long,
        "/dev/video1",
        RTSP_OUT,
        &usb_mjpeg(),
        &rockchip(),
        false,
    );
    assert_eq!(flag_value(&got, "-g").as_deref(), Some("30"));
    // The derived half-second GOP at a high frame rate is held too.
    assert_eq!(
        gop_interval(&params(EncoderKind::Ffmpeg, 1280, 720, 120, 4000)),
        MAX_GOP_FRAMES
    );
    assert_eq!(
        gop_interval(&params(EncoderKind::Ffmpeg, 1280, 720, 1, 4000)),
        1
    );
}

/// A TLS camera URL is a network input like its plain form: ffmpeg reads it
/// directly over TCP and never through the V4L2 demuxer.
#[test]
fn a_tls_network_source_is_read_as_a_network_stream() {
    for src in ["rtsps://10.0.0.9:322/live", "https://10.0.0.9/live.m3u8"] {
        let got = build(
            &params(EncoderKind::Ffmpeg, 1280, 720, 30, 4000),
            src,
            RTSP_OUT,
            &ip_cam(),
            &rockchip(),
            false,
        );
        assert!(!got.join(" ").contains("v4l2 "), "{src}: {got:?}");
        assert_eq!(flag_value(&got, "-i").as_deref(), Some(src));
    }
    let rtsps = build(
        &params(EncoderKind::Ffmpeg, 1280, 720, 30, 4000),
        "rtsps://10.0.0.9:322/live",
        RTSP_OUT,
        &ip_cam(),
        &rockchip(),
        false,
    );
    assert_eq!(
        flag_value(&rtsps, "-rtsp_transport").as_deref(),
        Some("tcp")
    );
}

/// On a board whose default route is the OMX GStreamer encoder, a network
/// camera still gets the probed ffmpeg builder: the OMX pipeline opens its
/// input with `v4l2src`, which cannot read a URL.
#[test]
fn a_network_source_never_takes_the_omx_capture_pipeline() {
    for encoder in ["auto", "omx"] {
        let p = params_cfg(
            EncoderKind::Ffmpeg,
            1280,
            720,
            30,
            4000,
            encoder,
            0,
            false,
            false,
            0,
        );
        let got = build(
            &p,
            "rtsp://10.0.0.9:554/live",
            RTSP_OUT,
            &ip_cam(),
            &allwinner_omx(),
            false,
        );
        assert_eq!(got[0], "ffmpeg", "{encoder}: {got:?}");
        assert!(!got.join(" ").contains("v4l2src"));
    }
    // A local camera on the same board still takes the OMX path.
    let local = build(
        &params(EncoderKind::Ffmpeg, 1280, 720, 30, 4000),
        "/dev/video1",
        RTSP_OUT,
        &usb_mjpeg(),
        &allwinner_omx(),
        false,
    );
    assert!(local.join(" ").contains("omxh264videoenc"));
}
