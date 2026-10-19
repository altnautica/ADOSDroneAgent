//! Route-aware forwarding (`mavlink.route_aware`, default off).
//!
//! Off, every consumer reads the FC fan-out directly, byte for byte as it
//! always has, and nothing here runs. On, one classifier task decodes each FC
//! frame once, picks its audience from the [`table::RouteTable`], and publishes
//! it on the hub's lane; each consumer's [`FrameFeed`] passes only frames
//! addressed to it. Frames a client sends are learned (their source ids name
//! the client as the owner of that system) and republished on the same lane to
//! every other client, never back to the sender.

pub mod table;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ados_protocol::shutdown::Shutdown;
use tokio::sync::broadcast::{self, error::RecvError};

use crate::rx_frame::RxFrame;

use table::RouteTable;
pub use table::{Audience, EndpointCounters, EndpointId};

/// The sidecar the route table is published to, under the run dir.
pub const SIDECAR_NAME: &str = "mavlink-router.json";

/// How often the sidecar is considered for a rewrite.
pub const SIDECAR_INTERVAL: Duration = Duration::from_secs(2);

/// An unchanged sidecar is still rewritten this often, so its age tells a
/// reader the router is alive (readers treat it as stale past 10 s).
pub const SIDECAR_REFRESH: Duration = Duration::from_secs(5);

/// The last sidecar body written and when.
pub type SidecarLast = Option<(Vec<u8>, Instant)>;

/// Depth of the routed lane, matching the FC fan-out.
const LANE_CAP: usize = 256;

/// The FC's own endpoint label.
const FC_LABEL: &str = "fc";

/// One frame on the routed lane.
#[derive(Debug, Clone)]
pub enum RoutedFrame {
    /// From the FC, for `audience`.
    Fc { audience: Audience, frame: RxFrame },
    /// From client endpoint `src`, for every other client.
    Client { src: EndpointId, frame: RxFrame },
}

/// A registered endpoint: its id and its counters.
#[derive(Debug, Clone)]
pub struct Endpoint {
    pub id: EndpointId,
    counters: Arc<EndpointCounters>,
}

/// The process-wide route state, owned by the FC connection.
#[derive(Debug)]
pub struct RouteHub {
    enabled: bool,
    table: Mutex<RouteTable>,
    lane: broadcast::Sender<RoutedFrame>,
    next_client: AtomicU64,
    fc: Endpoint,
}

impl RouteHub {
    /// A hub; `enabled` is `mavlink.route_aware`.
    pub fn new(enabled: bool) -> Arc<Self> {
        let mut table = RouteTable::default();
        let (id, counters) = table.endpoint(FC_LABEL);
        Arc::new(Self {
            enabled,
            table: Mutex::new(table),
            lane: broadcast::channel(LANE_CAP).0,
            next_client: AtomicU64::new(1),
            fc: Endpoint { id, counters },
        })
    }

    /// Whether route-aware forwarding is on.
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    fn table(&self) -> std::sync::MutexGuard<'_, RouteTable> {
        self.table
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The fixed endpoint named `label` (`ipc`, `cloud`, `aux`).
    pub fn register(&self, label: &str) -> Endpoint {
        let (id, counters) = self.table().endpoint(label);
        Endpoint { id, counters }
    }

    /// A fresh endpoint for one proxy client, labelled `<kind>#<n>`.
    pub fn register_client(&self, kind: &str) -> Endpoint {
        let n = self.next_client.fetch_add(1, Ordering::Relaxed);
        self.register(&format!("{kind}#{n}"))
    }

    /// A client sent `data` (whole frames): learn its source ids and hand it
    /// to every other client. A no-op with route-aware forwarding off.
    pub fn ingress(&self, ep: &Endpoint, data: &[u8]) {
        if !self.enabled || data.is_empty() {
            return;
        }
        ep.counters.rx.fetch_add(1, Ordering::Relaxed);
        self.table().learn(ep.id, data, Instant::now());
        let _ = self.lane.send(RoutedFrame::Client {
            src: ep.id,
            frame: RxFrame::now(bytes::Bytes::copy_from_slice(data)),
        });
    }

    /// Classify one FC frame and publish it on the lane.
    pub fn publish_fc(&self, frame: RxFrame) {
        self.fc.counters.rx.fetch_add(1, Ordering::Relaxed);
        let audience = self.table().classify(self.fc.id, &frame, Instant::now());
        let _ = self.lane.send(RoutedFrame::Fc { audience, frame });
    }

