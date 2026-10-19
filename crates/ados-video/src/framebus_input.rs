//! The `framebus://<camera_id>` encoder input.
//!
//! A camera marked `capture: true` belongs to the vision engine, which is then
//! the only process that opens its device or URL. Its frames still reach the
//! encoders: the engine publishes every frame into a shared-memory ring and
//! announces it on `vision-frames.sock`; a [`FramebusFeed`] here subscribes to
//! those descriptors, reads each frame out of the ring and writes the raw
//! pixels into a named pipe, `framebus-<camera_id>.fifo` in the run dir. The
//! encoder argv reads that pipe as raw video (`-f rawvideo` for ffmpeg,
//! `filesrc ! rawvideoparse` for GStreamer), so a framebus leg runs through
//! the same encoder families, mediamtx paths and health ladders as any other.
//!
//! The feed writes whole frames only and starts a fresh frame boundary on every
//! open of the pipe, so an encoder that dies and respawns never reads a frame
//! split across two runs. It is latest-frame-wins: while the encoder is slower
//! than the camera, older descriptors are skipped rather than queued.

use std::path::PathBuf;

use ados_protocol::framebus::FrameFormat;

/// Run-dir file name of the named pipe a framebus leg's encoder reads.
pub fn fifo_path(camera_id: &str) -> PathBuf {
    ados_protocol::paths::run_dir().join(format!("framebus-{camera_id}.fifo"))
}

/// The vision engine's frame-descriptor broadcast socket.
pub fn frames_socket_path() -> PathBuf {
    ados_protocol::paths::run_dir().join("vision-frames.sock")
}

/// The ffmpeg `-pix_fmt` of a raw frame-bus format.
pub fn ffmpeg_pix_fmt(format: FrameFormat) -> &'static str {
    match format {
        FrameFormat::Rgb24 => "rgb24",
        FrameFormat::Nv12 => "nv12",
        FrameFormat::Yuv420p => "yuv420p",
        FrameFormat::Gray8 => "gray",
        FrameFormat::Gray16 => "gray16le",
        FrameFormat::BayerRggb8 => "bayer_rggb8",
        FrameFormat::BayerRggb16 => "bayer_rggb16le",
    }
}

/// The GStreamer `rawvideoparse format=` of a raw frame-bus format.
/// `rawvideoparse` has no Bayer formats, so a Bayer mosaic is parsed as the
/// grey image of the same byte layout: the preview shows the raw mosaic.
pub fn gst_raw_format(format: FrameFormat) -> &'static str {
    match format {
        FrameFormat::Rgb24 => "rgb",
        FrameFormat::Nv12 => "nv12",
        FrameFormat::Yuv420p => "i420",
        FrameFormat::Gray8 | FrameFormat::BayerRggb8 => "gray8",
        FrameFormat::Gray16 | FrameFormat::BayerRggb16 => "gray16-le",
    }
}

#[cfg(target_os = "linux")]
mod feed;
#[cfg(target_os = "linux")]
pub use feed::{FeedSpec, FramebusFeed};

/// The feeds a node runs: one per captured camera an encoder reads.
#[derive(Default)]
pub struct Feeds {
    #[cfg(target_os = "linux")]
    running: std::collections::HashMap<String, FramebusFeed>,
}

impl Feeds {
    /// Run exactly one feed per `(camera_id, raw input)` in `want`. A feed whose
    /// geometry changed is restarted; a feed no longer wanted stops.
    pub fn reconcile(&mut self, want: &[(String, crate::encoder::RawInput)]) {
        #[cfg(target_os = "linux")]
        {
            self.running.retain(|id, feed| {
                want.iter().any(|(w, raw)| {
                    let s = feed.spec();
                    w == id && (s.format, s.width, s.height) == (raw.format, raw.width, raw.height)
                })
            });
            for (id, raw) in want {
                if self.running.contains_key(id) {
                    continue;
                }
                match FramebusFeed::spawn(FeedSpec::new(id, raw.format, raw.width, raw.height)) {
                    Ok(feed) => {
                        tracing::info!(camera = %id, "framebus_feed_started");
                        self.running.insert(id.clone(), feed);
                    }
                    Err(e) => {
                        tracing::warn!(camera = %id, error = %e, "framebus_feed_start_failed")
                    }
                }
            }
        }
        #[cfg(not(target_os = "linux"))]
        if !want.is_empty() {
            tracing::warn!("framebus_feed_unavailable: the frame-bus input needs Linux");
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
#[path = "framebus_input/feed_tests.rs"]
mod feed_tests;
