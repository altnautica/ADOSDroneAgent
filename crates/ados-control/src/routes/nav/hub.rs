//! The navigation hub: the latest value of each nav-stream kind, the live
//! occupancy tiles, and the follow events, fed by the plugin event-bus export
//! reader and read by the nav routes.

use std::sync::Arc;
use std::time::Instant;

use parking_lot::Mutex;
use serde_json::{json, Map, Value};
use tokio::sync::{broadcast, watch};

use super::tiles::TileStore;

/// The plugin that owns the pose, scan, occupancy and map-status topics.
pub const WORLD_ENGINE_ID: &str = "com.altnautica.world-engine";
/// The plugin that publishes `nav.state`.
pub const VISION_NAV_ID: &str = "com.altnautica.vision-nav";
/// The plugin that publishes `follow.state` and the follow events.
pub const FOLLOW_ME_ID: &str = "com.altnautica.follow-me";

pub const TOPIC_POSE: &str = "plugin.world-engine.pose";
pub const TOPIC_SCAN: &str = "plugin.world-engine.scan";
pub const TOPIC_OCCUPANCY: &str = "plugin.world-engine.occupancy";
pub const TOPIC_MAP_STATUS: &str = "plugin.world-engine.map_status";
pub const TOPIC_NAV_STATE: &str = "nav.state";
pub const TOPIC_FOLLOW_STATE: &str = "follow.state";
pub const TOPIC_FOLLOW_REACQUIRED: &str = "follow.reacquired";
pub const TOPIC_FOLLOW_LOST: &str = "follow.lost";

/// Every topic the hub subscribes to on the export socket.
pub const EXPORT_TOPICS: [&str; 8] = [
    TOPIC_POSE,
    TOPIC_SCAN,
    TOPIC_OCCUPANCY,
    TOPIC_MAP_STATUS,
    TOPIC_NAV_STATE,
    TOPIC_FOLLOW_STATE,
    TOPIC_FOLLOW_REACQUIRED,
    TOPIC_FOLLOW_LOST,
];

/// Depth of the follow-event fanout. The events are rare (one per lock loss or
/// re-acquisition); a stream that lags past this loses the oldest.
const FOLLOW_EVENT_DEPTH: usize = 32;

/// One latest-value kind of the nav stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavKind {
    Pose,
    Scan,
    Nav,
    Follow,
    Map,
}

impl NavKind {
    pub const ALL: [NavKind; 5] = [
        NavKind::Pose,
        NavKind::Scan,
        NavKind::Nav,
        NavKind::Follow,
        NavKind::Map,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            NavKind::Pose => "pose",
            NavKind::Scan => "scan",
            NavKind::Nav => "nav",
            NavKind::Follow => "follow",
            NavKind::Map => "map",
        }
    }

    fn index(self) -> usize {
        self as usize
    }
}

/// Where one exported event goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Routed {
    Latest(NavKind),
    Tiles,
    FollowEvent,
    Ignored,
}

/// Route an event by topic and host-stamped publisher. Each topic is pinned to
/// the plugin that owns it, so another plugin publishing a look-alike
/// `follow.state` (any plugin with `event.publish` may) never reaches the
/// operator's display.
pub fn route(topic: &str, publisher: &str) -> Routed {
    let owner = match topic {
        TOPIC_POSE | TOPIC_SCAN | TOPIC_OCCUPANCY | TOPIC_MAP_STATUS => WORLD_ENGINE_ID,
        TOPIC_NAV_STATE => VISION_NAV_ID,
        TOPIC_FOLLOW_STATE | TOPIC_FOLLOW_REACQUIRED | TOPIC_FOLLOW_LOST => FOLLOW_ME_ID,
        _ => return Routed::Ignored,
    };
    if publisher != owner {
        return Routed::Ignored;
    }
    match topic {
        TOPIC_POSE => Routed::Latest(NavKind::Pose),
        TOPIC_SCAN => Routed::Latest(NavKind::Scan),
        TOPIC_MAP_STATUS => Routed::Latest(NavKind::Map),
        TOPIC_NAV_STATE => Routed::Latest(NavKind::Nav),
        TOPIC_FOLLOW_STATE => Routed::Latest(NavKind::Follow),
        TOPIC_OCCUPANCY => Routed::Tiles,
        _ => Routed::FollowEvent,
    }
}

