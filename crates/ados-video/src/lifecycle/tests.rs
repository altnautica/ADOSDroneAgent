use super::*;
use crate::encoder::{EncoderEnv, EncoderKind};
use crate::process::cmdline_publishes_to;
use ados_protocol::hwcaps::{AbsenceReason, Probed};

const RTSP_PORT: u16 = 8554;

fn env() -> EncoderEnv {
    EncoderEnv {
        hw_h264: Probed::absent(AbsenceReason::NodeMissing),
        has_mpph264enc: false,
        has_omxh264videoenc: false,
        has_rtspclientsink: true,
        encoder_api: "unknown".into(),
        pi5_class: false,
        cpu_threads: 4,
    }
}

fn encoder_params(kind: EncoderKind, rtp_fanout: bool) -> EncoderParams {
    EncoderParams {
        kind,
        codec: "h264".into(),
        width: 1280,
        height: 720,
        fps: 30,
        bitrate_kbps: 4000,
        encoder: "auto".into(),
        rotation: 0,
        hflip: false,
        vflip: false,
        keyframe_interval: 0,
        rtp_fanout,
        passthrough: false,
        framebus_input: None,
    }
}

fn usb_cam() -> CameraInfo {
    CameraInfo {
        camera_type: CameraType::Usb,
        device_path: "/dev/video1".into(),
        capabilities: vec!["mjpeg".into()],
    }
}

fn csi_cam() -> CameraInfo {
    CameraInfo {
        camera_type: CameraType::Csi,
        device_path: "/dev/video0".into(),
        capabilities: vec!["h264".into()],
    }
}

/// How procfs presents a process: argv joined by spaces.
fn cmdline(program: &str, args: &[String]) -> String {
    std::iter::once(program.to_string())
        .chain(args.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ")
}

/// An attention switch (and every adaptive-bitrate step) respawns ONLY the
/// encoder. The pre-respawn sweep that clears a stale publisher off
/// mediamtx's `main` slot used to match the bare URI — which the radio tap,
/// the cloud push, the vision tap and the SEI tap all carry too, because
/// they read that exact stream. So a routine bitrate step SIGKILLed all
/// four and dropped the operator's radio picture, the cloud feed, the
/// detection stream and the latency probe.
///
/// Every command line here comes from the real argv builders, so the test
/// cannot drift away from what actually runs.
#[test]
fn encoder_respawn_sweep_kills_publishers_and_spares_every_reader() {
    let uri = crate::wfb_tee::local_rtsp_url(RTSP_PORT);

    // --- must be swept: everything that can hold the `main` publisher slot.
    let ffmpeg_plain = build_encoder_command(
        &encoder_params(EncoderKind::Ffmpeg, false),
        "/dev/video1",
        &uri,
        Some(&usb_cam()),
        &env(),
    )
    .unwrap();
    let ffmpeg_fanout = build_encoder_command(
        &encoder_params(EncoderKind::Ffmpeg, true),
        "/dev/video1",
        &uri,
        Some(&usb_cam()),
        &env(),
    )
    .unwrap();
    let rpicam = build_encoder_command(
        &encoder_params(EncoderKind::RpicamVid, true),
        "/dev/video0",
        &uri,
        Some(&csi_cam()),
        &env(),
    )
    .unwrap();
    let gstreamer = build_encoder_command(
        &encoder_params(EncoderKind::Gstreamer, false),
        "/dev/video1",
        &uri,
        Some(&usb_cam()),
        &env(),
    )
    .unwrap();
    // The SEI split the latency bench runs: its publish stage is a process
    // of its own whose command line names the URI as its destination.
    let sei_publish = wrap_with_sei_inject(&ffmpeg_fanout, &uri)
        .expect("an ffmpeg publisher splits")
        .publish;
    // The GStreamer arm on a board without `rtspclientsink`: gst → ffmpeg
    // publish, also a `bash -c` pipeline.
    let gst_via_ffmpeg = build_encoder_command(
        &encoder_params(EncoderKind::Gstreamer, false),
        "/dev/video1",
        &uri,
        Some(&usb_cam()),
        &EncoderEnv {
            has_rtspclientsink: false,
            ..env()
        },
    )
    .unwrap();

    for (name, argv) in [
        ("ffmpeg plain publish", &ffmpeg_plain),
        ("ffmpeg radio fan-out", &ffmpeg_fanout),
        ("rpicam bash pipeline", &rpicam),
        ("gstreamer rtspclientsink", &gstreamer),
        ("sei-split publish stage", &sei_publish),
        ("gstreamer via ffmpeg publish", &gst_via_ffmpeg),
    ] {
        let line = cmdline(&argv[0], &argv[1..]);
        assert!(
            cmdline_publishes_to(&line, &uri),
            "{name} publishes into `main` and MUST be swept: {line}"
        );
    }

    // --- must survive: every consumer of the same stream.
    let readers: [(&str, String); 4] = [
        (
            "wfb radio tap",
            cmdline(
                "ffmpeg",
                &crate::wfb_tee::wfb_tee_args(&uri, &crate::wfb_tee::rtp_destination_url()),
            ),
        ),
        (
            "vision tap",
            cmdline("ffmpeg", &tap::vision_tap_args(&uri, 10, 640, 480, "rgb24")),
        ),
        (
            "cloud push",
            cmdline(
                "ffmpeg",
                &cloud_push_args(&uri, "rtsp://relay.example.com/main"),
            ),
        ),
        (
            "sei tap",
            cmdline("/opt/ados/venv/bin/python3", &sei_tap_args(&uri)),
        ),
    ];
    for (name, line) in &readers {
        // Negative control. The sweep this replaced was
        // `pgrep -f -- <uri>`, i.e. it matched any command line CONTAINING
        // the URI — so this assertion is simultaneously the proof that the
        // old sweep did kill this reader, and the guard that keeps the next
        // assertion from passing vacuously.
        assert!(
            line.contains(&uri),
            "{name} must carry the URI, or this test proves nothing: {line}"
        );
        assert!(
            !cmdline_publishes_to(line, &uri),
            "{name} only reads `main` and MUST survive an encoder respawn: {line}"
        );
    }
}
