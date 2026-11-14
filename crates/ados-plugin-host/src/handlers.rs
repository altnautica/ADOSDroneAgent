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

mod route;
mod topic_glob;

use std::collections::BTreeSet;

use rmpv::Value;
use tokio::sync::broadcast;

use crate::args::{arg_map, arg_str};
use crate::dispatch::Method;
use crate::host::{HostError, HostResult, HostServices};

pub use route::route_host_method;
use topic_glob::fnmatch;

/// Per-subscriber event-bus depth. Matches the Python `events.QUEUE_DEPTH`.
pub const EVENT_QUEUE_DEPTH: usize = 256;

/// Longest topic or subscription pattern, in bytes. Topics are short dotted
/// names; the cap bounds the glob match cost per event per subscription.
pub const EVENT_TOPIC_MAX_BYTES: usize = 256;

/// Largest event payload, in msgpack-encoded bytes. Together with
/// [`EVENT_QUEUE_DEPTH`] this bounds what one non-draining subscriber can pin
/// in the host (depth x payload, 16 MiB), where a full plugin frame per slot
/// would be a gigabyte.
pub const EVENT_PAYLOAD_MAX_BYTES: usize = 64 * 1024;

/// Most event subscriptions one plugin connection may hold.
pub const EVENT_MAX_SUBSCRIPTIONS: usize = 32;

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
    fnmatch(pattern.as_bytes(), topic.as_bytes())
}

/// Whether `topic` (or a pattern) is a well-formed bus name: non-empty,
/// printable ASCII, within [`EVENT_TOPIC_MAX_BYTES`].
pub(crate) fn topic_well_formed(topic: &str) -> bool {
    !topic.is_empty()
        && topic.len() <= EVENT_TOPIC_MAX_BYTES
        && topic.bytes().all(|b| b.is_ascii_graphic())
}

/// The namespace every plugin's own topics live under.
const PLUGIN_NAMESPACE: &str = "plugin.";

/// A topic one plugin owns and shares: it may publish it, and another plugin
/// may subscribe with `event.subscribe` plus `subscribe_capability`. Declared
/// in the owner's manifest (`agent.contributes.shared_topics`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedTopic {
    pub topic: String,
    pub owner: String,
    pub subscribe_capability: String,
}

/// The shared topics of the plugins currently served, kept current by the
/// reconciler and read on every subscribe, publish and delivery.
#[derive(Debug, Default)]
pub struct SharedTopics {
    inner: parking_lot::RwLock<Vec<SharedTopic>>,
}

impl SharedTopics {
    pub fn new(topics: Vec<SharedTopic>) -> Self {
        SharedTopics {
            inner: parking_lot::RwLock::new(topics),
        }
    }

    /// Replace the whole set.
    pub fn replace(&self, topics: Vec<SharedTopic>) {
        *self.inner.write() = topics;
    }

    /// The declaration of exactly `topic`, if a served plugin shares it.
    pub fn find(&self, topic: &str) -> Option<SharedTopic> {
        self.inner.read().iter().find(|t| t.topic == topic).cloned()
    }

    fn owned_by(&self, topic: &str, plugin_id: &str) -> bool {
        self.inner
            .read()
            .iter()
            .any(|t| t.topic == topic && t.owner == plugin_id)
    }
}

/// Whether an event may be delivered to `subscriber_id`.
///
/// Plugin ids are dotted reverse-DNS names, so `plugin.com.acme.` is a prefix
/// of `plugin.com.acme.tools.`: a topic-prefix check alone cannot tell the two
/// plugins' namespaces apart. Delivery closes that gap using the host-stamped
/// publisher: an event under `plugin.` reaches a subscriber only when that
/// subscriber published it, the host did, or it is a shared topic and its
/// declared owner did. So a plugin cannot read another plugin's private events
/// by subscribing to a longer prefix, and cannot spoof them by publishing under
/// one.
pub fn may_deliver(subscriber_id: &str, event: &Event, shared: &SharedTopics) -> bool {
    // The pilot's lease events belong to the one plugin the host stamped.
    if event.topic.starts_with(FLIGHT_NAMESPACE) {
        return event.publisher_plugin_id == crate::vehicle_events::HOST_PUBLISHER
            && crate::pilot_client::flight_event_for(subscriber_id, &event.payload);
    }
    if !event.topic.starts_with(PLUGIN_NAMESPACE) {
        return true;
    }
    event.publisher_plugin_id == subscriber_id
        || event.publisher_plugin_id == crate::vehicle_events::HOST_PUBLISHER
        || shared.owned_by(&event.topic, &event.publisher_plugin_id)
}

