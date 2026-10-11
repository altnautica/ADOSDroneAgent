//! The relay side of the cloud-publish socket: plugin data leaving the node.
//!
//! The plugin host applies the capability gate and forwards each admitted call
//! over `cloud-publish.sock` (the [`ados_protocol::cloud_publish`] contract).
//! This service owns the two things a publish needs and a plugin must never
//! hold: the broker session and the device's cloud key.
//!
//! * A **stream** message is queued and published at QoS 0 on
//!   `ados/{device}/plugin/{plugin_id}/{stream}` over this lane's own broker
//!   session (`ados-{device}-plugin-publish`). The stream
//!   [`VISION_DETECTIONS_STREAM`](wire::VISION_DETECTIONS_STREAM) is the one
//!   exception: its payload must be a JSON detection batch, and it goes to the
//!   core `ados/{device}/vision/detections` topic the ground station reads.
//! * A **record** is posted to `{convex}/agent/plugin-records` with the
//!   device's `X-ADOS-Key`, and the reply carries the cloud's own answer.
//!
//! ## Never blocking the listener
//!
//! The listener hands a stream message to a bounded [`StreamQueue`] and replies
//! at once. When the queue is full it evicts the OLDEST message and counts the
//! eviction: a live stream wants the newest data, and a stalled uplink must
//! never back up into the plugin host. A message is also dropped, and counted,
//! when the broker session is down at publish time rather than being held for
//! a reconnect. Records are bounded by a small number of in-flight posts; past
//! that the relay refuses with `busy` instead of queueing.
//!
//! Every node serves the socket, so a caller always gets an honest answer: a
//! local-only posture or an unpaired node refuses with the reason rather than
//! leaving the caller to guess from an absent socket.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ados_protocol::cloud_publish::{
    self as wire, CloudPublishKind, CloudPublishReply, CloudPublishRequest,
};
use ados_protocol::framebus::{DetectionBatch, VISION_DETECTION_VERSION};
use ados_protocol::ipc::{bind_command_socket, read_length_prefixed, OperatorListener};
use parking_lot::Mutex;
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::sync::{watch, Notify, Semaphore};

use crate::config::CloudConfig;
use crate::mqtt::transport::{MqttQos, MqttTransport, RumqttcTransport};
use crate::mqtt::{topic_plugin_stream, topic_vision_detections, PLUGIN_PUBLISH_LANE};
use crate::pairing::PairingState;

mod queue;
pub use queue::{PublishCounters, StreamQueue};

mod session;
use session::*;

/// Most stream messages held for the broker.
pub const STREAM_QUEUE_MESSAGES: usize = 256;

/// Most stream bytes held for the broker (topic plus payload). With the 64 KiB
/// payload cap this bounds the queue well inside the service's memory budget.
pub const STREAM_QUEUE_BYTES: usize = 2 * 1024 * 1024;

/// Record posts allowed in flight at once. Past this a record is refused with
/// `busy`: the caller learns now instead of waiting behind a slow cloud.
pub const RECORD_POSTS_IN_FLIGHT: usize = 4;

/// A connection that sends no request for this long is closed.
const CONNECTION_IDLE: Duration = Duration::from_secs(30);

/// Bound on writing one reply.
const REPLY_WRITE_TIMEOUT: Duration = Duration::from_secs(2);

/// How often the pairing state is re-read for the stream gate.
const PAIRING_RECHECK: Duration = Duration::from_secs(5);

/// How long a freshly opened broker session is given to come up before a
/// message waiting on it is dropped. An established session that is down drops
/// at once: waiting would only publish stale data later.
const CONNECT_WAIT: Duration = Duration::from_secs(5);

/// How often the lane's counters are logged, when any moved.
const STATS_INTERVAL: Duration = Duration::from_secs(60);

/// Pause after a failed `accept()` so a persistent error cannot spin.
const ACCEPT_RETRY_BACKOFF: Duration = Duration::from_millis(200);

/// Longest refusal reason sent back to the caller.
const MAX_REASON_CHARS: usize = 512;

/// One stream message bound for the broker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamMessage {
    pub topic: String,
    pub payload: Vec<u8>,
}

