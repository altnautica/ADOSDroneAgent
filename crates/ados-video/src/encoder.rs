//! Encoder command builder: composes the exact `rpicam-vid` / `ffmpeg` /
//! `gst-launch-1.0` argv vectors that drive H.264 capture-and-publish.
//!
//! This is a pure, I/O-free arg-vector builder — it never spawns anything
//! (that is [`crate::process`]'s job). It is the parity-critical surface of the
//! video pipeline: a single wrong ffmpeg/rpicam flag silently breaks browser
//! WHEP, colour rendering, or latency, so every flag here is byte-for-byte
//! identical to its predecessor and is held in place by the fixture-driven
//! tests at the bottom of this file.
//!
//! ## Encoder selection
//! - **CSI** camera → `rpicam-vid` (Pi VideoCore HW encoder), falling back to
//!   ffmpeg when rpicam is absent.
//! - **USB / IP** camera → ffmpeg `libx264` (software). On Rockchip SoCs the
//!   `h264_v4l2m2m` ffmpeg plugin is present but hangs when probed, and
//!   GStreamer's `mpph264enc` VPU path emits corrupt frames that stall the
//!   browser decoder — so Rockchip is short-circuited to `libx264` and the HW
//!   encoder probe is skipped entirely. On non-Rockchip boards the HW
//!   `h264_v4l2m2m` path is used when ffmpeg advertises it.
//!
//! ## Hardware detection as an input
//! All runtime probes (the Rockchip `/proc/device-tree/compatible` read, the
//! ffmpeg `-encoders` probe, the `gst-inspect-1.0` probes) are gathered into
//! [`EncoderEnv`] so the builder itself is pure and testable without touching
//! `/proc` or any subprocess. [`EncoderEnv::detect`] does the real probing on
//! Linux; the builder takes the resolved env.

use std::path::Path;

use crate::config::CameraConfig;

/// Which encoder backend a command targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderKind {
    /// `rpicam-vid` — the Pi VideoCore HW encoder (CSI cameras).
    RpicamVid,
    /// `ffmpeg` — software `libx264` or HW `h264_v4l2m2m` (USB / IP cameras).
    Ffmpeg,
    /// `gst-launch-1.0` — Rockchip `mpph264enc` VPU or `x264enc` software.
    Gstreamer,
}

/// A camera as seen by the builder. Mirrors the fields of the Python
/// `hal.camera.CameraInfo` that the encoder reads: type, device path,
/// geometry, and the capability list that drives input-format selection.
#[derive(Debug, Clone)]
pub struct CameraInfo {
    pub camera_type: CameraType,
    pub device_path: String,
    pub capabilities: Vec<String>,
}

/// Camera bus class — selects the encoder backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CameraType {
    /// MIPI-CSI ribbon camera (rpicam path).
    Csi,
    /// USB UVC camera (ffmpeg / gstreamer v4l2 path).
    Usb,
    /// Network / RTSP camera (ffmpeg network-input path).
    Ip,
}

/// The resolved runtime environment the builder needs. Gathering these probes
/// up front keeps [`build_encoder_command`] pure and unit-testable.
#[derive(Debug, Clone)]
pub struct EncoderEnv {
    /// `/proc/device-tree/compatible` contains "rockchip". On Rockchip the HW
    /// H.264 probe is short-circuited to `None` (forces `libx264`).
    pub is_rockchip: bool,
    /// The ffmpeg HW H.264 encoder name (e.g. `h264_v4l2m2m`) when ffmpeg
    /// advertises one *and* the board is not Rockchip; otherwise `None`.
    pub hw_h264: Option<String>,
    /// GStreamer `mpph264enc` (Rockchip VPU) is installed.
    pub has_mpph264enc: bool,
    /// GStreamer `rtspclientsink` element is installed (direct RTSP RECORD;
    /// otherwise the gstreamer RTSP path pipes through ffmpeg).
    pub has_rtspclientsink: bool,
    /// Absolute path to the Python interpreter used to splice the SEI
    /// injector (`<python> -m ados.services.video.sei_injector`). Equivalent
    /// to Python's `sys.executable`.
    pub python_executable: String,
}