/// Topics any plugin may subscribe to without an explicit allowlist entry.
/// Every topic here has a host publisher (`crate::vehicle_events`); a topic
/// nothing publishes is not advertised, so a subscription to it fails loudly.
pub const PUBLIC_TOPICS_FOR_SUBSCRIBE: &[&str] = &[
    "vehicle.armed",
    "vehicle.disarmed",
    "vehicle.mode_changed",
    "vehicle.battery_low",
    "vehicle.geofence_breach",
    "agent.ready",
    "agent.shutdown",
];

/// Host-published pilot lease topics: a `flight.motion` holder may subscribe,
/// and each event reaches only the plugin it names.
pub const FLIGHT_TOPICS_FOR_SUBSCRIBE: &[&str] = &[
    crate::pilot_client::STICKS_TOPIC,
    crate::pilot_client::REVOKED_TOPIC,
    crate::pilot_client::GOAL_STATUS_TOPIC,
];

/// Prefix of the host-only pilot lease topics.
const FLIGHT_NAMESPACE: &str = "flight.";

/// Reserved namespaces a plugin must not publish into. The set is enforced inline by
/// `is_publish_allowed`. The `vision.` prefix is host-publish-only: the engine
/// publishes frame descriptors and detections there, and a plugin reaches the surface
/// through the gated `vision.*` methods, not by publishing the topic itself. A plugin
/// may still subscribe to `vision.*` with `event.subscribe` plus the matching read cap.
/// `telemetry.` is the host's too: `telemetry.state` is the vehicle state a plugin
/// reads through `telemetry.subscribe`, so no plugin may publish a look-alike.
/// `plugin.` is reserved too: every plugin publishes only under its own
/// `plugin.<id>.` prefix or on the shared topics it declares, never into another
/// plugin's.
const RESERVED_PUBLISH_PREFIXES: &[&str] = &[
    "vehicle.",
    "mavlink.",
    "mission.",
    "safety.",
    "agent.",
    "swarm.",
    "gps.",
    "vision.",
    "telemetry.",
    "plugin.",
    "flight.",
];

/// Whether the plugin may subscribe to `topic_pattern`. Mirrors
/// `events.is_subscribe_allowed`: requires `event.subscribe`, then a shared
/// topic, the plugin's own `plugin.<id>.` namespace, or a public lifecycle
/// topic.
///
/// The shared-topic arm is an ADDITIONAL requirement on top of
/// `event.subscribe`, never a replacement for it: a subscriber needs the
/// capability the owner declared for that topic (a catalog capability such as
/// `telemetry.read`, or one the owner declares itself), which the operator
/// granted it. The owner itself needs none. Shared topics match exactly, so a
/// look-alike such as `<shared topic>.evil` inherits nothing and falls through
/// to the ordinary namespace rule.
pub fn is_subscribe_allowed(
    plugin_id: &str,
    topic_pattern: &str,
    granted_caps: &BTreeSet<String>,
    shared: &SharedTopics,
) -> bool {
    if !granted_caps.contains("event.subscribe") || !topic_well_formed(topic_pattern) {
        return false;
    }
    if let Some(topic) = shared.find(topic_pattern) {
        return topic.owner == plugin_id || granted_caps.contains(&topic.subscribe_capability);
    }
    if topic_pattern.starts_with(&format!("{PLUGIN_NAMESPACE}{plugin_id}.")) {
        return true;
    }
    if FLIGHT_TOPICS_FOR_SUBSCRIBE.contains(&topic_pattern) {
        return granted_caps.contains("flight.motion");
    }
    PUBLIC_TOPICS_FOR_SUBSCRIBE.contains(&topic_pattern)
}

