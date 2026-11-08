//! View leases: which camera and which overlay layers each live surface is
//! actually showing, so the overlay producers compute only what someone is
//! watching.
//!
//! A stream client sends `{"type":"view","camera_id":"<served id>","layers":[…]}`
//! whenever its visible view, camera or layer set changes and re-sends it every
//! 2 s; a polling client passes the same lease as `?view=<camera_id>:<layer,…>`
//! on `GET /api/v1/nav/status`. A lease expires [`VIEW_LEASE_TTL`] after its
//! last refresh. The union of the live leases is the [`ViewDemand`], published
//! on a `watch` channel the producers (the `ar` builder, the pilot's obstacle
//! projection) read.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;

/// How long a lease lives after its last refresh.
pub const VIEW_LEASE_TTL: Duration = Duration::from_secs(6);

/// The overlay layers a lease can name. Anything else is ignored.
pub const VIEW_LAYERS: [&str; 5] = ["ar_route", "ar_motion", "obstacles", "depth", "surfaces"];

/// Most cameras one demand tracks; a lease for a new camera past this is
/// ignored, bounding what a misbehaving client can make the producers compute.
const MAX_CAMERAS: usize = 8;

/// The longest camera id accepted.
const MAX_CAMERA_ID: usize = 64;

/// The union of the live view leases: per served camera id, the layers some
/// surface is drawing on it. Empty means nobody is watching anything.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ViewDemand {
    pub cameras: BTreeMap<String, BTreeSet<String>>,
}

impl ViewDemand {
    /// Whether `camera` is watched with `layer` on.
    pub fn wants(&self, camera: &str, layer: &str) -> bool {
        self.cameras.get(camera).is_some_and(|l| l.contains(layer))
    }

    /// The `view_demand` field of `GET /api/v1/nav/status`:
    /// `{<camera_id>: [<layer>…]}`.
    pub fn to_json(&self) -> Value {
        serde_json::to_value(&self.cameras).unwrap_or(Value::Null)
    }
}

/// A client's `type:"view"` frame: a lease, or a release. An empty
/// `camera_id` releases whatever the client holds; an empty `layers` list
/// releases the lease on that camera only (a client switching cameras leases
/// the new one first, then releases the old one).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewMsg {
    Lease(ViewLease),
    Release { camera_id: Option<String> },
}

impl ViewMsg {
    /// Parse a stream client frame; `None` for anything else.
    pub fn from_frame(frame: &Value) -> Option<Self> {
        if frame.get("type")?.as_str()? != "view" {
            return None;
        }
        let camera = frame.get("camera_id").and_then(Value::as_str).unwrap_or("");
        let empty_layers = frame
            .get("layers")
            .and_then(Value::as_array)
            .is_none_or(|l| l.is_empty());
        if camera.is_empty() {
            return Some(ViewMsg::Release { camera_id: None });
        }
        if empty_layers {
            return Some(ViewMsg::Release {
                camera_id: Some(camera.to_owned()),
            });
        }
        ViewLease::from_frame(frame).map(ViewMsg::Lease)
    }

    /// Apply to the lease a client holds; returns whether it changed.
    pub fn apply(self, held: &mut Option<ViewLease>) -> bool {
        match self {
            ViewMsg::Lease(l) => {
                let changed = held.as_ref() != Some(&l);
                *held = Some(l);
                changed
            }
            ViewMsg::Release { camera_id: None } => held.take().is_some(),
            ViewMsg::Release { camera_id: Some(c) } => {
                if held.as_ref().is_some_and(|l| l.camera_id == c) {
                    *held = None;
                    true
                } else {
                    false
                }
            }
        }
    }
}

/// One surface's lease: the camera it shows and the layers it draws there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewLease {
    pub camera_id: String,
    pub layers: BTreeSet<String>,
}

impl ViewLease {
    /// Parse a stream client frame. `None` for anything that is not a
    /// well-formed `type:"view"` frame.
    pub fn from_frame(frame: &Value) -> Option<Self> {
        if frame.get("type")?.as_str()? != "view" {
            return None;
        }
        let camera = frame.get("camera_id")?.as_str()?;
        let layers = frame
            .get("layers")?
            .as_array()?
            .iter()
            .filter_map(Value::as_str);
        Self::new(camera, layers)
    }

    /// Parse the `?view=<camera_id>:<layer,…>` query value.
    pub fn from_query(view: &str) -> Option<Self> {
        let (camera, layers) = view.split_once(':').unwrap_or((view, ""));
        Self::new(camera, layers.split(',').filter(|s| !s.is_empty()))
    }

