//! Ground-side presence beacon: emit + listen + the watchdog's presence cache.
//!
//! The 68-byte PresenceBeacon wire format and its HMAC are already a verified
//! port in `ados_radio::hop` (`build_presence_beacon` / `parse_presence_beacon`
//! / `derive_pair_key`); this module reuses them and adds the ground-station
//! glue:
//!
//! * `emit_loop` transmits a beacon every 10 s to **127.0.0.1:5810**, NOT 5803.
//!   That asymmetry is load-bearing: on the GS, `wfb_tx_control` binds UDP 5810
//!   (its outbound ingress over the air), while UDP 5803 is `wfb_rx_control`'s
//!   output AND the listener's bound port. Sending to 5803 would loop straight
//!   back through the kernel loopback into the listener and self-pair the GS
//!   with its own device-id. Sending to 5810 makes `wfb_tx_control` transmit the
//!   frame over RF instead.
//! * `PresenceCache` holds the decoded peer state, exposing `get_peer_presence`
//!   (`peer_channel` + `peer_last_seen_unix`), the watchdog's presence source.
//! * the listener decodes inbound beacons on the control port and updates the
//!   cache, skipping a frame whose device-id is our own (the same self-pair
//!   guard the Python listener applies).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use tokio::net::UdpSocket;

use ados_radio::hop::{
    build_presence_beacon, hop_announce_delay_ms, key_status, now_unix, now_unix_ms, pair_key_for,
    parse_hop_announce, parse_presence_beacon, PEER_STALE_SECS,
};

use crate::acquire::ChannelSetter;
use crate::watchdog::PresenceCache;
use crate::wfb_rx::{self, SharedValidCounter};
use crate::{FleetRegistry, FLEET_RECONCILE_INTERVAL, FLEET_REGISTRY_PATH};

mod cache;
mod emit;
mod follow;
mod listen;
pub use emit::{
    emit_loop, hop_supervisor_payload, hop_supervisor_persist_loop, linked_peers_payload,
    linked_peers_persist_loop, HOP_PERSIST_CADENCE,
};
use follow::handle_control_frame;
pub use follow::{hop_follow_refusal, FOLLOW_VERIFY_WINDOW};
pub use listen::{listen_loop, listen_supervisor};

/// Beacon cadence (10 s, matching the air side).
pub const PRESENCE_CADENCE: Duration = Duration::from_secs(10);

/// GS presence emit destination: `wfb_tx_control`'s loopback ingress. NOT 5803
/// (the listener's bound port); see the module docstring for the self-pair
/// trap that asymmetry avoids.
pub const PRESENCE_EMIT_PORT: u16 = 5810;

/// The control-plane port the listener binds for inbound beacons from fleet
/// slot `slot` (the same port that slot's `wfb_rx -p 1` re-emits decoded
/// HopAnnounce/Presence frames on).
pub const fn presence_listen_port(slot: u8) -> u16 {
    ados_radio::config::CONTROL_RX_PORT_BASE + slot as u16
}

/// HopAck echo destination: `wfb_tx_control`'s loopback ingress (the same port
/// the presence emit uses). The drone broadcasts a HopAnnounce and waits for a
/// HopAck echo before executing its epoch-synced channel flip; sending the
/// verbatim 51 bytes here makes `wfb_tx_control` transmit the ACK back over RF,
/// so the drone's "acked" gate goes true and coordinated hopping fires.
pub const HOP_ACK_ECHO_PORT: u16 = 5810;

/// HopAnnounce/HopAck wire length (the 51-byte control frame). A frame of this
/// length on the control port is a hop frame; a 68-byte frame is a
/// PresenceBeacon. The length gate is checked before the magic/HMAC verify.
const HOP_FRAME_LEN: usize = 51;
/// PresenceBeacon wire length (68 bytes).
const PRESENCE_FRAME_LEN: usize = 68;

/// Resolve the symmetric pair key used to authenticate the presence beacon
/// and hop frames, reusing the verified `ados_radio::hop::pair_key_for`.
///
/// Reads the 64-byte shared key through `ados_radio::paths::load_shared_key`
/// (`/etc/drone.key`). A node that has never been bound (no key file) uses the
/// deterministic cold-start constant so a pre-bind beacon still parses. A key
/// file that exists but cannot be read yields `None`: nothing is authenticated
/// until it reads cleanly, rather than everything being authenticated under a
/// constant anyone can compute. The second value is the status label the hop
/// snapshot publishes (`bound`, `cold_start`, `key_unavailable`).
///
/// HARD CONSTRAINT, do not reintroduce the gs.key/tx.key divergence: an earlier
/// version hashed `/etc/ados/wfb/tx.key` on the drone and `/etc/ados/wfb/rx.key`
/// on the GS. Those are the two DIFFERENT halves of the crypto_box pair (the
/// drone keeps `drone.key`, the GS keeps `gs.key`), so the derived HMAC key
/// diverged across the rigs and every beacon was silently dropped at the
/// listener. The shared file is `/etc/drone.key`, present byte-identical on both
/// rigs after bind. Only ever derive from that.
pub fn resolve_pair_key() -> (Option<[u8; 32]>, &'static str) {
    let shared = ados_radio::paths::load_shared_key();
    let key = pair_key_for(&shared);
    // Log the transition into and out of the refused state, not every frame.
    let unavailable = key.is_none();
    if KEY_UNAVAILABLE_LOGGED.swap(unavailable, Ordering::Relaxed) != unavailable {
        if let ados_radio::paths::SharedKey::Unavailable(reason) = &shared {
            tracing::error!(%reason, "ground_pair_key_unavailable: hop and presence authentication refused");
        } else {
            tracing::info!("ground_pair_key_readable_again");
        }
    }
    (key, key_status(&shared))
}

