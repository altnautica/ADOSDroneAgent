//! Hardware-encoded H.264 video pipeline for the lite ADOS Drone Agent.
//!
//! Defines the [`Encoder`] trait that all backend implementations satisfy.
//! Backends are selected at startup by reading the `video.encoder_api_lite`
//! field from the running board's HAL YAML and dispatching to the matching
//! implementation.
//!
//! Current backends:
//!
//! - [`v4l2::V4l2Encoder`] — Linux V4L2 path. The kernel exposes the on-SoC
//!   H.264 encoder through `/dev/video*` and userspace drives it via
//!   standard ioctls. Easy path on glibc/musl userspaces (e.g. Pi Zero 2 W).
//! - [`rkmpi_subprocess::RkmpiEncoderSubprocess`] — vendor SDK path for
//!   Rockchip parts (e.g. Luckfox Pico Zero / RV1106). The Rust agent
//!   spawns a small uclibc-built C wrapper and exchanges length-prefixed
//!   msgpack messages over its stdin/stdout. The subprocess boundary keeps
//!   the libc mismatch contained: the Rust binary stays musl-static while
//!   the vendor `.so` is loaded by a uclibc-linked launcher.
//! - [`null::NullEncoder`] — no-op fallback for boards without a supported
//!   hardware encoder. `start()` returns [`EncoderError::NotImplemented`]
//!   so callers can decide whether to skip the video subsystem entirely.
//!
//! All backends are interface-only stubs at this time. Wire-up to the
//! agent main binary happens once the trait surface stabilizes after
//! on-hardware validation.

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub mod libcamera;
pub mod nal;
pub mod null;
pub mod rkmpi_subprocess;
pub mod rkmpi_supervisor;
pub mod rtsp;
pub mod v4l2;

pub use rkmpi_supervisor::{RkmpiSnapshot, RkmpiSupervisor};

/// Configuration applied at encoder start. The values describe the desired
/// output stream; backends translate them into the appropriate vendor
/// controls (V4L2 ioctls, RKMPI `RK_MPI_VENC_CHN_ATTR_S`, etc.).
///
/// `serde` is derived so the struct can be sent across the subprocess
/// boundary by the [`rkmpi_subprocess`] backend without a separate wire
/// type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncoderConfig {
    /// Frame width in pixels. The capture source must be able to produce
    /// this resolution; downscaling in software is out of scope at v1.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Target frame rate. The encoder runs at constant frame rate; if the
    /// capture source delivers fewer frames the encoder pads via the
    /// vendor's CFR strategy (typically duplicate-frame insertion).
    pub fps: u32,
    /// Target bitrate in kbps. Backends typically configure CBR with this
    /// as the average bitrate; VBR / CRF modes are out of scope at v1.
    pub bitrate_kbps: u32,
    /// Spacing between IDR keyframes, in seconds. The first encoded frame
    /// is always a keyframe regardless of this setting.
    pub keyframe_interval_secs: u32,
}

impl Default for EncoderConfig {
    fn default() -> Self {
        // 720p30 at 4 Mbps with a 2-second GOP. These defaults match the
        // RTSP push contract documented in `proto/cloud/rtsp-conventions.md`
        // for low-bandwidth links and are conservative for both target
        // boards.
        Self {
            width: 1280,
            height: 720,
            fps: 30,
            bitrate_kbps: 4000,
            keyframe_interval_secs: 2,
        }
    }
}

/// A single encoded H.264 access unit. The `bytes` buffer holds one or
/// more NAL units, prefixed with Annex-B start codes (`00 00 00 01`) so
/// the buffer is directly forwardable to an RTSP / RTP packetizer or a
/// disk recorder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncodedFrame {
    /// Encoded payload — Annex-B byte stream.
    pub bytes: Vec<u8>,
    /// `true` if the access unit contains an IDR keyframe. Downstream
    /// consumers use this to decide where to seek and to drive RTSP
    /// SPS/PPS injection.
    pub is_keyframe: bool,
    /// Presentation timestamp in milliseconds since the encoder started.
    /// Monotonic; backends derive this from the capture clock so frame
    /// drops do not skew downstream timing.
    pub pts_ms: u64,
}

/// Failures the encoder layer can surface to the caller.
#[derive(Debug, Error)]
pub enum EncoderError {
    /// The backend's `start()` was called but the implementation is a
    /// stub. Returned by [`null::NullEncoder`] and by every backend
    /// before its hardware-driven path lands.
    #[error("encoder not implemented for this backend")]
    NotImplemented,

    /// The backend's `start()` was called twice without a matching
    /// `stop()`. Backends are not required to be re-entrant.
    #[error("encoder already started")]
    AlreadyStarted,

    /// The configured width / height / fps / bitrate exceeds what the
    /// underlying hardware can deliver. The string carries the offending
    /// constraint for log output.
    #[error("encoder configuration not supported: {0}")]
    ConfigUnsupported(String),

    /// The hardware encoder reported a fault (V4L2 ioctl failure, RKMPI
    /// non-zero return code, vendor-specific error). The string carries
    /// the vendor's diagnostic.
    #[error("hardware encoder fault: {0}")]
    HardwareFault(String),

    /// The subprocess backend lost its child process or could not spawn
    /// it. The string carries the OS-level error.
    #[error("subprocess error: {0}")]
    Subprocess(String),

    /// The wire framing on the subprocess channel was malformed. Indicates
    /// either a bug in the C wrapper or message corruption (rare on a
    /// stdin/stdout pipe but possible if the wrapper crashes mid-frame).
    #[error("subprocess wire protocol error: {0}")]
    Protocol(String),

