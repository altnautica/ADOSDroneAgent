//! `rpicam-vid` argv builder plus shared shell-quoting and input-format helpers.

use super::*;

/// `rpicam-vid` command for CSI camera encoding.
///
/// For RTSP output the raw H.264 elementary stream is piped into ffmpeg with
/// explicit `-rtsp_transport tcp -c copy` (rpicam's embedded RTSP muxer fails
/// to negotiate with mediamtx). The `h264_metadata` bsf stamps BT.709 colour
/// primaries / transfer / matrix into the SPS VUI so browsers render natural
/// colour instead of a magenta cast. For non-RTSP sinks the direct rpicam
/// output is kept.
pub(super) fn build_rpicam_command(
    params: &EncoderParams,
    source: &str,
    output: &str,
    env: &EncoderEnv,
) -> Vec<String> {
    // The GOP contract, not a hardcoded frame count. `gop_interval` yields
    // frames (default = fps/2, a 0.5s GOP), and `--intra` takes a frame count,
    // so it is passed straight through. The previous hardcoded `--intra 30`
    // was a 1.0s GOP at 30fps — double the contracted worst-case radio-FEC
    // recovery time, on the flagship drone camera, regardless of what
    // `keyframe_interval` was configured to.
    let gop = gop_interval(params);
    let mut rpicam_args: Vec<String> = vec![
        "rpicam-vid".into(),
        "--width".into(),
        params.width.to_string(),
        "--height".into(),
        params.height.to_string(),
        "--framerate".into(),
        params.fps.to_string(),
        "--bitrate".into(),
        (params.bitrate_kbps * 1000).to_string(),
        "--codec".into(),
        params.codec.clone(),
        "--timeout".into(),
        "0".into(),
        "--nopreview".into(),
        // --inline embeds SPS/PPS before every IDR so a downstream parser can
        // recover mid-stream without restarting the pipeline.
        "--inline".into(),
        // High 4.1 — `avc1.640029`, which is exactly what the browser MSE
        // player is pinned to. This used to emit `--profile baseline --level 4`
        // (Constrained Baseline), so a CSI drone published a stream the player
        // could not initialise: a silent, permanent black screen on the receive
        // side with no error anywhere. The ffmpeg libx264 path already encodes
        // High 4.1; all three encode paths now agree with the player.
        // `rpicam-vid --profile` accepts baseline|main|high and `--level`
        // accepts 4|4.1|4.2.
        "--profile".into(),
        "high".into(),
        "--level".into(),
        "4.1".into(),
        "--intra".into(),
        gop.to_string(),
    ];

    if env.pi5_class {
        // Pi-5-class silicon (BCM2712: Pi 5, CM5) has NO hardware H.264
        // encoder — VideoCore VII dropped the encode block VideoCore VI
        // (Pi 4 / CM4) had — so `rpicam-vid` runs libavcodec/x264 in software
        // here. The software encoder buffers a constant 8-frame pipeline
        // (~267ms at 30fps), which on its own exceeds this system's entire
        // glass-to-glass budget. `--low-latency` drops that to ~1 frame by
        // giving up B-frames and CABAC. It is only accepted by
        // rpicam-apps >= 1.6.0, and it is a no-op on hardware-encoder boards,
        // which is why it is gated on the board family rather than always
        // emitted.
        rpicam_args.push("--low-latency".into());
    }

    if !source.is_empty() && source != "-" {
        // rpicam-vid expects a camera index (0, 1, ...) not a device path.
        let cam_idx = source
            .strip_prefix("/dev/video")
            .unwrap_or(source)
            .to_string();
        rpicam_args.push("--camera".into());
        rpicam_args.push(cam_idx);
    }

    if output.starts_with("rtsp://") {
        rpicam_args.push("-o".into());
        rpicam_args.push("-".into());
        let ffmpeg_args: Vec<String> = vec![
            "ffmpeg".into(),
            "-loglevel".into(),
            "error".into(),
            "-fflags".into(),
            "nobuffer".into(),
            "-flags".into(),
            "low_delay".into(),
            "-f".into(),
            "h264".into(),
            "-i".into(),
            "-".into(),
            "-c".into(),
            "copy".into(),
            "-bsf:v".into(),
            "h264_metadata=colour_primaries=1:transfer_characteristics=1:\
             matrix_coefficients=1:video_full_range_flag=0"
                .into(),
            // Strip the muxer's mux delay + preload + packet aggregation so
            // this path does not quietly reintroduce ~1.2s of latency.
            "-muxdelay".into(),
            "0".into(),
            "-muxpreload".into(),
            "0".into(),
            "-flush_packets".into(),
            "1".into(),
            "-f".into(),
            "rtsp".into(),
            "-rtsp_transport".into(),
            "tcp".into(),
            output.to_string(),
        ];
        let rpicam_str = join_shell(&rpicam_args);
        let ffmpeg_str = join_shell(&ffmpeg_args);
        return vec![
            "bash".into(),
            "-c".into(),
            format!("{rpicam_str} | {ffmpeg_str}"),
        ];
    }

    rpicam_args.push("-o".into());
    rpicam_args.push(output.to_string());
    rpicam_args
}

/// Join an argv into a single shell command string, quoting each token the
/// same way the predecessor's `_shell_quote` does.
pub(super) fn join_shell(args: &[String]) -> String {
    args.iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Minimal POSIX single-quote escape for arguments inside `bash -c`. Quotes
/// when the argument is empty or contains any shell-significant character;
/// otherwise returns it verbatim.
pub(super) fn shell_quote(arg: &str) -> String {
    const SPECIAL: &[char] = &[
        ' ', '\'', '"', '$', '&', ';', '|', '<', '>', '(', ')', '*', '?', '{', '}', '\\', '`',
        '\n', '\t',
    ];
    if arg.is_empty() || arg.chars().any(|c| SPECIAL.contains(&c)) {
        format!("'{}'", arg.replace('\'', "'\\''"))
    } else {
        arg.to_string()
    }
}

/// Choose the V4L2 input format from camera capabilities.
///
/// Priority: mjpeg (compressed, high fps) > yuyv (raw). Returns `None` when
/// capabilities are unknown (let ffmpeg auto-detect).
pub(super) fn select_input_format(camera: Option<&CameraInfo>) -> Option<&'static str> {
    let camera = camera?;
    let caps: Vec<String> = camera
        .capabilities
        .iter()
        .map(|c| c.to_lowercase())
        .collect();
    if caps.iter().any(|c| c == "mjpeg" || c == "mjpg") {
        Some("mjpeg")
    } else if caps.iter().any(|c| c == "yuyv" || c == "rawvideo") {
        Some("yuyv")
    } else {
        None
    }
}

/// Capture-queue depth (`-thread_queue_size`) for a V4L2 input.
///
/// Compressed MJPEG frames are tens of KB, so 128 packets (a >4 s stall at
/// 30 fps) is a small transient. A raw frame is `w*h*2` bytes (YUYV), so the
/// raw depth is capped to about 32 MiB of queued frames, never below 8 and
/// never above 128. An unknown format is sized as raw: the safe assumption.
pub fn input_queue_depth(input_format: Option<&str>, width: u32, height: u32) -> u32 {
    const MJPEG_DEPTH: u32 = 128;
    const RAW_BUDGET_BYTES: u64 = 32 * 1024 * 1024;
    if input_format == Some("mjpeg") {
        return MJPEG_DEPTH;
    }
    let frame = u64::from(width) * u64::from(height) * 2;
    let depth = RAW_BUDGET_BYTES.checked_div(frame).unwrap_or(128);
    depth.clamp(8, 128) as u32
}