impl StreamMessage {
    fn cost(&self) -> usize {
        self.topic.len() + self.payload.len()
    }
}

/// Map an admitted stream request to its broker message: the plugin's own
/// subtree, or the core vision-detection topic for
/// [`wire::VISION_DETECTIONS_STREAM`], whose payload must be a detection batch
/// in the `vision.detection` contract's JSON form. That payload is re-encoded
/// from the typed batch, so the core topic only ever carries the shape its
/// readers parse.
pub fn stream_message(device_id: &str, req: &CloudPublishRequest) -> Result<StreamMessage, String> {
    if device_id.is_empty() {
        return Err("this node has no device id".into());
    }
    let stream = req
        .stream
        .as_deref()
        .ok_or("a stream request needs a stream name")?;
    if stream == wire::VISION_DETECTIONS_STREAM {
        let batch: DetectionBatch = serde_json::from_slice(&req.payload)
            .map_err(|e| format!("{stream} needs a JSON detection batch: {e}"))?;
        if batch.v != VISION_DETECTION_VERSION {
            return Err(format!(
                "{stream} batch version {} is not {VISION_DETECTION_VERSION}",
                batch.v
            ));
        }
        let payload = serde_json::to_vec(&batch).map_err(|e| e.to_string())?;
        return Ok(StreamMessage {
            topic: topic_vision_detections(device_id),
            payload,
        });
    }
    Ok(StreamMessage {
        topic: topic_plugin_stream(device_id, &req.plugin_id, stream),
        payload: req.payload.clone(),
    })
}

/// The `/agent/plugin-records` body for an admitted record request. The poster
/// is this node, proven by its key; the subject defaults to this node too.
pub fn record_body(poster_device_id: &str, req: &CloudPublishRequest) -> Result<Value, String> {
    if poster_device_id.is_empty() {
        return Err("this node has no device id".into());
    }
    let (Some(collection), Some(key)) = (req.collection.as_deref(), req.key.as_deref()) else {
        return Err("a record request needs a collection and a key".into());
    };
    let data: Value = serde_json::from_slice(&req.payload)
        .map_err(|e| format!("record payload is not JSON: {e}"))?;
    Ok(json!({
        "posterDeviceId": poster_device_id,
        "pluginId": req.plugin_id,
        "collection": collection,
        "key": key,
        "data": data,
        "deviceId": req.device_id.as_deref().unwrap_or(poster_device_id),
    }))
}

/// POST one record body. `Ok` only on a 2xx; otherwise the reason, carrying the
/// cloud's own `error` text when it sent one.
pub async fn post_record(
    client: &reqwest::Client,
    convex_url: &str,
    api_key: &str,
    body: &Value,
) -> Result<(), String> {
    let url = format!("{}/agent/plugin-records", convex_url.trim_end_matches('/'));
    let resp = client
        .post(&url)
        .header("X-ADOS-Key", api_key)
        .json(body)
        .send()
        .await
        .map_err(|e| format!("cloud unreachable: {e}"))?;
    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    let detail = resp
        .json::<Value>()
        .await
        .ok()
        .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_string));
    Err(match detail {
        Some(error) => format!("cloud refused the record ({}): {error}", status.as_u16()),
        None => format!("cloud refused the record ({})", status.as_u16()),
    })
}

/// Where the record path gets the device key. Production reads the pairing
/// file per record; tests supply a fixed answer.
pub type KeySource = fn() -> Option<String>;

fn pairing_key() -> Option<String> {
    PairingState::load().api_key().map(str::to_string)
}

/// The shared state of one relay: the socket's connection tasks and the broker
/// publisher both hold it.
pub struct PluginPublishRelay {
    device_id: String,
    /// Whether this node's server posture has a broker at all.
    has_broker: bool,
    convex_url: String,
    http: Arc<reqwest::Client>,
    key_source: KeySource,
    /// The stream gate's view of the pairing state, refreshed by the publisher.
    paired: AtomicBool,
    queue: StreamQueue,
    record_slots: Semaphore,
    pub counters: PublishCounters,
}