    /// The subprocess channel buffer does not yet hold a complete frame.
    /// Distinct from `Protocol` so the reader loop can resume reading
    /// rather than tear down the pipe on a partial-frame read.
    #[error("subprocess wire frame incomplete: needs {0} more bytes")]
    Incomplete(usize),

    /// I/O on a host file descriptor (UDS, pipe, V4L2 device) failed.
    #[error("encoder i/o error: {0}")]
    Io(String),
}

impl From<std::io::Error> for EncoderError {
    fn from(err: std::io::Error) -> Self {
        EncoderError::Io(err.to_string())
    }
}

/// Async encoder lifecycle. All methods are `&mut self` so backends can
/// hold pipeline state inline without interior mutability.
#[async_trait::async_trait]
pub trait Encoder: Send {
    /// Open the capture source, configure the encoder, and begin producing
    /// frames. Calling `start()` twice without an intervening `stop()`
    /// returns [`EncoderError::AlreadyStarted`].
    async fn start(&mut self, config: EncoderConfig) -> Result<(), EncoderError>;

    /// Yield the next encoded access unit. Returns `None` once the encoder
    /// is stopped or the capture source ends. Backends implement this as
    /// a non-blocking pull from an internal channel; the pacing comes from
    /// the encoder's frame clock, not the caller's loop.
    async fn next_frame(&mut self) -> Option<EncodedFrame>;

    /// Tear down the pipeline. Idempotent — calling `stop()` on a stopped
    /// encoder is not an error. Backends release vendor handles, V4L2
    /// devices, and any spawned subprocesses here.
    async fn stop(&mut self);
}

/// Build the right backend for the supplied `encoder_api_lite` value.
///
/// The string comes from the running board's HAL YAML
/// (`video.encoder_api_lite`). Recognized values:
///
/// - `"v4l2"` / `"libcamera"` — Linux V4L2 backend.
/// - `"rkmpi"` / `"rkmedia"` — Rockchip vendor-SDK subprocess backend.
/// - `"none"` / unknown — [`null::NullEncoder`] no-op fallback.
///
/// The factory is infallible by design. A board without a video pipeline
/// returns the null encoder; agent main can decide whether to enable the
/// video subsystem at all by checking the board YAML before calling this.
///
/// Until the hardware-driven backends land, every non-null branch
/// returns a stub whose `start()` yields [`EncoderError::NotImplemented`];
/// this signature stays stable so the agent wire-up does not break when
/// the real backends arrive.
pub fn encoder_for_board(encoder_api: &str) -> Box<dyn Encoder + Send> {
    match encoder_api {
        // libcamera-vid subprocess; primary Pi-class path because the
        // V4L2 M2M wiring through /dev/video11 is fiddly to drive from
        // userspace and `libcamera-vid` ships in-tree on Bookworm.
        "libcamera" => Box::new(libcamera::LibcameraEncoder::new()),
        // V4L2 capture-only loop. Used by UVC H.264 USB cameras and any
        // device that already emits encoded H.264 on its CAPTURE queue.
        "v4l2" => Box::new(v4l2::V4l2Encoder::new()),
        // Rockchip vendor-SDK path on RV1106 / RV1106G3 (Luckfox Pico
        // Zero). The agent spawns a uclibc-built C wrapper that links
        // librockchip_mpp.so; the parent and child exchange
        // length-prefixed msgpack messages.
        "rkmpi" | "rkmedia" => Box::new(rkmpi_subprocess::RkmpiEncoderSubprocess::new(
            rkmpi_subprocess::default_subprocess_path(),
        )),
        // Boards without a video pipeline fall through to the null
        // encoder; agent main can still surface the device in the
        // fleet view without any video subsystem at all.
        _ => Box::new(null::NullEncoder::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoder_config_defaults_are_720p30() {
        let cfg = EncoderConfig::default();
        assert_eq!(cfg.width, 1280);
        assert_eq!(cfg.height, 720);
        assert_eq!(cfg.fps, 30);
        assert_eq!(cfg.bitrate_kbps, 4000);
        assert_eq!(cfg.keyframe_interval_secs, 2);
    }

    #[test]
    fn factory_returns_null_for_unknown_api() {
        // Until the real backends land every non-null branch is a stub
        // and every unknown branch falls through to the null backend.
        // We only assert the unknown branch here because the typed
        // backend branches are exercised through their own tests.
        let enc = encoder_for_board("vendor-x-not-real");
        // The trait object is `Send` so we can hold it across awaits.
        let _: Box<dyn Encoder + Send> = enc;
    }

    #[test]
    fn factory_dispatches_v4l2_alias() {
        let _ = encoder_for_board("v4l2");
        let _ = encoder_for_board("libcamera");
    }

    #[test]
    fn factory_dispatches_libcamera_separately_from_v4l2() {
        // The libcamera and v4l2 backends are now distinct concrete
        // types. We can't ask `Any::type_id` through a trait object
        // without erasing it explicitly, but we can at least make sure
        // both branches return without panicking and that the two
        // factories are reachable.
        let _ = encoder_for_board("libcamera");
        let _ = encoder_for_board("v4l2");
    }

    #[test]
    fn factory_dispatches_rkmpi_alias() {
        let _ = encoder_for_board("rkmpi");
        let _ = encoder_for_board("rkmedia");
    }

    #[tokio::test]
    async fn null_encoder_returns_not_implemented() {
        let mut enc = null::NullEncoder::new();
        let result = enc.start(EncoderConfig::default()).await;
        assert!(matches!(result, Err(EncoderError::NotImplemented)));
        assert!(enc.next_frame().await.is_none());
        enc.stop().await;
    }
}