    /// The frame source for the consumer of endpoint `labels[0]`. Further
    /// labels are endpoints sharing the same transport (the cloud relay is one
    /// of the IPC socket's writers): their frames are not echoed back and FC
    /// frames addressed to them are delivered. With route-aware forwarding off
    /// this is `flat` itself, untouched.
    pub fn feed(&self, labels: &[&str], flat: broadcast::Receiver<RxFrame>) -> FrameFeed {
        let Some((first, rest)) = labels.split_first().filter(|_| self.enabled) else {
            return FrameFeed::Flat(flat);
        };
        let aliases = rest.iter().map(|l| self.register(l).id).collect();
        self.routed_feed(self.register(first), aliases)
    }

    /// [`Self::feed`] for a proxy client: a fresh `<kind>#<n>` endpoint, also
    /// returned so the client's inbound half can [`Self::ingress`] with it.
    pub fn client_feed(
        &self,
        kind: &str,
        flat: broadcast::Receiver<RxFrame>,
    ) -> (FrameFeed, Option<Endpoint>) {
        if !self.enabled {
            return (FrameFeed::Flat(flat), None);
        }
        let ep = self.register_client(kind);
        (self.routed_feed(ep.clone(), Vec::new()), Some(ep))
    }

    fn routed_feed(&self, ep: Endpoint, aliases: Vec<EndpointId>) -> FrameFeed {
        FrameFeed::Routed {
            rx: self.lane.subscribe(),
            ep,
            aliases,
        }
    }

    /// Forget an endpoint that went away (a disconnected or evicted client).
    pub fn retire(&self, ep: &Endpoint) {
        self.table().retire(ep.id);
    }

    /// The sidecar body (see [`RouteTable::snapshot`]).
    pub fn snapshot(&self) -> serde_json::Value {
        self.table().snapshot(Instant::now())
    }
}

impl RoutedFrame {
    /// Whether this frame goes to `ep`; a delivery is counted on its `tx`.
    pub fn accepts(&self, ep: &Endpoint) -> bool {
        let ok = match self {
            RoutedFrame::Fc { audience, .. } => audience.includes(ep.id),
            RoutedFrame::Client { src, .. } => *src != ep.id,
        };
        if ok {
            ep.counters.tx.fetch_add(1, Ordering::Relaxed);
        }
        ok
    }
}

/// The FC frame source of a connectionless proxy that serves many peers from
/// one socket: the flat fan-out, or the whole routed lane, filtered per peer
/// by the caller with [`RoutedFrame::accepts`].
#[derive(Debug)]
pub enum PeerFeed {
    Flat(broadcast::Receiver<RxFrame>),
    Lane(broadcast::Receiver<RoutedFrame>),
}

impl PeerFeed {
    /// The source for `hub`, given the flat fan-out to use when it is off.
    pub fn new(hub: &RouteHub, flat: broadcast::Receiver<RxFrame>) -> Self {
        if hub.enabled {
            PeerFeed::Lane(hub.lane.subscribe())
        } else {
            PeerFeed::Flat(flat)
        }
    }

    /// The next frame, with its routing when the lane is in use.
    pub async fn recv(&mut self) -> Result<(RxFrame, Option<RoutedFrame>), RecvError> {
        match self {
            PeerFeed::Flat(rx) => Ok((rx.recv().await?, None)),
            PeerFeed::Lane(rx) => {
                let routed = rx.recv().await?;
                let frame = match &routed {
                    RoutedFrame::Fc { frame, .. } | RoutedFrame::Client { frame, .. } => {
                        frame.clone()
                    }
                };
                Ok((frame, Some(routed)))
            }
        }
    }
}

/// One endpoint per peer address of a connectionless proxy, labelled
/// `<kind>/<addr>`, cached so the lookup does not allocate per frame, and
/// shared by the proxy's send and receive halves (clones share the cache).
#[derive(Debug, Clone)]
pub struct PeerEndpoints {
    hub: Arc<RouteHub>,
    kind: Arc<str>,
    map: Arc<Mutex<std::collections::HashMap<std::net::SocketAddr, Endpoint>>>,
}

impl PeerEndpoints {
    pub fn new(hub: Arc<RouteHub>, kind: &str) -> Self {
        Self {
            hub,
            kind: Arc::from(kind),
            map: Arc::default(),
        }
    }

    fn map(
        &self,
    ) -> std::sync::MutexGuard<'_, std::collections::HashMap<std::net::SocketAddr, Endpoint>> {
        self.map
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The endpoint of `addr`, registered on first sight.
    pub fn get(&self, addr: std::net::SocketAddr) -> Endpoint {
        self.map()
            .entry(addr)
            .or_insert_with(|| self.hub.register(&format!("{}/{addr}", self.kind)))
            .clone()
    }

