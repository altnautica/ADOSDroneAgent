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

use crate::config::CameraConfig;

mod detect;
mod env;
mod ffmpeg;
mod gstreamer;
mod rpicam;
mod sei;
mod tap_splice;

pub use detect::{binary_present, detect_encoder_for_camera, encoder_is_hardware, encoder_label};
pub use env::{soc_is_pi5_class, EncoderEnv};
pub use rpicam::input_queue_depth;
pub use sei::{wrap_with_sei_inject, SeiSplit, SEI_STAGE_TOKEN};

use ffmpeg::build_ffmpeg_command;
use gstreamer::build_gstreamer_command;
use rpicam::build_rpicam_command;

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
    /// A camera the vision engine captures, read back off the frame bus as raw
    /// video through its `framebus-<id>.fifo` pipe.
    Framebus,
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
    /// `video.camera.passthrough`: on a network source, copy the camera's
    /// H.264 as-is (`-c:v copy`) instead of decoding and re-encoding it. The
    /// camera's own GOP and bitrate then reach the radio unchanged, which is
    /// why it is opt-in. Ignored for local devices.
    pub passthrough: bool,
    /// The raw frames a `framebus://` source delivers. Required for that source
    /// (the encoder reads headerless raw video) and ignored otherwise. Its
    /// geometry is the captured frame's; `width`/`height`/`fps` above stay the
    /// encoded output's, so an attention switch rescales instead of misreading
    /// the pipe.
    pub framebus_input: Option<RawInput>,
}

/// The format and geometry of the raw frames a `framebus://` source carries:
/// what the vision engine publishes for that camera.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawInput {
    pub format: ados_protocol::framebus::FrameFormat,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
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
            passthrough: cfg.passthrough,
            framebus_input: None,
        }
    }
}

/// Allowlist for camera source / output strings: alphanumeric, slashes, dots,
/// hyphens, underscores, colons. `-` (stdin/stdout) is allowed verbatim. A
/// `framebus://<id>` source passes as is; the builder turns it into the pipe.
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
    /// A `framebus://` source was given without the raw frame format the
    /// encoder must read it in.
    MissingFramebusFormat(String),
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
            EncoderError::MissingFramebusFormat(s) => write!(
                f,
                "Frame-bus source {s:?} has no raw frame format; the camera's \
                 cameras[].format must reach the encoder."
            ),
        }
    }
}

impl std::error::Error for EncoderError {}

/// The longest GOP (keyframe interval, in frames) any encoder arm is given. At
/// the default 30 fps that is one second, the most a radio FEC recovery or a
/// late joiner should ever wait for the next keyframe.
pub const MAX_GOP_FRAMES: u32 = 30;

/// The effective GOP (keyframe) interval for a camera, always in
/// `1..=MAX_GOP_FRAMES`. An explicit `keyframe_interval` (frames) wins; the
/// default 0 yields a short low-latency GOP of half a second at the configured
/// fps so radio FEC recovers fast.
fn gop_interval(params: &EncoderParams) -> u32 {
    let gop = if params.keyframe_interval > 0 {
        params.keyframe_interval
    } else {
        params.fps / 2
    };
    gop.clamp(1, MAX_GOP_FRAMES)
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
///
/// A network source never takes the OMX path: that pipeline starts with
/// `v4l2src`, which cannot open a URL, so it keeps the probed base kind.
///
/// A frame-bus source on a Rockchip board with `mpph264enc` takes the
/// GStreamer VPU path. The frames arrive as raw video the agent already holds,
/// so the encoder is fed NV12 with no RGA stage — the configuration the retest
/// procedure on [`detect_encoder_for_camera`] calls for. The software fallback
/// still applies: once the health tick has forced `encoder: "software"`, the
/// leg keeps its probed base kind and a software codec.
fn resolve_kind(
    base: EncoderKind,
    params: &EncoderParams,
    env: &EncoderEnv,
    network_source: bool,
    framebus: bool,
) -> EncoderKind {
    let omx =
        !network_source && !framebus && env.encoder_api == "vendor" && env.has_omxh264videoenc;
    match params.encoder.as_str() {
        "v4l2m2m" => EncoderKind::Ffmpeg,
        "software" => base,
        _ if framebus && env.has_mpph264enc => EncoderKind::Gstreamer,
        _ if omx => EncoderKind::Gstreamer,
        _ => base,
    }
}

/// A `framebus://<id>` source resolved to the pipe its feed writes and the raw
/// frames in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FramebusInput {
    pub fifo: String,
    pub raw: RawInput,
}

/// The input an encoder for `source` actually opens: the frame-bus pipe for a
/// `framebus://` source, else the source itself. Orphan sweeps match on it.
pub fn input_locator(source: &str) -> String {
    match ados_config::cameras::framebus_camera_id(source) {
        Some(id) => crate::framebus_input::fifo_path(id)
            .to_string_lossy()
            .into_owned(),
        None => source.to_string(),
    }
}

/// Build the full argv vector for the given encoder configuration.
///
/// Returns the program plus its arguments. For the bash-pipeline cases (rpicam
/// → RTSP) the returned vector is `["bash", "-c", "<pipeline>"]`, exactly as
/// the predecessor composes it.
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
    let framebus = match ados_config::cameras::framebus_camera_id(source) {
        Some(_) => Some(FramebusInput {
            fifo: input_locator(source),
            raw: params
                .framebus_input
                .ok_or_else(|| EncoderError::MissingFramebusFormat(source.to_string()))?,
        }),
        None => None,
    };
    // Apply the per-camera encoder override + board HAL encoder_api before
    // dispatching (builder-private; the probed base kind stays on `params.kind`).
    let kind = resolve_kind(
        params.kind,
        params,
        env,
        crate::config::is_network_url(source),
        framebus.is_some(),
    );
    let cmd = match kind {
        EncoderKind::RpicamVid => build_rpicam_command(params, source, output, env),
        EncoderKind::Ffmpeg => {
            build_ffmpeg_command(params, source, output, camera, env, framebus.as_ref())
        }
        EncoderKind::Gstreamer => {
            build_gstreamer_command(params, source, output, camera, env, framebus.as_ref())
        }
    };
    Ok(cmd)
}

#[cfg(test)]
mod tests;