impl PluginPublishRelay {
    pub fn new(
        device_id: String,
        has_broker: bool,
        convex_url: String,
        http: Arc<reqwest::Client>,
        key_source: KeySource,
    ) -> Self {
        let paired = key_source().is_some();
        Self {
            device_id,
            has_broker,
            convex_url,
            http,
            key_source,
            paired: AtomicBool::new(paired),
            queue: StreamQueue::new(STREAM_QUEUE_MESSAGES, STREAM_QUEUE_BYTES),
            record_slots: Semaphore::new(RECORD_POSTS_IN_FLIGHT),
            counters: PublishCounters::default(),
        }
    }

    /// The queue the publisher drains.
    pub fn queue(&self) -> &StreamQueue {
        &self.queue
    }

    /// Answer one decoded request.
    pub async fn handle(&self, req: &CloudPublishRequest) -> CloudPublishReply {
        match req.kind {
            CloudPublishKind::Stream => self.accept_stream(req),
            CloudPublishKind::Record => self.forward_record(req).await,
        }
    }

    fn accept_stream(&self, req: &CloudPublishRequest) -> CloudPublishReply {
        if !self.has_broker {
            return self.refuse("this node's server posture has no cloud broker");
        }
        if !self.paired.load(Ordering::Relaxed) {
            return self.refuse("cloud relay is not paired");
        }
        match stream_message(&self.device_id, req) {
            Ok(msg) => {
                self.queue.push(msg);
                PublishCounters::bump(&self.counters.streams_queued);
                CloudPublishReply::accepted()
            }
            Err(reason) => self.refuse(reason),
        }
    }

    async fn forward_record(&self, req: &CloudPublishRequest) -> CloudPublishReply {
        if self.convex_url.is_empty() {
            return self.refuse("this node's server posture has no cloud");
        }
        let Some(api_key) = (self.key_source)() else {
            return self.refuse("cloud relay is not paired");
        };
        let Ok(_slot) = self.record_slots.try_acquire() else {
            return self.refuse("busy: too many record writes in flight");
        };
        let body = match record_body(&self.device_id, req) {
            Ok(body) => body,
            Err(reason) => return self.refuse(reason),
        };
        match post_record(&self.http, &self.convex_url, &api_key, &body).await {
            Ok(()) => {
                PublishCounters::bump(&self.counters.records_written);
                CloudPublishReply::accepted()
            }
            Err(reason) => {
                PublishCounters::bump(&self.counters.records_failed);
                tracing::debug!(plugin = %req.plugin_id, reason = %reason, "plugin record not written");
                CloudPublishReply::refused(truncate(reason))
            }
        }
    }

    fn refuse(&self, reason: impl Into<String>) -> CloudPublishReply {
        PublishCounters::bump(&self.counters.refused);
        CloudPublishReply::refused(truncate(reason.into()))
    }

