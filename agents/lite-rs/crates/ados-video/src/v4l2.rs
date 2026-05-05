//! V4L2-backed H.264 encoder for boards that expose a hardware encoder
//! via `/dev/video*`. Target: Pi Zero 2 W with libcamera + the on-SoC
//! H.264 encoder (Broadcom VideoCore IV).
//!
//! This module ships interface-only stubs today. The hardware-driven
//! work (open device, set controls, dequeue buffers) lands once a
//! Pi Zero 2 W with a CSI camera is in hand. Until then every method
//! returns [`EncoderError::NotImplemented`] so callers exercising the
//! trait get a typed error rather than a panic.
//!
//! The module is gated on `target_os = "linux"` so non-Linux developer
//! hosts (e.g. macOS) can still build the workspace. The non-Linux stub
//! provides the same surface but always reports the platform mismatch.

use crate::{EncodedFrame, Encoder, EncoderConfig, EncoderError};

/// V4L2 encoder facade. The real implementation will hold the open
/// device handle, the configured control state, and the buffer queue.
#[derive(Debug, Default)]
pub struct V4l2Encoder {
    // Tracks whether `start()` has been called, so a second `start()`
    // before `stop()` reports `AlreadyStarted` instead of silently
    // re-initializing the device. Only read on Linux for now (the
    // non-Linux stub short-circuits to NotImplemented before touching
    // the field).
    #[allow(dead_code)]
    started: bool,
}

impl V4l2Encoder {
    /// Build a fresh facade. No system resources are acquired until
    /// `start()` is called.
    pub fn new() -> Self {
        Self::default()
    }
}

#[cfg(target_os = "linux")]
#[async_trait::async_trait]
impl Encoder for V4l2Encoder {
    async fn start(&mut self, _config: EncoderConfig) -> Result<(), EncoderError> {
        if self.started {
            return Err(EncoderError::AlreadyStarted);
        }
        // TODO(hardware bringup): open the V4L2 capture + encoder
        // device (typically `/dev/video10` on Raspberry Pi for the
        // H.264 encoder, plus a separate capture device for the
        // camera); negotiate format via VIDIOC_S_FMT; apply bitrate via
        // V4L2_CID_MPEG_VIDEO_BITRATE; apply GOP via
        // V4L2_CID_MPEG_VIDEO_H264_I_PERIOD; allocate output buffers
        // via VIDIOC_REQBUFS + VIDIOC_QUERYBUF + mmap; feed capture
        // frames into the encoder; spawn a poll loop that drains
        // encoded NAL units off VIDIOC_DQBUF and pushes them onto an
        // mpsc channel drained by `next_frame`.
        tracing::debug!(
            width = _config.width,
            height = _config.height,
            fps = _config.fps,
            "v4l2 encoder start called (stub)"
        );
        Err(EncoderError::NotImplemented)
    }

    async fn next_frame(&mut self) -> Option<EncodedFrame> {
        // TODO(hardware bringup): pull from the internal mpsc receiver
        // populated by the V4L2 dequeue loop. Annex-B start codes are
        // emitted by the kernel encoder for free; no extra
        // packetization is needed at this layer.
        None
    }

    async fn stop(&mut self) {
        // TODO(hardware bringup): VIDIOC_STREAMOFF on both queues,
        // drop mmap'd buffers, close the device. Idempotent.
        self.started = false;
    }
}

#[cfg(not(target_os = "linux"))]
#[async_trait::async_trait]
impl Encoder for V4l2Encoder {
    async fn start(&mut self, _config: EncoderConfig) -> Result<(), EncoderError> {
        // Non-Linux build target: V4L2 is a Linux kernel API. Fall through
        // to a typed not-implemented so workspace builds succeed on macOS
        // and Windows developer machines without breaking the trait
        // contract.
        Err(EncoderError::NotImplemented)
    }

    async fn next_frame(&mut self) -> Option<EncodedFrame> {
        None
    }

    async fn stop(&mut self) {
        // No state held on non-Linux.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn start_returns_not_implemented_at_stub_stage() {
        let mut enc = V4l2Encoder::new();
        let result = enc.start(EncoderConfig::default()).await;
        assert!(matches!(result, Err(EncoderError::NotImplemented)));
    }

    #[tokio::test]
    async fn next_frame_yields_none_when_idle() {
        let mut enc = V4l2Encoder::new();
        assert!(enc.next_frame().await.is_none());
    }

    #[tokio::test]
    async fn stop_is_idempotent() {
        let mut enc = V4l2Encoder::new();
        enc.stop().await;
        enc.stop().await;
    }
}