/// Whether the refused-key state was the last one logged.
static KEY_UNAVAILABLE_LOGGED: AtomicBool = AtomicBool::new(false);

/// Cap on the hop-history ring (matches the Python listener's 32-entry trim).
const HOP_HISTORY_CAP: usize = 32;

/// One recorded GS-side channel-follow event for the hop-supervisor snapshot.
/// Shape matches the Python `HopListener` history entry exactly: `at` (wall
/// unix), `from`/`to` channel numbers, the `trigger` label, and the `ok` flag.
#[derive(Debug, Clone, serde::Serialize)]
pub struct HopFollowEntry {
    pub at: f64,
    pub from: u8,
    pub to: u8,
    pub trigger: String,
    pub ok: bool,
}

/// The GS-side hop-supervisor snapshot, byte-shaped like the Python
/// `HopListener.snapshot()` so a reader (REST + the on-box channel-hops page)
/// sees the same JSON whichever language drove the receive plane. The drone-only
/// threshold fields are `null` on the listener side; `source` is `"listener"`.
/// `last_refusal` is the most recent announce this station would not follow and
/// why; `key_status` says which key the control plane authenticates under.
#[derive(Debug, Clone, serde::Serialize)]
pub struct HopSnapshot {
    pub enabled: bool,
    pub band: String,
    pub hop_period_seconds: Option<f64>,
    pub loss_threshold_percent: Option<f64>,
    pub rssi_threshold_dbm: Option<f64>,
    pub last_hop_at: f64,
    pub history: Vec<HopFollowEntry>,
    pub source: &'static str,
    pub last_refusal: Option<HopRefusal>,
    pub key_status: Option<&'static str>,
}

/// An announced hop this ground station refused to ack or follow.
#[derive(Debug, Clone, serde::Serialize)]
pub struct HopRefusal {
    /// Wall-clock unix of the refusal.
    pub at: f64,
    /// The announced target channel.
    pub channel: u8,
    /// `fleet_hop_refused`, `no_receive_iface`, `channel_not_permitted`.
    pub reason: &'static str,
}

/// Decoded peer-presence cache, shared between the listener (writer) and the
/// watchdog (reader). `peer_channel` + `peer_last_seen_unix` are the two
/// fields the watchdog consumes. The hop-follow history ring + `last_hop_at`
/// let the receive plane export `hop-supervisor.json` from the same cache the
/// listener already owns.
#[derive(Debug, Default)]
struct PeerState {
    peer_device_id: Option<String>,
    peer_role: Option<String>,
    peer_channel: Option<u8>,
    peer_rssi_dbm: Option<i8>,
    peer_last_seen_unix: Option<f64>,
    /// Channel-follow history: a new entry lands each time the peer announces a
    /// channel that differs from where the receiver last followed it, mirroring
    /// the Python listener's `hop_listener_followed_peer_channel`. Trimmed to the
    /// last `HOP_HISTORY_CAP` entries.
    hop_history: Vec<HopFollowEntry>,
    /// Wall-clock unix of the last recorded follow (0.0 until the first).
    last_hop_at: f64,
    /// Every distinct WFB peer this receiver currently decodes a beacon from,
    /// keyed by the peer's device-id. The fields above track only the FRESHEST
    /// peer (the watchdog's presence signal); this map is the full set a ground
    /// station relays, published to `linked-peers.json` for the heartbeat's
    /// `linkedPeers[]`. A ground station can relay more than one drone, so the
    /// heartbeat needs the list, not just the last-heard scalar. Pruned to the
    /// fresh set at read time.
    peers: std::collections::BTreeMap<String, LinkedPeer>,
    /// Identities learned from the auxiliary lane (device id -> optional name).
    ///
    /// A separate map from `peers` on purpose: these come from a different
    /// source with different evidence. A beacon proves an RF decode and carries
    /// a channel and an RSSI; an identity frame proves the peer is speaking and
    /// carries a name, but says nothing about signal. Keeping them apart is what
    /// stops the enrichment from inventing radio telemetry it never measured.
    aux_identities: std::collections::BTreeMap<String, Option<String>>,
    /// The most recent announce this station refused to follow.
    last_refusal: Option<HopRefusal>,
    /// The key status the listener last authenticated under.
    key_status: Option<&'static str>,
}

