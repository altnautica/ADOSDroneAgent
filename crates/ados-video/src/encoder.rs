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
//! - **CSI** camera → `rpicam-vid`, falling back to ffmpeg when rpicam is
//!   absent. Note that `rpicam-vid` is only a *hardware* encoder up to
//!   BCM2711 (Pi 4 / CM4, VideoCore VI). On BCM2712 (Pi 5 / CM5, VideoCore
//!   VII) there is no H.264 encode block at all and rpicam runs
//!   libavcodec/x264 in software, which is why the builder gates
//!   `--low-latency` on [`EncoderEnv::pi5_class`].
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
//! The H.264 encoder decision is a probed [`Probed<EncoderDevice>`] and the
//! board family is a probed device-tree `compatible` list, both carried on
//! [`EncoderEnv`]; the GStreamer-element probes are plain booleans. Gathering
//! them up front keeps the builder itself pure and testable without touching any
//! device or subprocess. [`EncoderEnv::detect`] does the real probing on Linux
//! (the HW encoder and the SoC identity via [`ados_hal_probe`]); the builder
//! takes the resolved env. `detect` blocks on subprocesses and device ioctls,
//! so async callers use [`EncoderEnv::detect_async`].

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
    /// The board is Pi-5-class silicon (BCM2712 — Raspberry Pi 5 / CM5).
    ///
    /// Probed from the kernel's own device-tree `compatible` list via
    /// [`ados_hal_probe::probe_soc`] and classified by [`soc_is_pi5_class`].
    /// This family has **no** hardware H.264 encoder (VideoCore VII dropped
    /// the encode block that VideoCore VI on the Pi 4 / CM4 had), so
    /// `rpicam-vid` there is a software x264 encoder and must be given
    /// `--low-latency` or it buffers a constant 8-frame pipeline.
    pub pi5_class: bool,
    /// Absolute path to the Python interpreter used to splice the SEI
    /// injector (`<python> -m ados.services.video.sei_injector`). Equivalent
    /// to Python's `sys.executable`.
    pub python_executable: String,
    /// Usable CPU parallelism, as the kernel reports it
    /// ([`std::thread::available_parallelism`]).
    ///
    /// This is an encoder LATENCY input, not a throughput one. The software
    /// x264 path is the only encode path left on Pi-5-class silicon (BCM2712
    /// dropped the encode block), and its pipeline delay is set by how the
    /// frame is threaded: frame-level threading holds `threads` frames in
    /// flight before the first slice comes out, while sliced threading splits
    /// ONE frame across the cores and emits it whole. `-tune zerolatency`
    /// already asks for the latter; the builder used to override it back off.
    /// Emitting the sliced-threads levers needs real cores to split across, so
    /// the count is probed rather than assumed: on a 1-2 core board the levers
    /// would oversubscribe and cost more than they save.
    pub cpu_threads: u32,
}

