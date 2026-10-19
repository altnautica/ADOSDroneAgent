//! `ffmpeg` argv builder for USB / IP cameras.

use super::rpicam::select_input_format;
use super::tap_splice::ffmpeg_output_stage;
use super::*;

/// `ffmpeg` command for USB / IP / frame-bus camera encoding.
///
/// Network sources skip the v4l2 wrapper. V4L2 sources prepend the
/// low-latency input flags and the capability-selected `-input_format`. A
/// frame-bus source reads headerless raw video of the leg's geometry out of its
/// pipe. The output tuning differs by codec: `libx264` gets the full
/// low-latency browser-compat block + x264-params; `h264_v4l2m2m` gets a
/// leaner HW block.
pub(super) fn build_ffmpeg_command(
    params: &EncoderParams,
    source: &str,
    output: &str,
    camera: Option<&CameraInfo>,
    env: &EncoderEnv,
    framebus: Option<&FramebusInput>,
) -> Vec<String> {
    // Use the HW H.264 encoder only when the HAL probe confirmed a real V4L2
    // encoder device is Present; otherwise map the codec to a sw encoder. A
    // mere `-encoders` listing is NOT enough — a wrapper with no backing device
    // makes ffmpeg exit at init and the camera streams zero bytes.
    // An explicit `encoder` override biases this: "software" always picks
    // libx264; "v4l2m2m" forces the HW M2M wrapper even if the probe was back-
    // level. "omx"/"auto" keep the probe-driven decision.
    let force_sw = params.encoder == "software";
    let force_v4l2m2m = params.encoder == "v4l2m2m";
    let use_hw_h264 = !force_sw && (force_v4l2m2m || env.hw_h264.is_present());

    // H.264 is the only admitted codec (`validate_codec`, applied in
    // `build_encoder_command` before any arm runs), so the choice left here is
    // hardware vs software — not which codec. A `libx265`/`mjpeg` mapping used
    // to live here and fell through BOTH tuning blocks below, emitting no `-g`
    // and no in-band parameter-set filter onto an H.264-framed RTP leg.
    let ffmpeg_codec: String = if use_hw_h264 {
        "h264_v4l2m2m".to_string()
    } else {
        "libx264".to_string()
    };

    let mut cmd: Vec<String> = vec!["ffmpeg".into(), "-y".into()];

    if let Some(fb) = framebus {
        // Raw frames the vision engine already captured: no device to probe
        // and nothing to demux, so the input is fully described up front.
        cmd.extend(
            [
                "-fflags",
                "nobuffer",
                "-flags",
                "low_delay",
                "-f",
                "rawvideo",
                "-pix_fmt",
            ]
            .iter()
            .map(|s| s.to_string()),
        );
        cmd.push(crate::framebus_input::ffmpeg_pix_fmt(fb.raw.format).to_string());
        cmd.push("-video_size".into());
        cmd.push(format!("{}x{}", fb.raw.width, fb.raw.height));
        cmd.push("-framerate".into());
        cmd.push(fb.raw.fps.to_string());
        cmd.push("-i".into());
        cmd.push(fb.fifo.clone());
    } else if crate::config::is_network_url(source) {
        // Network / IP camera source. Low-latency input; force TCP for RTSP so a
        // lossy link cannot drop RTP packets and truncate frames. UDP is the
        // ffmpeg default for RTSP and a single lost packet shreds an H.264 frame
        // into macroblock garbage (the input transport must be set BEFORE -i;
        // set after -i it would only bind the output muxer).
        cmd.extend(
            ["-fflags", "nobuffer", "-flags", "low_delay"]
                .iter()
                .map(|s| s.to_string()),
        );
        if source.starts_with("rtsp://") || source.starts_with("rtsps://") {
            cmd.push("-rtsp_transport".into());
            cmd.push("tcp".into());
        }
        cmd.push("-i".into());
        cmd.push(source.to_string());
    } else {
        // V4L2 device — low-latency input flags then the selected format.
        //
        // `-thread_queue_size` is queue DEPTH in packets between the kernel's
        // V4L2 buffers and ffmpeg's demuxer thread, and it is NOT a latency
        // knob: a queue only adds latency while it is occupied, which is only
        // while the consumer is behind. Too shallow a queue makes a scheduler
        // hiccup DROP capture frames outright. Too deep a queue of RAW frames
        // is an OOM risk on a 1 GB board, so the depth is sized by format (see
        // `input_queue_depth`).
        let input_fmt = select_input_format(camera);
        let queue = input_queue_depth(input_fmt, params.width, params.height);
        cmd.extend(
            [
                "-fflags",
                "nobuffer",
                "-flags",
                "low_delay",
                "-probesize",
                "32",
                "-analyzeduration",
                "0",
                "-thread_queue_size",
            ]
            .iter()
            .map(|s| s.to_string()),
        );
        cmd.push(queue.to_string());
        cmd.push("-f".into());
        cmd.push("v4l2".into());
        if let Some(fmt) = input_fmt {
            cmd.push("-input_format".into());
            cmd.push(fmt.to_string());
        }
        cmd.push("-video_size".into());
        cmd.push(format!("{}x{}", params.width, params.height));
        cmd.push("-framerate".into());
        cmd.push(params.fps.to_string());
        cmd.push("-i".into());
        cmd.push(source.to_string());
    }

    // Passthrough: a network camera that already publishes H.264 is copied,
    // not decoded and re-encoded. `dump_extra` still puts SPS/PPS in-band
    // before every keyframe so a late joiner can bootstrap.
    if params.passthrough && crate::config::is_network_url(source) {
        cmd.extend(
            ["-c:v", "copy", "-bsf:v", "dump_extra=freq=keyframe"]
                .iter()
                .map(|s| s.to_string()),
        );
        cmd.extend(ffmpeg_output_stage(output, params.rtp_fanout));
        return cmd;
    }

    // Output framerate cap — drops frames to target fps BEFORE encoding so the
    // libx264 software path keeps up (USB cameras ignore the input -framerate
    // hint and deliver at native rate).
    cmd.push("-r".into());
    cmd.push(params.fps.to_string());
    // A frame-bus input arrives at its captured size; the encoded size is the
    // leg's current profile (an attention switch shrinks it).
    if let Some(fb) = framebus {
        if (fb.raw.width, fb.raw.height) != (params.width, params.height) {
            cmd.push("-s".into());
            cmd.push(format!("{}x{}", params.width, params.height));
        }
    }

    // Image orientation: when rotation/hflip/vflip are set, insert a `-vf`
    // transform before encode. On the HW paths (h264_v4l2m2m) this is a CPU
    // filter between capture and encode — a small latency/CPU cost, acceptable
    // because the OMX/V4L2 HW encoders on these boards do not rotate natively.
    if let Some(vf) = ffmpeg_vf(params.rotation, params.hflip, params.vflip) {
        cmd.push("-vf".into());
        cmd.push(vf);
    }

    cmd.push("-c:v".into());
    cmd.push(ffmpeg_codec.clone());
    cmd.push("-b:v".into());
    cmd.push(format!("{}k", params.bitrate_kbps));

    // Encoder-specific tuning.
    if ffmpeg_codec == "libx264" {
        // Pin the encoder to High profile / level 4.1 / 4:2:0 (avc1.640029)
        // which the browser MSE player hardcodes; force a 0.5s GOP and the
        // low-latency x264-params. intra-refresh is forbidden — it removes
        // true IDR NALs and the ingest parser cannot bootstrap SPS/PPS.
        let gop = gop_interval(params);
        cmd.extend(
            [
                "-pix_fmt",
                "yuv420p",
                "-profile:v",
                "high",
                "-level:v",
                "4.1",
                "-preset",
                "ultrafast",
                "-tune",
                "zerolatency",
            ]
            .iter()
            .map(|s| s.to_string()),
        );
        cmd.push("-g".into());
        cmd.push(gop.to_string());
        // Threading is a LATENCY decision on this path, not a throughput one.
        //
        // Frame-level threading (the x264 default, and what `-threads N` alone
        // buys) holds N frames in flight before the first one comes out: at
        // 30 fps and 4 threads that is ~133 ms of pure pipeline delay. SLICED
        // threading splits one frame across the cores and emits it whole, so
        // the delay is ~1/2 frame regardless of the thread count. `-tune
        // zerolatency` already asks x264 for sliced threads — and this builder
        // used to override it straight back off with `sliced-threads=0`, which
        // on Pi-5-class silicon (no encode block at all, so x264 IS the
        // encoder) was the single largest avoidable term left in the encode
        // hop. `slices=4` pins the split so it does not depend on x264's
        // thread heuristic.
        //
        // Gated on PROBED cores: below four, four encode threads oversubscribe
        // and the slice split costs more than it saves, so such a board keeps
        // the conservative two-thread frame-threaded form.
        let sliced = env.cpu_threads >= 4;
        cmd.extend(["-bf", "0", "-refs", "1"].iter().map(|s| s.to_string()));
        cmd.push("-threads".into());
        cmd.push(if sliced { "4" } else { "2" }.into());
        cmd.push("-flush_packets".into());
        cmd.push("1".into());
        cmd.push("-x264-params".into());
        cmd.push(
            if sliced {
                "no-mbtree=1:sync-lookahead=0:rc-lookahead=0:sliced-threads=1:slices=4:scenecut=0"
            } else {
                "no-mbtree=1:sync-lookahead=0:rc-lookahead=0:sliced-threads=0:scenecut=0"
            }
            .into(),
        );
        // Two bitstream filters, in order:
        //
        // * `h264_mp4toannexb` turns AVCC length-prefixed NALs into Annex-B
        //   start codes for RTSP / WebRTC;
        // * `dump_extra=freq=keyframe` re-inserts SPS/PPS in-band before every
        //   IDR. This is NOT redundant with x264's own header repetition: the
        //   RTSP muxer advertises `AVFMT_GLOBALHEADER`, which makes ffmpeg set
        //   `AV_CODEC_FLAG_GLOBAL_HEADER`, which makes libx264 emit the
        //   parameter sets ONLY into extradata (the SDP) and never in-band.
        //   Measured on ffmpeg 9.0.1 publishing this exact argv into an RTSP
        //   server: 0 in-band SPS/PPS without this filter, 8 SPS + 8 PPS (one
        //   pair per IDR, 4 s at a 15-frame GOP) with it, for 280 bytes over
        //   the same 4 s. Without it a browser that loses sync on the direct
        //   LAN feed has nothing to re-bootstrap the decoder from and freezes
        //   on the last decoded frame until the page is reloaded.
        //   The filter is idempotent — it skips a packet that already starts
        //   with the extradata — so the paths where x264 does repeat headers
        //   (a non-global-header muxer) do not double them.
        cmd.push("-bsf:v".into());
        cmd.push("h264_mp4toannexb,dump_extra=freq=keyframe".into());
    } else if ffmpeg_codec == "h264_v4l2m2m" {
        // Pi V4L2 M2M HW encoder (`bcm2835-codec` on Pi 4 / CM4): force
        // yuv420p, same short GOP, no B-frames, plus an EXPLICIT quantizer
        // bound.
        //
        // The quantizer bound is the rate control on this path, and it is not
        // optional. `bcm2835-codec` registers `V4L2_CID_MPEG_VIDEO_H264_MIN_QP`
        // with a driver default of 20 — a sane floor for its VBR controller,
        // which is also the driver's default bitrate mode. But ffmpeg's
        // `v4l2_m2m_enc.c` writes that control unconditionally, and its own
        // fallback for H.264 when `-qmin` is unset is **0**. So an argv without
        // `-qmin` does not "leave the driver alone": it actively replaces the
        // firmware's floor of 20 with 0 and licenses the encoder to spend
        // unbounded bits at QP 0 on a scene change. That surplus lands in the
        // wfb_tx FEC block, where it is not extra quality — it is queue depth,
        // and then loss. `-qmin` sized to the link budget is what stops it.
        //
        // Deliberately NOT emitted here: `-maxrate` / `-bufsize`. They set
        // `rc_max_rate` / `rc_buffer_size`, and `v4l2_m2m_enc.c` never reads
        // either — it plumbs only BITRATE, FRAME_RC_ENABLE, GOP_SIZE, B_FRAMES,
        // and MIN_QP/MAX_QP to the driver. Emitting them would look like a VBV
        // and enforce nothing. There is no VBV control exposed on this path;
        // the quantizer floor is the substitute, and this is why.
        let gop_hw = gop_interval(params);
        cmd.push("-pix_fmt".into());
        cmd.push("yuv420p".into());
        cmd.push("-g".into());
        cmd.push(gop_hw.to_string());
        cmd.extend(
            ["-bf", "0", "-flush_packets", "1"]
                .iter()
                .map(|s| s.to_string()),
        );
        cmd.push("-qmin".into());
        cmd.push(V4L2M2M_MIN_QP.to_string());
        cmd.push("-qmax".into());
        cmd.push("51".into());
        // Per-IDR SPS/PPS, for the same reason as the libx264 branch: the RTSP
        // muxer's global-header flag keeps the driver's parameter sets
        // out-of-band, so a mid-stream joiner or a browser that lost sync has
        // nothing in-band to re-bootstrap from. The M2M driver exposes no
        // "repeat headers" control, so the bitstream filter is the only lever
        // on this path. No `h264_mp4toannexb` here — the M2M encoder already
        // emits Annex-B, and applying it twice would corrupt the NAL
        // boundaries.
        cmd.push("-bsf:v".into());
        cmd.push("dump_extra=freq=keyframe".into());
    }

    cmd.extend(ffmpeg_output_stage(output, params.rtp_fanout));
    cmd
}