#[derive(Debug, Clone)]
struct Latest {
    data: Value,
    ts_ms: i64,
    received: Instant,
    seq: u64,
}

#[derive(Debug, Default)]
struct Inner {
    latest: [Option<Latest>; 5],
    seq: u64,
    tiles: TileStore,
}

/// The shared hub. Cheap to clone; every clone reads the same state.
#[derive(Clone)]
pub struct NavHub {
    inner: Arc<Mutex<Inner>>,
    changed: Arc<watch::Sender<u64>>,
    events: broadcast::Sender<Arc<Value>>,
}

impl Default for NavHub {
    fn default() -> Self {
        Self::new()
    }
}

impl NavHub {
    /// An empty hub: every kind `null`, no tiles.
    pub fn new() -> Self {
        let (changed, _) = watch::channel(0);
        let (events, _) = broadcast::channel(FOLLOW_EVENT_DEPTH);
        Self {
            inner: Arc::new(Mutex::new(Inner::default())),
            changed: Arc::new(changed),
            events,
        }
    }

    /// Apply one exported event.
    pub fn ingest(&self, topic: &str, publisher: &str, ts_ms: i64, payload: Value) {
        match route(topic, publisher) {
            Routed::Latest(kind) => {
                let seq = {
                    let mut inner = self.inner.lock();
                    inner.seq += 1;
                    let seq = inner.seq;
                    inner.latest[kind.index()] = Some(Latest {
                        data: payload,
                        ts_ms,
                        received: Instant::now(),
                        seq,
                    });
                    seq
                };
                self.changed.send_replace(seq);
            }
            Routed::Tiles => self.inner.lock().tiles.apply(&payload),
            Routed::FollowEvent => {
                let frame = json!({
                    "kind": "follow_event",
                    "ts_ms": ts_ms,
                    "topic": topic,
                    "data": payload,
                });
                let _ = self.events.send(Arc::new(frame));
            }
            Routed::Ignored => {}
        }
    }

    /// The `GET /api/v1/nav/status` body: per kind `{ts_ms, age_ms, data}` or
    /// `null` when nothing of that kind has arrived.
    pub fn status_body(&self) -> Value {
        let inner = self.inner.lock();
        let mut body = Map::new();
        for kind in NavKind::ALL {
            let entry = match &inner.latest[kind.index()] {
                Some(l) => json!({
                    "ts_ms": l.ts_ms,
                    "age_ms": u64::try_from(l.received.elapsed().as_millis()).unwrap_or(u64::MAX),
                    "data": l.data,
                }),
                None => Value::Null,
            };
            body.insert(kind.as_str().to_owned(), entry);
        }
        Value::Object(body)
    }

    /// The `GET /api/v1/map/live` body for `since`.
    pub fn map_live_body(&self, since: u64) -> Value {
        self.inner.lock().tiles.body(since)
    }

    /// The stream frames of every kind whose latest value is newer than the
    /// seq recorded in `sent`, advancing `sent`. One frame per kind: a kind
    /// that changed several times since the last flush sends only its latest.
    pub(crate) fn frames_since(&self, sent: &mut [u64; 5]) -> Vec<Value> {
        let inner = self.inner.lock();
        let mut frames = Vec::new();
        for kind in NavKind::ALL {
            let Some(l) = &inner.latest[kind.index()] else {
                continue;
            };
            if l.seq > sent[kind.index()] {
                sent[kind.index()] = l.seq;
                frames.push(json!({"kind": kind.as_str(), "ts_ms": l.ts_ms, "data": l.data}));
            }
        }
        frames
    }

    /// A receiver that wakes whenever a latest-value kind changes.
    pub(crate) fn watch(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }

    /// A receiver of the follow events (`follow.reacquired`, `follow.lost`).
    pub(crate) fn follow_events(&self) -> broadcast::Receiver<Arc<Value>> {
        self.events.subscribe()
    }
}
