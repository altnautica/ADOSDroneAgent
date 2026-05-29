//! Handler routing and the in-process event bus.
//!
//! Ports the handler surface of `src/ados/plugins/ipc_server.py` plus
//! `src/ados/plugins/events.py`. Splits cleanly into two groups:
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
use crate::host::{HostResult, HostServices};

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

/// Reserved namespaces a plugin must not publish into. Mirrors the
/// `reserved_prefixes` tuple in `events.is_publish_allowed`.
const RESERVED_PUBLISH_PREFIXES: &[&str] = &[
    "vehicle.", "mavlink.", "mission.", "safety.", "agent.", "swarm.", "gps.",
];

/// Whether the plugin may subscribe to `topic_pattern`. Mirrors
/// `events.is_subscribe_allowed`: requires `event.subscribe`, then the
/// plugin's own `plugin.<id>.` namespace or a public lifecycle topic.
pub fn is_subscribe_allowed(
    plugin_id: &str,
    topic_pattern: &str,
    granted_caps: &BTreeSet<String>,
) -> bool {
    if !granted_caps.contains("event.subscribe") {
        return false;
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

/// Build a `ping` result: `{"pong": true, "plugin_id": <id>}`. Mirrors
/// `_handle_ping`.
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

/// Validate an `event.publish` request and build the event to fan out, applying
/// the inline per-topic check (`is_publish_allowed`). Mirrors
/// `_handle_event_publish` up to the bus call; the caller publishes and shapes
/// `{"delivered": n}`.
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
/// (`is_subscribe_allowed`). Returns the topic pattern to subscribe to, or a
/// refusal. Mirrors `_handle_event_subscribe` up to the bus call.
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

/// Build the `event.deliver` envelope `args` the server pushes to a subscriber
/// when a matching event fans out. Mirrors the args map in `_pump_subscription`.
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

/// Route a host-coupled method to its [`HostServices`] hook. The event surface
/// and `ping` are handled in the server before this is reached; this routes the
/// 17 host-coupled methods. With the [`NoopHost`](crate::host::NoopHost) every
/// one returns the `not_implemented` shape, mirroring the Python stub bodies.
pub fn route_host_method<H: HostServices + ?Sized>(
    host: &H,
    method: Method,
    plugin_id: &str,
    args: &Value,
) -> HostResult {
    match method {
        Method::TelemetrySubscribe => host.telemetry_subscribe(plugin_id, args),
        Method::TelemetryExtend => host.telemetry_extend(plugin_id, args),
        Method::MissionRead => host.mission_read(plugin_id, args),
        Method::MissionWrite => host.mission_write(plugin_id, args),
        Method::RecordingStart => host.recording_start(plugin_id, args),
        Method::RecordingStop => host.recording_stop(plugin_id, args),
        Method::MavlinkSubscribe => host.mavlink_subscribe(plugin_id, args),
        Method::MavlinkSend => host.mavlink_send(plugin_id, args),
        Method::MavlinkRegisterComponent => host.mavlink_register_component(plugin_id, args),
        Method::PeripheralRegisterDriver => host.peripheral_register_driver(plugin_id, args),
        Method::PeripheralUnregisterDriver => host.peripheral_unregister_driver(plugin_id, args),
        Method::CameraClaim => host.camera_claim(plugin_id, args),
        Method::CameraRelease => host.camera_release(plugin_id, args),
        Method::CameraGetFrame => host.camera_get_frame(plugin_id, args),
        Method::ConfigGet => host.config_get(plugin_id, args),
        Method::ConfigSet => host.config_set(plugin_id, args),
        Method::ProcessSpawn => host.process_spawn(plugin_id, args),
        // The event surface and ping never reach here; the server short-circuits
        // them. Treat as a programming error guarded by a stable response.
        Method::EventPublish | Method::EventSubscribe | Method::Ping => {
            crate::host::not_implemented("event")
        }
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
