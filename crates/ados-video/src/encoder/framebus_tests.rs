//! Encoder input selection for a `framebus://<camera_id>` source.

use super::*;
use ados_protocol::framebus::FrameFormat;

const SRC: &str = "framebus://cam_fl";

fn fifo() -> String {
    crate::framebus_input::fifo_path("cam_fl")
        .to_string_lossy()
        .into_owned()
}

fn framebus_params(kind: EncoderKind, format: Option<FrameFormat>) -> EncoderParams {
    EncoderParams {
        framebus_input: format.map(|format| RawInput {
            format,
            width: 1280,
            height: 800,
            fps: 20,
        }),
        ..params(kind, 1280, 800, 20, 4000)
    }
}

fn framebus_cam() -> CameraInfo {
    CameraInfo {
        camera_type: CameraType::Framebus,
        device_path: SRC.into(),
        capabilities: vec!["framebus".into()],
    }
}

#[test]
fn a_framebus_source_is_read_as_raw_video_from_its_pipe() {
    let p = framebus_params(EncoderKind::Ffmpeg, Some(FrameFormat::Gray8));
    let cmd = build_encoder_command(&p, SRC, RTSP_OUT, Some(&framebus_cam()), &rockchip()).unwrap();
    assert_eq!(cmd[0], "ffmpeg");
    assert!(has_pair(&cmd, "-f", "rawvideo"), "{cmd:?}");
    assert!(has_pair(&cmd, "-pix_fmt", "gray"), "{cmd:?}");
    assert!(has_pair(&cmd, "-video_size", "1280x800"), "{cmd:?}");
    assert!(has_pair(&cmd, "-framerate", "20"), "{cmd:?}");
    assert!(has_pair(&cmd, "-i", &fifo()), "{cmd:?}");
    // The device is the vision engine's: never a V4L2 input, never the URI.
    assert!(!has_pair(&cmd, "-f", "v4l2"), "{cmd:?}");
    assert!(!cmd.iter().any(|t| t.contains("framebus://")), "{cmd:?}");
    assert!(has_pair(&cmd, "-c:v", "libx264"), "{cmd:?}");
    assert!(!cmd.iter().any(|t| t == "-s"), "no rescale at the captured size: {cmd:?}");
}

#[test]
fn an_attention_switch_rescales_instead_of_misreading_the_pipe() {
    let p = EncoderParams {
        width: 320,
        height: 180,
        fps: 1,
        ..framebus_params(EncoderKind::Ffmpeg, Some(FrameFormat::Gray8))
    };
    let cmd = build_encoder_command(&p, SRC, RTSP_OUT, Some(&framebus_cam()), &rockchip()).unwrap();
    assert!(has_pair(&cmd, "-video_size", "1280x800"), "{cmd:?}");
    assert!(has_pair(&cmd, "-framerate", "20"), "{cmd:?}");
    assert!(has_pair(&cmd, "-s", "320x180"), "{cmd:?}");
    assert!(has_pair(&cmd, "-r", "1"), "{cmd:?}");
    let line = build_encoder_command(&p, SRC, RTSP_OUT, Some(&framebus_cam()), &rk_mpp())
        .unwrap()
        .join(" ");
    assert!(line.contains("rawvideoparse width=1280 height=800"), "{line}");
    assert!(line.contains("video/x-raw,format=NV12,width=320,height=180"), "{line}");
}

#[test]
fn a_framebus_source_takes_the_rockchip_vpu_when_mpp_is_present() {
    let p = framebus_params(EncoderKind::Ffmpeg, Some(FrameFormat::Rgb24));
    let cmd = build_encoder_command(&p, SRC, RTSP_OUT, Some(&framebus_cam()), &rk_mpp()).unwrap();
    assert_eq!(cmd[0], "gst-launch-1.0", "{cmd:?}");
    let line = cmd.join(" ");
    assert!(line.contains(&format!("filesrc location={}", fifo())), "{line}");
    assert!(
        line.contains("rawvideoparse width=1280 height=800 format=rgb framerate=20/1"),
        "{line}"
    );
    assert!(line.contains("video/x-raw,format=NV12,width=1280,height=800"), "{line}");
    assert!(line.contains("mpph264enc"), "{line}");
    assert!(line.contains("rc-mode=vbr"), "{line}");
    assert!(!line.contains("v4l2src"), "{line}");
    assert!(!line.contains("rga"), "{line}");
    assert_eq!(encoder_label(EncoderKind::Gstreamer, &cmd), "gstreamer-mpph264enc");
}

#[test]
fn the_forced_software_fallback_leaves_the_vpu() {
    // What the no-first-packet ladder does after the VPU produced nothing.
    let p = EncoderParams {
        encoder: "software".into(),
        ..framebus_params(EncoderKind::Ffmpeg, Some(FrameFormat::Gray8))
    };
    let cmd = build_encoder_command(&p, SRC, RTSP_OUT, Some(&framebus_cam()), &rk_mpp()).unwrap();
    assert_eq!(cmd[0], "ffmpeg", "{cmd:?}");
    assert!(has_pair(&cmd, "-c:v", "libx264"), "{cmd:?}");
    assert!(has_pair(&cmd, "-i", &fifo()), "{cmd:?}");
    // On a GStreamer-only board the fallback stays in the family, in software.
    let gst = EncoderParams {
        kind: EncoderKind::Gstreamer,
        ..p
    };
    let line = build_encoder_command(&gst, SRC, RTSP_OUT, Some(&framebus_cam()), &rk_mpp())
        .unwrap()
        .join(" ");
    assert!(line.contains("x264enc") && !line.contains("mpph264enc"), "{line}");
}

#[test]
fn a_framebus_source_never_takes_the_v4l2src_omx_path() {
    let p = framebus_params(EncoderKind::Ffmpeg, Some(FrameFormat::Nv12));
    let cmd =
        build_encoder_command(&p, SRC, RTSP_OUT, Some(&framebus_cam()), &allwinner_omx()).unwrap();
    assert_eq!(cmd[0], "ffmpeg", "{cmd:?}");
    assert!(has_pair(&cmd, "-pix_fmt", "nv12"), "{cmd:?}");
}

#[test]
fn a_framebus_source_without_a_format_is_refused() {
    let p = framebus_params(EncoderKind::Ffmpeg, None);
    assert_eq!(
        build_encoder_command(&p, SRC, RTSP_OUT, Some(&framebus_cam()), &rockchip()),
        Err(EncoderError::MissingFramebusFormat(SRC.into()))
    );
}

#[test]
fn framebus_detection_and_input_locator() {
    assert_eq!(
        detect_encoder_for_camera(CameraType::Framebus, false, true, true),
        Some(EncoderKind::Ffmpeg)
    );
    assert_eq!(
        detect_encoder_for_camera(CameraType::Framebus, false, false, true),
        Some(EncoderKind::Gstreamer)
    );
    assert_eq!(input_locator(SRC), fifo());
    assert_eq!(input_locator("/dev/video0"), "/dev/video0");
}
