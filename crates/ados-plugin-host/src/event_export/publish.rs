//! The export socket's publish op: a host service publishing a host-owned
//! topic onto the bus.
//!
//! The plugin host owns the bus, so a host service in another process (the
//! MAVLink router publishing the operator's shared phone position) reaches it
//! here. A publisher writes one `{"v": 1, "op": "publish", "topic", "payload"}`
//! line per event and reads nothing back unless a line is refused, when the
//! host answers `{"v": 1, "error": "<reason>"}`.
//!
//! What may publish, and what, is narrow on purpose:
//!
//! * only a host-service peer: root or this daemon's own uid by the kernel's
//!   peer credentials, and never a plugin-plane uid or group, whatever else it
//!   is. Any other peer is refused on its first publish line, counted and
//!   disconnected. The socket already sits in the root-only control dir; this
//!   check holds even if that dir's mode is ever relaxed;
//! * only the topics in [`crate::handlers::HOST_PLUGIN_TOPICS`];
//! * a payload of at most [`EVENT_PAYLOAD_MAX_BYTES`] msgpack bytes, the same
//!   cap a plugin's own publish meets;
//! * at most [`PUBLISH_RATE_PER_S`] events per second per publisher uid,
//!   across all its connections.
//!
//! Every accepted event is published with publisher id
//! [`crate::vehicle_events::HOST_PUBLISHER`].

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use ados_protocol::ipc::PeerClass;
use serde::Deserialize;
use serde_json::json;
use tokio::io::BufReader;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::time::Instant;

use super::{write_line, EVENT_EXPORT_VERSION};
use crate::handlers::{EventBus, EVENT_PAYLOAD_MAX_BYTES, HOST_PLUGIN_TOPICS};

/// Longest publish line. JSON spells a payload out larger than its msgpack
/// form, so the line may be twice the payload cap; the cap itself is applied to
/// the msgpack encoding.
pub(super) const PUBLISH_LINE_MAX: usize = 2 * EVENT_PAYLOAD_MAX_BYTES;

/// Sustained events per second one publisher uid may publish.
pub const PUBLISH_RATE_PER_S: f64 = 20.0;

/// Events one publisher uid may publish back to back before the rate applies.
pub const PUBLISH_BURST: f64 = 20.0;

/// Who is on the other end of an export connection, from the kernel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublishPeer {
    /// The peer may publish: a host service off the plugin plane.
    pub allowed: bool,
    /// The peer's uid, the rate limit's key.
    pub uid: u32,
}

/// Whether a peer may publish: a host-service peer ([`PeerClass::Service`],
/// root or this daemon's uid) that is not on the plugin plane. Pure over the
/// kernel-reported credentials so the decision is testable without a second
/// uid.
pub fn publish_peer_allowed(
    class: PeerClass,
    uid: u32,
    gids: &[u32],
    plugin_user_uid: Option<u32>,
    plugin_gid: Option<u32>,
) -> bool {
    class == PeerClass::Service
        && !ados_protocol::ipc::is_plugin_peer(uid, gids, plugin_user_uid, plugin_gid)
}

/// The plugin plane's ids: the legacy shared plugin user and the plugin group.
/// Resolved once when the socket is served, not per connection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PluginPlane {
    pub user_uid: Option<u32>,
    pub gid: Option<u32>,
}

impl PluginPlane {
    /// Look the ids up on this host; an absent user or group is `None`.
    pub fn resolve() -> Self {
        PluginPlane {
            user_uid: nix::unistd::User::from_name(ados_protocol::ipc::LEGACY_PLUGIN_USER)
                .ok()
                .flatten()
                .map(|u| u.uid.as_raw()),
            gid: nix::unistd::Group::from_name(ados_protocol::ipc::PLUGIN_GROUP)
                .ok()
                .flatten()
                .map(|g| g.gid.as_raw()),
        }
    }
}

/// Classify an accepted export connection for the publish op, from the
/// kernel's credentials including the supplementary groups. Unreadable
/// credentials may not publish.
pub(super) fn publish_peer(
    stream: &tokio::net::UnixStream,
    class: PeerClass,
    plane: PluginPlane,
) -> PublishPeer {
    let Ok((uid, gids)) = ados_protocol::ipc::peer_credentials(stream) else {
        return PublishPeer {
            allowed: false,
            uid: u32::MAX,
        };
    };
    PublishPeer {
        allowed: publish_peer_allowed(class, uid, &gids, plane.user_uid, plane.gid),
        uid,
    }
}

/// Why a publish line was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The peer is not a host service.
    Peer,
    /// The topic is not a host-owned one.
    Topic,
    /// The line or the payload is over its cap.
    Oversize,
    /// The publisher is over its rate.
    Rate,
    /// Not a well-formed publish line.
    Malformed,
}

impl Refusal {
    fn reason(self) -> &'static str {
        match self {
            Self::Peer => "publish is not permitted for this peer",
            Self::Topic => "topic is not a host-published topic",
            Self::Oversize => "event payload exceeds the size cap",
            Self::Rate => "publish rate exceeded",
            Self::Malformed => "malformed publish line",
        }
    }
}

