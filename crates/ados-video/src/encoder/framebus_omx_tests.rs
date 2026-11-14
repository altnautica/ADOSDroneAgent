//! A captured camera on an Allwinner vendor board: the frame bus feeds the
//! board's OMX hardware encoder.

use super::*;

/// The Cubie A7S as the HAL describes it (`encoder_api: vendor`) with
/// `omxh264videoenc` present, the leg probed as ffmpeg exactly as the
/// orchestrator probes a frame-bus camera.
#[test]
fn a_framebus_source_on_a_vendor_board_takes_the_omx_encoder() {
    let p = framebus_params(EncoderKind::Ffmpeg, Some(FrameFormat::Rgb24));
    let cmd =
        build_encoder_command(&p, SRC, RTSP_OUT, Some(&framebus_cam()), &allwinner_omx()).unwrap();
    assert_eq!(cmd[0], "gst-launch-1.0", "{cmd:?}");
    let line = cmd.join(" ");
    // Read off the pipe, never off the device the vision engine owns.
    assert!(
        line.contains(&format!(
            "filesrc location={} ! rawvideoparse width=1280 height=800 format=rgb framerate=20/1",
            fifo()
        )),
        "{line}"
    );
    assert!(!line.contains("v4l2src"), "{line}");
    // `omxh264videoenc` does not negotiate behind `do-timestamp=true` or an NV12
    // capsfilter that pins a framerate.
    assert!(!line.contains("do-timestamp"), "{line}");
    assert!(
        line.contains(
            "video/x-raw,format=NV12,width=1280,height=800 ! \
             queue max-size-buffers=4 leaky=downstream ! omxh264videoenc"
        ),
        "{line}"
    );
    assert!(
        line.contains("control-rate=constant target-bitrate=4000000"),
        "{line}"
    );
    assert!(line.contains("h264parse config-interval=1"), "{line}");
    // The sidecar label names the hardware encoder even though the caller
    // holds the probed (ffmpeg) kind.
    let label = encoder_label(p.kind, &cmd);
    assert_eq!(label, "gstreamer-omxh264videoenc");
    assert!(encoder_is_hardware(&label));
}

#[test]
fn an_nv12_bus_frame_reaches_the_omx_encoder_as_nv12() {
    let p = framebus_params(EncoderKind::Ffmpeg, Some(FrameFormat::Nv12));
    let line = build_encoder_command(&p, SRC, RTSP_OUT, Some(&framebus_cam()), &allwinner_omx())
        .unwrap()
        .join(" ");
    assert!(line.contains("format=nv12 framerate=20/1"), "{line}");
    assert!(line.contains("omxh264videoenc"), "{line}");
}

#[test]
fn a_vendor_board_without_the_omx_element_keeps_libx264() {
    let env = EncoderEnv {
        has_omxh264videoenc: false,
        ..allwinner_omx()
    };
    let p = framebus_params(EncoderKind::Ffmpeg, Some(FrameFormat::Rgb24));
    let cmd = build_encoder_command(&p, SRC, RTSP_OUT, Some(&framebus_cam()), &env).unwrap();
    assert_eq!(cmd[0], "ffmpeg", "{cmd:?}");
    assert!(has_pair(&cmd, "-c:v", "libx264"), "{cmd:?}");
    assert!(has_pair(&cmd, "-i", &fifo()), "{cmd:?}");
    let label = encoder_label(p.kind, &cmd);
    assert_eq!(label, "ffmpeg-libx264");
    assert!(!encoder_is_hardware(&label));
}

#[test]
fn the_forced_software_fallback_leaves_the_omx_encoder() {
    let p = EncoderParams {
        encoder: "software".into(),
        ..framebus_params(EncoderKind::Ffmpeg, Some(FrameFormat::Rgb24))
    };
    let cmd =
        build_encoder_command(&p, SRC, RTSP_OUT, Some(&framebus_cam()), &allwinner_omx()).unwrap();
    assert_eq!(cmd[0], "ffmpeg", "{cmd:?}");
    assert!(has_pair(&cmd, "-c:v", "libx264"), "{cmd:?}");
}

#[test]
fn the_rockchip_frame_bus_label_names_the_gstreamer_family() {
    // Probed as ffmpeg, built as GStreamer mpp: the label follows the element.
    let p = framebus_params(EncoderKind::Ffmpeg, Some(FrameFormat::Rgb24));
    let cmd = build_encoder_command(&p, SRC, RTSP_OUT, Some(&framebus_cam()), &rk_mpp()).unwrap();
    assert_eq!(encoder_label(p.kind, &cmd), "gstreamer-mpph264enc");
}
