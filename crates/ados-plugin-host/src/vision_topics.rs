//! Forwards the vision engine's broadcast streams to the plugin bus as
//! host-only topics.
//!
//! Each [`Stream`] is one engine broadcast socket of 4-byte big-endian
//! length-prefixed msgpack records. This module reads it and publishes each
//! record that decodes under its contract as a host event on the stream's
//! topic, unchanged unless the stream says otherwise:
//!
//! - [`DEPTH_FRAMES`]: `vision-depth-frames.sock` → `vision.depth.v2`
//!   ([`ados_protocol::vision_depth_frame`]), read with `vision.depth.read`;
//! - [`SUBJECT`]: `vision-subject.sock` → `vision.subject.v1`
//!   ([`ados_protocol::vision_subject`]), read with `vision.subject.read`;
//! - [`DETECTIONS`]: `vision-detections.sock` → `vision.detection`
//!   ([`ados_protocol::framebus::DetectionBatch`]), read with
//!   `vision.detection.subscribe`. Only the engine's own batches travel: a
//!   batch of a plugin's model ([`is_owned_model_id`]), which is every batch
//!   a plugin publishes, is refused, so no plugin can place a person or a
//!   vehicle the pilot avoids; so is a batch older than
//!   [`DETECTION_MAX_AGE_MS`] (the socket's last batch, replayed to a reader
//!   that connects). Each detection travels without the fields in
//!   [`DETECTION_DROPPED`]; a plugin that needs them takes the batch whole
//!   through `vision.subscribe_detections`. The pilot places the people and
//!   vehicles the tracker sees from this topic.
//!
//! A plugin reads a topic with `event.subscribe` plus its capability
//! (`crate::handlers::HOST_VISION_TOPICS`); no plugin can publish one.
//!
//! **Size.** Every record travels whole as one event, so each must fit the
//! event payload cap ([`EVENT_PAYLOAD_MAX_BYTES`], 64 KiB) as forwarded: a
//! depth frame is about 48 KiB, a subject state under 4 KiB, a detection
//! batch without its embeddings about 200 bytes a detection. A larger record
//! is refused, never split.
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

use ados_protocol::framebus::{mono_ns, VISION_DETECTION_TOPIC, VISION_DETECTION_VERSION};
use ados_protocol::ipc::read_length_prefixed;
use ados_protocol::state::STATE_V2_MAX_FRAME;
use ados_protocol::vision_depth_frame::{DepthFrame, FRAME_MAX_BYTES, VISION_DEPTH_FRAME_TOPIC};
use ados_protocol::vision_rpc::is_owned_model_id;
use ados_protocol::vision_subject::{SubjectRecord, SUBJECT_MAX_BYTES, VISION_SUBJECT_TOPIC};
use rmpv::Value;
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
    /// The record as forwarded, from its parsed value: `None` when it does
    /// not decode under the contract or is not fit to forward.
    pub shape: fn(Value) -> Option<Value>,
}

/// The engine's metric depth frames.
pub const DEPTH_FRAMES: Stream = Stream {
    socket: "vision-depth-frames.sock",
    topic: VISION_DEPTH_FRAME_TOPIC,
    max_bytes: FRAME_MAX_BYTES,
    shape: |v| DepthFrame::from_value(&v).is_ok().then_some(v),
};

/// The longest a subject state may have gone without a measurement and
/// still be forwarded, ms. An older one is a stale position (a coasting
/// estimate, or a last state the engine kept past its subject); consumers
/// see the subject as lost instead.
pub const SUBJECT_MAX_FIX_AGE_MS: u32 = 2_000;

/// The designated subject's world state (last-state: a new reader gets the
/// latest one at once). A released record always passes; a state only while
/// its fix is at most [`SUBJECT_MAX_FIX_AGE_MS`] old.
pub const SUBJECT: Stream = Stream {
    socket: "vision-subject.sock",
    topic: VISION_SUBJECT_TOPIC,
    max_bytes: SUBJECT_MAX_BYTES,
    shape: |v| {
        let fit = match SubjectRecord::from_value(&v) {
            Ok(SubjectRecord::State(s)) => s.fix_age_ms <= SUBJECT_MAX_FIX_AGE_MS,
            Ok(SubjectRecord::Released(_)) => true,
            Err(_) => false,
        };
        fit.then_some(v)
    },
};