    /// [`RouteHub::ingress`] for the peer at `addr`; a no-op when off.
    pub fn ingress(&self, addr: std::net::SocketAddr, data: &[u8]) {
        if self.hub.enabled {
            self.hub.ingress(&self.get(addr), data);
        }
    }

    /// Retire peers no longer in `live`, so the table tracks the peer set.
    pub fn retain(&self, live: &[std::net::SocketAddr]) {
        let hub = &self.hub;
        self.map().retain(|addr, ep| {
            let keep = live.contains(addr);
            if !keep {
                hub.retire(ep);
            }
            keep
        });
    }
}

/// A consumer's frame source: the flat FC fan-out, or the routed lane
/// filtered to one endpoint (plus the aliases sharing its transport).
#[derive(Debug)]
pub enum FrameFeed {
    Flat(broadcast::Receiver<RxFrame>),
    Routed {
        rx: broadcast::Receiver<RoutedFrame>,
        ep: Endpoint,
        aliases: Vec<EndpointId>,
    },
}

impl From<broadcast::Receiver<RxFrame>> for FrameFeed {
    fn from(rx: broadcast::Receiver<RxFrame>) -> Self {
        FrameFeed::Flat(rx)
    }
}

impl FrameFeed {
    /// The next frame for this consumer, with the broadcast receiver's error
    /// semantics (`Lagged` after a shed, `Closed` at shutdown).
    pub async fn recv(&mut self) -> Result<RxFrame, RecvError> {
        match self {
            FrameFeed::Flat(rx) => rx.recv().await,
            FrameFeed::Routed { rx, ep, aliases } => loop {
                let ours = |id: EndpointId| id == ep.id || aliases.contains(&id);
                let deliver = match rx.recv().await? {
                    RoutedFrame::Fc { audience, frame } => {
                        let to_us = match &audience {
                            Audience::All => true,
                            Audience::Only(eps) => eps.iter().any(|id| ours(*id)),
                        };
                        to_us.then_some(frame)
                    }
                    RoutedFrame::Client { src, frame } => (!ours(src)).then_some(frame),
                };
                if let Some(frame) = deliver {
                    ep.counters.tx.fetch_add(1, Ordering::Relaxed);
                    return Ok(frame);
                }
            },
        }
    }
}

/// Classify every FC frame onto the routed lane until `cancel`.
pub async fn run_classifier(
    hub: Arc<RouteHub>,
    mut frames: broadcast::Receiver<RxFrame>,
    cancel: Shutdown,
) {
    loop {
        tokio::select! {
            r = frames.recv() => match r {
                Ok(frame) => hub.publish_fc(frame),
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => break,
            },
            _ = cancel.wait() => break,
        }
    }
}

/// Write the route table and the requested FC sensor-stream rates to `path`
/// every [`SIDECAR_INTERVAL`] when the body changed, and at least every
/// [`SIDECAR_REFRESH`] regardless, until `cancel`.
pub async fn run_sidecar(
    fc: Arc<crate::connection::FcConnection>,
    path: PathBuf,
    cancel: Shutdown,
) {
    let mut last: SidecarLast = None;
    let mut tick = tokio::time::interval(SIDECAR_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = tick.tick() => {
                // Only a live link has streams requested from it.
                let rates = if fc.transport_open() {
                    fc.sensor_streams().sensor_rates_hz()
                } else {
                    serde_json::Map::new()
                };
                write_if_due(fc.routes(), rates, &path, &mut last, Instant::now());
            }
            _ = cancel.wait() => break,
        }
    }
}

/// One sidecar pass at `now`: serialise the route table plus
/// `sensor_rates_hz` (`{"fc_imu": 200.0, ...}`, the FC sensor streams
/// currently requested), and write it when it differs from the last body
/// written or that write is [`SIDECAR_REFRESH`] old. Returns whether it wrote.
pub fn write_if_due(
    hub: &RouteHub,
    sensor_rates_hz: serde_json::Map<String, serde_json::Value>,
    path: &std::path::Path,
    last: &mut SidecarLast,
    now: Instant,
) -> bool {
    let mut snapshot = hub.snapshot();
    if let Some(obj) = snapshot.as_object_mut() {
        obj.insert(
            "sensor_rates_hz".into(),
            serde_json::Value::Object(sensor_rates_hz),
        );
    }
    let Ok(body) = serde_json::to_vec(&snapshot) else {
        return false;
    };
    if let Some((prev, at)) = last.as_ref() {
        if *prev == body && now.saturating_duration_since(*at) < SIDECAR_REFRESH {
            return false;
        }
    }
    match ados_protocol::sidecar::write_durable(path, &body) {
        Ok(()) => {
            *last = Some((body, now));
            true
        }
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "route_sidecar_write_failed");
            false
        }
    }
}

#[cfg(test)]
mod tests;