/// Schema version for the `linked-peers.json` sidecar (a best-effort drift
/// signal a reader can gate on, mirroring the inline `version` field the
/// hop-supervisor sidecar carries). Bump on any breaking entry-shape change.
pub const LINKED_PEERS_SIDECAR_VERSION: u16 = 1;

/// A peer is dropped from the published list once its last decoded beacon is
/// older than this: the radio's own peer-stale threshold, after which the drone
/// itself has declared the link gone and returned home. Holding the peer for
/// longer showed a silent drone as linked well after both ends had given up.
pub const LINKED_PEER_STALE_AFTER_S: f64 = PEER_STALE_SECS;

/// Persist cadence for `linked-peers.json` (5 s, matching the hop-supervisor
/// persister: the GCS heartbeat is 5 s and the fleet list does not need
/// sub-second freshness).
pub const LINKED_PEERS_PERSIST_CADENCE: Duration = Duration::from_secs(5);

/// One decoded WFB peer as it is published to `linked-peers.json`. The fields
/// are exactly what the PresenceBeacon + the listener already carry (device-id,
/// role, channel, RSSI), the fleet slot whose control port it was decoded on,
/// and the wall-clock time of the last decode. Serialized snake_case; the
/// heartbeat producers remap to the camelCase wire keys
/// (`deviceId`/`rssiDbm`/`seenAtUnix`).
#[derive(Debug, Clone, serde::Serialize)]
pub struct LinkedPeer {
    pub device_id: String,
    pub role: String,
    pub channel: u8,
    pub rssi_dbm: i8,
    /// Wall-clock unix seconds of the last decoded beacon from this peer.
    pub last_seen_unix: f64,
    /// The fleet slot whose receive chain decoded the beacon. A peer heard on a
    /// slot other than the one it was paired to is told apart from a healthy
    /// one by this.
    pub slot: u8,
    /// The peer's human-facing name, learned from the auxiliary lane's identity
    /// frame rather than the beacon. The beacon is a fixed 68 bytes with its
    /// identity field already full, so a name cannot travel on it.
    ///
    /// Additive and skipped when absent: existing readers ignore unknown keys,
    /// and the field is present only when the peer actually told us its name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// Thread-safe presence cache. Implements the watchdog's `PresenceCache` so it
/// can be handed straight to the receive loop's watchdog as its presence seam.
#[derive(Debug, Default, Clone)]
pub struct GsPresenceCache {
    inner: Arc<Mutex<PeerState>>,
}

/// Restart-accounting snapshot for the presence listener supervisor.
#[derive(Debug, Clone, serde::Serialize)]
struct ListenerHealth {
    /// Times the listener task has been (re)spawned, including the first start.
    starts: u64,
    /// Times a listener generation ended and had to be re-spawned (a fatal bind
    /// error or a panic). Zero on a healthy service.
    restarts: u64,
    /// Times the listener task ended by panicking (a `JoinError`), surfaced here
    /// because the supervisor awaits the handle instead of dropping it.
    panics: u64,
    /// Last exit reason, for the operator-facing panel.
    last_exit: String,
    /// Wall-clock unix of the last (re)start.
    started_at_unix: f64,
}

/// How one scheduled follow ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FollowOutcome {
    /// Retuned, and the new channel delivered traffic (or there was no
    /// receive chain to verify against).
    Followed,
    /// The retune failed; the radio was put back on the channel it left.
    RevertedRetuneFailed,
    /// Retuned, but no valid packet arrived inside [`FOLLOW_VERIFY_WINDOW`];
    /// the radio was put back on the channel it left.
    RevertedNoTraffic,
}

/// Drives the GS receive radios to follow a drone-announced channel hop. Holds
/// the same group `ChannelSetter` the acquirer uses plus the radio-group cell
/// the receive loop writes, so on a verified HopAnnounce the listener retunes
/// the live receive adapters to the announced channel when the countdown expires —
/// the coordinated counterpart to the drone's flip. Without this the GS only
/// learned the new channel into its cache and waited for the valid-packet
/// watchdog to notice the blackout and sweep, costing a guaranteed gap on every
/// hop.
///
/// A follow is verified: the receive generation's valid-packet counter must
/// advance on the new channel inside [`FOLLOW_VERIFY_WINDOW`], or the radio goes
/// back to the channel it left. That is what recovers a hop the drone acked but
/// then failed to carry out.
#[derive(Clone)]
pub struct HopFollower {
    setter: Arc<dyn ChannelSetter>,
    radio_group: crate::wfb_rx::RadioGroup,
    /// The current receive generation's valid-decode counter. Replaced on every
    /// generation; `None` before the first.
    valid: Arc<Mutex<Option<SharedValidCounter>>>,
    /// The channel a follow is already scheduled toward. The drone repeats its
    /// announce until it hears the ack, so one hop arrives several times; only
    /// the first schedules a retune.
    pending: Arc<Mutex<Option<u8>>>,
    verify_window: Duration,
}

/// Where a control frame arrived: the fleet slot whose control port decoded
/// it, and how many slots the fleet has registered right now.
#[derive(Debug, Clone, Copy)]
struct ControlContext {
    slot: u8,
    fleet_slots: usize,
}

#[cfg(test)]
mod tests;
