//! Forwards the vision engine's broadcast streams to the plugin bus as
//! host-only topics.
//!
//! Each [`Stream`] is one engine broadcast socket of 4-byte big-endian
//! length-prefixed msgpack records. This module reads it and publishes each
//! record that decodes under its contract, unchanged, as a host event on the
//! stream's topic:
//!
//! - [`DEPTH_FRAMES`]: `vision-depth-frames.sock` → `vision.depth.v2`
//!   ([`ados_protocol::vision_depth_frame`]), read with `vision.depth.read`;
//! - [`SUBJECT`]: `vision-subject.sock` → `vision.subject.v1`
//!   ([`ados_protocol::vision_subject`]), read with `vision.subject.read`.
//!
//! A plugin reads a topic with `event.subscribe` plus its capability
//! (`crate::handlers::HOST_VISION_TOPICS`); no plugin can publish one.
//!
//! **Size.** Every record travels whole as one event, so each must fit the
//! event payload cap ([`EVENT_PAYLOAD_MAX_BYTES`], 64 KiB): a depth frame is
//! about 48 KiB, a subject state under 4 KiB. A larger record is refused,
//! never split.
//!
//! **Back-pressure.** The bus is a broadcast channel: publishing never waits
//! on a subscriber, and a plugin that drains slower than a stream loses its
//! oldest events (the bus lags it) while every other subscriber, and the
//! engine behind this reader, carry on. The engine's broadcast server drops
//! this reader if it ever falls behind, and it then reconnects.
//!
//! An absent socket is normal (vision off, no model, no subject): the reader
//! retries quietly every [`RETRY`].

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use ados_protocol::ipc::read_length_prefixed;
use ados_protocol::vision_depth_frame::{DepthFrame, FRAME_MAX_BYTES, VISION_DEPTH_FRAME_TOPIC};
use ados_protocol::vision_subject::{SubjectState, SUBJECT_MAX_BYTES, VISION_SUBJECT_TOPIC};
use tokio::net::UnixStream;
use tokio::task::JoinHandle;

use crate::handlers::{EventBus, EVENT_PAYLOAD_MAX_BYTES};
use crate::vehicle_events::publish_host_event;

/// One engine broadcast forwarded to one bus topic.
#[derive(Debug, Clone, Copy)]
pub struct Stream {
    /// The socket's file name under the run directory.
    pub socket: &'static str,
    pub topic: &'static str,
    /// The largest record the contract allows.
    pub max_bytes: usize,
    /// Whether a record decodes under the contract.
    pub valid: fn(&[u8]) -> bool,
}

/// The engine's metric depth frames.
pub const DEPTH_FRAMES: Stream = Stream {
    socket: "vision-depth-frames.sock",
    topic: VISION_DEPTH_FRAME_TOPIC,
    max_bytes: FRAME_MAX_BYTES,
    valid: |b| DepthFrame::from_msgpack(b).is_ok(),
};

/// The designated subject's world state (last-state: a new reader gets the
/// latest one at once).
pub const SUBJECT: Stream = Stream {
    socket: "vision-subject.sock",
    topic: VISION_SUBJECT_TOPIC,
    max_bytes: SUBJECT_MAX_BYTES,
    valid: |b| SubjectState::from_msgpack(b).is_ok(),
};

/// Every forwarded stream.
pub const STREAMS: [Stream; 2] = [DEPTH_FRAMES, SUBJECT];

/// Pause between reconnect attempts.
pub const RETRY: Duration = Duration::from_secs(2);

/// The decoded event payload of one record of `stream`, or `None` when it
/// does not decode under the contract or would not fit one event.
pub fn payload(stream: &Stream, body: &[u8]) -> Option<rmpv::Value> {
    if body.len() > EVENT_PAYLOAD_MAX_BYTES || !(stream.valid)(body) {
        return None;
    }
    rmpv::decode::read_value(&mut &body[..]).ok()
}

/// Read `conn` until it closes, publishing every valid record on `bus`.
/// Returns the number published.
async fn forward(bus: &EventBus, stream: &Stream, mut conn: UnixStream) -> u64 {
    let mut published = 0;
    loop {
        match read_length_prefixed(&mut conn, stream.max_bytes, true).await {
            Ok(Some(body)) => match payload(stream, &body) {
                Some(p) => {
                    publish_host_event(bus, stream.topic, p);
                    published += 1;
                }
                None => tracing::debug!(
                    topic = stream.topic,
                    len = body.len(),
                    "vision_topic_refused"
                ),
            },
            Ok(None) => return published,
            Err(e) => {
                tracing::debug!(topic = stream.topic, error = %e, "vision_topic_read_failed");
                return published;
            }
        }
    }
}

/// Forward `stream` from `run_dir` to `bus` forever, reconnecting while the
/// engine is absent or restarts.
pub fn spawn(bus: Arc<EventBus>, run_dir: &Path, stream: Stream) -> JoinHandle<()> {
    let path = run_dir.join(stream.socket);
    tokio::spawn(async move {
        loop {
            if let Ok(conn) = UnixStream::connect(&path).await {
                tracing::info!(topic = stream.topic, "vision_topic_connected");
                let n = forward(&bus, &stream, conn).await;
                tracing::info!(
                    topic = stream.topic,
                    records = n,
                    "vision_topic_disconnected"
                );
            }
            tokio::time::sleep(RETRY).await;
        }
    })
}

#[cfg(test)]
mod tests;