/// Every tracker batch the engine publishes of its own models (last-state),
/// at most [`DETECTION_MAX_AGE_MS`] old, each detection without the fields
/// in [`DETECTION_DROPPED`].
pub const DETECTIONS: Stream = Stream {
    socket: "vision-detections.sock",
    topic: VISION_DETECTION_TOPIC,
    // The engine frames a batch under the state-frame cap.
    max_bytes: STATE_V2_MAX_FRAME,
    shape: |v| detection_batch(v, mono_ns() / 1_000_000),
};

/// The oldest a tracker batch may be and still be forwarded: its frame's
/// capture (`ts_ms`, host monotonic) at most this long ago, ms. The socket
/// hands a reader that connects the last batch however old it is; its
/// people are no longer where it says, and the pilot would place them from
/// where the vehicle is now.
pub const DETECTION_MAX_AGE_MS: u64 = 2_000;

/// Per-detection fields a forwarded batch leaves out: the appearance
/// embeddings riding in `attributes` (2-5 KiB a person, so a crowd's batch
/// would not fit one event), the segmentation outline, the keypoints and the
/// appearance model's id.
pub const DETECTION_DROPPED: [&str; 4] = ["attributes", "mask", "keypoints", "appearance_id"];

/// Every forwarded stream.
pub const STREAMS: [Stream; 3] = [DEPTH_FRAMES, SUBJECT, DETECTIONS];

/// Pause between reconnect attempts.
pub const RETRY: Duration = Duration::from_secs(2);

fn field<'a>(map: &'a [(Value, Value)], key: &str) -> Option<&'a Value> {
    map.iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .map(|(_, v)| v)
}

/// A `vision.detection` batch of this build's version from one of the
/// engine's own models, naming its camera, taken at most
/// [`DETECTION_MAX_AGE_MS`] before `now_ms` (host monotonic), whose
/// detections are maps, with [`DETECTION_DROPPED`] taken off each.
fn detection_batch(v: Value, now_ms: u64) -> Option<Value> {
    let Value::Map(mut batch) = v else {
        return None;
    };
    let version = field(&batch, "v")?.as_u64()?;
    let model = field(&batch, "model_id")?.as_str()?;
    let taken_ms = field(&batch, "ts_ms")?.as_u64()?;
    if version != u64::from(VISION_DETECTION_VERSION)
        || is_owned_model_id(model)
        || now_ms.saturating_sub(taken_ms) > DETECTION_MAX_AGE_MS
        || !field(&batch, "camera_id")?.is_str()
    {
        return None;
    }
    let (_, Value::Array(dets)) = batch
        .iter_mut()
        .find(|(k, _)| k.as_str() == Some("detections"))?
    else {
        return None;
    };
    for d in dets {
        let Value::Map(fields) = d else {
            return None;
        };
        fields.retain(|(k, _)| !k.as_str().is_some_and(|k| DETECTION_DROPPED.contains(&k)));
    }
    Some(Value::Map(batch))
}

/// Bytes written, none kept: a value's encoded size.
struct Count(usize);

impl std::io::Write for Count {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0 += buf.len();
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The event payload of one record of `stream`, or `None` when it does not
/// decode under the contract or would not fit one event as forwarded. The
/// record is parsed once; the contract check and the shaping run on the
/// parsed value.
pub fn payload(stream: &Stream, body: &[u8]) -> Option<Value> {
    let value = (stream.shape)(rmpv::decode::read_value(&mut &body[..]).ok()?)?;
    // Shaping only takes fields off: a record that fit as it came fits.
    let fits = body.len() <= EVENT_PAYLOAD_MAX_BYTES || {
        let mut n = Count(0);
        rmpv::encode::write_value(&mut n, &value).is_ok() && n.0 <= EVENT_PAYLOAD_MAX_BYTES
    };
    fits.then_some(value)
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