    fn new<'a>(camera: &str, layers: impl Iterator<Item = &'a str>) -> Option<Self> {
        if camera.is_empty()
            || camera.len() > MAX_CAMERA_ID
            || !camera
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.')
        {
            return None;
        }
        let layers = layers
            .filter(|l| VIEW_LAYERS.contains(l))
            .map(str::to_owned)
            .collect();
        Some(Self {
            camera_id: camera.to_owned(),
            layers,
        })
    }

    /// Whether this lease draws `layer`.
    pub fn has(&self, layer: &str) -> bool {
        self.layers.contains(layer)
    }
}

/// Who holds a lease: a stream connection, or a polling client keyed by the
/// lease it passes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum LeaseHolder {
    Stream(u64),
    Poll(String),
}

/// The live leases.
#[derive(Debug, Default)]
pub struct ViewLeases {
    leases: HashMap<LeaseHolder, (ViewLease, Instant)>,
}

impl ViewLeases {
    /// Set or refresh `holder`'s lease.
    pub fn put(&mut self, holder: LeaseHolder, lease: ViewLease, now: Instant) {
        self.leases.insert(holder, (lease, now));
    }

    /// Drop `holder`'s lease (a stream that closed or hid its view).
    pub fn remove(&mut self, holder: &LeaseHolder) {
        self.leases.remove(holder);
    }

    /// Drop expired leases and return the union of the rest.
    pub fn demand(&mut self, now: Instant) -> ViewDemand {
        self.leases
            .retain(|_, (_, at)| now.saturating_duration_since(*at) < VIEW_LEASE_TTL);
        let mut demand = ViewDemand::default();
        for (lease, _) in self.leases.values() {
            if !demand.cameras.contains_key(&lease.camera_id) && demand.cameras.len() >= MAX_CAMERAS
            {
                continue;
            }
            demand
                .cameras
                .entry(lease.camera_id.clone())
                .or_default()
                .extend(lease.layers.iter().cloned());
        }
        demand
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_a_view_frame_and_drops_unknown_layers() {
        let lease = ViewLease::from_frame(&json!({
            "type": "view", "camera_id": "main", "layers": ["ar_route", "bogus", "depth"]
        }))
        .unwrap();
        assert_eq!(lease.camera_id, "main");
        assert!(lease.has("ar_route") && lease.has("depth") && !lease.has("bogus"));
        assert!(ViewLease::from_frame(&json!({"type": "other"})).is_none());
        assert!(
            ViewLease::from_frame(&json!({"type": "view", "camera_id": "a b", "layers": []}))
                .is_none()
        );
    }

    #[test]
    fn empty_camera_releases_everything_empty_layers_release_that_camera() {
        let mut held = None;
        let lease = |c: &str| {
            ViewMsg::from_frame(&json!({"type": "view", "camera_id": c, "layers": ["ar_route"]}))
                .unwrap()
        };
        let release = |c: &str| {
            ViewMsg::from_frame(&json!({"type": "view", "camera_id": c, "layers": []})).unwrap()
        };
        assert!(lease("main").apply(&mut held));
        // A camera switch: lease the new one, then release the old one.
        assert!(lease("down").apply(&mut held));
        assert!(!release("main").apply(&mut held));
        assert_eq!(held.as_ref().unwrap().camera_id, "down");
        assert!(release("down").apply(&mut held));
        assert!(held.is_none());
        assert!(lease("main").apply(&mut held));
        assert!(release("").apply(&mut held));
        assert!(held.is_none());
    }

    #[test]
    fn parses_the_query_form() {
        let lease = ViewLease::from_query("down:obstacles,ar_motion").unwrap();
        assert_eq!(lease.camera_id, "down");
        assert!(lease.has("obstacles") && lease.has("ar_motion"));
        assert!(ViewLease::from_query("main").unwrap().layers.is_empty());
        assert!(ViewLease::from_query("").is_none());
    }

    #[test]
    fn demand_is_the_union_and_leases_expire() {
        let mut leases = ViewLeases::default();
        let t0 = Instant::now();
        leases.put(
            LeaseHolder::Stream(1),
            ViewLease::from_query("main:ar_route").unwrap(),
            t0,
        );
        leases.put(
            LeaseHolder::Stream(2),
            ViewLease::from_query("main:obstacles").unwrap(),
            t0,
        );
        leases.put(
            LeaseHolder::Poll("down:depth".into()),
            ViewLease::from_query("down:depth").unwrap(),
            t0 + Duration::from_secs(4),
        );
        let d = leases.demand(t0 + Duration::from_secs(1));
        assert!(
            d.wants("main", "ar_route") && d.wants("main", "obstacles") && d.wants("down", "depth")
        );
        let d = leases.demand(t0 + Duration::from_secs(7));
        assert!(!d.cameras.contains_key("main"));
        assert!(d.wants("down", "depth"));
        let d = leases.demand(t0 + Duration::from_secs(11));
        assert!(d.cameras.is_empty());
    }
}
