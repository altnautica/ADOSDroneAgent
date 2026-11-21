//! Forwards the vision engine's metric depth frames to the plugin bus.
//!
//! The engine broadcasts every `vision.depth.v2` frame on
//! `vision-depth-frames.sock` (4-byte big-endian length-prefixed msgpack,
//! [`ados_protocol::vision_depth_frame`]). This task reads that socket and
//! publishes each frame that decodes, unchanged, as a host event on
//! [`VISION_DEPTH_FRAME_TOPIC`]. A plugin reads the topic with
//! `event.subscribe` plus `vision.depth.read`
//! (`crate::handlers::HOST_VISION_TOPICS`); no plugin can publish it.
//!
//! **Size.** A frame (two 128 x 96 `u16` planes, 48 KiB, plus a small
//! header) fits one event payload ([`crate::handlers::EVENT_PAYLOAD_MAX_BYTES`],
//! 64 KiB), so it travels whole as one event; a record over that cap is
//! refused here, never split.
//!
//! **Back-pressure.** The bus is a broadcast channel: publishing never waits
//! on a subscriber, and a plugin that drains slower than 5 Hz loses its
//! oldest frames (the bus lags it) while every other subscriber, and the
//! engine behind this reader, carry on. The engine's own broadcast server
//! drops this reader if it ever falls behind, and it then reconnects.
//!
//! An absent socket is normal (vision off, no depth model): the reader retries
//! quietly every [`RETRY`].

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ados_protocol::ipc::read_length_prefixed;
use ados_protocol::vision_depth_frame::{DepthFrame, FRAME_MAX_BYTES, VISION_DEPTH_FRAME_TOPIC};
use tokio::net::UnixStream;
use tokio::task::JoinHandle;

use crate::handlers::{EventBus, EVENT_PAYLOAD_MAX_BYTES};
use crate::vehicle_events::publish_host_event;

/// The engine's depth-frame broadcast, under the run directory.
pub const SOCKET_NAME: &str = "vision-depth-frames.sock";

/// Pause between reconnect attempts.
pub const RETRY: Duration = Duration::from_secs(2);

/// The decoded event payload of one broadcast record, or `None` when it is
/// not a valid depth frame or would not fit one event.
pub fn frame_payload(body: &[u8]) -> Option<rmpv::Value> {
    if body.len() > EVENT_PAYLOAD_MAX_BYTES {
        return None;
    }
    DepthFrame::from_msgpack(body).ok()?;
    rmpv::decode::read_value(&mut &body[..]).ok()
}

/// Read `path` until it closes, publishing every frame on `bus`. Returns the
/// number of frames published.
async fn forward(bus: &EventBus, mut stream: UnixStream) -> u64 {
    let mut published = 0;
    loop {
        match read_length_prefixed(&mut stream, FRAME_MAX_BYTES, true).await {
            Ok(Some(body)) => match frame_payload(&body) {
                Some(payload) => {
                    publish_host_event(bus, VISION_DEPTH_FRAME_TOPIC, payload);
                    published += 1;
                }
                None => tracing::debug!(len = body.len(), "vision_depth_frame_refused"),
            },
            Ok(None) => return published,
            Err(e) => {
                tracing::debug!(error = %e, "vision_depth_frames_read_failed");
                return published;
            }
        }
    }
}

/// Forward the depth frames at `path` to `bus` forever, reconnecting while
/// the engine is absent or restarts.
pub fn spawn(bus: Arc<EventBus>, path: PathBuf) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if let Ok(stream) = UnixStream::connect(&path).await {
                tracing::info!(path = %path.display(), "vision_depth_frames_connected");
                let n = forward(&bus, stream).await;
                tracing::info!(frames = n, "vision_depth_frames_disconnected");
            }
            tokio::time::sleep(RETRY).await;
        }
    })
}

#[cfg(test)]
mod tests;
