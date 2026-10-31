//! Cache the node descriptions arriving on the auxiliary lane's status and
//! identity channels.
//!
//! The drone pushes a compact status snapshot and an identity frame over the
//! radio because it has no address on which it could be asked. This module is
//! where those land on the ground side: one record per peer device id, holding
//! the newest of each frame and when it arrived.
//!
//! The cache is in the receive process, but the surface an operator reads is
//! served by a different process, so the records are published to a sidecar the
//! way every other cross-process state on this node is.
//!
//! ## Age travels with the data, always
//!
//! Every record carries the wall-clock time each frame arrived, and every reader
//! is handed an age computed against a caller-supplied `now`. A record whose
//! status is older than [`STATUS_STALE_AFTER_S`] is not served as current: the
//! peer is either dropped from the published set, or (when its identity is still
//! fresh) published with its status omitted and a `status_fresh: false` flag.
//!
//! That distinction matters. A drone whose radio went quiet and a drone sitting
//! at 12% CPU look identical if the last snapshot keeps being served, and the
//! operator would be reading a number from a link that is gone. Losing the
//! reading is correct; keeping it is a lie.
//!
//! ## Bounded
//!
//! The peer map is capped at [`MAX_PEERS`], evicting the stalest, and each record
//! holds one status and one identity rather than a history. Frames arrive off a
//! radio lane this node does not control, so a peer count that grew with the
//! traffic would be a memory fault waiting for a misbehaving or hostile
//! transmitter.
//!
//! ## Ordering
//!
//! Status frames carry a sequence number and travel over an unreliable datagram
//! lane, so they can arrive out of order. A frame older than the one already
//! held is DROPPED rather than allowed to overwrite it, which would make a
//! surface flicker backwards in time. The comparison is wrapping, so a sequence
//! that rolls over is not mistaken for a reordering; a producer that restarted
//! (and so restarted its sequence) is accepted, because by then the held record
//! has aged past the stale window.

use ados_protocol::shutdown::Shutdown;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ados_protocol::node_status::{NodeIdentity, NodeStatus};
use serde_json::{json, Value};

mod cache;
mod sidecar;
use sidecar::{expand_status, round2};
pub use sidecar::{persist_loop, slot_system_ids_from_sidecar};

/// Sidecar schema version, a drift signal for the reader.
pub const AUX_PEERS_SIDECAR_VERSION: u16 = 1;

/// The sidecar filename under the run dir.
pub const AUX_PEERS_SIDECAR: &str = "relayed-status.json";

/// A status snapshot older than this is not served as a current reading.
///
/// Generous relative to the 1 Hz default cadence: a handful of lost datagrams on
/// a radio lane is normal and must not blank the surface, while a link that has
/// genuinely stopped is caught well inside the time an operator would act on it.
pub const STATUS_STALE_AFTER_S: f64 = 15.0;

/// A peer with no frame of any kind inside this window is dropped entirely.
pub const PEER_STALE_AFTER_S: f64 = 120.0;

/// A slot's flight-controller system id older than this no longer counts as
/// that slot's current identity. The drone tees its autopilot's 1 Hz HEARTBEAT,
/// so this rides out a burst of lost datagrams while a drone that has gone
/// quiet stops holding a system id another aircraft may now need.
pub const SYSTEM_ID_STALE_AFTER_S: f64 = 15.0;

/// Most peers held at once.
///
/// A fleet is up to `FLEET_MAX_SLOTS` (24) drones on one ground radio, and the
/// cap has to clear that with room for a re-pair that has not yet aged its old
/// entry out. What it still stops is a misbehaving transmitter growing the map
/// without bound.
pub const MAX_PEERS: usize = 64;

/// How often the sidecar is rewritten.
pub const PERSIST_CADENCE: Duration = Duration::from_secs(2);

/// Wall-clock unix seconds.
fn now_unix() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// One relayed peer: the newest status and identity, and when each arrived.
#[derive(Debug, Clone, Default)]
struct PeerRecord {
    status: Option<NodeStatus>,
    status_at: f64,
    identity: Option<NodeIdentity>,
    identity_at: f64,
    /// Status frames accepted for this peer.
    status_frames: u64,
    /// Status frames dropped for arriving older than the one already held.
    out_of_order: u64,
}

/// Whether `candidate` is newer than `held` under wrapping sequence arithmetic.
///
/// A datagram lane reorders, so a frame that arrives is not necessarily the
/// latest. Wrapping comparison treats the smaller half of the number space as
/// "ahead", which keeps a sequence rollover from reading as a huge step
/// backwards.
fn seq_is_newer(candidate: u32, held: u32) -> bool {
    candidate != held && candidate.wrapping_sub(held) < u32::MAX / 2
}

/// The relayed-peer cache, shared between the lane reader and the persister.
#[derive(Debug, Default, Clone)]
pub struct AuxPeerCache {
    inner: Arc<Mutex<BTreeMap<String, PeerRecord>>>,
    /// The flight-controller system id most recently seen on each fleet slot,
    /// with the wall-clock time it was last seen.
    ///
    /// Kept apart from the status-derived records above because it comes from a
    /// different source with different evidence: a status frame is a node
    /// describing itself, while this is read off the MAVLink header of the
    /// HEARTBEAT that node's flight controller actually produced. Only the
    /// second can answer whether two aircraft are addressable apart, which is
    /// the question that matters here -- two flight controllers on one system id
    /// are ONE vehicle to a ground station, and a command sent to that id is
    /// accepted by both of them.
    system_ids: Arc<Mutex<BTreeMap<u8, (u8, f64)>>>,
}

/// One slot's flight-controller identity as the fleet roster reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotSystemId {
    /// The system id the slot's flight controller is heartbeating with.
    pub fc_system_id: u8,
    /// Whether another linked slot presents the same system id.
    pub conflict: bool,
}

#[cfg(test)]
pub(crate) mod tests;
