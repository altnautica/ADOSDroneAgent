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

/// A bounded, drop-oldest queue between the socket's connection tasks and the
/// broker publisher. `push` never waits beyond a short lock.
pub struct StreamQueue {
    inner: Mutex<QueueInner>,
    ready: Notify,
    max_messages: usize,
    max_bytes: usize,
    dropped_oldest: AtomicU64,
}

#[derive(Default)]
struct QueueInner {
    items: VecDeque<StreamMessage>,
    bytes: usize,
}

impl StreamQueue {
    pub fn new(max_messages: usize, max_bytes: usize) -> Self {
        Self {
            inner: Mutex::new(QueueInner::default()),
            ready: Notify::new(),
            max_messages,
            max_bytes,
            dropped_oldest: AtomicU64::new(0),
        }
    }

    /// Enqueue `msg`, evicting the oldest messages until it fits. Returns how
    /// many were evicted.
    pub fn push(&self, msg: StreamMessage) -> usize {
        let cost = msg.cost();
        let mut evicted = 0;
        {
            let mut q = self.inner.lock();
            while !q.items.is_empty()
                && (q.items.len() >= self.max_messages || q.bytes + cost > self.max_bytes)
            {
                if let Some(old) = q.items.pop_front() {
                    q.bytes -= old.cost();
                    evicted += 1;
                }
            }
            q.bytes += cost;
            q.items.push_back(msg);
        }
        if evicted > 0 {
            self.dropped_oldest
                .fetch_add(evicted as u64, Ordering::Relaxed);
        }
        self.ready.notify_one();
        evicted
    }

    /// Take the oldest message, if any.
    pub fn try_pop(&self) -> Option<StreamMessage> {
        let mut q = self.inner.lock();
        let msg = q.items.pop_front()?;
        q.bytes -= msg.cost();
        Some(msg)
    }

    /// Wait for the next message. Single consumer.
    pub async fn pop(&self) -> StreamMessage {
        loop {
            if let Some(msg) = self.try_pop() {
                return msg;
            }
            self.ready.notified().await;
        }
    }

