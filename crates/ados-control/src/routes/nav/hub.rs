//! The navigation hub: the latest value of each nav-stream kind, the live
//! occupancy tiles, and the follow events, fed by the plugin event-bus export
//! reader and read by the nav routes.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use parking_lot::Mutex;
use serde_json::{json, Map, Value};
use tokio::sync::{broadcast, watch};

use super::tiles::TileStore;
use super::view::{LeaseHolder, ViewDemand, ViewLease, ViewLeases};

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
    /// The pilot daemon's authority and flight state (host producer).
    Pilot,
    /// The pilot's obstacle sectors and per-camera near grids (host producer).
    Obstacles,
    /// The AR scene, projected per camera (host producer).
    Ar,
    /// The model depth grid per camera, only while leased (host producer).
    Depth,
    /// Landing-zone candidates per camera, only while leased (host producer).
    Surfaces,
}

/// How many latest-value kinds the hub holds.
pub const KIND_COUNT: usize = 10;

/// In the all-camera `depth` value, a camera whose grid is this much older
/// than the newest camera's has stopped producing and is left out.
pub const DEPTH_CAMERA_STALE_MS: i64 = 2_000;

/// Per-kind seq a stream has already sent.
pub(crate) type Sent = [u64; KIND_COUNT];

impl NavKind {
    pub const ALL: [NavKind; KIND_COUNT] = [
        NavKind::Pose,
        NavKind::Scan,
        NavKind::Nav,
        NavKind::Follow,
        NavKind::Map,
        NavKind::Pilot,
        NavKind::Obstacles,
        NavKind::Ar,
        NavKind::Depth,
        NavKind::Surfaces,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            NavKind::Pose => "pose",
            NavKind::Scan => "scan",
            NavKind::Nav => "nav",
            NavKind::Follow => "follow",
            NavKind::Map => "map",
            NavKind::Pilot => "pilot",
            NavKind::Obstacles => "obstacles",
            NavKind::Ar => "ar",
            NavKind::Depth => "depth",
            NavKind::Surfaces => "surfaces",
        }
    }

    fn index(self) -> usize {
        self as usize
    }

    /// The kinds also kept per camera: a client leasing a camera reads that
    /// camera's own latest frame, with its own `ts_ms`, so one camera
    /// producing never re-sends another's grid as fresh.
    fn per_camera(self) -> bool {
        matches!(self, NavKind::Depth | NavKind::Surfaces)
    }
}

/// What one client may receive: everything (status without a lease, tests),
/// or what its own view lease names.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ClientView<'a> {
    All,
    Lease(Option<&'a ViewLease>),
}

impl ClientView<'_> {
    /// The frame `data` this client gets for `kind`, or `None` to send nothing.
    /// The per-camera kinds keep only the leased camera, and only when the
    /// lease draws the layer; `obstacles` always goes out (its 72 sectors feed
    /// the radar card) with `cameras` narrowed the same way. Every other kind
    /// goes to every client.
    pub(crate) fn project(self, kind: NavKind, data: &Value) -> Option<Value> {
        let ClientView::Lease(lease) = self else {
            return Some(data.clone());
        };
        let layer_on = |layers: &[&str]| lease.is_some_and(|l| layers.iter().any(|x| l.has(x)));
        let camera = lease.map(|l| l.camera_id.as_str());
        match kind {
            NavKind::Ar => layer_on(&["ar_route", "ar_motion"]).then(|| narrow(data, camera)),
            NavKind::Depth => layer_on(&["depth"]).then(|| narrow(data, camera)),
            NavKind::Obstacles => Some(narrow(data, camera.filter(|_| layer_on(&["obstacles"])))),
            NavKind::Surfaces => (layer_on(&["surfaces"])
                && data.get("camera_id").and_then(Value::as_str) == camera)
                .then(|| data.clone()),
            _ => Some(data.clone()),
        }
    }
}

/// `data` with its `cameras` object reduced to `camera` (empty for `None`).
fn narrow(data: &Value, camera: Option<&str>) -> Value {
    let mut out = data.clone();
    if let Some(Value::Object(cams)) = out.get_mut("cameras") {
        cams.retain(|id, _| Some(id.as_str()) == camera);
    }
    out
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
    latest: [Option<Latest>; KIND_COUNT],
    /// Per camera id, the latest frame of each per-camera kind.
    cameras: [BTreeMap<String, Latest>; KIND_COUNT],
    seq: u64,
    tiles: TileStore,
    views: ViewLeases,
}