impl EncoderEnv {
    /// Probe the real environment. On non-Linux hosts the probes are no-ops
    /// so the builder is exercisable on the dev host; the rig path is Linux.
    ///
    /// This blocks: it shells `gst-inspect-1.0` up to three times and
    /// trial-inits a V4L2 encoder device, and `gst-inspect` on a cold SBC page
    /// cache is not fast. Async callers MUST use [`EncoderEnv::detect_async`]
    /// instead; this entry point stays for the synchronous callers and tests.
    #[cfg(target_os = "linux")]
    pub fn detect() -> Self {
        // Probe for a REAL hardware H.264 encoder device rather than trusting
        // the ffmpeg `-encoders` listing. Runs at the pre-arm boot phase: the
        // trial-init opens and configures a device, so it is never run during
        // armed runtime (the probe defers to its cached result there).
        let hw_h264 = ados_hal_probe::probe::video::probe_h264_encoder(
            ados_protocol::hwcaps::ProbePhase::BootPreArm,
        );

        // Board family, from the same probe-first HAL: read-only, phase-free,
        // and the kernel's own answer rather than a board-YAML declaration.
        let pi5_class = ados_hal_probe::probe_soc()
            .value()
            .is_some_and(|soc| soc_is_pi5_class(&soc.0));

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
            pi5_class,
            python_executable: current_python_executable(),
            cpu_threads: detect_cpu_threads(),
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
            pi5_class: false,
            python_executable: current_python_executable(),
            cpu_threads: detect_cpu_threads(),
        }
    }

    /// [`EncoderEnv::detect`] moved off the async runtime.
    ///
    /// The probes are blocking subprocess spawns and device ioctls; running
    /// them inline on a tokio worker stalls every other task on that worker
    /// for as long as `gst-inspect-1.0` takes to fault its plugin registry in.
    /// Async callers must use this. The closure is the unchanged sync body, so
    /// there is exactly one probe implementation.
    pub async fn detect_async() -> Self {
        tokio::task::spawn_blocking(Self::detect)
            .await
            // `detect` cannot panic (every probe is fallible-and-defaulted), so
            // a JoinError here can only mean runtime shutdown; the honest answer
            // then is the same all-absent env the non-Linux path returns.
            .unwrap_or_else(|_| Self {
                hw_h264: Probed::NotProbed,
                has_mpph264enc: false,
                has_omxh264videoenc: false,
                has_rtspclientsink: false,
                encoder_api: "unknown".to_string(),
                pi5_class: false,
                python_executable: current_python_executable(),
                cpu_threads: detect_cpu_threads(),
            })
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

/// Usable CPU parallelism, or 1 when the kernel will not say.
///
/// `available_parallelism` honours the cgroup CPU quota and the affinity mask,
/// which is what this decision needs: a video unit pinned to two cores by a
/// `CPUAffinity=` must not be handed four encode threads. Falling back to 1
/// (not to a guess of 4) keeps the conservative argv on an unknown host — the
/// sliced-threads levers are only emitted where the cores are proven present.
fn detect_cpu_threads() -> u32 {
    std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(1)
}

/// True when a device-tree `compatible` list names Pi-5-class silicon.
///
/// The list is what `/proc/device-tree/compatible` exposes, most-specific
/// first: `raspberrypi,5-model-b` + `brcm,bcm2712` on a Pi 5,
/// `raspberrypi,5-compute-module` + `brcm,bcm2712` on a CM5. Either the SoC
/// entry or the board entry is enough, so a downstream device tree that omits
/// one of them still classifies correctly.
///
/// The family matters to the builder because it is the boundary of the
/// hardware H.264 encoder: BCM2711 (Pi 4 / CM4, VideoCore VI) has one and
/// BCM2712 (Pi 5 / CM5, VideoCore VII) does not, so on Pi-5-class boards
/// `rpicam-vid` is a software encoder and needs `--low-latency`.
pub fn soc_is_pi5_class(compatibles: &[String]) -> bool {
    compatibles.iter().any(|c| {
        let c = c.to_ascii_lowercase();
        c.contains("bcm2712") || c.contains("raspberrypi,5")
    })
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
    /// Also emit the wfb radio's RTP copy straight out of this encoder, as a
    /// second muxer on the SAME encode, instead of letting a separate `ffmpeg`
    /// re-read the published RTSP stream back out of mediamtx.
    ///
    /// This is a pipeline decision, not a camera setting: only the PRIMARY leg
    /// of a radio-carrying node wants it, and only on the `ffmpeg` family
    /// (the `rpicam` / GStreamer arms are `bash -c` pipelines and keep the
    /// separate tap). The caller sets it explicitly;
    /// [`EncoderParams::from_camera_config`] leaves it off.
    pub rtp_fanout: bool,
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
            rtp_fanout: false,
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

/// The only wire codec this builder can produce a shippable stream for.
///
/// H.264 is not a preference here, it is the only value with a tuning arm.
/// `h265` / `hevc` / `mjpeg` parse clean out of `video.camera.codec` and used
/// to be mapped straight onto `libx265` / `mjpeg`, where **neither** tuning
/// block in [`build_ffmpeg_command`] fires: no `-g`, so the GOP is whatever
/// the encoder defaults to, and no in-band parameter-set bitstream filter, so
/// SPS/PPS/VPS never reach the wire. Meanwhile the primary leg's radio branch
/// frames every payload as RTP payload type 96 — H.264 per RFC 6184 — against
/// a fixed receiver SDP, and the browser leg is pinned to an H.264 profile.
/// A node that configured one of them therefore published an unbounded-GOP
/// stream with no in-band headers into an H.264-typed leg: nothing decodes on
/// the ground, and the pipeline still reported `streaming`. Refusing the
/// build names the real cause instead of shipping that.
///
/// Exactly one spelling is accepted. There is no `H264` alias: the `rpicam-vid`
/// arm passes this value verbatim to `--codec`, which only knows the lowercase
/// form, so the alias was never a working configuration on that arm.
fn validate_codec(codec: &str) -> Result<(), EncoderError> {
    if codec == "h264" {
        Ok(())
    } else {
        Err(EncoderError::UnsupportedCodec(codec.to_string()))
    }
}

/// Error from the encoder command builder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncoderError {
    /// A source / output string contained a disallowed character.
    InvalidSource(String),
    /// The configured wire codec has no tuning arm, so it cannot be published
    /// without breaking the radio and browser legs. Only `h264` is accepted;
    /// the `Display` text carries the full reason.
    UnsupportedCodec(String),
}

impl std::fmt::Display for EncoderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EncoderError::InvalidSource(s) => write!(
                f,
                "Invalid source path: {s:?}. Only alphanumeric, slashes, dots, \
                 hyphens, underscores, and colons are allowed."
            ),
            EncoderError::UnsupportedCodec(c) => write!(
                f,
                "Unsupported video codec {c:?}. Only \"h264\" is emitted with a \
                 bounded keyframe interval (-g) and in-band SPS/PPS before every \
                 IDR, and the radio leg frames every payload as RTP payload type \
                 96 (H.264, RFC 6184) against a fixed receiver SDP — so any other \
                 codec goes on air untuned and undecodable. Set \
                 video.camera.codec to \"h264\"."
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

/// The H.264 quantizer floor handed to the `h264_v4l2m2m` hardware encoder.
///
/// This is not an invented number: it is the default `bcm2835-codec` itself
/// registers for `V4L2_CID_MPEG_VIDEO_H264_MIN_QP`, i.e. the floor the Pi
/// firmware considers sane for its own rate controller. ffmpeg's
/// `v4l2_m2m_enc.c` writes that control unconditionally and substitutes **0**
/// whenever `-qmin` is unset, so an argv that omits `-qmin` silently replaces
/// the firmware's floor with "spend whatever you like".
///
/// It is deliberately a fixed floor rather than something derived from
/// `bitrate_kbps`: the QP-to-bitrate mapping is content-dependent and cannot be
/// computed while building an argv. The bitrate-derived clamp on this path
/// would be a VBV, and ffmpeg exposes no VBV control for `h264_v4l2m2m`
/// (`rc_max_rate` / `rc_buffer_size` are never read), so this floor is the
/// bound that actually reaches the driver.
const V4L2M2M_MIN_QP: u32 = 20;

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
    // Refuse a codec no arm tunes, BEFORE anything is spawned. All three
    // builder families are gated here rather than each arm checking for itself:
    // the GStreamer arm ignores `codec` entirely and the rpicam arm forwards it
    // verbatim, so a per-arm check would disagree with itself.
    validate_codec(&params.codec)?;
    // Apply the per-camera encoder override + board HAL encoder_api before
    // dispatching (builder-private; the probed base kind stays on `params.kind`).
    let kind = resolve_kind(params.kind, params, env);
    let cmd = match kind {
        EncoderKind::RpicamVid => build_rpicam_command(params, source, output, env),
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
fn build_rpicam_command(
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
        //
        // `-thread_queue_size 128`: this is queue DEPTH in packets between the
        // kernel's V4L2 buffers and ffmpeg's demuxer thread, and it is NOT a
        // latency knob. A queue only adds latency while it is actually
        // occupied, and it is only occupied while the consumer is behind. The
        // previous depth of 4 meant any scheduler hiccup longer than ~130ms at
        // 30fps overran it, and an overrun on a capture input does not delay
        // frames, it DROPS them outright — the one failure mode a video link
        // cannot recover from. 128 packets covers a >4s stall at 30fps, far
        // beyond any plausible hiccup on a loaded SBC, while staying bounded:
        // this builder prefers the compressed `mjpeg` input format
        // (see `select_input_format`), where 128 queued frames is tens of MB
        // of transient buffer, and even the raw `yuyv` fallback stays in the
        // low hundreds of MB in the pathological case where it fills at all.
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
                "128",
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

/// The output half of an `ffmpeg` publish argv: the muxer flags, the muxer
/// selection, and the destination(s).
///
/// Shared by [`build_ffmpeg_command`] and the SEI-spliced publish stage in
/// [`wrap_with_sei_inject`] so the two cannot drift — a publish stage that
/// differs between "SEI off" and "SEI on" is how a latency flag gets fixed on
/// one path only.
///
/// `rtp_fanout` (RTSP destinations only) replaces the single RTSP output with
/// ffmpeg's `tee` muxer: ONE encode, two muxers, the second emitting the
/// radio's RTP copy directly. That is what removes the whole extra `ffmpeg`
/// that used to re-read the published stream back out of mediamtx.
fn ffmpeg_output_stage(output: &str, rtp_fanout: bool) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if output.starts_with("rtsp://") {
        // `-muxdelay 0 -muxpreload 0` strip ffmpeg's default ~0.7 s mux delay
        // and ~0.5 s preload. Every other ffmpeg in this tree sets them and
        // says so; the primary encoder — the one a USB or IP camera uses, i.e.
        // the default path — did not, which left up to 1.2 s of avoidable
        // latency in front of every downstream hop.
        //
        // They are ffmpeg CLI *output* options, NOT `AVFormatContext`
        // options, so they cannot be passed per-branch inside a tee spec:
        // ffmpeg answers `Unknown option 'muxdelay'` and the tee aborts the
        // whole output (verified on ffmpeg 9.0.1). They belong here, ahead of
        // the muxer selection, on both the plain and the tee form.
        out.extend(
            ["-max_delay", "0", "-muxdelay", "0", "-muxpreload", "0"]
                .iter()
                .map(|s| s.to_string()),
        );
        if rtp_fanout {
            // The tee muxer needs an explicit `-map`; without one ffmpeg
            // refuses to build the output.
            out.push("-map".into());
            out.push("0:v".into());
            out.push("-f".into());
            out.push("tee".into());
            out.push(tee_spec(output));
        } else {
            // TCP RTSP avoids UDP fragmentation of large keyframe NALs.
            out.extend(
                ["-rtsp_transport", "tcp", "-f", "rtsp"]
                    .iter()
                    .map(|s| s.to_string()),
            );
            out.push(output.to_string());
        }
    } else if output.starts_with("udp://") || output.starts_with("tcp://") {
        // Same mux-delay strip as the RTSP branch: the MPEG-TS muxer inherits
        // the same ffmpeg defaults, and this sink is a live feed too.
        out.extend(
            ["-muxdelay", "0", "-muxpreload", "0", "-f", "mpegts"]
                .iter()
                .map(|s| s.to_string()),
        );
        out.push(output.to_string());
    } else {
        out.push(output.to_string());
    }
    out
}

/// The `-f tee` branch specification: the RTSP publish into mediamtx, plus the
/// wfb radio's RTP copy on UDP 5600.
///
/// Both branches carry the same already-encoded packets, so the RTP copy costs
/// no second encode and no re-read. Framing is RTP (RFC 6184) because that is
/// wfb-ng's contract: each datagram must survive single-packet loss on its own,
/// and raw H.264 over UDP corrupts silently to the next start code. The payload
/// type, SSRC, packet size and destination all come from [`crate::wfb_tee`], so
/// the receiver's static SDP keeps describing the stream exactly.
///
/// Deliberately NO `onfail=ignore`: if the radio branch cannot be opened, this
/// encoder MUST fail and be restarted by the supervisor rather than quietly
/// serving LAN video with a dead radio leg. That shared fate is what replaces
/// the retired tap's own progress watchdog — one encode, one process, one
/// liveness signal, with the bytes-on-air counter (`wfb_tx`) as the
/// independent downstream check.
///
/// Per-branch options are `AVFormatContext` options only (`max_delay`,
/// `flush_packets`, `rtsp_transport`, `payload_type`, `ssrc`); the CLI-level
/// mux-delay strip is emitted by [`ffmpeg_output_stage`].
fn tee_spec(rtsp_output: &str) -> String {
    format!(
        "[f=rtsp:rtsp_transport=tcp:max_delay=0:flush_packets=1]{rtsp_output}\
         |[f=rtp:payload_type={pt}:ssrc={ssrc}:max_delay=0:flush_packets=1]{rtp}",
        pt = crate::wfb_tee::WFB_TEE_PAYLOAD_TYPE,
        ssrc = crate::wfb_tee::WFB_TEE_SSRC,
        rtp = crate::wfb_tee::rtp_destination_url(),
    )
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
        // Did the builder append the radio fan-out? Then the output stage ends
        // in the tee spec rather than the bare URI, and the rebuilt publish
        // stage has to carry the same two branches — otherwise turning the SEI
        // probe on would silently take the radio leg down.
        let fanout = cmd
            .last()
            .is_some_and(|t| t.starts_with("[f=") && t.contains(output_uri));
        let mut encoded: Vec<String> = cmd.to_vec();
        // Strip the destination: the output URI, or the tee spec that carries
        // it (both are the last token).
        if fanout || encoded.last().map(String::as_str) == Some(output_uri) {
            encoded.pop();
        }
        // Strip the output stage's muxer selection + flags, right-to-left, so
        // the INPUT side's `-f v4l2` / `-fflags` are never touched.
        strip_flag_with_value(&mut encoded, "-f");
        if fanout {
            strip_flag_with_value(&mut encoded, "-map");
        }
        strip_flag_with_value(&mut encoded, "-rtsp_transport");
        strip_flag_with_value(&mut encoded, "-muxpreload");
        strip_flag_with_value(&mut encoded, "-muxdelay");
        strip_flag_with_value(&mut encoded, "-max_delay");
        // Encode-only ffmpeg now emits raw Annex-B H.264 on stdout.
        encoded.push("-f".into());
        encoded.push("h264".into());
        encoded.push("-".into());

        // Publish-only ffmpeg pulls the SEI-stamped Annex-B from stdin and
        // re-mounts exactly the destination(s) the builder had chosen, through
        // the SAME output-stage builder — so the mux-delay strip and the radio
        // fan-out cannot be present on one path and missing on the other.
        if !(output_uri.starts_with("rtsp://")
            || output_uri.starts_with("udp://")
            || output_uri.starts_with("tcp://"))
        {
            // Unknown output URI — cannot rebuild the publisher; leave unchanged.
            return cmd.to_vec();
        }
        let mut publish: Vec<String> = [
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
            "-flush_packets",
            "1",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        publish.extend(ffmpeg_output_stage(output_uri, fanout));

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

/// Augment a raw `ffmpeg` encoder command with a second `rawvideo` output for
/// the vision frame tap, keeping the existing encode/RTSP output settings.
///
/// This is the opt-in pre-encode split (gated by `video.vision.raw_tap`): one
/// decode, one `-filter_complex split`, two outputs. `[enc]` carries the full-
/// rate branch into the existing encoder + publish stage; `[vis]` is throttled
/// and scaled and written as `rawvideo` to the sink.
///
/// # Every pad has to terminate
///
/// A labelled complex-filter output is NOT "left alone" when nothing maps it:
/// ffmpeg fails filtergraph init with an unconnected output and the process
/// exits before it opens the camera. The earlier form declared `[enc]` and
/// mapped only `[visout]`, so enabling the tap took the encoder — and with it
/// every downstream leg — off the air entirely. So `[enc]` is explicitly
/// `-map`ped into the first output file, and the split's input pad is bound to
/// `[0:v]` rather than left implicit.
///
/// # Why `-vf` moves into the graph
///
/// The orientation transform is a SIMPLE filtergraph (`-vf`), and ffmpeg
/// refuses `-vf` on an output stream fed by a complex filtergraph. It is
/// therefore lifted out of the argv and spliced in ahead of the split, which
/// also means the tap sees the same orientation the operator sees.
///
/// Returns the command unchanged when it is not a raw `ffmpeg` command (the
/// rpicam / gstreamer `bash -c` pipelines), when the output URI is not the last
/// token (the `-f tee` radio fan-out form), or when the argv already maps or
/// filters its streams. Those callers fall back to the decoupled third-ffmpeg
/// tap, which never perturbs the encoder at all.
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
    // This is also what excludes the `-f tee` fan-out form, whose last token is
    // the branch spec.
    if cmd.last().map(String::as_str) != Some(existing_output) {
        return cmd.to_vec();
    }
    // An argv that already routes its own streams owns its output stage; a
    // second rewrite on top would double-map it. Refuse rather than emit a
    // graph whose shape depends on which rewrite ran first.
    if cmd
        .iter()
        .any(|t| t == "-map" || t == "-filter_complex" || t == "-filter:v")
    {
        return cmd.to_vec();
    }

    let fps = fps.max(1);
    let mut out: Vec<String> = cmd.to_vec();

    // Lift the simple `-vf` chain into the complex graph (see the doc above).
    let vf = take_flag_value(&mut out, "-vf");
    let split = match vf.as_deref() {
        Some(chain) => format!("[0:v]{chain},split=2[enc][vis]"),
        None => "[0:v]split=2[enc][vis]".to_string(),
    };
    let graph = format!("{split};[vis]fps={fps},scale={width}:{height}[visout]");

    // `-map [enc]` is an option of the FIRST output file, so it goes before
    // that file's URI — which is still the last token at this point.
    let uri_idx = out.len() - 1;
    out.splice(uri_idx..uri_idx, ["-map".to_string(), "[enc]".to_string()]);
    // `-filter_complex` is a global option; emit it ahead of the input so the
    // argv reads in ffmpeg's documented order.
    out.splice(1..1, ["-filter_complex".to_string(), graph]);

    // The tap's own output file.
    out.extend(
        [
            "-map",
            "[visout]",
            "-an",
            "-pix_fmt",
            pixel_format,
            "-f",
            "rawvideo",
            sink,
        ]
        .iter()
        .map(|s| s.to_string()),
    );
    out
}

/// Remove the last occurrence of `flag` and its following value from `args`,
/// scanning right-to-left, and return the value.
fn take_flag_value(args: &mut Vec<String>, flag: &str) -> Option<String> {
    if args.len() < 2 {
        return None;
    }
    // i runs from len-1 down to 1; act when args[i]==flag
    // and a value follows (i+1 < len). Take the highest such i (right-most).
    for i in (1..args.len()).rev() {
        if args[i] == flag && i + 1 < args.len() {
            let value = args.remove(i + 1);
            args.remove(i);
            return Some(value);
        }
    }
    None
}

/// Remove the last occurrence of `flag` and its following value from `args`,
/// discarding the value.
fn strip_flag_with_value(args: &mut Vec<String>, flag: &str) {
    let _ = take_flag_value(args, flag);
}

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
        CameraType::Usb | CameraType::Ip => {
            // mpph264enc (Rockchip VPU) is disabled — fall back to ffmpeg
            // libx264, then gstreamer x264enc. See this function's doc comment
            // for the exact retest procedure before re-enabling it.
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
            python_executable: PY_EXE.into(),
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
            python_executable: PY_EXE.into(),
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
            python_executable: PY_EXE.into(),
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
            python_executable: PY_EXE.into(),
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
            python_executable: PY_EXE.into(),
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
            45,
        );
        let got = build(&p, "/dev/video0", RTSP_OUT, &csi(), &rockchip(), false);
        assert_eq!(flag_value(&got, "--intra").as_deref(), Some("45"));
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

    // --- opt-in pre-encode raw tap ------------------------------------

    const SINK: &str = "/run/ados/vision-tap-main.sock";

    /// The pads of an ffmpeg `-filter_complex` string: `(inputs, outputs)`.
    ///
    /// Chains are `;`-separated. Bracketed labels before the first filter name
    /// are that chain's inputs; bracketed labels after the last filter are its
    /// outputs. Deriving them from the emitted string is what lets the tests
    /// below assert graph well-formedness on real builder output instead of
    /// eyeballing a literal.
    fn graph_pads(graph: &str) -> (Vec<String>, Vec<String>) {
        let mut ins: Vec<String> = Vec::new();
        let mut outs: Vec<String> = Vec::new();
        for chain in graph.split(';') {
            let mut rest = chain.trim();
            while let Some(stripped) = rest.strip_prefix('[') {
                let end = stripped.find(']').expect("filter label is terminated");
                ins.push(stripped[..end].to_string());
                rest = &stripped[end + 1..];
            }
            let mut tail = rest;
            let mut trailing: Vec<String> = Vec::new();
            while tail.ends_with(']') {
                let start = tail.rfind('[').expect("filter label is terminated");
                trailing.push(tail[start + 1..tail.len() - 1].to_string());
                tail = &tail[..start];
            }
            trailing.reverse();
            outs.extend(trailing);
        }
        (ins, outs)
    }

    /// The label of every `-map` target in `cmd`, brackets stripped.
    fn mapped_labels(cmd: &[String]) -> Vec<String> {
        cmd.windows(2)
            .filter(|w| w[0] == "-map")
            .map(|w| {
                w[1].trim_start_matches('[')
                    .trim_end_matches(']')
                    .to_string()
            })
            .collect()
    }

    fn has_pair(cmd: &[String], flag: &str, value: &str) -> bool {
        cmd.windows(2).any(|w| w[0] == flag && w[1] == value)
    }

    /// `Ok(())` when the emitted graph is one ffmpeg will accept: every filter
    /// output pad terminates (consumed by another chain or `-map`ped), every
    /// filter input pad has a producer, every `-map` names a pad that exists,
    /// and each output file is explicitly mapped. `Err(reason)` otherwise.
    ///
    /// A predicate rather than a bare assertion so the test below can feed it
    /// the broken shape as a negative control.
    fn graph_termination_check(cmd: &[String]) -> Result<(), String> {
        let Some(fc) = cmd.iter().position(|t| t == "-filter_complex") else {
            return Err("no -filter_complex emitted".into());
        };
        let graph = &cmd[fc + 1];
        let (ins, outs) = graph_pads(graph);
        let mapped = mapped_labels(cmd);

        // Guard against a vacuous pass: the whole point is a fan-out.
        if outs.len() < 2 {
            return Err(format!("graph does not fan out: {graph}"));
        }
        for pad in &outs {
            if !mapped.contains(pad) && !ins.contains(pad) {
                return Err(format!(
                    "filter output [{pad}] terminates nowhere. ffmpeg cannot bind the \
                     graph and the encoder exits before it opens the camera, so the node \
                     loses ALL video. graph={graph} maps={mapped:?}"
                ));
            }
        }
        for pad in &ins {
            // A `file:stream` reference (`0:v`) is produced by the input file,
            // not by the graph.
            if !pad.contains(':') && !outs.contains(pad) {
                return Err(format!(
                    "filter input [{pad}] has no producer. graph={graph}"
                ));
            }
        }
        for label in &mapped {
            if !outs.contains(label) {
                return Err(format!(
                    "-map [{label}] names no graph output. graph={graph}"
                ));
            }
        }
        // One `-map` per output file: the encode/publish file and the tap file.
        // An unmapped output file falls back to ffmpeg's automatic stream
        // selection, which claims the raw input stream, bypasses the split and
        // leaves the split's own input pad unfed.
        if mapped.len() != 2 {
            return Err(format!("expected one -map per output file: {mapped:?}"));
        }
        Ok(())
    }

    /// The base argv the splice is defined against: no radio fan-out, because
    /// the two features rewrite the same output stage and the orchestrator
    /// requests exactly one of them (`lifecycle::raw_tap_splice_wanted`).
    fn raw_tap_base(rotation: u32) -> Vec<String> {
        build_encoder_command(
            &EncoderParams {
                rtp_fanout: false,
                rotation,
                ..params(EncoderKind::Ffmpeg, 1280, 720, 30, 4000)
            },
            "/dev/video1",
            RTSP_OUT,
            Some(&usb_mjpeg()),
            &rockchip(),
        )
        .unwrap()
    }

    #[test]
    fn raw_tap_graph_terminates_every_branch() {
        let cmd =
            augment_encoder_with_raw_tap(&raw_tap_base(0), RTSP_OUT, 10, 640, 480, "rgb24", SINK);
        graph_termination_check(&cmd).expect("emitted graph must be fully terminated");

        // Negative control — the shape this replaced: `[enc]` declared, only
        // `[visout]` mapped, the split's input pad left implicit and the first
        // output file left to automatic stream selection. Measured on ffmpeg
        // 9.0.1: "Cannot find an unused video input stream to feed the unlabeled
        // input pad split:default / Error binding filtergraph inputs/outputs",
        // exit 234, zero bytes on either output. The checker must reject it, or
        // the assertion above proves nothing.
        let unterminated: Vec<String> = [
            "ffmpeg",
            "-filter_complex",
            "split=2[enc][vis];[vis]fps=10,scale=640:480[visout]",
            "-i",
            "/dev/video1",
            "-f",
            "rtsp",
            RTSP_OUT,
            "-map",
            "[visout]",
            "-f",
            "rawvideo",
            SINK,
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert!(
            graph_termination_check(&unterminated).is_err(),
            "the unterminated-[enc] form must be rejected"
        );
    }

    #[test]
    fn raw_tap_preserves_the_encode_and_publish_settings() {
        // The wire output has to come out of the same encoder at the same
        // settings — the tap adds a branch, it does not re-tune the stream.
        let base = raw_tap_base(0);
        let cmd = augment_encoder_with_raw_tap(&base, RTSP_OUT, 10, 640, 480, "rgb24", SINK);

        for (flag, value) in [
            ("-c:v", "libx264"),
            ("-b:v", "4000k"),
            ("-g", "15"),
            ("-pix_fmt", "yuv420p"),
            ("-bsf:v", "h264_mp4toannexb,dump_extra=freq=keyframe"),
            ("-rtsp_transport", "tcp"),
            ("-f", "rtsp"),
        ] {
            assert!(
                has_pair(&base, flag, value) && has_pair(&cmd, flag, value),
                "{flag} {value} must survive the splice"
            );
        }
        // The publish destination is unchanged and is still the FIRST output
        // file: it precedes the tap's sink.
        let uri_at = cmd.iter().position(|t| t == RTSP_OUT).expect("publish URI");
        let sink_at = cmd.iter().position(|t| t == SINK).expect("tap sink");
        assert!(
            uri_at < sink_at,
            "publish output comes before the tap output"
        );
        // The tap's own output file: raw frames at the requested format.
        assert!(has_pair(&cmd, "-pix_fmt", "rgb24"));
        assert!(has_pair(&cmd, "-f", "rawvideo"));
        assert_eq!(cmd.last().unwrap(), SINK);
        // And the throttle/scale the tap asked for reached the graph.
        let fc = cmd.iter().position(|t| t == "-filter_complex").unwrap();
        assert!(cmd[fc + 1].contains("fps=10"));
        assert!(cmd[fc + 1].contains("scale=640:480"));
    }

    #[test]
    fn raw_tap_folds_the_orientation_transform_into_the_graph() {
        // `-vf` is a SIMPLE filtergraph and ffmpeg refuses it on a stream fed
        // by a complex one ("-vf/-filter_complex cannot be used together for
        // the same stream"), so a rotated camera would abort the encoder. The
        // transform moves inside the split instead, ahead of the fan-out, which
        // also means the vision tap sees the same orientation as the operator.
        let base = raw_tap_base(90);
        assert!(has_pair(&base, "-vf", "transpose=1"), "base carries -vf");

        let cmd = augment_encoder_with_raw_tap(&base, RTSP_OUT, 10, 640, 480, "rgb24", SINK);
        assert!(
            !cmd.iter().any(|t| t == "-vf"),
            "no -vf may survive beside -filter_complex: {cmd:?}"
        );
        let fc = cmd.iter().position(|t| t == "-filter_complex").unwrap();
        let graph = &cmd[fc + 1];
        let rot = graph
            .find("transpose=1")
            .expect("transform is in the graph");
        let split = graph.find("split=2").expect("split is in the graph");
        assert!(rot < split, "the transform feeds the split: {graph}");
        graph_termination_check(&cmd).expect("rotated graph must be fully terminated");
    }

    #[test]
    fn raw_tap_is_a_noop_on_the_radio_fanout_form() {
        // With the fan-out in force the last token is the tee spec, not the
        // output URI, so the splice refuses to touch the command rather than
        // corrupting an output stage it does not understand. A node that wants
        // the pre-encode vision split therefore MUST be built without the
        // fan-out — which is exactly what the orchestrator does.
        let base = build_encoder_command(
            &params(EncoderKind::Ffmpeg, 1280, 720, 30, 4000),
            "/dev/video1",
            RTSP_OUT,
            Some(&usb_mjpeg()),
            &rockchip(),
        )
        .unwrap();
        assert!(base.last().unwrap().starts_with("[f=rtsp:"), "fan-out form");
        let augmented = augment_encoder_with_raw_tap(
            &base,
            RTSP_OUT,
            10,
            640,
            480,
            "rgb24",
            "/run/ados/vision-tap-main.sock",
        );
        assert_eq!(augmented, base);
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
        assert_eq!(sync.python_executable, async_env.python_executable);
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
        let cmd = build(
            &params(EncoderKind::Ffmpeg, 1280, 720, 30, 4000),
            "/dev/video1",
            RTSP_OUT,
            &usb_mjpeg(),
            &rockchip(),
            true,
        );
        assert_eq!(cmd[0], "bash");
        let body = &cmd[2];
        let (encode, publish) = body
            .split_once("| /opt/ados/venv/bin/python3 -m ados.services.video.sei_injector |")
            .expect("the injector is spliced between two ffmpeg stages");
        // The encode half ends at stdout and carries no output-muxer leftovers.
        assert!(encode.trim_end().ends_with("-f h264 -"));
        assert!(
            !encode.contains("-f tee"),
            "no muxer left on the encode half"
        );
        assert!(!encode.contains("-map 0:v"));
        assert!(!encode.contains("-rtsp_transport"));
        assert!(!encode.contains("-muxdelay"));
        // The publish half re-mounts BOTH destinations, through the same
        // output-stage builder the non-SEI path uses.
        assert!(publish.contains("-f tee"));
        assert!(publish.contains(RTSP_OUT));
        assert!(publish.contains(&crate::wfb_tee::rtp_destination_url()));
        assert!(crate::wfb_tee::cmd_emits_wfb_rtp(&cmd));
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
}