impl EncoderEnv {
    /// Probe the real environment. On non-Linux hosts the probes are no-ops
    /// so the builder is exercisable on the dev host; the rig path is Linux.
    #[cfg(target_os = "linux")]
    pub fn detect() -> Self {
        let is_rockchip = std::fs::read("/proc/device-tree/compatible")
            .map(|b| b.windows(8).any(|w| w == b"rockchip"))
            .unwrap_or(false);

        // ffmpeg's h264_v4l2m2m plugin is listed on Rockchip but hangs when
        // probed, so the probe is skipped entirely on Rockchip (matches the
        // Python short-circuit before the ffmpeg -encoders call).
        let hw_h264 = if is_rockchip {
            None
        } else {
            detect_hw_h264_encoder()
        };

        Self {
            is_rockchip,
            hw_h264,
            has_mpph264enc: gst_element_present("mpph264enc"),
            has_rtspclientsink: gst_element_present("rtspclientsink"),
            python_executable: current_python_executable(),
        }
    }

    /// Non-Linux fallback: software libx264 everywhere, no GStreamer HW.
    #[cfg(not(target_os = "linux"))]
    pub fn detect() -> Self {
        Self {
            is_rockchip: false,
            hw_h264: None,
            has_mpph264enc: false,
            has_rtspclientsink: false,
            python_executable: current_python_executable(),
        }
    }
}

#[cfg(target_os = "linux")]
fn detect_hw_h264_encoder() -> Option<String> {
    let output = std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-encoders"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    // Hardware encoders in order of preference (same list as the Python probe).
    for enc in ["h264_v4l2m2m", "h264_nvenc", "h264_vaapi", "h264_omx"] {
        if text.contains(enc) {
            return Some(enc.to_string());
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn gst_element_present(element: &str) -> bool {
    std::process::Command::new("gst-inspect-1.0")
        .arg(element)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Best-effort resolution of the running interpreter for the SEI splice.
/// Falls back to the installed venv interpreter path the agent ships with.
fn current_python_executable() -> String {
    std::env::var("ADOS_PYTHON")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/opt/ados/venv/bin/python3".to_string())
}

/// Encoder invocation parameters. Mirrors the Python `EncoderConfig`.
#[derive(Debug, Clone)]
pub struct EncoderParams {
    pub kind: EncoderKind,
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
}

impl EncoderParams {
    /// Build params from the resolved encoder kind and the camera config
    /// block (the same fields the Python pipeline copies into EncoderConfig).
    pub fn from_camera_config(kind: EncoderKind, cfg: &CameraConfig) -> Self {
        Self {
            kind,
            codec: cfg.codec.clone(),
            width: cfg.width,
            height: cfg.height,
            fps: cfg.fps,
            bitrate_kbps: cfg.bitrate_kbps,
        }
    }
}

/// Allowlist for camera source / output strings: alphanumeric, slashes, dots,
/// hyphens, underscores, colons. `-` (stdin/stdout) is allowed verbatim.
fn validate_source(source: &str) -> Result<&str, EncoderError> {
    if source == "-" {
        return Ok(source);
    }
    let ok = !source.is_empty()
        && source
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '.' | '-' | ':'));
    if ok {
        Ok(source)
    } else {
        Err(EncoderError::InvalidSource(source.to_string()))
    }
}

/// Error from the encoder command builder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncoderError {
    /// A source / output string contained a disallowed character.
    InvalidSource(String),
}

impl std::fmt::Display for EncoderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EncoderError::InvalidSource(s) => write!(
                f,
                "Invalid source path: {s:?}. Only alphanumeric, slashes, dots, \
                 hyphens, underscores, and colons are allowed."
            ),
        }
    }
}

impl std::error::Error for EncoderError {}

/// Build the full argv vector for the given encoder configuration.
///
/// Returns the program plus its arguments. For the bash-pipeline cases (rpicam
/// → RTSP, and the SEI-wrapped variants) the returned vector is
/// `["bash", "-c", "<pipeline>"]`, exactly as the predecessor composes it.
///
/// Mirrors `build_encoder_command(config, source, output, camera)`.
pub fn build_encoder_command(
    params: &EncoderParams,
    source: &str,
    output: &str,
    camera: Option<&CameraInfo>,
    env: &EncoderEnv,
) -> Result<Vec<String>, EncoderError> {
    let source = validate_source(source)?;
    let output = validate_source(output)?;
    let cmd = match params.kind {
        EncoderKind::RpicamVid => build_rpicam_command(params, source, output),
        EncoderKind::Ffmpeg => build_ffmpeg_command(params, source, output, camera, env),
        EncoderKind::Gstreamer => build_gstreamer_command(params, source, output, camera, env),
    };
    Ok(cmd)
}