impl Inner {
    /// The stored value `view` reads for `kind`: under a lease, a per-camera
    /// kind is that camera's own latest frame.
    fn source(&self, kind: NavKind, view: ClientView<'_>) -> Option<&Latest> {
        match view {
            ClientView::Lease(Some(lease)) if kind.per_camera() => {
                self.cameras[kind.index()].get(lease.camera_id.as_str())
            }
            _ => self.latest[kind.index()].as_ref(),
        }
    }

    /// File a per-camera kind's frame under each camera it carries. `surfaces`
    /// names its camera (`camera_id`); its all-camera value stays the latest
    /// frame of any camera (`None`). `depth` carries its cameras under
    /// `cameras`; the returned all-camera value merges every camera still
    /// producing, each entry keeping its own `ts_ms`.
    fn store_cameras(&mut self, kind: NavKind, l: &Latest) -> Option<Value> {
        let slots = &mut self.cameras[kind.index()];
        let slot = |data: Value| Latest {
            data,
            ts_ms: l.ts_ms,
            received: l.received,
            seq: l.seq,
        };
        if kind == NavKind::Surfaces {
            if let Some(id) = l.data.get("camera_id").and_then(Value::as_str) {
                slots.insert(id.to_owned(), slot(l.data.clone()));
            }
            return None;
        }
        if let Some(Value::Object(cams)) = l.data.get("cameras") {
            for (id, entry) in cams {
                let mut own: Map<String, Value> = l
                    .data
                    .as_object()
                    .into_iter()
                    .flatten()
                    .filter(|(k, _)| k.as_str() != "cameras")
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                if let Some(scale) = entry.get("scale_source") {
                    own.insert("scale_source".into(), scale.clone());
                }
                let mut one = Map::new();
                one.insert(id.clone(), entry.clone());
                own.insert("cameras".into(), Value::Object(one));
                slots.insert(id.clone(), slot(Value::Object(own)));
            }
        }
        let newest = slots.values().map(|s| s.ts_ms).max().unwrap_or(l.ts_ms);
        let mut cameras = Map::new();
        let mut all_lidar = true;
        for (id, s) in slots.iter() {
            if s.ts_ms < newest - DEPTH_CAMERA_STALE_MS {
                continue;
            }
            if let Some(entry) = s.data.get("cameras").and_then(|c| c.get(id)) {
                all_lidar &= entry.get("scale_source").and_then(Value::as_str) == Some("lidar");
                cameras.insert(id.clone(), entry.clone());
            }
        }
        Some(json!({
            "v": 1,
            "ts_ms": newest,
            "scale_source": if all_lidar && !cameras.is_empty() { "lidar" } else { "none" },
            "cameras": cameras,
        }))
    }
}

