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
//! - **USB / IP** camera → ffmpeg. The H.264 backend is chosen by *probing for a
//!   real V4L2 hardware encoder device*, not by trusting ffmpeg's `-encoders`
//!   listing. A board can list the `h264_v4l2m2m` wrapper while shipping no
//!   backing encoder device; ffmpeg then exits at init and the camera streams
//!   zero bytes. The HAL probe opens the real V4L2 nodes, confirms one
//!   enumerates an H.264 output FourCC, and trial-inits it; only a device that
//!   passes selects `h264_v4l2m2m`. Otherwise the builder uses software
//!   `libx264`.
//!
//! ## Hardware detection as an input
//! The H.264 encoder decision is a probed [`Probed<EncoderDevice>`] carried on
//! [`EncoderEnv`]; the GStreamer-element probes are plain booleans. Gathering
//! them up front keeps the builder itself pure and testable without touching any
//! device or subprocess. [`EncoderEnv::detect`] does the real probing on Linux
//! (the HW encoder via [`ados_hal_probe`]); the builder takes the resolved env.

use std::path::Path;

use ados_protocol::hwcaps::{EncoderDevice, Probed};

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
    /// The probed hardware H.264 encoder device. `Present` only when a real
    /// V4L2 node enumerates an H.264 output FourCC AND accepted a bounded
    /// trial-init; the builder then selects `h264_v4l2m2m`. `Absent`
    /// (or `NotProbed`) selects software `libx264`. This is a *probe*, not a
    /// trust of ffmpeg's `-encoders` listing.
    pub hw_h264: Probed<EncoderDevice>,
    /// GStreamer `mpph264enc` (Rockchip VPU) is installed.
    pub has_mpph264enc: bool,
    /// GStreamer `omxh264videoenc` (Allwinner Cedar OMX HW encoder) is installed.
    /// Its presence is the honest Allwinner vendor gate: the element only ships
    /// on a board with the Cedar video engine, so probing it IS how we learn the
    /// board's HAL `encoder_api == "vendor"` from Rust (there is no Rust-side
    /// board-YAML reader; the element is the ground truth).
    pub has_omxh264videoenc: bool,
    /// GStreamer `rtspclientsink` element is installed (direct RTSP RECORD;
    /// otherwise the gstreamer RTSP path pipes through ffmpeg).
    pub has_rtspclientsink: bool,
    /// The board's HAL `video.encoder_api` capability: "vendor" (Allwinner
    /// OMX/Cedar), "rkmpp"/"mpp" (Rockchip VPU), "v4l2", "rkmedia", "none",
    /// "unknown". Derived at probe time from which HW GStreamer element is
    /// present (see [`EncoderEnv::detect`]); consumed to pick the OMX branch.
    pub encoder_api: String,
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
        // Probe for a REAL hardware H.264 encoder device rather than trusting
        // the ffmpeg `-encoders` listing. Runs at the pre-arm boot phase: the
        // trial-init opens and configures a device, so it is never run during
        // armed runtime (the probe defers to its cached result there).
        let hw_h264 = ados_hal_probe::probe::video::probe_h264_encoder(
            ados_protocol::hwcaps::ProbePhase::BootPreArm,
        );

        let has_mpph264enc = gst_element_present("mpph264enc");
        let has_omxh264videoenc = gst_element_present("omxh264videoenc");
        let encoder_api = if has_omxh264videoenc {
            "vendor".to_string()
        } else if has_mpph264enc {
            "rkmpp".to_string()
        } else {
            "unknown".to_string()
        };
        Self {
            hw_h264,
            has_mpph264enc,
            has_omxh264videoenc,
            has_rtspclientsink: gst_element_present("rtspclientsink"),
            encoder_api,
            python_executable: current_python_executable(),
        }
    }

    /// Non-Linux fallback: software libx264 everywhere, no GStreamer HW.
    #[cfg(not(target_os = "linux"))]
    pub fn detect() -> Self {
        Self {
            hw_h264: Probed::NotProbed,
            has_mpph264enc: false,
            has_omxh264videoenc: false,
            has_rtspclientsink: false,
            encoder_api: "unknown".to_string(),
            python_executable: current_python_executable(),
        }
    }
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

/// Encoder invocation parameters.
#[derive(Debug, Clone)]
pub struct EncoderParams {
    pub kind: EncoderKind,
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    /// Encoder override: "auto" (probe) | "omx" | "v4l2m2m" | "software".
    pub encoder: String,
    /// Clockwise image rotation in degrees (0 | 90 | 180 | 270).
    pub rotation: u32,
    pub hflip: bool,
    pub vflip: bool,
    /// Keyframe (GOP) interval in frames; 0 ⇒ encoder picks a short low-latency
    /// GOP (0.5 s at the configured fps).
    pub keyframe_interval: u32,
}