/// Whether the plugin may publish to `topic`. Mirrors
/// `events.is_publish_allowed`: the plugin's own namespace is always
/// publishable; a shared topic is publishable by its owner with
/// `event.publish`; otherwise `event.publish` is required and the reserved
/// namespaces (including every other plugin's) are refused.
pub fn is_publish_allowed(
    plugin_id: &str,
    topic: &str,
    granted_caps: &BTreeSet<String>,
    shared: &SharedTopics,
) -> bool {
    if !topic_well_formed(topic) {
        return false;
    }
    if topic.starts_with(&format!("{PLUGIN_NAMESPACE}{plugin_id}.")) {
        return true;
    }
    if !granted_caps.contains("event.publish") {
        return false;
    }
    if let Some(declared) = shared.find(topic) {
        return declared.owner == plugin_id;
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
    shared: &SharedTopics,
    now_ms: i64,
) -> PublishOutcome {
    let Some(topic) = arg_str(args, "topic") else {
        return PublishOutcome::Denied(RpcError("topic must be a string".to_string()));
    };
    if !topic_well_formed(topic) {
        return PublishOutcome::Denied(RpcError(format!(
            "topic must be 1-{EVENT_TOPIC_MAX_BYTES} printable ASCII bytes"
        )));
    }
    if !is_publish_allowed(plugin_id, topic, granted_caps, shared) {
        return PublishOutcome::Denied(RpcError(format!("publish not permitted on topic {topic}")));
    }
    let payload = arg_map(args, "payload");
    let size = rmp_serde::to_vec(&payload).map_or(usize::MAX, |b| b.len());
    if size > EVENT_PAYLOAD_MAX_BYTES {
        return PublishOutcome::Denied(RpcError(format!(
            "event payload exceeds {EVENT_PAYLOAD_MAX_BYTES} bytes"
        )));
    }
    PublishOutcome::Publish(Event {
        topic: topic.to_string(),
        timestamp_ms: now_ms,
        publisher_plugin_id: plugin_id.to_string(),
        payload,
    })
}

/// Validate an `event.subscribe` request, applying the inline per-topic check
/// (`is_subscribe_allowed`). Returns the topic pattern to subscribe to, or a refusal.
pub fn prepare_subscribe(
    plugin_id: &str,
    args: &Value,
    granted_caps: &BTreeSet<String>,
    shared: &SharedTopics,
) -> Result<String, RpcError> {
    let Some(pattern) = arg_str(args, "topic") else {
        return Err(RpcError("topic must be a string".to_string()));
    };
    if !topic_well_formed(pattern) {
        return Err(RpcError(format!(
            "topic must be 1-{EVENT_TOPIC_MAX_BYTES} printable ASCII bytes"
        )));
    }
    if !is_subscribe_allowed(plugin_id, pattern, granted_caps, shared) {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn none() -> SharedTopics {
        SharedTopics::default()
    }

    /// `com.example.mapper` shares a pose topic (catalog capability) and a
    /// world topic (a capability it declares itself).
    fn mapper() -> SharedTopics {
        SharedTopics::new(vec![
            SharedTopic {
                topic: "plugin.mapper.pose".to_string(),
                owner: "com.example.mapper".to_string(),
                subscribe_capability: "telemetry.read".to_string(),
            },
            SharedTopic {
                topic: "plugin.mapper.occupancy".to_string(),
                owner: "com.example.mapper".to_string(),
                subscribe_capability: "plugin.mapper.world.read".to_string(),
            },
        ])
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
        assert!(is_publish_allowed(
            "demo",
            "plugin.demo.metric",
            &caps(&[]),
            &none()
        ));
    }

    #[test]
    fn publish_refuses_reserved_namespace_even_with_cap() {
        assert!(!is_publish_allowed(
            "demo",
            "mavlink.x",
            &caps(&["event.publish"]),
            &none()
        ));
        assert!(is_publish_allowed(
            "demo",
            "custom.topic",
            &caps(&["event.publish"]),
            &none()
        ));
    }

    #[test]
    fn a_shared_topic_needs_the_owners_declared_capability_on_top_of_subscribe() {
        let shared = mapper();
        let topic = "plugin.mapper.occupancy";
        // `event.subscribe` alone is not enough, and a different capability
        // does not substitute for the declared one.
        assert!(!is_subscribe_allowed(
            "com.example.viewer",
            topic,
            &caps(&["event.subscribe"]),
            &shared
        ));
        assert!(!is_subscribe_allowed(
            "com.example.viewer",
            topic,
            &caps(&["event.subscribe", "telemetry.read"]),
            &shared
        ));
        assert!(is_subscribe_allowed(
            "com.example.viewer",
            topic,
            &caps(&["event.subscribe", "plugin.mapper.world.read"]),
            &shared
        ));
        // A catalog capability works the same way.
        assert!(is_subscribe_allowed(
            "com.example.viewer",
            "plugin.mapper.pose",
            &caps(&["event.subscribe", "telemetry.read"]),
            &shared
        ));
        // The declared capability never replaces event.subscribe.
        assert!(!is_subscribe_allowed(
            "com.example.viewer",
            topic,
            &caps(&["plugin.mapper.world.read"]),
            &shared
        ));
        // The owner needs no extra capability to read its own topic.
        assert!(is_subscribe_allowed(
            "com.example.mapper",
            topic,
            &caps(&["event.subscribe"]),
            &shared
        ));
    }

    #[test]
    fn a_shared_topic_is_matched_exactly() {
        // A longer topic that merely starts with a shared one names no
        // capability and falls through to the ordinary namespace rule.
        assert!(!is_subscribe_allowed(
            "com.example.viewer",
            "plugin.mapper.occupancy.evil",
            &caps(&["event.subscribe", "plugin.mapper.world.read"]),
            &mapper()
        ));
        // Undeclared, the same topic is some other plugin's private namespace.
        assert!(!is_subscribe_allowed(
            "com.example.viewer",
            "plugin.mapper.occupancy",
            &caps(&["event.subscribe", "plugin.mapper.world.read"]),
            &none()
        ));
    }

    #[test]
    fn only_the_owner_publishes_a_shared_topic_and_only_with_event_publish() {
        let shared = mapper();
        let topic = "plugin.mapper.pose";
        assert!(is_publish_allowed(
            "com.example.mapper",
            topic,
            &caps(&["event.publish"]),
            &shared
        ));
        assert!(!is_publish_allowed(
            "com.example.mapper",
            topic,
            &caps(&[]),
            &shared
        ));
        assert!(!is_publish_allowed(
            "com.example.impostor",
            topic,
            &caps(&["event.publish"]),
            &shared
        ));
    }

    #[test]
    fn a_shared_topic_is_delivered_only_from_its_owner() {
        let shared = mapper();
        let from_owner = event("plugin.mapper.pose", "com.example.mapper");
        assert!(may_deliver("com.example.viewer", &from_owner, &shared));
        let spoofed = event("plugin.mapper.pose", "com.example.impostor");
        assert!(!may_deliver("com.example.viewer", &spoofed, &shared));
        // Without the declaration the owner's event stays private.
        assert!(!may_deliver("com.example.viewer", &from_owner, &none()));
    }

    #[test]
    fn subscribe_allows_public_topic_with_cap() {
        assert!(is_subscribe_allowed(
            "demo",
            "agent.ready",
            &caps(&["event.subscribe"]),
            &none()
        ));
        assert!(!is_subscribe_allowed(
            "demo",
            "agent.ready",
            &caps(&[]),
            &none()
        ));
        assert!(is_subscribe_allowed(
            "demo",
            "plugin.demo.x",
            &caps(&["event.subscribe"]),
            &none()
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
        match prepare_publish("demo", &args, &caps(&["event.publish"]), &none(), 0) {
            PublishOutcome::Denied(e) => {
                assert_eq!(e.0, "publish not permitted on topic mavlink.x")
            }
            PublishOutcome::Publish(_) => panic!("reserved topic must be denied"),
        }
    }

    #[test]
    fn publish_refuses_other_plugins_namespaces() {
        let publish = caps(&["event.publish"]);
        assert!(!is_publish_allowed(
            "com.example.a",
            "plugin.com.example.b.status",
            &publish,
            &none()
        ));
        assert!(is_publish_allowed(
            "com.example.a",
            "plugin.com.example.a.status",
            &caps(&[]),
            &none()
        ));
    }

    fn event(topic: &str, publisher: &str) -> Event {
        Event {
            topic: topic.to_string(),
            timestamp_ms: 0,
            publisher_plugin_id: publisher.to_string(),
            payload: Value::Map(vec![]),
        }
    }

    #[test]
    fn dotted_ids_do_not_share_a_namespace_at_delivery() {
        // com.acme's own-namespace prefix also covers com.acme.tools' topics;
        // delivery keys on the host-stamped publisher, so neither plugin reads
        // or spoofs the other's events.
        let spoof = event("plugin.com.acme.tools.status", "com.acme");
        assert!(!may_deliver("com.acme.tools", &spoof, &none()));
        let private = event("plugin.com.acme.tools.status", "com.acme.tools");
        assert!(!may_deliver("com.acme", &private, &none()));
        assert!(may_deliver("com.acme.tools", &private, &none()));
        let host = event(
            "plugin.com.example.shared",
            crate::vehicle_events::HOST_PUBLISHER,
        );
        assert!(may_deliver("com.acme", &host, &none()));
        assert!(may_deliver(
            "com.acme",
            &event("vehicle.armed", "host"),
            &none()
        ));
    }

    #[test]
    fn oversize_topics_patterns_and_payloads_are_refused() {
        let long = format!("plugin.demo.{}", "a".repeat(EVENT_TOPIC_MAX_BYTES));
        let args = Value::Map(vec![(Value::from("topic"), Value::from(long.as_str()))]);
        assert!(matches!(
            prepare_publish("demo", &args, &caps(&[]), &none(), 0),
            PublishOutcome::Denied(_)
        ));
        assert!(prepare_subscribe("demo", &args, &caps(&["event.subscribe"]), &none()).is_err());

        let big = Value::Map(vec![(
            Value::from("blob"),
            Value::Binary(vec![0u8; EVENT_PAYLOAD_MAX_BYTES]),
        )]);
        let args = Value::Map(vec![
            (Value::from("topic"), Value::from("plugin.demo.x")),
            (Value::from("payload"), big),
        ]);
        assert!(matches!(
            prepare_publish("demo", &args, &caps(&[]), &none(), 0),
            PublishOutcome::Denied(_)
        ));
    }
}