/// `rpicam-vid` command for CSI camera encoding.
///
/// For RTSP output the raw H.264 elementary stream is piped into ffmpeg with
/// explicit `-rtsp_transport tcp -c copy` (rpicam's embedded RTSP muxer fails
/// to negotiate with mediamtx). The `h264_metadata` bsf stamps BT.709 colour
/// primaries / transfer / matrix into the SPS VUI so browsers render natural
/// colour instead of a magenta cast. For non-RTSP sinks the direct rpicam
/// output is kept.
fn build_rpicam_command(params: &EncoderParams, source: &str, output: &str) -> Vec<String> {
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
        // Constrained Baseline profile is the safe least-common-denominator
        // across all WebRTC stacks (Chromium / Safari / Firefox / WebView).
        "--profile".into(),
        "baseline".into(),
        "--level".into(),
        "4".into(),
        // Tighter intra interval so a dropped frame recovers within ~1s.
        "--intra".into(),
        "30".into(),
    ];

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
fn join_shell(args: &[String]) -> String {
    args.iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Minimal POSIX single-quote escape for arguments inside `bash -c`. Quotes
/// when the argument is empty or contains any shell-significant character;
/// otherwise returns it verbatim. Mirrors `_shell_quote`.
fn shell_quote(arg: &str) -> String {
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
/// capabilities are unknown (let ffmpeg auto-detect). Mirrors
/// `_select_input_format`.
fn select_input_format(camera: Option<&CameraInfo>) -> Option<&'static str> {
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

/// `ffmpeg` command for USB / IP camera encoding.
///
/// Network sources skip the v4l2 wrapper. V4L2 sources prepend the
/// low-latency input flags and the capability-selected `-input_format`. The
/// output tuning differs by codec: `libx264` gets the full low-latency
/// browser-compat block + x264-params; `h264_v4l2m2m` gets a leaner HW block.
fn build_ffmpeg_command(
    params: &EncoderParams,
    source: &str,
    output: &str,
    camera: Option<&CameraInfo>,
    env: &EncoderEnv,
) -> Vec<String> {
    // Try the HW H.264 encoder first (already short-circuited to None on
    // Rockchip inside EncoderEnv); otherwise map the codec to a sw encoder.
    let hw_encoder = if matches!(params.codec.as_str(), "h264" | "H264") {
        env.hw_h264.clone()
    } else {
        None
    };

    let ffmpeg_codec: String = if let Some(hw) = hw_encoder {
        hw
    } else {
        match params.codec.as_str() {
            "h264" => "libx264",
            "h265" | "hevc" => "libx265",
            "mjpeg" => "mjpeg",
            _ => "libx264",
        }
        .to_string()
    };

    let mut cmd: Vec<String> = vec!["ffmpeg".into(), "-y".into()];

    if source.starts_with("rtsp://") || source.starts_with("http://") {
        // Network / IP camera source.
        cmd.push("-i".into());
        cmd.push(source.to_string());
    } else {
        // V4L2 device — low-latency input flags then the selected format.
        let input_fmt = select_input_format(camera);
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
                "4",
                "-f",
                "v4l2",
            ]
            .iter()
            .map(|s| s.to_string()),
        );
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

    // Output framerate cap — drops frames to target fps BEFORE encoding so the
    // libx264 software path keeps up (USB cameras ignore the input -framerate
    // hint and deliver at native rate).
    cmd.push("-r".into());
    cmd.push(params.fps.to_string());

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
        let gop = (params.fps / 2).max(1);
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
        cmd.extend(
            [
                "-bf",
                "0",
                "-refs",
                "1",
                "-threads",
                "2",
                "-flush_packets",
                "1",
            ]
            .iter()
            .map(|s| s.to_string()),
        );
        cmd.push("-x264-params".into());
        cmd.push("no-mbtree=1:sync-lookahead=0:rc-lookahead=0:sliced-threads=0:scenecut=0".into());
        // AVCC length-prefixed NALs → Annex-B start codes for RTSP / WebRTC.
        cmd.push("-bsf:v".into());
        cmd.push("h264_mp4toannexb".into());
    } else if ffmpeg_codec == "h264_v4l2m2m" {
        // Pi V4L2 M2M HW encoder: force yuv420p, same 0.5s GOP, no B-frames.
        let gop_hw = (params.fps / 2).max(1);
        cmd.push("-pix_fmt".into());
        cmd.push("yuv420p".into());
        cmd.push("-g".into());
        cmd.push(gop_hw.to_string());
        cmd.extend(
            ["-bf", "0", "-flush_packets", "1"]
                .iter()
                .map(|s| s.to_string()),
        );
    }

    // Output muxer.
    if output.starts_with("rtsp://") {
        // TCP RTSP avoids UDP fragmentation of large keyframe NALs;
        // -max_delay 0 flushes encoded frames to the muxer immediately.
        cmd.extend(
            ["-max_delay", "0", "-rtsp_transport", "tcp", "-f", "rtsp"]
                .iter()
                .map(|s| s.to_string()),
        );
    } else if output.starts_with("udp://") || output.starts_with("tcp://") {
        cmd.push("-f".into());
        cmd.push("mpegts".into());
    }

    cmd.push(output.to_string());
    cmd
}