    /// Accept connections until `shutdown`. Each connection runs on its own
    /// task, so a slow peer never holds up another.
    pub async fn serve(
        self: Arc<Self>,
        listener: OperatorListener,
        mut shutdown: watch::Receiver<bool>,
    ) {
        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() { return; }
                }
                accepted = listener.accept() => match accepted {
                    Ok((stream, _)) => {
                        let relay = self.clone();
                        tokio::spawn(async move { relay.serve_connection(stream).await });
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "cloud-publish accept failed");
                        tokio::time::sleep(ACCEPT_RETRY_BACKOFF).await;
                    }
                },
            }
        }
    }

    /// Answer requests on one connection until it closes, idles out, or sends
    /// a frame that cannot be read (which desynchronises the stream, so the
    /// connection ends after the refusal).
    pub async fn serve_connection(&self, mut stream: UnixStream) {
        loop {
            let read = tokio::time::timeout(
                CONNECTION_IDLE,
                read_length_prefixed(&mut stream, wire::MAX_FRAME, true),
            )
            .await;
            let body = match read {
                Err(_) | Ok(Ok(None)) => return,
                Ok(Ok(Some(body))) => body,
                Ok(Err(e)) => {
                    let reply = self.refuse(format!("unreadable frame: {e}"));
                    let _ = write_reply(&mut stream, &reply).await;
                    return;
                }
            };
            let reply = match CloudPublishRequest::decode(&body) {
                Ok(req) => self.handle(&req).await,
                Err(e) => self.refuse(e.to_string()),
            };
            if write_reply(&mut stream, &reply).await.is_err() {
                return;
            }
        }
    }

    /// Drain the stream queue onto the broker until `shutdown`. The pairing key
    /// is re-read on a fixed cadence, not per message. The broker session is
    /// opened on the first message, re-opened when the key changes, and kept
    /// otherwise.
    pub async fn run_publisher(
        self: Arc<Self>,
        config: Arc<CloudConfig>,
        mut shutdown: watch::Receiver<bool>,
    ) {
        let mut session: Option<Session> = None;
        let mut key = (self.key_source)();
        let mut recheck = tokio::time::interval(PAIRING_RECHECK);
        let mut stats = tokio::time::interval(STATS_INTERVAL);
        let mut last_stats = self.counters.snapshot(0);
        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() { return; }
                }
                _ = recheck.tick() => {
                    key = (self.key_source)();
                    self.paired.store(key.is_some(), Ordering::Relaxed);
                }
                _ = stats.tick() => {
                    let now = self.counters.snapshot(self.queue.dropped_oldest());
                    if now != last_stats {
                        let [queued, published, dropped_oldest, dropped_offline, publish_failed, written, failed, refused] = now;
                        tracing::info!(
                            queued, published, dropped_oldest, dropped_offline, publish_failed,
                            records_written = written, records_failed = failed, refused,
                            "cloud-publish lane"
                        );
                        last_stats = now;
                    }
                }
                msg = self.queue.pop() => {
                    self.publish(&config, &mut session, key.as_deref(), msg).await;
                }
            }
        }
    }

    async fn publish(
        &self,
        config: &CloudConfig,
        session: &mut Option<Session>,
        key: Option<&str>,
        msg: StreamMessage,
    ) {
        let Some(key) = key else {
            PublishCounters::bump(&self.counters.streams_dropped_offline);
            return;
        };
        if session.as_ref().is_none_or(|s| s.key != key) {
            *session = open_session(config, key);
        }
        let Some(live) = session.as_ref() else {
            PublishCounters::bump(&self.counters.streams_dropped_offline);
            return;
        };
        if !live.transport.connected() {
            // A session still inside its first connect window is waited for;
            // one that was up and dropped is not.
            let deadline = live.opened + CONNECT_WAIT;
            while !live.transport.connected() && Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            if !live.transport.connected() {
                PublishCounters::bump(&self.counters.streams_dropped_offline);
                return;
            }
        }
        match live
            .transport
            .try_publish(&msg.topic, MqttQos::AtMostOnce, msg.payload)
        {
            Ok(()) => PublishCounters::bump(&self.counters.streams_published),
            Err(e) => {
                PublishCounters::bump(&self.counters.streams_publish_failed);
                tracing::trace!(error = %e, topic = %msg.topic, "cloud-publish broker queue full");
            }
        }
    }
}

/// Serve the cloud-publish socket and its broker publisher until `shutdown`.
/// Binds on every node and posture so a caller always gets an answer.
pub async fn run(
    config: Arc<CloudConfig>,
    http: Arc<reqwest::Client>,
    convex_url: String,
    shutdown: watch::Receiver<bool>,
) {
    let path = wire::socket_path();
    let listener = match bind_command_socket(&path, wire::SOCKET_MODE) {
        Ok(listener) => listener,
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "cloud-publish socket not bound");
            return;
        }
    };
    let relay = Arc::new(PluginPublishRelay::new(
        config.agent.device_id.clone(),
        config.mqtt_endpoint().is_some(),
        convex_url,
        http,
        pairing_key,
    ));
    tracing::info!(path = %path.display(), "cloud-publish socket serving");
    let publisher = tokio::spawn(relay.clone().run_publisher(config, shutdown.clone()));
    relay.serve(listener, shutdown).await;
    let _ = publisher.await;
    let _ = std::fs::remove_file(&path);
}

#[cfg(test)]
mod tests;
