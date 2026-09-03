//! Handler routing and the in-process event bus.
//!
//! The handler surface and the event bus, in one place. Splits cleanly into
//! two groups:
//!
//! * Fully-wired, host-independent handlers: `event.publish`,
//!   `event.subscribe`, and `ping`. The event bus is an in-process fanout
//!   owned by the host, so it is served here directly, exactly as the Python
//!   supervisor wires its `EventBus` rather than behind a host-service hook.
//! * Host-coupled handlers: everything else routes to a [`HostServices`]
//!   method. The default [`NoopHost`] returns the `not_implemented` shape for
//!   each, mirroring the Python `_handle_*` stub bodies and the
//!   `not_available` returns until the agent's service surfaces stabilize.

use std::collections::BTreeSet;

use rmpv::Value;
use tokio::sync::broadcast;

use crate::dispatch::Method;
use crate::host::{HostError, HostResult, HostServices};

/// Per-subscriber event-bus depth. Matches the Python `events.QUEUE_DEPTH`.
pub const EVENT_QUEUE_DEPTH: usize = 256;

/// One event on the in-process bus. Mirrors `events.Event`.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub topic: String,
    pub timestamp_ms: i64,
    pub publisher_plugin_id: String,
    pub payload: Value,
}

/// In-process fanout bus. Every subscriber gets a bounded receiver; a slow
/// consumer is lagged rather than allowed to block the publisher, mirroring the
/// drop-on-full-queue policy of the Python `EventBus`.
///
/// Built on `tokio::sync::broadcast` so the host can hand a fresh subscriber
/// receiver to each plugin's fan-out task. Topic matching is applied per
/// subscriber against the topic the event carries; the publisher does not
/// pre-filter, so the bus stays a single shared channel.
#[derive(Debug)]
pub struct EventBus {
    tx: broadcast::Sender<Event>,
}

impl EventBus {
    pub fn new() -> Self {
        let (tx, _rx) = broadcast::channel(EVENT_QUEUE_DEPTH);
        Self { tx }
    }

    /// A receiver a plugin fan-out task drains, applying its own topic match.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }

    /// Publish an event. Returns the number of receivers it reached. A send
    /// with no receivers returns 0 rather than erroring, matching the Python
    /// `publish` which returns a delivered count.
    pub fn publish(&self, event: Event) -> usize {
        self.tx.send(event).unwrap_or(0)
    }