/// The publish gate shared by every export connection: the per-uid rate
/// buckets and the refusal counts.
#[derive(Debug, Default)]
pub struct PublishGate {
    buckets: parking_lot::Mutex<HashMap<u32, (f64, Instant)>>,
    pub published: AtomicU64,
    pub refused_peer: AtomicU64,
    pub refused_topic: AtomicU64,
    pub refused_oversize: AtomicU64,
    pub refused_rate: AtomicU64,
    pub refused_malformed: AtomicU64,
}

#[derive(Debug, Deserialize)]
struct PublishLine {
    v: u32,
    op: String,
    topic: String,
    payload: serde_json::Value,
}

impl PublishGate {
    fn count(&self, refusal: Refusal) -> u64 {
        let counter = match refusal {
            Refusal::Peer => &self.refused_peer,
            Refusal::Topic => &self.refused_topic,
            Refusal::Oversize => &self.refused_oversize,
            Refusal::Rate => &self.refused_rate,
            Refusal::Malformed => &self.refused_malformed,
        };
        counter.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Take one token from `uid`'s bucket at `now`.
    fn take_token(&self, uid: u32, now: Instant) -> bool {
        let mut buckets = self.buckets.lock();
        let (tokens, last) = buckets.entry(uid).or_insert((PUBLISH_BURST, now));
        let refill = now.saturating_duration_since(*last).as_secs_f64() * PUBLISH_RATE_PER_S;
        *tokens = (*tokens + refill).min(PUBLISH_BURST);
        *last = now;
        if *tokens >= 1.0 {
            *tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Check one publish line from `peer` at `now`: the topic and msgpack
    /// payload to publish, or why not.
    pub fn admit(
        &self,
        peer: PublishPeer,
        line: &[u8],
        now: Instant,
    ) -> Result<(&'static str, rmpv::Value), Refusal> {
        if !peer.allowed {
            return Err(Refusal::Peer);
        }
        if line.len() > PUBLISH_LINE_MAX {
            return Err(Refusal::Oversize);
        }
        let req: PublishLine = serde_json::from_slice(line).map_err(|_| Refusal::Malformed)?;
        if req.v != EVENT_EXPORT_VERSION || req.op != "publish" || !req.payload.is_object() {
            return Err(Refusal::Malformed);
        }
        let Some(&(topic, _)) = HOST_PLUGIN_TOPICS.iter().find(|(t, _)| *t == req.topic) else {
            return Err(Refusal::Topic);
        };
        let payload = rmpv::ext::to_value(&req.payload).map_err(|_| Refusal::Malformed)?;
        let size = rmp_serde::to_vec(&payload).map_or(usize::MAX, |b| b.len());
        if size > EVENT_PAYLOAD_MAX_BYTES {
            return Err(Refusal::Oversize);
        }
        if !self.take_token(peer.uid, now) {
            return Err(Refusal::Rate);
        }
        Ok((topic, payload))
    }

    /// Record a refusal and build its error line.
    fn refuse(&self, peer: PublishPeer, refusal: Refusal) -> Vec<u8> {
        let count = self.count(refusal);
        match refusal {
            Refusal::Rate => {
                tracing::debug!(peer_uid = peer.uid, count, "event export publish over rate")
            }
            _ => tracing::warn!(
                peer_uid = peer.uid,
                reason = refusal.reason(),
                count,
                "event export publish refused"
            ),
        }
        let mut body =
            serde_json::to_vec(&json!({"v": EVENT_EXPORT_VERSION, "error": refusal.reason()}))
                .unwrap_or_default();
        body.push(b'\n');
        body
    }
}

/// Serve a publishing connection whose first line, `first`, asked to publish.
/// Runs until the peer closes, sends an over-long line, or is refused as a peer.
pub(super) async fn serve_publisher(
    bus: &EventBus,
    gate: &PublishGate,
    peer: PublishPeer,
    first: Vec<u8>,
    reader: &mut BufReader<OwnedReadHalf>,
    write_half: &mut OwnedWriteHalf,
) {
    let mut next = Some(first);
    loop {
        let line = match next.take() {
            Some(line) => line,
            None => match ados_protocol::ipc::read_newline_line(reader, PUBLISH_LINE_MAX).await {
                Ok(Some(line)) => line,
                Ok(None) => return,
                Err(_) => {
                    let _ = write_line(write_half, &gate.refuse(peer, Refusal::Oversize)).await;
                    return;
                }
            },
        };
        match gate.admit(peer, &line, Instant::now()) {
            Ok((topic, payload)) => {
                gate.published.fetch_add(1, Ordering::Relaxed);
                crate::vehicle_events::publish_host_event(bus, topic, payload);
            }
            Err(refusal) => {
                let body = gate.refuse(peer, refusal);
                if !write_line(write_half, &body).await || refusal == Refusal::Peer {
                    return;
                }
            }
        }
    }
}