/// The shared hub. Cheap to clone; every clone reads the same state.
#[derive(Clone)]
pub struct NavHub {
    inner: Arc<Mutex<Inner>>,
    changed: Arc<watch::Sender<u64>>,
    events: broadcast::Sender<Arc<Value>>,
    demand: Arc<watch::Sender<ViewDemand>>,
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
        let (demand, _) = watch::channel(ViewDemand::default());
        Self {
            inner: Arc::new(Mutex::new(Inner::default())),
            changed: Arc::new(changed),
            events,
            demand: Arc::new(demand),
        }
    }

    /// Store the latest value of a kind a host module produces (the pilot
    /// state, obstacles, the AR scene, depth, surfaces). No publisher pinning:
    /// only in-process producers reach this. `depth` and `surfaces` are also
    /// filed per camera (see [`Inner::store_cameras`]).
    pub fn ingest_host(&self, kind: NavKind, ts_ms: i64, data: Value) {
        self.store_latest(kind, ts_ms, data);
    }

    fn store_latest(&self, kind: NavKind, ts_ms: i64, data: Value) {
        let seq = {
            let mut inner = self.inner.lock();
            inner.seq += 1;
            let seq = inner.seq;
            let mut latest = Latest {
                data,
                ts_ms,
                received: Instant::now(),
                seq,
            };
            if kind.per_camera() {
                if let Some(all) = inner.store_cameras(kind, &latest) {
                    latest.ts_ms = all.get("ts_ms").and_then(Value::as_i64).unwrap_or(ts_ms);
                    latest.data = all;
                }
            }
            inner.latest[kind.index()] = Some(latest);
            seq
        };
        self.changed.send_replace(seq);
    }

    /// The union of the live view leases, refreshed as leases come and go.
    /// Producers compute only the cameras and layers it names.
    pub fn view_demand(&self) -> watch::Receiver<ViewDemand> {
        self.demand.subscribe()
    }

    /// Set or refresh a view lease.
    pub(crate) fn put_view(&self, holder: LeaseHolder, lease: ViewLease) {
        let demand = {
            let mut inner = self.inner.lock();
            inner.views.put(holder, lease, Instant::now());
            inner.views.demand(Instant::now())
        };
        self.publish_demand(demand);
    }

    /// Drop a view lease.
    pub(crate) fn drop_view(&self, holder: &LeaseHolder) {
        let demand = {
            let mut inner = self.inner.lock();
            inner.views.remove(holder);
            inner.views.demand(Instant::now())
        };
        self.publish_demand(demand);
    }

    /// Expire stale leases; returns the current demand. Producers call this
    /// on their own tick so an abandoned lease lapses without any traffic.
    pub fn prune_views(&self) -> ViewDemand {
        let demand = self.inner.lock().views.demand(Instant::now());
        self.publish_demand(demand.clone());
        demand
    }

    fn publish_demand(&self, demand: ViewDemand) {
        self.demand.send_if_modified(|cur| {
            if *cur == demand {
                false
            } else {
                *cur = demand;
                true
            }
        });
    }

    /// Apply one exported event.
    pub fn ingest(&self, topic: &str, publisher: &str, ts_ms: i64, payload: Value) {
        match route(topic, publisher) {
            Routed::Latest(kind) => self.store_latest(kind, ts_ms, payload),
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
    /// `null` when nothing of that kind has arrived, plus `view_demand`.
    pub fn status_body(&self) -> Value {
        self.status_body_for(None)
    }

    /// [`Self::status_body`] for a polling client passing `?view=`: the query
    /// refreshes its lease and the per-camera kinds are narrowed to it.
    pub fn status_body_for(&self, view: Option<&str>) -> Value {
        let lease = view.and_then(ViewLease::from_query);
        if let (Some(lease), Some(view)) = (&lease, view) {
            self.put_view(LeaseHolder::Poll(view.to_owned()), lease.clone());
        }
        let demand = self.prune_views();
        let filter = match (&lease, view) {
            (_, None) => ClientView::All,
            (lease, Some(_)) => ClientView::Lease(lease.as_ref()),
        };
        let inner = self.inner.lock();
        let mut body = Map::new();
        for kind in NavKind::ALL {
            let entry = match inner.source(kind, filter) {
                Some(l) => match filter.project(kind, &l.data) {
                    Some(data) => json!({
                        "ts_ms": l.ts_ms,
                        "age_ms": u64::try_from(l.received.elapsed().as_millis()).unwrap_or(u64::MAX),
                        "data": data,
                    }),
                    None => Value::Null,
                },
                None => Value::Null,
            };
            body.insert(kind.as_str().to_owned(), entry);
        }
        body.insert("view_demand".to_owned(), demand.to_json());
        Value::Object(body)
    }

    /// The `GET /api/v1/map/live` body for `since`.
    pub fn map_live_body(&self, since: u64) -> Value {
        self.inner.lock().tiles.body(since)
    }

    /// The stream frames of every kind whose latest value is newer than the
    /// seq recorded in `sent`, advancing `sent`. One frame per kind: a kind
    /// that changed several times since the last flush sends only its latest.
    pub(crate) fn frames_since(&self, sent: &mut Sent, view: ClientView<'_>) -> Vec<Value> {
        let inner = self.inner.lock();
        let mut frames = Vec::new();
        for kind in NavKind::ALL {
            let Some(l) = inner.source(kind, view) else {
                continue;
            };
            if l.seq > sent[kind.index()] {
                sent[kind.index()] = l.seq;
                if let Some(data) = view.project(kind, &l.data) {
                    frames.push(json!({"kind": kind.as_str(), "ts_ms": l.ts_ms, "data": data}));
                }
            }
        }
        frames
    }

    /// The latest `data` of `kind` and its age in milliseconds, `None` when
    /// nothing of that kind has arrived. Host producers read one another's
    /// kinds through this (the AR scene reads `pilot` and `follow`).
    pub fn latest(&self, kind: NavKind) -> Option<(Value, u64)> {
        let inner = self.inner.lock();
        inner.latest[kind.index()].as_ref().map(|l| {
            (
                l.data.clone(),
                u64::try_from(l.received.elapsed().as_millis()).unwrap_or(u64::MAX),
            )
        })
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