    pub fn len(&self) -> usize {
        self.inner.lock().items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Messages evicted to make room since start.
    pub fn dropped_oldest(&self) -> u64 {
        self.dropped_oldest.load(Ordering::Relaxed)
    }
}

/// The lane's counters since start.
#[derive(Debug, Default)]
pub struct PublishCounters {
    pub streams_queued: AtomicU64,
    pub streams_published: AtomicU64,
    /// Dropped because the broker session was down when its turn came.
    pub streams_dropped_offline: AtomicU64,
    /// The broker client's own request queue was full.
    pub streams_publish_failed: AtomicU64,
    pub records_written: AtomicU64,
    pub records_failed: AtomicU64,
    /// Requests refused before any cloud work: bad frame, contract breach,
    /// unpaired, no broker, busy.
    pub refused: AtomicU64,
}

impl PublishCounters {
    fn bump(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    fn snapshot(&self, dropped_oldest: u64) -> [u64; 8] {
        let get = |c: &AtomicU64| c.load(Ordering::Relaxed);
        [
            get(&self.streams_queued),
            get(&self.streams_published),
            dropped_oldest,
            get(&self.streams_dropped_offline),
            get(&self.streams_publish_failed),
            get(&self.records_written),
            get(&self.records_failed),
            get(&self.refused),
        ]
    }
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

struct Session {
    key: String,
    transport: Arc<RumqttcTransport>,
    opened: Instant,
}

fn open_session(config: &CloudConfig, key: &str) -> Option<Session> {
    let cfg = config.relay_transport(Some(PLUGIN_PUBLISH_LANE), key)?;
    tracing::info!("cloud-publish broker lane connecting");
    match RumqttcTransport::connect(&cfg) {
        Ok(transport) => Some(Session {
            key: key.to_string(),
            transport,
            opened: Instant::now(),
        }),
        Err(e) => {
            tracing::warn!(error = %e, "cloud-publish broker lane not built");
            None
        }
    }
}

async fn write_reply(stream: &mut UnixStream, reply: &CloudPublishReply) -> std::io::Result<()> {
    let frame = reply
        .encode()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    tokio::time::timeout(REPLY_WRITE_TIMEOUT, stream.write_all(&frame))
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "reply write timed out"))?
}

fn truncate(mut reason: String) -> String {
    if let Some((cut, _)) = reason.char_indices().nth(MAX_REASON_CHARS) {
        reason.truncate(cut);
    }
    reason
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
mod tests {
    use super::*;
    use ados_protocol::framebus::{BoundingBox, Detection};
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpListener;

    fn http_client() -> Arc<reqwest::Client> {
        Arc::new(
            reqwest::Client::builder()
                .use_preconfigured_tls(crate::tls::client_config())
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
        )
    }

    fn paired() -> Option<String> {
        Some("device-key".into())
    }

    fn unpaired() -> Option<String> {
        None
    }

    fn relay(has_broker: bool, convex_url: &str, key_source: KeySource) -> PluginPublishRelay {
        PluginPublishRelay::new(
            "dev1".into(),
            has_broker,
            convex_url.into(),
            http_client(),
            key_source,
        )
    }

    fn batch_json(v: u16) -> Vec<u8> {
        let batch = DetectionBatch {
            v,
            model_id: "offload".into(),
            camera_id: "front".into(),
            frame_id: 3,
            ts_ms: 1_700_000_000_000,
            frame_width: 640,
            frame_height: 480,
            detections: vec![Detection {
                bbox: Some(BoundingBox {
                    x: 1.0,
                    y: 2.0,
                    width: 3.0,
                    height: 4.0,
                }),
                class_label: "person".into(),
                confidence: 0.9,
                track_id: None,
                assoc_confidence: None,
                lock_state: None,
                attributes: None,
                mask: None,
                keypoints: None,
                depth: None,
                world_pos: None,
            }],
        };
        serde_json::to_vec(&batch).unwrap()
    }

    #[test]
    fn a_plugin_stream_lands_in_the_plugins_own_subtree_verbatim() {
        let req = CloudPublishRequest::stream("com.example.mapper", "map.pose", vec![1, 2, 3]);
        let msg = stream_message("dev1", &req).unwrap();
        assert_eq!(msg.topic, "ados/dev1/plugin/com.example.mapper/map.pose");
        assert_eq!(msg.payload, vec![1, 2, 3]);
    }

    #[test]
    fn the_detection_stream_lands_on_the_core_vision_topic_as_a_typed_batch() {
        let mut raw: Value = serde_json::from_slice(&batch_json(VISION_DETECTION_VERSION)).unwrap();
        // A key outside the contract is not carried onto the core topic.
        raw["extra"] = json!("dropped");
        let req = CloudPublishRequest::stream(
            "com.example.mapper",
            wire::VISION_DETECTIONS_STREAM,
            serde_json::to_vec(&raw).unwrap(),
        );
        let msg = stream_message("dev1", &req).unwrap();
        assert_eq!(msg.topic, "ados/dev1/vision/detections");
        let out: Value = serde_json::from_slice(&msg.payload).unwrap();
        assert_eq!(out["v"], VISION_DETECTION_VERSION);
        assert_eq!(out["detections"][0]["class_label"], "person");
        assert!(out.get("extra").is_none());
    }

    #[test]
    fn the_detection_stream_refuses_anything_but_a_current_batch() {
        for payload in [
            b"not json".to_vec(),
            br#"{"hello":"world"}"#.to_vec(),
            batch_json(VISION_DETECTION_VERSION - 1),
        ] {
            let req = CloudPublishRequest::stream(
                "com.example.mapper",
                wire::VISION_DETECTIONS_STREAM,
                payload,
            );
            assert!(stream_message("dev1", &req).is_err());
        }
    }

    #[test]
    fn a_record_body_names_the_poster_and_defaults_the_subject_to_this_node() {
        let req = CloudPublishRequest::record(
            "com.example.mapper",
            "jobs",
            "session-7",
            br#"{"status":"done","steps":3}"#.to_vec(),
        );
        assert_eq!(
            record_body("dev1", &req).unwrap(),
            json!({
                "posterDeviceId": "dev1",
                "pluginId": "com.example.mapper",
                "collection": "jobs",
                "key": "session-7",
                "data": {"status": "done", "steps": 3},
                "deviceId": "dev1",
            })
        );

        let about_a_drone = req.with_device_id("drone9");
        let body = record_body("dev1", &about_a_drone).unwrap();
        assert_eq!(body["posterDeviceId"], "dev1");
        assert_eq!(body["deviceId"], "drone9");
    }

    #[test]
    fn a_record_payload_must_be_json() {
        let req = CloudPublishRequest::record("com.example.mapper", "jobs", "k", b"{".to_vec());
        assert!(record_body("dev1", &req).is_err());
    }

    #[test]
    fn the_queue_evicts_the_oldest_by_count() {
        let q = StreamQueue::new(2, usize::MAX);
        let msg = |n: u8| StreamMessage {
            topic: "t".into(),
            payload: vec![n],
        };
        assert_eq!(q.push(msg(1)), 0);
        assert_eq!(q.push(msg(2)), 0);
        assert_eq!(q.push(msg(3)), 1);
        assert_eq!(q.dropped_oldest(), 1);
        assert_eq!(q.try_pop().unwrap().payload, vec![2]);
        assert_eq!(q.try_pop().unwrap().payload, vec![3]);
        assert!(q.try_pop().is_none());
    }

    #[test]
    fn the_queue_evicts_the_oldest_by_bytes() {
        // Each message costs 1 (topic) + 10 (payload) = 11 bytes; 25 holds two.
        let q = StreamQueue::new(100, 25);
        let msg = |n: u8| StreamMessage {
            topic: "t".into(),
            payload: vec![n; 10],
        };
        q.push(msg(1));
        q.push(msg(2));
        assert_eq!(q.push(msg(3)), 1);
        assert_eq!(q.len(), 2);
        assert_eq!(q.try_pop().unwrap().payload[0], 2);
        // The byte count follows pops, so room is reclaimed.
        q.push(msg(4));
        assert_eq!(q.len(), 2);
        assert_eq!(q.dropped_oldest(), 1);
    }

    #[tokio::test]
    async fn a_paired_node_with_a_broker_queues_a_stream_and_replies_at_once() {
        let r = relay(true, "", paired);
        let reply = r
            .handle(&CloudPublishRequest::stream(
                "com.example.a",
                "pose",
                b"x".to_vec(),
            ))
            .await;
        assert_eq!(reply, CloudPublishReply::accepted());
        let msg = r.queue().try_pop().unwrap();
        assert_eq!(msg.topic, "ados/dev1/plugin/com.example.a/pose");
    }

    #[tokio::test]
    async fn a_stream_is_refused_with_the_reason_when_it_cannot_leave() {
        let req = CloudPublishRequest::stream("com.example.a", "pose", b"x".to_vec());
        let no_broker = relay(false, "", paired).handle(&req).await;
        assert!(!no_broker.ok);
        assert!(no_broker.error.unwrap().contains("no cloud broker"));

        let not_paired = relay(true, "", unpaired);
        let reply = not_paired.handle(&req).await;
        assert!(!reply.ok);
        assert!(reply.error.unwrap().contains("not paired"));
        assert!(not_paired.queue().is_empty());
    }

    #[tokio::test]
    async fn a_record_is_refused_without_a_cloud_or_a_key() {
        let req = CloudPublishRequest::record("com.example.a", "jobs", "k", b"{}".to_vec());
        assert!(!relay(true, "", paired).handle(&req).await.ok);
        assert!(
            !relay(true, "http://127.0.0.1:9", unpaired)
                .handle(&req)
                .await
                .ok
        );
    }

    /// A one-shot HTTP server that records the request and answers with
    /// `status` and `body`.
    async fn one_shot_http(
        status: u16,
        body: &'static str,
    ) -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let n = s.read(&mut chunk).await.unwrap();
                buf.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buf).to_string();
                if let Some(head_end) = text.find("\r\n\r\n") {
                    let len = text[..head_end]
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if buf.len() >= head_end + 4 + len {
                        break;
                    }
                }
                if n == 0 {
                    break;
                }
            }
            let response = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            s.write_all(response.as_bytes()).await.unwrap();
            String::from_utf8(buf).unwrap()
        });
        (url, server)
    }

    #[tokio::test]
    async fn a_record_posts_the_body_with_the_device_key_and_returns_the_clouds_answer() {
        let (url, server) = one_shot_http(200, r#"{"ok":true}"#).await;
        let r = relay(true, &url, paired);
        let req =
            CloudPublishRequest::record("com.example.a", "jobs", "k1", br#"{"a":1}"#.to_vec());
        assert_eq!(r.handle(&req).await, CloudPublishReply::accepted());

        let request = server.await.unwrap();
        assert!(request.starts_with("POST /agent/plugin-records "));
        assert!(request
            .to_ascii_lowercase()
            .contains("x-ados-key: device-key"));
        let body: Value = serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(body, record_body("dev1", &req).unwrap());
    }

    #[tokio::test]
    async fn a_record_the_cloud_refuses_carries_the_clouds_reason_back() {
        let (url, server) = one_shot_http(
            403,
            r#"{"error":"plugin has no enabled install with cloud.records on this node"}"#,
        )
        .await;
        let r = relay(true, &url, paired);
        let req = CloudPublishRequest::record("com.example.a", "jobs", "k1", b"{}".to_vec());
        let reply = r.handle(&req).await;
        server.await.unwrap();
        assert!(!reply.ok);
        let error = reply.error.unwrap();
        assert!(error.contains("403"), "{error}");
        assert!(error.contains("no enabled install"), "{error}");
        assert_eq!(r.counters.records_failed.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn the_socket_answers_each_request_on_one_connection_and_refuses_an_oversize_frame() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(wire::CLOUD_PUBLISH_SOCK_NAME);
        let listener = bind_command_socket(&path, wire::SOCKET_MODE).unwrap();
        let r = Arc::new(relay(true, "", paired));
        let (_stop_tx, stop_rx) = watch::channel(false);
        tokio::spawn(r.clone().serve(listener, stop_rx));

        // The client helper, end to end.
        let reply = wire::send(
            &path,
            &CloudPublishRequest::stream("com.example.a", "pose", b"p".to_vec()),
        )
        .await
        .unwrap();
        assert_eq!(reply, CloudPublishReply::accepted());
        assert_eq!(
            r.queue().try_pop().unwrap().topic,
            "ados/dev1/plugin/com.example.a/pose"
        );

        // Several exchanges on one connection: a readable frame whose body is
        // not a request is refused, and the connection keeps serving.
        let mut s = UnixStream::connect(&path).await.unwrap();
        let ok = CloudPublishRequest::stream("com.example.a", "a", vec![]);
        let garbage = b"not a msgpack request";
        let mut garbage_frame = (garbage.len() as u32).to_be_bytes().to_vec();
        garbage_frame.extend_from_slice(garbage);
        for (frame, want_ok) in [
            (ok.encode().unwrap(), true),
            (garbage_frame, false),
            (ok.encode().unwrap(), true),
        ] {
            s.write_all(&frame).await.unwrap();
            let body = read_length_prefixed(&mut s, wire::MAX_FRAME, true)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(CloudPublishReply::decode(&body).unwrap().ok, want_ok);
        }

        // A frame header announcing more than the cap is refused and closed
        // without the relay reading (or allocating) the announced body.
        let mut s = UnixStream::connect(&path).await.unwrap();
        s.write_all(&((wire::MAX_FRAME + 1) as u32).to_be_bytes())
            .await
            .unwrap();
        let body = read_length_prefixed(&mut s, wire::MAX_FRAME, true)
            .await
            .unwrap()
            .unwrap();
        let reply = CloudPublishReply::decode(&body).unwrap();
        assert!(!reply.ok);
        assert!(reply.error.unwrap().contains("exceeds"));
        let mut rest = Vec::new();
        s.read_to_end(&mut rest).await.unwrap();
        assert!(
            rest.is_empty(),
            "the connection is closed after the refusal"
        );
    }
}