impl EncoderParams {
    /// Build params from the resolved encoder kind and the camera config
    /// block.
    pub fn from_camera_config(kind: EncoderKind, cfg: &CameraConfig) -> Self {
        Self {
            kind,
            codec: cfg.codec.clone(),
            width: cfg.width,
            height: cfg.height,
            fps: cfg.fps,
            bitrate_kbps: cfg.bitrate_kbps,
            encoder: cfg.encoder.clone(),
            rotation: cfg.rotation,
            hflip: cfg.hflip,
            vflip: cfg.vflip,
            keyframe_interval: cfg.keyframe_interval,
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

/// The effective GOP (keyframe) interval for a camera. An explicit
/// `keyframe_interval` (frames) wins; the default 0 yields a short low-latency
/// GOP of half a second at the configured fps so radio FEC recovers fast.
fn gop_interval(params: &EncoderParams) -> u32 {
    if params.keyframe_interval > 0 {
        params.keyframe_interval
    } else {
        (params.fps / 2).max(1)
    }
}

/// The ffmpeg `-vf` image-transform chain for the configured orientation, or
/// `None` when no transform is set (so an unconfigured rig emits no `-vf` and
/// the argv stays byte-identical). 90 = one `transpose=1`; 270 = `transpose=2`;
/// 180 = `transpose=2,transpose=2`; hflip/vflip append `hflip`/`vflip`.
fn ffmpeg_vf(rotation: u32, hflip: bool, vflip: bool) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    match rotation {
        90 => parts.push("transpose=1"),
        180 => parts.push("transpose=2,transpose=2"),
        270 => parts.push("transpose=2"),
        _ => {}
    }
    if hflip {
        parts.push("hflip");
    }
    if vflip {
        parts.push("vflip");
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(","))
    }
}

/// The GStreamer `videoflip` element chain for the configured orientation,
/// returned as `"videoflip method=… ! … ! "` (trailing separator) when any
/// transform is set, else `""`. Each transform is its own element (GStreamer
/// `videoflip` carries a single method), so a rotate + flip emits two chained
/// elements. Map: 90 = clockwise, 180 = rotate-180, 270 = counterclockwise;
/// hflip = horizontal-flip, vflip = vertical-flip.
fn gst_videoflip_chain(rotation: u32, hflip: bool, vflip: bool) -> String {
    let mut els: Vec<&str> = Vec::new();
    match rotation {
        90 => els.push("videoflip method=clockwise"),
        180 => els.push("videoflip method=rotate-180"),
        270 => els.push("videoflip method=counterclockwise"),
        _ => {}
    }
    if hflip {
        els.push("videoflip method=horizontal-flip");
    }
    if vflip {
        els.push("videoflip method=vertical-flip");
    }
    let chain = els.join(" ! ");
    if chain.is_empty() {
        chain
    } else {
        format!("{chain} ! ")
    }
}

/// True when the GStreamer builder should use the Allwinner OMX hardware
/// encoder: the board's HAL `encoder_api` is "vendor" AND `omxh264videoenc` is
/// present. An explicit `encoder: "software"` always overrides off (the OMX
/// branch is a hardware path); `encoder: "omx"` requires the same vendor gate
/// (an OMX request on a non-Allwinner board falls through to mpp/x264).
fn use_omx_encoder(params: &EncoderParams, env: &EncoderEnv) -> bool {
    if params.encoder == "software" {
        return false;
    }
    env.encoder_api == "vendor" && env.has_omxh264videoenc
}

/// Resolve the effective [`EncoderKind`] after applying the per-camera `encoder`
/// override and the board's HAL `encoder_api`.
///
/// * "v4l2m2m" → ffmpeg (h264_v4l2m2m, decided inside [`build_ffmpeg_command`]).
/// * "software" → keep the probed builder family and use a software codec inside
///   it (libx264 in ffmpeg, x264enc in GStreamer) — "software" names the CODEC,
///   not the family, so a GStreamer-only board still gets a runnable command.
/// * "omx" → GStreamer when the board is an Allwinner vendor with the element
///   present; otherwise the probed base kind.
/// * "auto" (default) → GStreamer-OMX on an Allwinner vendor board (the whole
///   point: a USB camera there must hit the HW OMX encoder, not ffmpeg
///   libx264), else the probed base kind.
fn resolve_kind(base: EncoderKind, params: &EncoderParams, env: &EncoderEnv) -> EncoderKind {
    match params.encoder.as_str() {
        "v4l2m2m" => EncoderKind::Ffmpeg,
        "software" => base,
        "omx" => {
            if env.encoder_api == "vendor" && env.has_omxh264videoenc {
                EncoderKind::Gstreamer
            } else {
                base
            }
        }
        _ => {
            if env.encoder_api == "vendor" && env.has_omxh264videoenc {
                EncoderKind::Gstreamer
            } else {
                base
            }
        }
    }
}

/// Build the full argv vector for the given encoder configuration.
///
/// Returns the program plus its arguments. For the bash-pipeline cases (rpicam
/// → RTSP, and the SEI-wrapped variants) the returned vector is
/// `["bash", "-c", "<pipeline>"]`, exactly as the predecessor composes it.
pub fn build_encoder_command(
    params: &EncoderParams,
    source: &str,
    output: &str,
    camera: Option<&CameraInfo>,
    env: &EncoderEnv,
) -> Result<Vec<String>, EncoderError> {
    let source = validate_source(source)?;
    let output = validate_source(output)?;
    // Apply the per-camera encoder override + board HAL encoder_api before
    // dispatching (builder-private; the probed base kind stays on `params.kind`).
    let kind = resolve_kind(params.kind, params, env);
    let cmd = match kind {
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
/// otherwise returns it verbatim.
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
/// capabilities are unknown (let ffmpeg auto-detect).
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
    // Use the HW H.264 encoder only when the HAL probe confirmed a real V4L2
    // encoder device is Present; otherwise map the codec to a sw encoder. A
    // mere `-encoders` listing is NOT enough — a wrapper with no backing device
    // makes ffmpeg exit at init and the camera streams zero bytes.
    // An explicit `encoder` override biases this: "software" always picks
    // libx264; "v4l2m2m" forces the HW M2M wrapper even if the probe was back-
    // level. "omx"/"auto" keep the probe-driven decision.
    let force_sw = params.encoder == "software";
    let force_v4l2m2m = params.encoder == "v4l2m2m";
    let use_hw_h264 = !force_sw
        && (force_v4l2m2m
            || (matches!(params.codec.as_str(), "h264" | "H264") && env.hw_h264.is_present()));

    let ffmpeg_codec: String = if use_hw_h264 {
        "h264_v4l2m2m".to_string()
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
        if source.starts_with("rtsp://") {
            cmd.push("-rtsp_transport".into());
            cmd.push("tcp".into());
        }
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
        // Pi V4L2 M2M HW encoder: force yuv420p, same short GOP, no B-frames.
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
        let bps = params.bitrate_kbps * 1000;
        format!("omxh264videoenc control-rate=constant target-bitrate={bps} key-int-max={gop}")
    } else if env.has_mpph264enc {
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

    // Capture → decode → orientation prefix, ending right before h264parse.
    // The OMX path is capture-direct with `io-mode=mmap`, forces NV12 for the
    // OMX encoder, and adds the frame-dropping `queue max-size-buffers=4
    // leaky=downstream` so the HW encoder never backs up behind the source.
    let core = if use_omx {
        format!(
            "v4l2src device={safe_source} io-mode=mmap do-timestamp=true ! {src_caps} ! \
             {decode} ! {flip}video/x-raw,format=NV12,width={},height={},framerate={}/1 ! \
             queue max-size-buffers=4 leaky=downstream ! {encoder}",
            params.width, params.height, params.fps
        )
    } else {
        format!("v4l2src device={safe_source} ! {src_caps} ! {decode} ! {flip}{encoder}")
    };
    // `h264parse config-interval=1` on the OMX path re-stamps SPS/PPS before
    // every IDR so a radio FEC recovery / late joiner resyncs instantly.
    let h264parse = if use_omx {
        " ! h264parse config-interval=1"
    } else {
        " ! h264parse"
    };

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
    let pipeline = format!("{core}{h264parse} ! filesink location={safe_output}");
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

/// Augment a raw `ffmpeg` encoder command with an additive second `rawvideo`
/// output for the vision frame tap, WITHOUT changing the existing encode/RTSP
/// output bytes.
///
/// This is the opt-in pre-encode split (gated by `video.vision.raw_tap`). The
/// strategy is purely additive: the original argv is kept verbatim through its
/// existing output URI, then a SECOND output is appended that re-reads the same
/// decoded input via an `-filter_complex` split. ffmpeg's `-filter_complex
/// split` duplicates the decoded frames into two labelled streams; the first
/// (`[enc]`) is mapped nowhere extra — the original output keeps consuming the
/// input exactly as before — and the second (`[vis]`) is throttled, scaled, and
/// written as `rawvideo` to the sink. Because the original output args are
/// untouched and appear first, the existing encode bytes are bit-identical;
/// the tap is a strictly appended `-map [vis] ... <sink>` block.
///
/// Returns the command unchanged when it is not a raw `ffmpeg` command (e.g.
/// the rpicam / gstreamer `bash -c` pipelines): those callers fall back to the
/// decoupled third-ffmpeg tap, which never perturbs the encoder at all.
///
/// `existing_output` is the original output URI (the last token of the encoder
/// command). The split feeds it through the named `[enc]` label so the encoder
/// settings still apply to the wire output.
pub fn augment_encoder_with_raw_tap(
    cmd: &[String],
    existing_output: &str,
    fps: u32,
    width: u32,
    height: u32,
    pixel_format: &str,
    sink: &str,
) -> Vec<String> {
    // Only a raw ffmpeg command can carry a second mapped output. bash-pipeline
    // (rpicam / gstreamer) and gst-launch commands are left untouched.
    if cmd.first().map(String::as_str) != Some("ffmpeg") {
        return cmd.to_vec();
    }
    // The existing output URI must be the last token; if the command does not
    // end the way we expect, do not risk perturbing it — leave it unchanged.
    if cmd.last().map(String::as_str) != Some(existing_output) {
        return cmd.to_vec();
    }

    let fps = fps.max(1);
    let mut out: Vec<String> = cmd.to_vec();
    // Append a strictly additive second output. The split duplicates the input
    // frames; `[enc]` carries the untouched primary output, `[vis]` carries the
    // throttled/scaled raw tap. The primary output args above already encode
    // `[enc]` because, absent an explicit `-map`, ffmpeg routes the single
    // filtered video stream to the first output — so we keep the original
    // output as-is and only add the explicitly-mapped `[vis]` sink after it.
    out.push("-filter_complex".into());
    out.push(format!(
        "split=2[enc][vis];[vis]fps={fps},scale={width}:{height}[visout]"
    ));
    out.push("-map".into());
    out.push("[visout]".into());
    out.push("-an".into());
    out.push("-pix_fmt".into());
    out.push(pixel_format.to_string());
    out.push("-f".into());
    out.push("rawvideo".into());
    out.push(sink.to_string());
    out
}

/// Remove the last occurrence of `flag` and its following value from `args`,
/// scanning right-to-left.
fn strip_flag_with_value(args: &mut Vec<String>, flag: &str) {
    if args.len() < 2 {
        return;
    }
    // i runs from len-1 down to 1; act when args[i]==flag
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
/// gstreamer). The binary-presence flags are taken as inputs to keep this pure.
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

/// The codec elements the label is derived from, hardware first. An argv can
/// only ever carry one of them, so the order is a tie-break that never fires
/// in practice; it is fixed so the emitted value is deterministic.
const ENCODER_ELEMENTS: [&str; 8] = [
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
        // The rpicam / gstreamer / SEI-wrapped forms are `bash -c "<pipeline>"`,
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    /// The frozen argv vectors this builder must reproduce byte for byte.
    ///
    /// Originally captured from the Python encoder that this module replaced, by
    /// a script that mocked every runtime probe (the Rockchip `/proc` read, the
    /// ffmpeg and gst-inspect probes, `sys.executable`) so each vector was
    /// deterministic on any host. Both the Python builder and that script are
    /// deleted: this file IS the reference now, and a new case is written here
    /// by hand rather than captured. See the module test for how each case maps
    /// onto the Rust builder.
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

    fn rockchip() -> EncoderEnv {
        EncoderEnv {
            hw_h264: hw_node_missing(),
            has_mpph264enc: false,
            has_omxh264videoenc: false,
            has_rtspclientsink: true,
            encoder_api: "unknown".into(),
            python_executable: PY_EXE.into(),
        }
    }
    fn non_rk_sw() -> EncoderEnv {
        EncoderEnv {
            hw_h264: hw_node_missing(),
            has_mpph264enc: false,
            has_omxh264videoenc: false,
            has_rtspclientsink: true,
            encoder_api: "unknown".into(),
            python_executable: PY_EXE.into(),
        }
    }
    fn non_rk_hw() -> EncoderEnv {
        EncoderEnv {
            hw_h264: hw_present(),
            has_mpph264enc: false,
            has_omxh264videoenc: false,
            has_rtspclientsink: true,
            encoder_api: "unknown".into(),
            python_executable: PY_EXE.into(),
        }
    }
    fn rk_mpp() -> EncoderEnv {
        EncoderEnv {
            hw_h264: hw_node_missing(),
            has_mpph264enc: true,
            has_omxh264videoenc: false,
            has_rtspclientsink: true,
            encoder_api: "rkmpp".into(),
            python_executable: PY_EXE.into(),
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
            python_executable: PY_EXE.into(),
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

    // --- probe-first HW encoder selection ------------------------------

    #[test]
    fn probe_absent_node_selects_libx264() {
        // The live bug: a board advertises the h264_v4l2m2m wrapper but ships
        // no real V4L2 encoder device. With the probe reporting NodeMissing the
        // builder MUST fall back to software libx264 — never the wrapper, which
        // would make ffmpeg exit at init and stream zero bytes.
        let env = EncoderEnv {
            hw_h264: hw_node_missing(),
            has_mpph264enc: false,
            has_omxh264videoenc: false,
            has_rtspclientsink: true,
            encoder_api: "unknown".into(),
            python_executable: PY_EXE.into(),
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
        assert_eq!(got[ci + 1], "libx264");
        assert!(!got.iter().any(|t| t == "h264_v4l2m2m"));
    }

    #[test]
    fn probe_present_device_selects_h264_v4l2m2m() {
        // A real V4L2 M2M node enumerated H.264 and passed the bounded
        // trial-init → the builder uses the hardware encoder.
        let env = EncoderEnv {
            hw_h264: hw_present(),
            has_mpph264enc: false,
            has_omxh264videoenc: false,
            has_rtspclientsink: true,
            encoder_api: "unknown".into(),
            python_executable: PY_EXE.into(),
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

    // --- opt-in pre-encode raw tap (Option B) --------------------------

    #[test]
    fn raw_tap_appends_without_changing_encode_prefix() {
        // The existing encode/RTSP output bytes MUST be untouched: the original
        // command is a strict prefix of the augmented one.
        let base = build_encoder_command(
            &params(EncoderKind::Ffmpeg, 1280, 720, 30, 4000),
            "/dev/video1",
            RTSP_OUT,
            Some(&usb_mjpeg()),
            &rockchip(),
        )
        .unwrap();
        let augmented = augment_encoder_with_raw_tap(
            &base,
            RTSP_OUT,
            10,
            640,
            480,
            "rgb24",
            "/run/ados/vision-tap-main.sock",
        );
        // Every original token, in order, comes first.
        assert!(augmented.len() > base.len());
        assert_eq!(&augmented[..base.len()], base.as_slice());
        // The appended block is the additive raw-tap output.
        let tail = &augmented[base.len()..];
        assert!(tail.iter().any(|t| t == "-filter_complex"));
        assert!(tail.iter().any(|t| t == "rawvideo"));
        assert_eq!(tail.last().unwrap(), "/run/ados/vision-tap-main.sock");
        let pf = tail.iter().position(|t| t == "-pix_fmt").unwrap();
        assert_eq!(tail[pf + 1], "rgb24");
    }

    #[test]
    fn raw_tap_leaves_bash_pipeline_unchanged() {
        // The rpicam path is a `bash -c` pipeline: it cannot safely carry a
        // second mapped output, so the augmentation is a no-op (the caller
        // falls back to the decoupled tap).
        let base = build_encoder_command(
            &params(EncoderKind::RpicamVid, 1280, 720, 30, 4000),
            "/dev/video0",
            RTSP_OUT,
            Some(&csi()),
            &rockchip(),
        )
        .unwrap();
        assert_eq!(base[0], "bash");
        let augmented =
            augment_encoder_with_raw_tap(&base, RTSP_OUT, 10, 640, 480, "rgb24", "/s.sock");
        assert_eq!(augmented, base);
    }

    #[test]
    fn raw_tap_leaves_mismatched_output_unchanged() {
        // If the last token is not the expected output URI, do not risk
        // perturbing the command — return it unchanged.
        let base = build_encoder_command(
            &params(EncoderKind::Ffmpeg, 1280, 720, 30, 4000),
            "/dev/video1",
            RTSP_OUT,
            Some(&usb_mjpeg()),
            &rockchip(),
        )
        .unwrap();
        let augmented = augment_encoder_with_raw_tap(
            &base,
            "rtsp://wrong/output",
            10,
            640,
            480,
            "rgb24",
            "/s.sock",
        );
        assert_eq!(augmented, base);
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
        let got =
            build_encoder_command(&p, "/dev/video1", RTSP_OUT, Some(&usb_mjpeg()), &rockchip())
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
    fn gstreamer_keyframe_interval_reaches_key_int_max() {
        // keyframe_interval=5 → key-int-max=5 on the x264/mpp/omx paths.
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
            tok.iter().any(|t| t.contains("key-int-max=5")),
            "missing key-int-max=5"
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
}