/// GStreamer pipeline command.
///
/// On Rockchip with `mpph264enc` present: hardware VPU encode. Otherwise
/// software `x264enc`. RTSP output uses `rtspclientsink` (RTSP RECORD) when the
/// element is available, else pipes the elementary stream through ffmpeg for
/// RTSP muxing. File output uses a direct `filesink` pipeline.
fn build_gstreamer_command(
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

    let gop = (params.fps / 2).max(1);
    let encoder = if env.is_rockchip && env.has_mpph264enc {
        // mpph264enc HW VPU: bps = bits/sec, VBR (rc-mode=1) with bounded
        // bps-max/bps-min so a scene change cannot starve the wfb_tx FEC,
        // header-mode=1 inserts SPS/PPS before every IDR for late joiners.
        let bps = params.bitrate_kbps * 1000;
        let bps_max = (params.bitrate_kbps as f64 * 1.5) as u32 * 1000;
        let bps_min = (params.bitrate_kbps as f64 * 0.5) as u32 * 1000;
        format!(
            "mpph264enc bps={bps} bps-max={bps_max} bps-min={bps_min} \
             qp-min=5 qp-max=51 rc-mode=1 gop={gop} header-mode=1"
        )
    } else {
        // x264enc software fallback bounded to ~2 frames of pipeline latency.
        format!(
            "x264enc bitrate={} speed-preset=ultrafast tune=zerolatency \
             threads=2 sliced-threads=false key-int-max={gop}",
            params.bitrate_kbps
        )
    };

    if output.starts_with("rtsp://") {
        let safe_output = gst_quote(output);
        if env.has_rtspclientsink {
            // Direct GStreamer → mediamtx via rtspclientsink (RTSP RECORD).
            let pipeline = format!(
                "v4l2src device={safe_source} ! {src_caps} ! \
                 {decode} ! {encoder} ! h264parse ! \
                 rtspclientsink location={safe_output} protocols=tcp latency=0"
            );
            let mut out: Vec<String> = vec!["gst-launch-1.0".into(), "-e".into()];
            out.extend(pipeline.split(' ').map(|s| s.to_string()));
            return out;
        }
        // Fallback: pipe GStreamer H.264 → ffmpeg for RTSP muxing.
        let gst_cmd = format!(
            "gst-launch-1.0 -q v4l2src device={safe_source} ! {src_caps} ! \
             {decode} ! {encoder} ! h264parse ! \
             'video/x-h264,stream-format=byte-stream' ! fdsink fd=1"
        );
        let ffmpeg_cmd = format!(
            "ffmpeg -y -fflags nobuffer -f h264 -i pipe:0 \
             -c:v copy \
             -max_delay 0 -rtsp_transport tcp -f rtsp {safe_output}"
        );
        return vec![
            "bash".into(),
            "-c".into(),
            format!("{gst_cmd} 2>/dev/null | {ffmpeg_cmd}"),
        ];
    }

    // File / other output: direct GStreamer pipeline.
    let safe_output = gst_quote(output);
    let pipeline = format!(
        "v4l2src device={safe_source} ! {src_caps} ! \
         {decode} ! {encoder} ! h264parse ! \
         filesink location={safe_output}"
    );
    let mut out: Vec<String> = vec!["gst-launch-1.0".into(), "-e".into()];
    out.extend(pipeline.split(' ').map(|s| s.to_string()));
    out
}

