//! `gst-launch-1.0` pipeline builder.

use super::rpicam::select_input_format;
use super::*;

/// GStreamer pipeline command.
///
/// On Rockchip with `mpph264enc` present: hardware VPU encode. Otherwise
/// software `x264enc`. RTSP output uses `rtspclientsink` (RTSP RECORD) when the
/// element is available, else pipes the elementary stream through ffmpeg for
/// RTSP muxing. File output uses a direct `filesink` pipeline.
pub(super) fn build_gstreamer_command(
    params: &EncoderParams,
    source: &str,
    output: &str,
    camera: Option<&CameraInfo>,
    env: &EncoderEnv,
) -> Vec<String> {
    let safe_source = gst_quote(source);

    let input_fmt = select_input_format(camera);
    let (src_caps, decode) = if input_fmt == Some("mjpeg") {
        (
            format!(
                "image/jpeg,width={},height={},framerate={}/1",
                params.width, params.height, params.fps
            ),
            "jpegdec ! videoconvert",
        )
    } else {
        (
            format!(
                "video/x-raw,width={},height={},framerate={}/1",
                params.width, params.height, params.fps
            ),
            "videoconvert",
        )
    };

    // Image orientation: a chain of `videoflip` elements applied after decode /
    // before encode. On the OMX/HW path this is a CPU transform (the HW
    // encoders do not rotate natively) — a small latency/CPU cost, acceptable.
    let flip = gst_videoflip_chain(params.rotation, params.hflip, params.vflip);

    let gop = gop_interval(params);
    let use_omx = use_omx_encoder(params, env);
    let encoder = if params.encoder == "software" {
        // Explicit software override — always desktop x264enc, never a HW path.
        format!(
            "x264enc bitrate={} speed-preset=ultrafast tune=zerolatency \
             threads=2 sliced-threads=false key-int-max={gop}",
            params.bitrate_kbps
        )
    } else if use_omx {
        // Allwinner Cedar OMX HW H.264 encoder. CBR via
        // `control-rate=constant target-bitrate=<bps>` (bps = kbps*1000) so the
        // radio FEC never sees a starving/scene-crazy bitrate; a short GOP
        // aligned to FEC recovery. Property names verified on-rig
        // (`gst-inspect-1.0 omxh264videoenc`); if the board's OMX proves
        // unusable, software libx264 remains the fallback.
        //
        // VERDICT — settled, do not re-litigate. On Allwinner this OMX path is
        // correct and it is the *only* viable hardware encode path. Mainline
        // Cedrus is decode-only, and out-of-tree Cedar encode support exists
        // only for V3 / V3s / S3. So the alternatives to `omxh264videoenc`
        // here are not "a cleaner V4L2 M2M encoder" — they are software
        // x264enc. The `encoder_api == "vendor"` + element-presence gate above
        // is the honest board test (there is no Rust-side board-YAML reader),
        // and the code below stays as it is.
        let bps = params.bitrate_kbps * 1000;
        format!(
            "omxh264videoenc control-rate=constant target-bitrate={bps} interval-intraframes={gop}"
        )
    } else if env.has_mpph264enc {
        // Rockchip VPU. Rate control is pinned to the radio's link budget:
        //
        // * `rc-mode=vbr` is spelled **by name**, never by number.
        //   `MppEncRcMode` is `VBR=0, CBR=1, FIXQP=2`, so the `rc-mode=1` this
        //   builder used to emit was CBR — the mode with the known upstream
        //   RK3588 rate-control defect (rockchip-linux/mpp#429) — while the
        //   comment beside it claimed VBR. A numeric literal here has already
        //   cost two blind retest cycles; see
        //   [`detect_encoder_for_camera`] for the full retest procedure.
        // * `bps-max` IS the VBV ceiling, and it is sized to the configured
        //   bitrate (1.0x), not 1.5x. The old 1.5x let a scene change put 50%
        //   more than the link can carry into the wfb_tx FEC block, where the
        //   surplus does not become "more quality", it becomes queue depth and
        //   then loss. A ceiling above the link budget is not a latency win.
        // * `qp-min=18` bounds the first I-frame, which is emitted before the
        //   rate controller has converged and is therefore the one frame
        //   `bps-max` cannot retroactively clamp. `qp-min=5` permitted a
        //   near-lossless keyframe straight into a bitrate-sized radio link.
        //
        // `header-mode=1` inserts SPS/PPS before every IDR for late joiners.
        let bps = params.bitrate_kbps * 1000;
        let bps_max = bps;
        let bps_min = params.bitrate_kbps / 2 * 1000;
        format!(
            "mpph264enc bps={bps} bps-max={bps_max} bps-min={bps_min} \
             qp-min=18 qp-max=51 rc-mode=vbr gop={gop} header-mode=1"
        )
    } else {
        // x264enc software fallback bounded to ~2 frames of pipeline latency.
        format!(
            "x264enc bitrate={} speed-preset=ultrafast tune=zerolatency \
             threads=2 sliced-threads=false key-int-max={gop}",
            params.bitrate_kbps
        )
    };

    // Capture → decode → orientation prefix, ending right before h264parse. The
    // OMX path is capture-direct with `io-mode=mmap` + the frame-dropping `queue
    // max-size-buffers=4 leaky=downstream` so the Cedar HW encoder never backs up
    // behind the source. NOTE (verified on the A733 Cedar): `omxh264videoenc`
    // will NOT negotiate if the source carries `do-timestamp=true` OR the NV12
    // caps pin a `framerate` — in either case the encoder silently falls back to
    // its 176x144 default and the pipeline fails to preroll. So set neither here;
    // the framerate flows from the `image/jpeg` source caps.
    let core = if use_omx {
        format!(
            "v4l2src device={safe_source} io-mode=mmap ! {src_caps} ! \
             {decode} ! {flip}video/x-raw,format=NV12,width={},height={} ! \
             queue max-size-buffers=4 leaky=downstream ! {encoder}",
            params.width, params.height
        )
    } else {
        format!("v4l2src device={safe_source} ! {src_caps} ! {decode} ! {flip}{encoder}")
    };
    // `h264parse config-interval=1` re-stamps SPS/PPS in-band before every IDR,
    // so a radio FEC recovery or a late joiner resyncs at the next keyframe
    // (≤0.5 s at the contracted GOP) instead of freezing on the last decoded
    // frame until the pipeline is restarted. It used to be emitted on the OMX
    // arm only, which left the `x264enc` software arm — the fallback every
    // board without a VPU lands on — publishing parameter sets at stream start
    // and never again. `mpph264enc`'s own `header-mode=1` does the same job at
    // the encoder; the parse element is harmless beside it and the arms now
    // agree.
    let h264parse = " ! h264parse config-interval=1";

    if output.starts_with("rtsp://") {
        let safe_output = gst_quote(output);
        if env.has_rtspclientsink {
            // Direct GStreamer → mediamtx via rtspclientsink (RTSP RECORD).
            let pipeline = format!(
                "{core}{h264parse} ! \
                 rtspclientsink location={safe_output} protocols=tcp latency=0"
            );
            let mut out: Vec<String> = vec!["gst-launch-1.0".into(), "-e".into()];
            out.extend(pipeline.split(' ').map(|s| s.to_string()));
            return out;
        }
        // Fallback: pipe GStreamer H.264 → ffmpeg for RTSP muxing.
        let gst_cmd = format!(
            "gst-launch-1.0 -q {core}{h264parse} ! \
             'video/x-h264,stream-format=byte-stream' ! fdsink fd=1"
        );
        let ffmpeg_cmd = format!(
            "ffmpeg -y -fflags nobuffer -f h264 -i pipe:0 \
             -c:v copy \
             -max_delay 0 -muxdelay 0 -muxpreload 0 -flush_packets 1 \
             -rtsp_transport tcp -f rtsp {safe_output}"
        );
        return vec![
            "bash".into(),
            "-c".into(),
            format!("{gst_cmd} 2>/dev/null | {ffmpeg_cmd}"),
        ];
    }

    // File / other output: direct GStreamer pipeline.
    let safe_output = gst_quote(output);
    let pipeline = format!("{core}{h264parse} ! filesink location={safe_output}");
    let mut out: Vec<String> = vec!["gst-launch-1.0".into(), "-e".into()];
    out.extend(pipeline.split(' ').map(|s| s.to_string()));
    out
}

/// `shlex.quote` equivalent: returns the string verbatim when it is non-empty
/// and contains only "safe" characters, otherwise wraps it in single quotes
/// with embedded single-quotes escaped. Used for the GStreamer pipeline tokens
/// (device path / output location) the way the predecessor uses `shlex.quote`.
pub(super) fn gst_quote(s: &str) -> String {
    // shlex's _find_unsafe allowlist: ASCII letters, digits, and @%+=:,./-_
    const SAFE: &[char] = &['@', '%', '+', '=', ':', ',', '.', '/', '-', '_'];
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || SAFE.contains(&c))
    {
        s.to_string()
    } else if s.is_empty() {
        "''".to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\"'\"'"))
    }
}