    /// Current receiver count.
    pub fn receiver_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

/// Glob-style topic match. `mavlink.*` matches `mavlink.heartbeat` but not the
/// bare `mavlink`. Mirrors `events._topic_matches` (exact match, else fnmatch).
pub fn topic_matches(pattern: &str, topic: &str) -> bool {
    if pattern == topic {
        return true;
    }
    fnmatch(pattern, topic)
}

/// Minimal fnmatch supporting `*` (any run, including across `.`) and `?` (one
/// char), which is all the topic taxonomy uses. Implemented locally so the
/// crate carries no extra dependency for one glob.
fn fnmatch(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    // Iterative backtracking matcher.
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star_p, mut star_t): (Option<usize>, usize) = (None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star_p = Some(pi);
            star_t = ti;
            pi += 1;
        } else if let Some(sp) = star_p {
            pi = sp + 1;
            star_t += 1;
            ti = star_t;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Topics any plugin may subscribe to without an explicit allowlist entry.
/// Mirrors `events._PUBLIC_TOPICS_FOR_SUBSCRIBE`.
pub const PUBLIC_TOPICS_FOR_SUBSCRIBE: &[&str] = &[
    "vehicle.armed",
    "vehicle.disarmed",
    "vehicle.mode_changed",
    "vehicle.battery_low",
    "vehicle.geofence_breach",
    "mission.started",
    "mission.completed",
    "mission.aborted",
    "agent.ready",
    "agent.shutdown",
];

/// Reserved namespaces a plugin must not publish into. The set is enforced inline by
/// `is_publish_allowed`. The `vision.` prefix is host-publish-only: the engine
/// publishes frame descriptors and detections there, and a plugin reaches the surface
/// through the gated `vision.*` methods, not by publishing the topic itself. A plugin
/// may still subscribe to `vision.*` with `event.subscribe` plus the matching read cap.
const RESERVED_PUBLISH_PREFIXES: &[&str] = &[
    "vehicle.", "mavlink.", "mission.", "safety.", "agent.", "swarm.", "gps.", "vision.",
];

/// Whether the plugin may subscribe to `topic_pattern`. Mirrors
/// `events.is_subscribe_allowed`: requires `event.subscribe`, then a
/// capability-gated shared-data topic, the plugin's own `plugin.<id>.`
/// namespace, or a public lifecycle topic.
///
/// The shared-data arm is an ADDITIONAL requirement on top of
/// `event.subscribe`, never a replacement for it. `plugin.atlas.*` is
/// documented as a namespace any plugin may consume, but the namespace check
/// below grants a plugin only its OWN `plugin.<id>.` prefix, so before this
/// arm existed the world model was reachable by exactly one plugin: whichever
/// one happened to be named `atlas`. A plugin id is not a capability model.
///
/// Adding the topics to `PUBLIC_TOPICS_FOR_SUBSCRIBE` would have been the
/// opposite error. A reconstruction is derived imagery of wherever the
/// aircraft flew and an occupancy field is a planning input, so neither is
/// public. The mapping lives in `ados_protocol::atlas` beside the topic
/// constants themselves and matches exact topics, so a look-alike such as
/// `plugin.atlas.occupancy.evil` inherits nothing.
pub fn is_subscribe_allowed(
    plugin_id: &str,
    topic_pattern: &str,
    granted_caps: &BTreeSet<String>,
) -> bool {
    if !granted_caps.contains("event.subscribe") {
        return false;
    }
    if let Some(cap) = ados_protocol::atlas::atlas_topic_subscribe_capability(topic_pattern) {
        return granted_caps.contains(cap);
    }
    if topic_pattern.starts_with(&format!("plugin.{plugin_id}.")) {
        return true;
    }
    PUBLIC_TOPICS_FOR_SUBSCRIBE.contains(&topic_pattern)
}

/// Whether the plugin may publish to `topic`. Mirrors
/// `events.is_publish_allowed`: the plugin's own namespace is always
/// publishable; otherwise `event.publish` is required and the reserved
/// namespaces are refused.
pub fn is_publish_allowed(plugin_id: &str, topic: &str, granted_caps: &BTreeSet<String>) -> bool {
    if topic.starts_with(&format!("plugin.{plugin_id}.")) {
        return true;
    }
    if !granted_caps.contains("event.publish") {
        return false;
    }
    !RESERVED_PUBLISH_PREFIXES
        .iter()
        .any(|p| topic.starts_with(p))
}

/// Build a `ping` result: `{"pong": true, "plugin_id": <id>}`.
pub fn ping_result(plugin_id: &str) -> HostResult {
    Value::Map(vec![
        (Value::from("pong"), Value::Boolean(true)),
        (Value::from("plugin_id"), Value::from(plugin_id)),
    ])
}

/// A soft handler failure that becomes the envelope `error` field, mirroring
/// the Python `_RpcError`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcError(pub String);

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RpcError {}

/// Read a string field from a msgpack-map `args` value.
fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    match args {
        Value::Map(entries) => entries
            .iter()
            .find(|(k, _)| k.as_str() == Some(key))
            .and_then(|(_, v)| v.as_str()),
        _ => None,
    }
}

/// Read a map field from a msgpack-map `args`, coercing a missing or non-map
/// value to an empty map (`env.args.get("payload") or {}`).
fn arg_map(args: &Value, key: &str) -> Value {
    match args {
        Value::Map(entries) => entries
            .iter()
            .find(|(k, _)| k.as_str() == Some(key))
            .map(|(_, v)| v.clone())
            .filter(|v| matches!(v, Value::Map(_)))
            .unwrap_or_else(|| Value::Map(vec![])),
        _ => Value::Map(vec![]),
    }
}

/// Outcome of an `event.publish` request that has passed the dispatch gate.
pub enum PublishOutcome {
    /// The event to fan out on the bus, plus the response `{"delivered": n}` is
    /// built by the caller after publishing.
    Publish(Event),
    /// The per-topic inline check refused the publish.
    Denied(RpcError),
}

/// Validate an `event.publish` request and build the event to fan out, applying the
/// inline per-topic check (`is_publish_allowed`). This stops at the bus call; the
/// caller publishes and shapes `{"delivered": n}`.
pub fn prepare_publish(
    plugin_id: &str,
    args: &Value,
    granted_caps: &BTreeSet<String>,
    now_ms: i64,
) -> PublishOutcome {
    let Some(topic) = arg_str(args, "topic") else {
        return PublishOutcome::Denied(RpcError("topic must be a string".to_string()));
    };
    if !is_publish_allowed(plugin_id, topic, granted_caps) {
        return PublishOutcome::Denied(RpcError(format!("publish not permitted on topic {topic}")));
    }
    PublishOutcome::Publish(Event {
        topic: topic.to_string(),
        timestamp_ms: now_ms,
        publisher_plugin_id: plugin_id.to_string(),
        payload: arg_map(args, "payload"),
    })
}

/// Validate an `event.subscribe` request, applying the inline per-topic check
/// (`is_subscribe_allowed`). Returns the topic pattern to subscribe to, or a refusal.
pub fn prepare_subscribe(
    plugin_id: &str,
    args: &Value,
    granted_caps: &BTreeSet<String>,
) -> Result<String, RpcError> {
    let Some(pattern) = arg_str(args, "topic") else {
        return Err(RpcError("topic must be a string".to_string()));
    };
    if !is_subscribe_allowed(plugin_id, pattern, granted_caps) {
        return Err(RpcError(format!("subscribe not permitted on {pattern}")));
    }
    Ok(pattern.to_string())
}

/// Build the `event.deliver` envelope `args` the server pushes to a subscriber when a
/// matching event fans out.
pub fn event_deliver_args(event: &Event) -> Value {
    Value::Map(vec![
        (Value::from("topic"), Value::from(event.topic.as_str())),
        (Value::from("payload"), event.payload.clone()),
        (
            Value::from("publisher"),
            Value::from(event.publisher_plugin_id.as_str()),
        ),
        (
            Value::from("timestamp_ms"),
            Value::Integer(event.timestamp_ms.into()),
        ),
    ])
}

/// Route a host-coupled method to its [`HostServices`] hook. The event surface,
/// `ping`, `mavlink.subscribe`, and `vision.subscribe_frames` are handled in the
/// server before this is reached (they arm a push stream); this routes the
/// remaining host-coupled methods. With the [`NoopHost`](crate::host::NoopHost)
/// every one returns `Ok(not_implemented(...))`, mirroring the Python stub
/// bodies; a real host returns [`Err(HostError)`](HostError) for a soft failure,
/// which the server renders into the response envelope `error` field.
///
/// Async because the three vision request methods proxy to the vision engine
/// socket and await its reply; the other methods complete synchronously and are
/// awaited as already-ready futures.
///
/// `granted_caps` is the caller's verified capability set. Only the three
/// payload-gated methods (`mavlink.send`, `mavlink.register_component`,
/// `peripheral.register_driver`) consume it; they apply their capability gate
/// inside the handler, after argument validation, exactly where the Python
/// handlers apply it. The other methods are fully gated at the dispatch level and
/// ignore it.
pub async fn route_host_method<H: HostServices + ?Sized>(
    host: &H,
    method: Method,
    plugin_id: &str,
    args: &Value,
    granted_caps: &BTreeSet<String>,
) -> Result<HostResult, HostError> {
    match method {
        Method::TelemetrySubscribe => host.telemetry_subscribe(plugin_id, args),
        Method::TelemetryExtend => host.telemetry_extend(plugin_id, args),
        Method::MissionRead => host.mission_read(plugin_id, args),
        Method::MissionWrite => host.mission_write(plugin_id, args),
        Method::RecordingStart => host.recording_start(plugin_id, args),
        Method::RecordingStop => host.recording_stop(plugin_id, args),
        Method::MavlinkSubscribe => host.mavlink_subscribe(plugin_id, args),
        Method::MavlinkSend => host.mavlink_send(plugin_id, args, granted_caps),
        // msp.send forwards raw MSP bytes to the FC; the dispatch-level msp.write
        // cap is the whole gate, so no granted_caps inline check.
        Method::MspSend => host.msp_send(plugin_id, args),
        Method::MavlinkTunnelSend => host.mavlink_tunnel_send(plugin_id, args),
        Method::MavlinkRegisterComponent => {
            host.mavlink_register_component(plugin_id, args, granted_caps)
        }
        Method::PeripheralRegisterDriver => {
            host.peripheral_register_driver(plugin_id, args, granted_caps)
        }
        Method::PeripheralUnregisterDriver => host.peripheral_unregister_driver(plugin_id, args),
        Method::CameraClaim => host.camera_claim(plugin_id, args),
        Method::CameraRelease => host.camera_release(plugin_id, args),
        Method::CameraGetFrame => host.camera_get_frame(plugin_id, args),
        Method::VideoSourceSet => host.video_source_set(plugin_id, args),
        Method::ConfigGet => host.config_get(plugin_id, args),
        Method::ConfigSet => host.config_set(plugin_id, args),
        Method::ProcessSpawn => host.process_spawn(plugin_id, args),
        Method::DisplayPageSet => host.display_page_set(plugin_id, args),
        Method::GpioOutputSet => host.gpio_output_set(plugin_id, args),
        Method::GpioBuzzerBeep => host.gpio_buzzer_beep(plugin_id, args),
        Method::GuidedSetpointSend => host.guided_setpoint_send(plugin_id, args),
        Method::RateSetpointSend => host.rate_setpoint_send(plugin_id, args),
        Method::RadioAuxStreamOpen => host.radio_aux_stream_open(plugin_id, args),
        Method::RadioAuxStreamClose => host.radio_aux_stream_close(plugin_id, args),
        Method::RadioAuxStreamSend => host.radio_aux_stream_send(plugin_id, args),
        // Subscribe is handled in the server (it arms the per-connection aux
        // push stream) and never reaches here, exactly like button.subscribe.
        Method::RadioAuxStreamSubscribe => {
            Ok(crate::host::not_implemented("radio.aux_stream.subscribe"))
        }
        // Vision request/response methods proxy to the engine and await its
        // reply. (vision.subscribe_frames is handled in the server, where it
        // arms the frame-descriptor push stream, never reaching here.)
        Method::VisionRegisterModel => host.vision_register_model(plugin_id, args).await,
        // Reads the plugin's own resolved model status off the install record —
        // the one vision method that does not proxy to the engine.
        Method::VisionReadModel => host.vision_read_model(plugin_id, args).await,
        Method::VisionInfer => host.vision_infer(plugin_id, args).await,
        Method::VisionPublishDetection => host.vision_publish_detection(plugin_id, args).await,
        Method::VisionDesignateTrack => host.vision_designate_track(plugin_id, args).await,
        // Compute offload: proxy to the paired compute node over HTTP, await the
        // reply, and return the node's response map.
        Method::ComputeDatasetWrite => host.compute_dataset_write(plugin_id, args).await,
        Method::ComputeJobSubmit => host.compute_job_submit(plugin_id, args).await,
        Method::ComputeJobRead => host.compute_job_read(plugin_id, args).await,
        Method::ComputeJobOutputs => host.compute_job_outputs(plugin_id, args).await,
        Method::ComputeJobCancel => host.compute_job_cancel(plugin_id, args).await,
        // Streaming perception offload: open / close / read-health of a live
        // frames→detections session on the paired compute node.
        Method::ComputeStreamOpen => host.compute_stream_open(plugin_id, args).await,
        Method::ComputeStreamClose => host.compute_stream_close(plugin_id, args).await,
        Method::ComputeStreamHealth => host.compute_stream_health(plugin_id, args).await,
        // The event surface, ping, and the streaming subscribe methods never
        // reach here; the server short-circuits `vision.subscribe_frames`,
        // `vision.subscribe_detections` and `button.subscribe`, arming the
        // per-connection push streams before they could route to the facade.
        // Reaching this arm is a programming error guarded by a stable response.
        Method::EventPublish
        | Method::EventSubscribe
        | Method::Ping
        | Method::VisionSubscribeFrames
        | Method::VisionSubscribeDetections
        | Method::MspSubscribe
        | Method::ButtonSubscribe
        // display.zone.subscribe arms the tap push stream in the server (like
        // button.subscribe) and never reaches the facade.
        | Method::DisplayZoneSubscribe => Ok(crate::host::not_implemented("event")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn topic_match_segments() {
        assert!(topic_matches("mavlink.*", "mavlink.heartbeat"));
        assert!(topic_matches("plugin.demo.*", "plugin.demo.metric"));
        assert!(topic_matches("vehicle.armed", "vehicle.armed"));
        assert!(!topic_matches("mavlink.*", "mavlinkx"));
        assert!(!topic_matches("vehicle.armed", "vehicle.disarmed"));
    }

    #[test]
    fn publish_allows_own_namespace_without_publish_cap() {
        assert!(is_publish_allowed("demo", "plugin.demo.metric", &caps(&[])));
    }

    #[test]
    fn publish_refuses_reserved_namespace_even_with_cap() {
        assert!(!is_publish_allowed(
            "demo",
            "mavlink.x",
            &caps(&["event.publish"])
        ));
        assert!(is_publish_allowed(
            "demo",
            "custom.topic",
            &caps(&["event.publish"])
        ));
    }

    #[test]
    fn subscribe_refuses_shared_world_data_without_the_read_capability() {
        // `event.subscribe` alone is not enough. A reconstruction descriptor
        // names where imagery of wherever the aircraft flew can be fetched, so
        // it is gated on top of the subscribe capability rather than by it.
        assert!(!is_subscribe_allowed(
            "demo",
            ados_protocol::atlas::PLUGIN_ATLAS_OCCUPANCY_TOPIC,
            &caps(&["event.subscribe"])
        ));
    }

    #[test]
    fn subscribe_allows_shared_world_data_with_the_read_capability() {
        assert!(is_subscribe_allowed(
            "demo",
            ados_protocol::atlas::PLUGIN_ATLAS_OCCUPANCY_TOPIC,
            &caps(&[
                "event.subscribe",
                ados_protocol::atlas::ATLAS_WORLD_READ_CAP
            ])
        ));
    }

    #[test]
    fn subscribe_gates_the_world_pose_on_telemetry_read() {
        let topic = ados_protocol::atlas::PLUGIN_ATLAS_POSE_TOPIC;
        // The pose is the same class of data as vehicle telemetry, in the
        // world frame, so it takes the capability that already covers that —
        // and the artifact capability does not substitute for it.
        assert!(!is_subscribe_allowed(
            "demo",
            topic,
            &caps(&["event.subscribe"])
        ));
        assert!(!is_subscribe_allowed(
            "demo",
            topic,
            &caps(&[
                "event.subscribe",
                ados_protocol::atlas::ATLAS_WORLD_READ_CAP
            ])
        ));
        assert!(is_subscribe_allowed(
            "demo",
            topic,
            &caps(&["event.subscribe", ados_protocol::atlas::ATLAS_POSE_READ_CAP])
        ));
    }

    #[test]
    fn a_plugin_named_atlas_no_longer_gets_the_namespace_for_free() {
        // The revocation this gate performs, stated as a test so it cannot be
        // mistaken for a no-op: the own-namespace rule used to hand the whole
        // shared world-model namespace to whichever plugin was named `atlas`.
        assert!(!is_subscribe_allowed(
            "atlas",
            ados_protocol::atlas::PLUGIN_ATLAS_SPLAT_TOPIC,
            &caps(&["event.subscribe"])
        ));
        // Its own non-shared topics are unaffected.
        assert!(is_subscribe_allowed(
            "atlas",
            "plugin.atlas.private-metric",
            &caps(&["event.subscribe"])
        ));
    }

    #[test]
    fn subscribe_does_not_let_a_look_alike_topic_inherit_the_grant() {
        // The mapping matches exact topics, so a longer topic that merely
        // starts with a gated one names no capability and falls through to the
        // ordinary namespace rule.
        assert!(!is_subscribe_allowed(
            "demo",
            "plugin.atlas.occupancy.evil",
            &caps(&[
                "event.subscribe",
                ados_protocol::atlas::ATLAS_WORLD_READ_CAP
            ])
        ));
    }

    #[test]
    fn subscribe_allows_public_topic_with_cap() {
        assert!(is_subscribe_allowed(
            "demo",
            "agent.ready",
            &caps(&["event.subscribe"])
        ));
        assert!(!is_subscribe_allowed("demo", "agent.ready", &caps(&[])));
        assert!(is_subscribe_allowed(
            "demo",
            "plugin.demo.x",
            &caps(&["event.subscribe"])
        ));
    }

    #[tokio::test]
    async fn event_bus_fans_out_to_a_subscriber() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let evt = Event {
            topic: "plugin.demo.metric".to_string(),
            timestamp_ms: 42,
            publisher_plugin_id: "demo".to_string(),
            payload: Value::Map(vec![]),
        };
        let delivered = bus.publish(evt.clone());
        assert_eq!(delivered, 1);
        let got = rx.recv().await.unwrap();
        assert_eq!(got, evt);
    }

    #[test]
    fn prepare_publish_denies_reserved_topic() {
        let args = Value::Map(vec![(Value::from("topic"), Value::from("mavlink.x"))]);
        match prepare_publish("demo", &args, &caps(&["event.publish"]), 0) {
            PublishOutcome::Denied(e) => {
                assert_eq!(e.0, "publish not permitted on topic mavlink.x")
            }
            PublishOutcome::Publish(_) => panic!("reserved topic must be denied"),
        }
    }
}