/// `shlex.quote` equivalent: returns the string verbatim when it is non-empty
/// and contains only "safe" characters, otherwise wraps it in single quotes
/// with embedded single-quotes escaped. Used for the GStreamer pipeline tokens
/// (device path / output location) the way the predecessor uses `shlex.quote`.
fn gst_quote(s: &str) -> String {
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

/// Splice the SEI injector into the encoder→publish pipeline.
///
/// The injector writes a wall-clock SEI NAL in front of every VCL slice so any
/// downstream consumer sees the same timestamp on the same frame (this is what
/// makes browser glass-to-glass measurement possible). Three cases:
/// - bash pipeline (rpicam path): splice `| <python> -m injector |` before the
///   final ffmpeg stage;
/// - raw ffmpeg → RTSP/UDP/TCP: split into encode-to-stdout + injector +
///   publish-stdin, rebuilding the publisher for the original URI;
/// - GStreamer / unknown: returned unchanged (the wfb-tee injector stays the
///   sole SEI source).
///
/// Mirrors `wrap_with_sei_inject`.
pub fn wrap_with_sei_inject(cmd: &[String], output_uri: &str, env: &EncoderEnv) -> Vec<String> {
    let inject_cmd = format!(
        "{} -m ados.services.video.sei_injector",
        shell_quote(&env.python_executable)
    );

    // Case 1: rpicam path is already a bash pipeline.
    if cmd.len() >= 3 && cmd[0] == "bash" && cmd[1] == "-c" {
        let bash_body = &cmd[2];
        // The pipeline ends `... | ffmpeg ... <output>`. Splice the injector
        // before the final stage (rsplit on '|' with maxsplit=1).
        match bash_body.rsplit_once('|') {
            Some((head, tail)) => {
                let head = head.trim_end();
                let tail = tail.trim_start();
                vec![
                    "bash".into(),
                    "-c".into(),
                    format!("{head} | {inject_cmd} | {tail}"),
                ]
            }
            // No pipe stage to splice — leave unchanged.
            None => cmd.to_vec(),
        }
    }
    // Case 2: raw ffmpeg cmd publishing to RTSP/UDP/TCP. Split into two stages.
    else if cmd.first().map(String::as_str) == Some("ffmpeg") {
        let mut encoded: Vec<String> = cmd.to_vec();
        // Strip the output URI (must be the last token).
        if encoded.last().map(String::as_str) == Some(output_uri) {
            encoded.pop();
        }
        // Strip the muxer format specifier (`-f rtsp`, `-f mpegts`, ...).
        strip_flag_with_value(&mut encoded, "-f");
        // Strip RTSP transport hint if present.
        strip_flag_with_value(&mut encoded, "-rtsp_transport");
        // Strip max_delay if present (was paired with rtsp output).
        strip_flag_with_value(&mut encoded, "-max_delay");
        // Encode-only ffmpeg now emits raw Annex-B H.264 on stdout.
        encoded.push("-f".into());
        encoded.push("h264".into());
        encoded.push("-".into());

        // Publish-only ffmpeg pulls Annex-B from stdin and re-mounts the URI.
        let publish: Vec<String> = if output_uri.starts_with("rtsp://") {
            vec![
                "ffmpeg",
                "-loglevel",
                "error",
                "-fflags",
                "nobuffer",
                "-flags",
                "low_delay",
                "-f",
                "h264",
                "-i",
                "-",
                "-c",
                "copy",
                "-muxdelay",
                "0",
                "-muxpreload",
                "0",
                "-flush_packets",
                "1",
                "-rtsp_transport",
                "tcp",
                "-f",
                "rtsp",
            ]
            .iter()
            .map(|s| s.to_string())
            .chain(std::iter::once(output_uri.to_string()))
            .collect()
        } else if output_uri.starts_with("udp://") || output_uri.starts_with("tcp://") {
            vec![
                "ffmpeg",
                "-loglevel",
                "error",
                "-fflags",
                "nobuffer",
                "-flags",
                "low_delay",
                "-f",
                "h264",
                "-i",
                "-",
                "-c",
                "copy",
                "-muxdelay",
                "0",
                "-muxpreload",
                "0",
                "-flush_packets",
                "1",
                "-f",
                "mpegts",
            ]
            .iter()
            .map(|s| s.to_string())
            .chain(std::iter::once(output_uri.to_string()))
            .collect()
        } else {
            // Unknown output URI — cannot rebuild the publisher; leave unchanged.
            return cmd.to_vec();
        };

        let encode_str = join_shell(&encoded);
        let publish_str = join_shell(&publish);
        vec![
            "bash".into(),
            "-c".into(),
            format!("{encode_str} | {inject_cmd} | {publish_str}"),
        ]
    }
    // Case 3: gstreamer or unknown — skip unchanged (legacy wfb-tee SEI).
    else {
        cmd.to_vec()
    }
}

/// Remove the last occurrence of `flag` and its following value from `args`,
/// scanning right-to-left. Mirrors the Python reverse-scan + double-pop.
fn strip_flag_with_value(args: &mut Vec<String>, flag: &str) {
    if args.len() < 2 {
        return;
    }
    // Match the Python loop: i runs from len-1 down to 1; act when args[i]==flag
    // and a value follows (i+1 < len). Take the highest such i (right-most).
    for i in (1..args.len()).rev() {
        if args[i] == flag && i + 1 < args.len() {
            args.remove(i + 1);
            args.remove(i);
            return;
        }
    }
}

/// Pick the encoder backend for a camera, given which binaries are present.
///
/// CSI → rpicam-vid (fallback ffmpeg). USB/IP → ffmpeg (the Rockchip
/// `mpph264enc` VPU path is disabled because it emits corrupt frames; fallback
/// gstreamer). Mirrors `detect_encoder_for_camera`. The binary-presence flags
/// are taken as inputs to keep this pure.
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
        CameraType::Usb | CameraType::Ip => {
            // mpph264enc (Rockchip VPU) is disabled — fall back to ffmpeg
            // libx264, then gstreamer x264enc.
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    /// Fixtures captured verbatim from the Python `build_encoder_command` +
    /// `wrap_with_sei_inject` by `tests/capture_encoder_fixtures.py`. See the
    /// module test for how each case maps onto the Rust builder.
    const FIXTURES: &str = include_str!("../tests/encoder_fixtures.json");

    /// The pinned interpreter path the capture script used for `sys.executable`.
    const PY_EXE: &str = "/opt/ados/venv/bin/python3";

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

    fn rockchip() -> EncoderEnv {
        EncoderEnv {
            is_rockchip: true,
            hw_h264: None,
            has_mpph264enc: false,
            has_rtspclientsink: true,
            python_executable: PY_EXE.into(),
        }
    }
    fn non_rk_sw() -> EncoderEnv {
        EncoderEnv {
            is_rockchip: false,
            hw_h264: None,
            has_mpph264enc: false,
            has_rtspclientsink: true,
            python_executable: PY_EXE.into(),
        }
    }
    fn non_rk_hw() -> EncoderEnv {
        EncoderEnv {
            is_rockchip: false,
            hw_h264: Some("h264_v4l2m2m".into()),
            has_mpph264enc: false,
            has_rtspclientsink: true,
            python_executable: PY_EXE.into(),
        }
    }
    fn rk_mpp() -> EncoderEnv {
        EncoderEnv {
            is_rockchip: true,
            hw_h264: None,
            has_mpph264enc: true,
            has_rtspclientsink: true,
            python_executable: PY_EXE.into(),
        }
    }
    fn rk_mpp_noclient() -> EncoderEnv {
        EncoderEnv {
            has_rtspclientsink: false,
            ..rk_mpp()
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
        }
    }

    /// Build (and optionally SEI-wrap) the way the capture script does.
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
            wrap_with_sei_inject(&cmd, out, env)
        } else {
            cmd
        }
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
        let got =
            build_encoder_command(&p, "/dev/video1", RTSP_OUT, Some(&usb_mjpeg()), &rockchip())
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
}
