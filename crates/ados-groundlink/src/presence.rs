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

/// Read the persistent device-id (`/etc/ados/device-id`), trimmed. Empty when
/// absent; the emit loop logs and still sends (an empty id zero-pads).
fn read_device_id() -> String {
    std::fs::read_to_string("/etc/ados/device-id")
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

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
/// watchdog (reader). Mirrors the Python `HopListener.get_peer_presence`
/// surface: `peer_channel` + `peer_last_seen_unix` are the two fields the
/// watchdog consumes. The hop-follow history ring + `last_hop_at` mirror the
/// Python listener's snapshot surface so the receive plane can export
/// `hop-supervisor.json` from the same cache the listener already owns.
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

impl GsPresenceCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a verified inbound beacon (writer side, from the listener).
    ///
    /// When the announced channel differs from where the receiver last followed
    /// the peer, a channel-follow entry is appended to the hop history ring (the
    /// GS-side equivalent of an actuated hop: the receiver tracks the channel the
    /// transmitter advertises). The ring is trimmed to the last `HOP_HISTORY_CAP`
    /// entries, matching the Python listener. A PresenceBeacon carries no hop
    /// trigger, so its follow entries are labelled "periodic".
    fn record_peer(&self, device_id: String, role: String, channel: u8, rssi_dbm: i8, slot: u8) {
        let mut s = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let prev_channel = s.peer_channel;
        let now = now_unix();
        // The multi-peer map: upsert this peer keyed by its device-id so a ground
        // station relaying several drones reports all of them. Keyed insert also
        // means a re-heard peer refreshes (not duplicates) its entry.
        s.peers.insert(
            device_id.clone(),
            LinkedPeer {
                device_id: device_id.clone(),
                role: role.clone(),
                channel,
                rssi_dbm,
                last_seen_unix: now,
                slot,
                // Beacons carry no name; the auxiliary lane supplies it at read
                // time, so this stays absent rather than being invented here.
                name: None,
            },
        );
        // The scalar fields track the FRESHEST peer (this beacon), the watchdog's
        // presence + channel-follow signal — unchanged single-peer behaviour.
        s.peer_device_id = Some(device_id);
        s.peer_role = Some(role);
        s.peer_channel = Some(channel);
        s.peer_rssi_dbm = Some(rssi_dbm);
        s.peer_last_seen_unix = Some(now);
        if prev_channel != Some(channel) {
            Self::push_follow(&mut s, prev_channel, channel, "periodic");
        }
    }

    /// The fresh set of decoded WFB peers (last beacon within
    /// [`LINKED_PEER_STALE_AFTER_S`]), newest-first. Prunes stale entries from
    /// the map as a side effect so a peer that stops beaconing is dropped, never
    /// republished as a stale confident entry. Empty until a peer is heard.
    /// Replace the set of identities learned from the auxiliary lane.
    ///
    /// Called with the current fresh set, so an identity whose peer stopped
    /// speaking disappears rather than lingering as a confident label.
    pub fn set_aux_identities(&self, identities: Vec<(String, Option<String>)>) {
        let mut s = self.inner.lock().unwrap();
        s.aux_identities = identities.into_iter().collect();
    }

    pub fn linked_peers(&self) -> Vec<LinkedPeer> {
        let mut s = self.inner.lock().unwrap();
        let now = now_unix();
        s.peers
            .retain(|_, p| (now - p.last_seen_unix) <= LINKED_PEER_STALE_AFTER_S);
        let aux = s.aux_identities.clone();
        let mut out: Vec<LinkedPeer> = s.peers.values().cloned().collect();

        // Enrich each beacon-derived row with what the auxiliary lane learned.
        //
        // A beacon carries a device id, but a node with no persistent id emits an
        // empty one, and a row with an empty id is dropped by every downstream
        // reader — so the peer vanishes entirely despite being audibly present.
        // The identity frame is the only place the real id can come from in that
        // case, so it is adopted here.
        //
        // Only when there is exactly ONE identity to adopt. With several peers
        // and no id on the beacon there is no way to tell which is which, and a
        // guess would attach one node's identity to another node's signal.
        let sole_aux_id = (aux.len() == 1).then(|| aux.iter().next().expect("len is 1"));
        for peer in out.iter_mut() {
            if peer.device_id.is_empty() {
                if let Some((id, name)) = sole_aux_id {
                    peer.device_id = id.clone();
                    peer.name = name.clone();
                }
                continue;
            }
            if let Some(name) = aux.get(&peer.device_id) {
                peer.name = name.clone();
            }
        }

        // Freshest peer first, so a single-drone reader that takes `[0]` gets the
        // most-recent decode.
        out.sort_by(|a, b| {
            b.last_seen_unix
                .partial_cmp(&a.last_seen_unix)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        out
    }

    /// Record a verified HopAnnounce (writer side, from the listener).
    ///
    /// A HopAnnounce is the drone telling the receiver which channel to follow
    /// to and why (its `trigger`). Unlike a PresenceBeacon it carries no device
    /// identity, RSSI, or liveness signal, so it must NOT reset
    /// `peer_last_seen_unix` (the watchdog's presence-age gate is beacon-driven)
    /// and must NOT clobber the peer identity learned from a prior beacon. It
    /// only updates the channel to follow and appends a follow entry with the
    /// announce's real trigger. This is the back-fill: existing identity is
    /// preserved, the channel-follow is recorded with the correct trigger label.
    fn record_hop_announce(&self, channel: u8, trigger: &str) {
        let mut s = self.inner.lock().unwrap();
        let prev_channel = s.peer_channel;
        if prev_channel != Some(channel) {
            s.peer_channel = Some(channel);
            Self::push_follow(&mut s, prev_channel, channel, trigger);
        }
    }

    /// Record an announce this station refused to follow, for the snapshot.
    fn record_hop_refusal(&self, channel: u8, reason: &'static str) {
        let mut s = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        s.last_refusal = Some(HopRefusal {
            at: now_unix(),
            channel,
            reason,
        });
    }

    /// Record which key the control plane is authenticating under.
    fn set_key_status(&self, status: &'static str) {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .key_status = Some(status);
    }

    /// Append a channel-follow entry to the bounded hop-history ring (shared by
    /// the beacon and HopAnnounce writers). `from` is the prior channel (0 when
    /// unknown, matching the Python listener) and `trigger` is the follow label.
    fn push_follow(s: &mut PeerState, prev_channel: Option<u8>, channel: u8, trigger: &str) {
        let now = now_unix();
        s.hop_history.push(HopFollowEntry {
            at: now,
            // The first beacon has no prior channel; record 0 (the Python
            // listener uses 0 for an unknown `from`).
            from: prev_channel.unwrap_or(0),
            to: channel,
            trigger: trigger.to_string(),
            ok: true,
        });
        if s.hop_history.len() > HOP_HISTORY_CAP {
            let trim = s.hop_history.len() - HOP_HISTORY_CAP;
            s.hop_history.drain(0..trim);
        }
        s.last_hop_at = now;
    }

    /// The hop-supervisor snapshot in the Python `HopListener.snapshot()` shape.
    /// `band` is the configured radio band the receive plane is sweeping; the
    /// drone-only thresholds are `null` on the listener side and `source` is
    /// `"listener"`.
    pub fn hop_snapshot(&self, band: &str) -> HopSnapshot {
        let s = self.inner.lock().unwrap();
        HopSnapshot {
            enabled: true,
            band: band.to_string(),
            hop_period_seconds: None,
            loss_threshold_percent: None,
            rssi_threshold_dbm: None,
            last_hop_at: s.last_hop_at,
            history: s.hop_history.clone(),
            source: "listener",
            last_refusal: s.last_refusal.clone(),
            key_status: s.key_status,
        }
    }

    /// The peer's last announced channel (the watchdog's beacon-guided hint).
    pub fn peer_channel(&self) -> Option<u8> {
        self.inner.lock().unwrap().peer_channel
    }

    /// Wall-clock unix of the last verified beacon (None until one is seen).
    pub fn peer_last_seen_unix(&self) -> Option<f64> {
        self.inner.lock().unwrap().peer_last_seen_unix
    }
}

impl PresenceCache for GsPresenceCache {
    /// Seconds since the last verified beacon, or `None` when none seen. Clamped
    /// at zero so a wall-clock step backwards never yields a negative age.
    fn presence_age_s(&self) -> Option<f64> {
        let last = self.inner.lock().unwrap().peer_last_seen_unix?;
        if last <= 0.0 {
            return None;
        }
        Some((now_unix() - last).max(0.0))
    }

    fn announced_channel(&self) -> Option<u8> {
        self.peer_channel()
    }
}

/// Emit a PresenceBeacon to `wfb_tx_control`'s loopback ingress every
/// `PRESENCE_CADENCE`. `channel` is read fresh each tick through the supplied
/// closure so a channel change between ticks is reflected. Returns only on a
/// fatal socket-bind error or task cancellation.
pub async fn emit_loop<F>(channel_fn: F) -> std::io::Result<()>
where
    F: Fn() -> u8 + Send,
{
    let device_id = read_device_id();
    if device_id.is_empty() {
        tracing::warn!("ground_presence_emit_no_device_id");
    }
    // Bind an ephemeral source port; we only ever send.
    let sock = UdpSocket::bind((std::net::Ipv4Addr::UNSPECIFIED, 0)).await?;
    let target = (std::net::Ipv4Addr::LOCALHOST, PRESENCE_EMIT_PORT);
    tracing::info!(device_id = %device_id, cadence_s = 10, "ground_presence_emit_started");

    loop {
        let (pair_key, _) = resolve_pair_key();
        // With no readable key there is nothing to sign a beacon with that a
        // peer should trust; stay silent until the key reads cleanly.
        if let Some(pair_key) = pair_key {
            let beacon = build_presence_beacon(
                &device_id,
                // GS role (role byte 0x02). `role_drone = false`.
                false,
                channel_fn(),
                0, // rssi unknown on the emit side
                now_unix_ms(),
                &pair_key,
            );
            if let Err(e) = sock.send_to(&beacon, target).await {
                tracing::debug!(error = %e, "presence_emit_send_failed");
            }
        }
        tokio::time::sleep(PRESENCE_CADENCE).await;
    }
}

/// Listen for inbound control frames on every registered slot's control port,
/// verify the HMAC, and update `cache`. Two frame classes share each port
/// (`wfb_rx_control` re-emits both): a 51-byte HopAnnounce and a 68-byte
/// PresenceBeacon. The listener length-gates first, then dispatches:
///
/// * **HopAnnounce (51 B):** verify the HMAC, then echo the verbatim frame back
///   as a HopAck to `wfb_tx_control`'s loopback ingress so the drone's "acked"
///   gate goes true and its epoch-synced channel flip fires (without the echo
///   the drone never coordinates a hop). The announce's channel + real trigger
///   are recorded as a channel-follow, leaving the beacon-driven presence
///   identity/liveness untouched.
/// * **PresenceBeacon (68 B):** verify the HMAC, drop a frame carrying our own
///   device-id (the self-pair guard), then record the peer.
///
/// One socket per slot, one reader task each, all under this single generation:
/// the cache, the self-pair guard and the restart accounting stay singular
/// while the receive plane fans out per drone. Every bind happens before any
/// reader starts, so a port already held (a leaked previous generation) is a
/// fatal error the supervisor backs off and retries rather than a half-bound
/// listener that silently hears only some of the fleet.
///
/// Returns only on a fatal bind error, or when any slot's reader ends.
pub async fn listen_loop(
    cache: GsPresenceCache,
    follower: Option<HopFollower>,
    slots: &[u8],
) -> std::io::Result<()> {
    let own_device_id = read_device_id();
    let mut socks = Vec::with_capacity(slots.len());
    for &slot in slots {
        let port = presence_listen_port(slot);
        socks.push((
            slot,
            UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, port)).await?,
        ));
    }
    if socks.is_empty() {
        return Err(std::io::Error::other(
            "presence listener asked to bind zero fleet slots",
        ));
    }
    tracing::info!(
        slots = ?slots,
        "ground_presence_listen_started"
    );

    let mut readers = tokio::task::JoinSet::new();
    let mut bound: std::collections::BTreeSet<u8> = std::collections::BTreeSet::new();
    for (slot, sock) in socks {
        bound.insert(slot);
        readers.spawn(listen_on_slot(
            sock,
            slot,
            cache.clone(),
            follower.clone(),
            own_device_id.clone(),
        ));
    }

    // Pick up slots issued after this generation started.
    //
    // The set above comes from one registry read, and a drone can be paired long
    // after the listener is running: the pair route deliberately skips
    // re-installing the receive unit when the fleet key is unchanged, which is
    // the normal case for the second and every subsequent drone. The receive
    // plane already reconciles its own processes, so without this the new
    // drone's control frames are decoded onto a port nobody is bound to. Its
    // HopAnnounce is never echoed, so its coordinated channel hop never fires;
    // it never reaches the valid-packet watchdog's presence input, so a healthy
    // link can be cold-swept; and it never appears in the linked-peers surface,
    // so a GCS paired to this node never enrols it. All silently.
    let mut tick = tokio::time::interval(FLEET_RECONCILE_INTERVAL);
    tick.tick().await; // the first tick completes immediately

    loop {
        tokio::select! {
            // A reader only ends on a fault, so the first completion ends the
            // whole generation; the remaining readers are aborted when the
            // JoinSet drops, and the supervisor re-binds every slot afresh.
            _ = readers.join_next() => {
                return Err(std::io::Error::other(
                    "a presence slot reader ended; re-binding the whole listener",
                ));
            }
            _ = tick.tick() => {
                let want = wfb_rx::fleet_slots(&FleetRegistry::load(std::path::Path::new(
                    FLEET_REGISTRY_PATH,
                )));
                let missing: Vec<u8> =
                    want.into_iter().filter(|s| !bound.contains(s)).collect();
                for slot in missing {
                    let port = presence_listen_port(slot);
                    // A late bind is NOT fatal. The up-front binds are fatal on
                    // purpose — a port already held means a leaked generation,
                    // and half a fleet heard is worse than a clean retry. That
                    // reasoning does not carry here: propagating an EADDRINUSE
                    // from one newly-paired slot would tear down every healthy
                    // reader for a slot that was working seconds ago. Retry it
                    // on the next tick instead and leave the rest alone.
                    match UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, port)).await {
                        Ok(sock) => {
                            bound.insert(slot);
                            readers.spawn(listen_on_slot(
                                sock,
                                slot,
                                cache.clone(),
                                follower.clone(),
                                own_device_id.clone(),
                            ));
                            tracing::info!(slot, port, "ground_presence_slot_bound");
                        }
                        Err(e) => {
                            tracing::warn!(
                                error = %e, slot, port,
                                "ground_presence_slot_bind_failed; retrying next tick"
                            );
                        }
                    }
                }
            }
        }
    }
}

/// One slot's receive loop. Never returns under normal operation.
async fn listen_on_slot(
    sock: UdpSocket,
    slot: u8,
    cache: GsPresenceCache,
    follower: Option<HopFollower>,
    own_device_id: String,
) {
    let ack_target = (std::net::Ipv4Addr::LOCALHOST, HOP_ACK_ECHO_PORT);
    let mut buf = [0u8; 256];
    loop {
        // A recv error must NOT end the listener: this loop is the sole writer of
        // the watchdog's peer-presence cache, so if it died the cache would
        // freeze, presence would age out, and the valid-packet watchdog would
        // fall through to a cold-sweep/teardown on a paired-but-idle link. Log a
        // transient error and read again, keeping the cache fed.
        let (len, _addr) = match sock.recv_from(&mut buf).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, slot, "ground_presence_recv_failed");
                continue;
            }
        };
        let (pair_key, status) = resolve_pair_key();
        cache.set_key_status(status);
        let Some(pair_key) = pair_key else {
            // The key file exists but cannot be read: nothing on the control
            // plane can be authenticated, so nothing is acted on.
            continue;
        };
        // Only a hop frame needs the fleet size, and it is read fresh: a drone
        // paired a moment ago turns a single-drone station into a fleet.
        let fleet_slots = if len == HOP_FRAME_LEN {
            wfb_rx::fleet_slots(&FleetRegistry::load(std::path::Path::new(
                FLEET_REGISTRY_PATH,
            )))
            .len()
        } else {
            0
        };
        handle_control_frame(
            &sock,
            &buf[..len],
            &pair_key,
            &cache,
            &own_device_id,
            ack_target,
            follower.as_ref(),
            ControlContext { slot, fleet_slots },
        )
        .await;
    }
}

/// Fixed interval between listener re-binds after a fatal socket error or a
/// panic in the listen path.
///
/// Flat, with no ceiling and no attempt cap: the ladder this replaces doubled to
/// 30 s, and the presence input is what tells a ground station where the drone
/// actually is — a re-bind that is minutes late is a rendezvous the operator has
/// to drive by hand. The interval floor is what keeps a hard-failing bind from
/// busy-spinning.
const LISTEN_RETRY_INTERVAL: Duration = Duration::from_secs(5);

/// Sidecar carrying the listener-supervisor's restart accounting so a flapping
/// presence listener is observable cross-process (the REST/heartbeat layer reads
/// the GS sidecars). Lives on tmpfs under the run dir, honouring the same
/// `ADOS_RUN_DIR` override the rest of the Contract-E sidecars use.
const PRESENCE_LISTENER_SIDECAR_NAME: &str = "ground-presence-listener.json";

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

/// Persist the listener-health snapshot to the GS sidecar. Best-effort: an I/O
/// error is logged and discarded so sidecar trouble never stalls the supervisor.
/// The run-dir path resolves via `run_path` (honouring the `ADOS_RUN_DIR`
/// override); the write is delegated to `write_listener_health_to` so tests can
/// target an explicit temp path without mutating process-global env.
fn write_listener_health(health: &ListenerHealth) {
    write_listener_health_to(
        std::path::Path::new(&crate::paths::run_path(PRESENCE_LISTENER_SIDECAR_NAME)),
        health,
    );
}

/// Persist the listener-health snapshot to an explicit sidecar path. The path
/// seam that lets a test write into its own temp dir without touching
/// `ADOS_RUN_DIR`. Best-effort with the same swallow-and-log contract as
/// [`write_listener_health`].
fn write_listener_health_to(path: &std::path::Path, health: &ListenerHealth) {
    if let Err(e) = crate::sidecars::write_json_atomic(path, health, 0o644) {
        tracing::debug!(error = %e, "ground_presence_listener_sidecar_failed");
    }
}

/// Supervise the presence listener for the whole service lifetime, binding one
/// control port per registered fleet `slot`.
///
/// `listen_loop` only returns on a fatal socket error (its per-slot recv loops
/// continue over transient errors), so a return is a genuine fault: the
/// supervisor re-binds every slot after a fixed [`LISTEN_RETRY_INTERVAL`]. The
/// listener handle is awaited rather than dropped, so a panic in the listen path
/// surfaces as a `JoinError` here (logged + counted) instead of being silently
/// swallowed. The restart accounting is published to a GS sidecar so a flapping
/// listener is visible to the REST/heartbeat layer — one sidecar for the whole
/// fleet, because there is one listener generation covering every slot.
///
/// This loop itself never returns; spawn it for the service lifetime.
pub async fn listen_supervisor(
    cache: GsPresenceCache,
    follower: Option<HopFollower>,
    slots: Vec<u8>,
) {
    // No `backoff` state: every re-bind waits exactly LISTEN_RETRY_INTERVAL.
    let mut health = ListenerHealth {
        starts: 0,
        restarts: 0,
        panics: 0,
        last_exit: "starting".to_string(),
        started_at_unix: now_unix(),
    };

    loop {
        health.starts = health.starts.saturating_add(1);
        health.started_at_unix = now_unix();
        write_listener_health(&health);
        tracing::info!(
            starts = health.starts,
            restarts = health.restarts,
            "ground_presence_listener_spawning"
        );

        let handle = {
            let cache = cache.clone();
            let follower = follower.clone();
            let slots = slots.clone();
            tokio::spawn(async move { listen_loop(cache, follower, &slots).await })
        };

        // Await the generation so a panic is observed (not dropped). A clean
        // return is a fatal bind error path; a JoinError is a panic.
        match handle.await {
            Ok(Ok(())) => {
                // listen_loop's recv path never returns Ok today, but treat a
                // future clean shutdown as a re-spawn trigger rather than leaving
                // the cache without a writer.
                health.last_exit = "loop_returned_ok".to_string();
                tracing::warn!("ground_presence_listener_returned");
            }
            Ok(Err(e)) => {
                health.last_exit = format!("bind_error: {e}");
                tracing::error!(error = %e, "ground_presence_listener_bind_failed");
            }
            Err(join_err) => {
                health.panics = health.panics.saturating_add(1);
                health.last_exit = format!("panic: {join_err}");
                tracing::error!(error = %join_err, "ground_presence_listener_panicked");
            }
        }

        health.restarts = health.restarts.saturating_add(1);
        write_listener_health(&health);

        tracing::warn!(
            retry_in_s = LISTEN_RETRY_INTERVAL.as_secs(),
            restarts = health.restarts,
            "ground_presence_listener_retrying"
        );
        tokio::time::sleep(LISTEN_RETRY_INTERVAL).await;
    }
}

/// Longest wait honoured before a follow. The drone's countdown is three
/// seconds from its first announce; anything longer is a malformed or forged
/// frame, so the retune happens at the cap rather than being parked.
const MAX_FOLLOW_WAIT: Duration = Duration::from_secs(3);

/// How long after a follow the new channel must deliver valid packets before
/// the ground station reverts to the channel it left.
pub const FOLLOW_VERIFY_WINDOW: Duration = Duration::from_secs(5);

/// Why a verified HopAnnounce is not followed, or `None` to follow it.
///
/// The ground station acks only an announce it can carry out. An ack commits
/// the drone to the hop, so acking one this station then cannot follow is what
/// split drone and ground onto different channels:
///
/// * more than one registered slot: the one receive radio serves the whole
///   fleet, so following one drone's hop strands every other drone;
/// * no resolved receive interface: there is nothing to retune;
/// * a target outside the adapter's permitted channel set (an empty set means
///   it could not be read, which does not restrict).
pub fn hop_follow_refusal(
    fleet_slots: usize,
    iface_resolved: bool,
    permitted: &std::collections::BTreeSet<u8>,
    channel: u8,
) -> Option<&'static str> {
    if fleet_slots > 1 {
        return Some("fleet_hop_refused");
    }
    if !iface_resolved {
        return Some("no_receive_iface");
    }
    if !permitted.is_empty() && !permitted.contains(&channel) {
        return Some("channel_not_permitted");
    }
    None
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

/// Drives the GS receive radio to follow a drone-announced channel hop. Holds the
/// same `ChannelSetter` the acquirer uses plus the resolved-iface cell the
/// receive loop writes, so on a verified HopAnnounce the listener retunes the
/// live receive interface to the announced channel when the countdown expires —
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
    resolved_iface: Arc<tokio::sync::Mutex<Option<String>>>,
    /// The current receive generation's valid-decode counter. Replaced on every
    /// generation; `None` before the first.
    valid: Arc<Mutex<Option<SharedValidCounter>>>,
    /// The channel a follow is already scheduled toward. The drone repeats its
    /// announce until it hears the ack, so one hop arrives several times; only
    /// the first schedules a retune.
    pending: Arc<Mutex<Option<u8>>>,
    verify_window: Duration,
}

impl HopFollower {
    pub fn new(
        setter: Arc<dyn ChannelSetter>,
        resolved_iface: Arc<tokio::sync::Mutex<Option<String>>>,
    ) -> Self {
        Self {
            setter,
            resolved_iface,
            valid: Arc::new(Mutex::new(None)),
            pending: Arc::new(Mutex::new(None)),
            verify_window: FOLLOW_VERIFY_WINDOW,
        }
    }

    /// Point the follow verification at the current receive generation's
    /// valid-decode counter.
    pub fn set_valid_counter(&self, counter: SharedValidCounter) {
        *self.valid.lock().unwrap_or_else(|e| e.into_inner()) = Some(counter);
    }

    /// A follower whose verification window is `window`, for tests.
    #[cfg(test)]
    fn with_verify_window(mut self, window: Duration) -> Self {
        self.verify_window = window;
        self
    }

    /// The resolved receive interface, if any.
    async fn iface(&self) -> Option<String> {
        self.resolved_iface.lock().await.clone()
    }

    /// Claim the follow toward `channel`. False when one is already scheduled.
    fn claim(&self, channel: u8) -> bool {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        if *pending == Some(channel) {
            return false;
        }
        *pending = Some(channel);
        true
    }

    fn release(&self, channel: u8) {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        if *pending == Some(channel) {
            *pending = None;
        }
    }

    /// Retune `iface` to `channel` after `delay`, then verify. `prev` is the
    /// channel to go back to when the follow does not hold; with none known the
    /// radio stays and the valid-packet watchdog owns recovery.
    async fn follow(
        &self,
        iface: &str,
        channel: u8,
        delay: Duration,
        prev: Option<u8>,
        cache: &GsPresenceCache,
    ) -> FollowOutcome {
        tokio::time::sleep(delay.min(MAX_FOLLOW_WAIT)).await;
        if !self.setter.set_channel(iface, channel).await {
            tracing::warn!(interface = %iface, channel, "ground_hop_follow_retune_failed");
            self.revert(iface, prev, cache).await;
            return FollowOutcome::RevertedRetuneFailed;
        }
        tracing::info!(interface = %iface, channel, "ground_hop_follow_retuned");
        let counter = self.valid.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let Some(counter) = counter else {
            return FollowOutcome::Followed;
        };
        let before = counter.get();
        tokio::time::sleep(self.verify_window).await;
        if counter.get() > before {
            return FollowOutcome::Followed;
        }
        tracing::warn!(
            interface = %iface,
            channel,
            window_s = self.verify_window.as_secs_f64(),
            "ground_hop_follow_no_traffic_reverting"
        );
        self.revert(iface, prev, cache).await;
        FollowOutcome::RevertedNoTraffic
    }

    async fn revert(&self, iface: &str, prev: Option<u8>, cache: &GsPresenceCache) {
        let Some(prev) = prev else {
            return;
        };
        if self.setter.set_channel(iface, prev).await {
            cache.record_hop_announce(prev, "revert");
            tracing::info!(interface = %iface, channel = prev, "ground_hop_follow_reverted");
        } else {
            tracing::warn!(interface = %iface, channel = prev, "ground_hop_follow_revert_failed");
        }
    }
}

/// Where a control frame arrived: the fleet slot whose control port decoded
/// it, and how many slots the fleet has registered right now.
#[derive(Debug, Clone, Copy)]
struct ControlContext {
    slot: u8,
    fleet_slots: usize,
}

/// Dispatch one inbound control frame: length-gate, verify, then either handle
/// a 51-byte HopAnnounce or record the peer (68-byte PresenceBeacon).
/// Extracted from `listen_loop` so the dispatch is unit-testable over real
/// loopback sockets. `sock` is the listener socket the HopAck echo is sent
/// from; `ack_target` is `wfb_tx_control`'s loopback ingress. A HopAnnounce is
/// acked only when `follower` can carry it out (see [`hop_follow_refusal`]);
/// otherwise no ack is sent, so the drone stays where it is, and the reason is
/// recorded on the hop snapshot.
#[allow(clippy::too_many_arguments)]
async fn handle_control_frame(
    sock: &UdpSocket,
    frame: &[u8],
    pair_key: &[u8; 32],
    cache: &GsPresenceCache,
    own_device_id: &str,
    ack_target: (std::net::Ipv4Addr, u16),
    follower: Option<&HopFollower>,
    ctx: ControlContext,
) {
    // Length gate first: a 51-byte frame is a HopAnnounce/HopAck, a 68-byte
    // frame is a PresenceBeacon. The magic + HMAC verify inside each parser is
    // the second gate, so a 51-byte non-hop frame (or a forged one) is dropped
    // rather than mis-routed.
    match frame.len() {
        HOP_FRAME_LEN => {
            let Some((channel, trigger)) = parse_hop_announce(frame, pair_key) else {
                return;
            };
            let delay = Duration::from_millis(hop_announce_delay_ms(frame).unwrap_or(0));
            let Some(follower) = follower else {
                cache.record_hop_refusal(channel, "no_receive_iface");
                return;
            };
            let iface = follower.iface().await;
            let permitted = match iface.as_deref() {
                Some(i) => ados_radio::adapter::enabled_channels(i).await,
                None => std::collections::BTreeSet::new(),
            };
            if let Some(reason) =
                hop_follow_refusal(ctx.fleet_slots, iface.is_some(), &permitted, channel)
            {
                cache.record_hop_refusal(channel, reason);
                tracing::info!(
                    channel,
                    trigger,
                    slot = ctx.slot,
                    fleet_slots = ctx.fleet_slots,
                    reason,
                    "ground_hop_refused"
                );
                return;
            }
            let Some(iface) = iface else { return };
            // The ack commits both sides, so it goes only after the checks
            // above. It is repeated for every announce of the same hop: the
            // drone keeps announcing until one ack gets through.
            if let Err(e) = sock.send_to(frame, ack_target).await {
                tracing::debug!(error = %e, "ground_hop_ack_send_failed");
            } else {
                tracing::info!(channel, trigger, "ground_hop_ack_echoed");
            }
            if !follower.claim(channel) {
                return;
            }
            // The channel being left, for the revert, read before the cache
            // moves its hint to the new one.
            let prev = cache.peer_channel();
            cache.record_hop_announce(channel, trigger);
            // Spawned so the listener's recv loop keeps feeding the presence
            // cache through the countdown and the verification window.
            let follower = follower.clone();
            let cache = cache.clone();
            tokio::spawn(async move {
                follower.follow(&iface, channel, delay, prev, &cache).await;
                follower.release(channel);
            });
        }
        PRESENCE_FRAME_LEN => {
            let Some(peer) = parse_presence_beacon(frame, pair_key) else {
                return;
            };
            // Self-pair guard: skip a beacon that carries our own device-id (the
            // emit loop's frame can loop back via wfb_rx_control's re-emit). The
            // Python listener compares against the first 16 chars (the beacon
            // device-id field is 16 bytes).
            if !own_device_id.is_empty() {
                let own_trunc: String = own_device_id.chars().take(16).collect();
                if peer.device_id == own_trunc {
                    return;
                }
            }
            cache.record_peer(
                peer.device_id,
                peer.role,
                peer.channel,
                peer.rssi_dbm,
                ctx.slot,
            );
        }
        _ => {}
    }
}

/// Hop-supervisor snapshot persist cadence (5 s, matching the Python listener:
/// the GCS chart polls at 1 Hz but does not need sub-second hop-history
/// freshness).
pub const HOP_PERSIST_CADENCE: Duration = Duration::from_secs(5);

/// Build the hop-supervisor JSON payload from a snapshot, stamping
/// `wall_time_unix` so a cross-process reader can age the file. Pure so the
/// shape is unit-testable without the filesystem; mirrors the Python
/// `_persist_snapshot` payload (the snapshot dict plus `wall_time_unix`).
pub fn hop_supervisor_payload(snap: &HopSnapshot) -> serde_json::Value {
    let mut v = serde_json::to_value(snap).unwrap_or_else(|_| serde_json::json!({}));
    if let Some(obj) = v.as_object_mut() {
        obj.insert("wall_time_unix".to_string(), serde_json::json!(now_unix()));
        // Sidecar schema version (best-effort drift signal for readers). Shared
        // with the drone-side hop supervisor via the one const.
        obj.insert(
            "version".to_string(),
            serde_json::json!(ados_radio::paths::HOP_SUPERVISOR_SIDECAR_VERSION),
        );
    }
    v
}

/// Persist the GS-side hop-supervisor snapshot to `/run/ados/hop-supervisor.json`
/// on the `HOP_PERSIST_CADENCE`, sourcing the hop-follow history from the shared
/// presence cache. Writes one immediate snapshot on entry (so the on-box
/// channel-hops page reads a valid file before the first beacon) and one every
/// cadence tick thereafter. The drone supervisor and the GS listener both target
/// this single file; a given rig runs only one of them so there is no
/// contention. Best-effort: an I/O error is logged and the loop continues.
/// Returns only on task cancellation.
pub async fn hop_supervisor_persist_loop(cache: GsPresenceCache, band: String) {
    use std::path::Path;
    let path = Path::new(crate::paths::HOP_SUPERVISOR_JSON);
    loop {
        let snap = cache.hop_snapshot(&band);
        let payload = hop_supervisor_payload(&snap);
        if let Err(e) = crate::sidecars::write_json_atomic(path, &payload, 0o644) {
            tracing::debug!(error = %e, "ground_hop_supervisor_persist_failed");
        }
        tokio::time::sleep(HOP_PERSIST_CADENCE).await;
    }
}

/// Build the `linked-peers.json` payload from the fresh peer set, stamping
/// `version` (a reader drift signal) + `wall_time_unix` (so a cross-process
/// reader can age the whole file, not just the per-peer `last_seen_unix`). Pure
/// so the shape is unit-testable without the filesystem.
pub fn linked_peers_payload(peers: &[LinkedPeer]) -> serde_json::Value {
    serde_json::json!({
        "version": LINKED_PEERS_SIDECAR_VERSION,
        "wall_time_unix": now_unix(),
        "peers": peers,
    })
}

/// Persist the linked-peers list to `/run/ados/linked-peers.json` on the
/// [`LINKED_PEERS_PERSIST_CADENCE`], pruning stale peers each tick (via
/// [`GsPresenceCache::linked_peers`]). Writes one immediate snapshot on entry so
/// the heartbeat reads a valid (empty) file before the first beacon, then one
/// every cadence tick. Best-effort: an I/O error is logged and the loop
/// continues. Returns only on task cancellation. Only the direct receive plane
/// runs the presence listener, so only it writes this file — no contention.
pub async fn linked_peers_persist_loop(cache: GsPresenceCache) {
    use std::path::PathBuf;
    let owned = PathBuf::from(crate::paths::run_path("linked-peers.json"));
    loop {
        let peers = cache.linked_peers();
        let payload = linked_peers_payload(&peers);
        if let Err(e) = crate::sidecars::write_json_atomic(&owned, &payload, 0o644) {
            tracing::debug!(error = %e, "ground_linked_peers_persist_failed");
        }
        tokio::time::sleep(LINKED_PEERS_PERSIST_CADENCE).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_radio::hop::derive_pair_key;

    /// The cold-start pair key, which a dev host without `/etc/drone.key` runs on.
    fn test_key() -> [u8; 32] {
        derive_pair_key(None)
    }

    /// A frame decoded on slot 1 of a single-drone station.
    const SOLO: ControlContext = ControlContext {
        slot: 1,
        fleet_slots: 1,
    };

    #[test]
    fn presence_ports_are_asymmetric() {
        // The trap: emit to 5810 (the single tx_control ingress), listen on the
        // per-slot control egress. If a listen port ever equalled the emit port
        // the GS would self-pair with its own device-id over loopback, so the
        // whole per-slot range must stay clear of it.
        assert_eq!(PRESENCE_EMIT_PORT, 5810);
        for slot in 0..=ados_radio::config::FLEET_MAX_SLOTS {
            assert_ne!(PRESENCE_EMIT_PORT, presence_listen_port(slot));
            assert_ne!(HOP_ACK_ECHO_PORT, presence_listen_port(slot));
        }
        // Distinct per slot: two slots sharing a listen port would collide on
        // the bind and leave one drone's control lane unheard.
        let ports: std::collections::BTreeSet<u16> = (1..=ados_radio::config::FLEET_MAX_SLOTS)
            .map(presence_listen_port)
            .collect();
        assert_eq!(ports.len(), ados_radio::config::FLEET_MAX_SLOTS as usize);
    }

    #[test]
    fn cold_start_pair_key_matches_radio_crate() {
        // With no key file on disk the resolver must produce the same cold-start
        // key the radio crate derives, so a pre-bind beacon round-trips.
        let (resolved, status) = resolve_pair_key();
        // On a dev host /etc/drone.key is absent, so this is the cold path.
        if ados_radio::paths::load_shared_key() == ados_radio::paths::SharedKey::Absent {
            assert_eq!(resolved, Some(derive_pair_key(None)));
            assert_eq!(status, "cold_start");
        }
    }

    #[test]
    fn cache_age_none_until_first_beacon() {
        let cache = GsPresenceCache::new();
        assert!(cache.presence_age_s().is_none());
        assert!(cache.announced_channel().is_none());
        assert!(cache.peer_last_seen_unix().is_none());
    }

    #[test]
    fn cache_records_peer_and_exposes_channel_and_fresh_age() {
        let cache = GsPresenceCache::new();
        cache.record_peer("drone-abc".into(), "drone".into(), 157, -48, 1);
        assert_eq!(cache.announced_channel(), Some(157));
        assert_eq!(cache.peer_channel(), Some(157));
        assert!(cache.peer_last_seen_unix().is_some());
        // Just recorded: age is small and non-negative.
        let age = cache.presence_age_s().expect("age present after record");
        assert!((0.0..5.0).contains(&age), "age {age} not fresh");
        // Fresh within the watchdog's 30 s window → peer_present() true.
        assert!(cache.peer_present());
    }

    #[test]
    fn linked_peers_empty_until_a_beacon_is_decoded() {
        let cache = GsPresenceCache::new();
        assert!(cache.linked_peers().is_empty());
    }

    #[test]
    fn linked_peers_lists_every_decoded_drone_newest_first() {
        // A ground station relaying two drones must report BOTH — the scalar
        // fields only ever track the last-heard one, but the list carries all.
        let cache = GsPresenceCache::new();
        cache.record_peer("drone-a".into(), "drone".into(), 149, -60, 1);
        cache.record_peer("drone-b".into(), "drone".into(), 157, -48, 2);
        let peers = cache.linked_peers();
        assert_eq!(peers.len(), 2);
        // Newest decode (drone-b) is first.
        assert_eq!(peers[0].device_id, "drone-b");
        assert_eq!(peers[0].channel, 157);
        assert_eq!(peers[0].rssi_dbm, -48);
        assert_eq!(peers[0].role, "drone");
        assert!(peers[0].last_seen_unix > 0.0);
        assert_eq!(peers[1].device_id, "drone-a");
        // The scalar watchdog surface still tracks the freshest peer only.
        assert_eq!(cache.peer_channel(), Some(157));
    }

    #[test]
    fn linked_peers_upserts_by_device_id_not_duplicates() {
        // Re-hearing the same drone refreshes its entry (channel/rssi/last-seen)
        // rather than appending a duplicate.
        let cache = GsPresenceCache::new();
        cache.record_peer("drone-a".into(), "drone".into(), 149, -60, 1);
        cache.record_peer("drone-a".into(), "drone".into(), 157, -45, 1);
        let peers = cache.linked_peers();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].channel, 157);
        assert_eq!(peers[0].rssi_dbm, -45);
    }

    #[test]
    fn linked_peers_payload_has_version_wall_time_and_snake_case_entries() {
        // The heartbeat producers read this file's snake_case keys and remap to
        // the camelCase wire shape; pin the on-disk keys so a producer reads the
        // right ones.
        let cache = GsPresenceCache::new();
        cache.record_peer("drone-a".into(), "drone".into(), 149, -60, 3);
        let payload = linked_peers_payload(&cache.linked_peers());
        assert_eq!(payload["version"], LINKED_PEERS_SIDECAR_VERSION);
        assert!(payload["wall_time_unix"].as_f64().unwrap() > 0.0);
        let entry = &payload["peers"][0];
        assert_eq!(entry["device_id"], "drone-a");
        assert_eq!(entry["role"], "drone");
        assert_eq!(entry["channel"], 149);
        assert_eq!(entry["rssi_dbm"], -60);
        assert!(entry["last_seen_unix"].as_f64().unwrap() > 0.0);
    }

    #[test]
    fn linked_peers_empty_payload_is_a_valid_empty_list() {
        // Before any beacon, the persister still writes a valid file so the
        // heartbeat reads an empty list, never a missing/garbage file.
        let payload = linked_peers_payload(&[]);
        assert_eq!(payload["version"], LINKED_PEERS_SIDECAR_VERSION);
        assert!(payload["peers"].as_array().unwrap().is_empty());
    }

    #[test]
    fn hop_snapshot_shape_matches_listener_keys() {
        // An untouched cache snapshots an empty, valid listener shape: source
        // "listener", thresholds null, history empty, last_hop_at 0.
        let cache = GsPresenceCache::new();
        let snap = cache.hop_snapshot("u-nii-3");
        assert!(snap.enabled);
        assert_eq!(snap.band, "u-nii-3");
        assert!(snap.hop_period_seconds.is_none());
        assert!(snap.loss_threshold_percent.is_none());
        assert!(snap.rssi_threshold_dbm.is_none());
        assert_eq!(snap.last_hop_at, 0.0);
        assert!(snap.history.is_empty());
        assert_eq!(snap.source, "listener");

        // The serialized payload carries the wall_time_unix stamp + null
        // thresholds (the JSON shape a cross-process reader sees).
        let v = hop_supervisor_payload(&snap);
        assert_eq!(v["source"], "listener");
        assert_eq!(v["enabled"], true);
        assert_eq!(v["band"], "u-nii-3");
        assert!(v["hop_period_seconds"].is_null());
        assert!(v["loss_threshold_percent"].is_null());
        assert!(v["rssi_threshold_dbm"].is_null());
        assert!(v["history"].as_array().unwrap().is_empty());
        assert!(v["wall_time_unix"].as_f64().unwrap() > 0.0);
    }

    #[test]
    fn record_peer_appends_a_follow_entry_only_on_channel_change() {
        let cache = GsPresenceCache::new();
        // First beacon: a follow from 0 (unknown prior) to 157.
        cache.record_peer("drone-1".into(), "drone".into(), 157, -50, 1);
        let s = cache.hop_snapshot("u-nii-3");
        assert_eq!(s.history.len(), 1);
        assert_eq!(s.history[0].from, 0);
        assert_eq!(s.history[0].to, 157);
        assert_eq!(s.history[0].trigger, "periodic");
        assert!(s.history[0].ok);
        assert!(s.last_hop_at > 0.0);

        // Same channel again: no new entry.
        cache.record_peer("drone-1".into(), "drone".into(), 157, -47, 1);
        assert_eq!(cache.hop_snapshot("u-nii-3").history.len(), 1);

        // New channel: a follow from 157 to 149.
        cache.record_peer("drone-1".into(), "drone".into(), 149, -45, 1);
        let s = cache.hop_snapshot("u-nii-3");
        assert_eq!(s.history.len(), 2);
        assert_eq!(s.history[1].from, 157);
        assert_eq!(s.history[1].to, 149);
    }

    #[test]
    fn hop_history_is_capped_at_thirty_two() {
        let cache = GsPresenceCache::new();
        // Alternate between two channels so every beacon is a change; drive well
        // past the 32-entry cap and confirm only the last 32 survive.
        for i in 0..50u8 {
            let ch = if i % 2 == 0 { 149 } else { 153 };
            cache.record_peer("drone-1".into(), "drone".into(), ch, -50, 1);
        }
        let s = cache.hop_snapshot("u-nii-3");
        assert_eq!(s.history.len(), HOP_HISTORY_CAP);
        // The last recorded channel is the most recent `to`.
        let last = s.history.last().unwrap();
        let expected_last = if 49 % 2 == 0 { 149 } else { 153 };
        assert_eq!(last.to, expected_last);
    }

    #[tokio::test]
    async fn emit_and_listen_round_trip_over_loopback() {
        // Wire the listener to a custom port so the emit hits it directly
        // (in production the wfb_tx_control bridge sits between the two ports;
        // here we point a local sender straight at the listener's port to prove
        // the verify + cache-update path). Use the real listen port via a
        // sender that targets it.
        let cache = GsPresenceCache::new();
        let listener_cache = cache.clone();

        // Bind the listener on an ephemeral port to avoid colliding with a real
        // 5803 on the dev host or a parallel test.
        let sock = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let listen_addr = sock.local_addr().unwrap();

        // Drive one decode by hand using the same verify path the listener uses.
        let pair_key = test_key();
        let beacon = build_presence_beacon("drone-xyz", true, 161, -55, 123_456, &pair_key);

        let sender = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        sender.send_to(&beacon, listen_addr).await.unwrap();

        let mut buf = [0u8; 256];
        let (len, _) = tokio::time::timeout(Duration::from_secs(2), sock.recv_from(&mut buf))
            .await
            .expect("listener recv timed out")
            .unwrap();
        let peer = parse_presence_beacon(&buf[..len], &pair_key).expect("beacon verifies");
        listener_cache.record_peer(peer.device_id, peer.role, peer.channel, peer.rssi_dbm, 1);

        assert_eq!(cache.announced_channel(), Some(161));
        assert_eq!(cache.peer_channel(), Some(161));
    }

    // ---- ports + frame-length constants -----------------------------------

    #[test]
    fn hop_ack_echo_targets_tx_control_ingress() {
        // The ACK must go to wfb_tx_control's loopback ingress (5810), the same
        // port the presence emit uses, so the ACK is transmitted over RF.
        assert_eq!(HOP_ACK_ECHO_PORT, 5810);
        assert_eq!(HOP_ACK_ECHO_PORT, PRESENCE_EMIT_PORT);
        // The two frame classes that share the control port have distinct lengths
        // so the length gate is unambiguous.
        assert_eq!(HOP_FRAME_LEN, 51);
        assert_eq!(PRESENCE_FRAME_LEN, 68);
        assert_ne!(HOP_FRAME_LEN, PRESENCE_FRAME_LEN);
    }

    // ---- HopAnnounce → follow with real trigger ---------------------------

    #[test]
    fn record_hop_announce_uses_real_trigger_and_preserves_presence_identity() {
        let cache = GsPresenceCache::new();
        // Seed a prior verified beacon: identity + liveness are now set.
        cache.record_peer("drone-1".into(), "drone".into(), 149, -50, 1);
        let seeded_age = cache.peer_last_seen_unix();
        assert!(seeded_age.is_some());

        // A reactive HopAnnounce to a new channel: the follow is recorded with
        // the REAL trigger, the channel updates, but the beacon-driven liveness
        // stamp and identity are untouched (an announce is not a presence beacon).
        cache.record_hop_announce(157, "reactive");
        assert_eq!(cache.peer_channel(), Some(157));
        let s = cache.hop_snapshot("u-nii-3");
        // First entry from the beacon (0 → 149, periodic), second from the
        // announce (149 → 157, reactive).
        assert_eq!(s.history.len(), 2);
        assert_eq!(s.history[1].from, 149);
        assert_eq!(s.history[1].to, 157);
        assert_eq!(s.history[1].trigger, "reactive");
        // Presence liveness stamp is unchanged by the announce (still the beacon's).
        assert_eq!(cache.peer_last_seen_unix(), seeded_age);
    }

    #[test]
    fn record_hop_announce_same_channel_is_a_noop() {
        let cache = GsPresenceCache::new();
        cache.record_hop_announce(149, "periodic");
        // First announce records a follow from unknown (0) → 149.
        assert_eq!(cache.hop_snapshot("u-nii-3").history.len(), 1);
        // Re-announcing the same channel adds no new follow entry.
        cache.record_hop_announce(149, "reactive");
        assert_eq!(cache.hop_snapshot("u-nii-3").history.len(), 1);
        assert_eq!(cache.peer_channel(), Some(149));
    }

    // ---- HopAnnounce decode → HopAck echo over loopback -------------------

    #[tokio::test]
    async fn hop_announce_decodes_and_echoes_verbatim_hop_ack() {
        use ados_radio::hop::{build_hop_announce, HopTrigger};

        let cache = GsPresenceCache::new();
        let pair_key = test_key();

        // The listener's socket (sends the echo from here) + a stand-in for
        // wfb_tx_control's loopback ingress (receives the ACK).
        let listen_sock = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let ack_recv = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let ack_addr = ack_recv.local_addr().unwrap();
        let ack_target = match ack_addr {
            std::net::SocketAddr::V4(a) => (*a.ip(), a.port()),
            _ => unreachable!("ipv4 loopback"),
        };
        let follower = HopFollower::new(
            RecordingSetter::ok(),
            Arc::new(tokio::sync::Mutex::new(Some("wlan1".to_string()))),
        );

        let announce = build_hop_announce(0, 157, HopTrigger::Reactive, &pair_key);
        handle_control_frame(
            &listen_sock,
            &announce,
            &pair_key,
            &cache,
            "", // own device id irrelevant to the hop path
            ack_target,
            Some(&follower),
            SOLO,
        )
        .await;

        // The ACK that landed at the tx-control ingress is the verbatim announce.
        let mut buf = [0u8; 256];
        let (len, _) = tokio::time::timeout(Duration::from_secs(2), ack_recv.recv_from(&mut buf))
            .await
            .expect("hop ack not echoed")
            .unwrap();
        assert_eq!(&buf[..len], &announce[..], "echoed ack must be verbatim");
        // The follow was recorded with the announce's real trigger + channel.
        assert_eq!(cache.peer_channel(), Some(157));
        let s = cache.hop_snapshot("u-nii-3");
        assert_eq!(s.history.last().unwrap().trigger, "reactive");
        assert_eq!(s.history.last().unwrap().to, 157);
    }

    /// Dispatch `announce` and report whether an ack reached the tx ingress.
    async fn acked(
        announce: &[u8],
        cache: &GsPresenceCache,
        follower: Option<&HopFollower>,
        ctx: ControlContext,
    ) -> bool {
        let listen_sock = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let ack_recv = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let ack_target = match ack_recv.local_addr().unwrap() {
            std::net::SocketAddr::V4(a) => (*a.ip(), a.port()),
            _ => unreachable!("ipv4 loopback"),
        };
        handle_control_frame(
            &listen_sock,
            announce,
            &test_key(),
            cache,
            "",
            ack_target,
            follower,
            ctx,
        )
        .await;
        let mut buf = [0u8; 64];
        tokio::time::timeout(Duration::from_millis(200), ack_recv.recv_from(&mut buf))
            .await
            .is_ok()
    }

    #[tokio::test]
    async fn a_fleet_station_neither_acks_nor_follows_one_drone_s_hop() {
        // One receive radio serves every slot. Following drone A's hop would
        // strand drones B..N on the old channel.
        use ados_radio::hop::{build_hop_announce, HopTrigger};
        let cache = GsPresenceCache::new();
        cache.record_peer("drone-a".into(), "drone".into(), 149, -50, 1);
        let setter = RecordingSetter::ok();
        let follower = HopFollower::new(
            setter.clone(),
            Arc::new(tokio::sync::Mutex::new(Some("wlan1".to_string()))),
        );
        let announce = build_hop_announce(0, 157, HopTrigger::Reactive, &test_key());
        let fleet = ControlContext {
            slot: 1,
            fleet_slots: 2,
        };
        assert!(!acked(&announce, &cache, Some(&follower), fleet).await);
        assert_eq!(
            cache.peer_channel(),
            Some(149),
            "the receive hint stays put"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(setter.calls().is_empty());
        let refusal = cache
            .hop_snapshot("u-nii-3")
            .last_refusal
            .expect("recorded");
        assert_eq!(refusal.reason, "fleet_hop_refused");
        assert_eq!(refusal.channel, 157);
    }

    #[tokio::test]
    async fn a_station_that_cannot_retune_does_not_ack() {
        // An ack commits the drone. With no receive interface resolved there is
        // nothing to retune, so acking would split the pair.
        use ados_radio::hop::{build_hop_announce, HopTrigger};
        let cache = GsPresenceCache::new();
        let follower = HopFollower::new(
            RecordingSetter::ok(),
            Arc::new(tokio::sync::Mutex::new(None)),
        );
        let announce = build_hop_announce(0, 157, HopTrigger::Periodic, &test_key());
        assert!(!acked(&announce, &cache, Some(&follower), SOLO).await);
        assert_eq!(
            cache.hop_snapshot("u-nii-3").last_refusal.unwrap().reason,
            "no_receive_iface"
        );
    }

    #[test]
    fn the_listener_rebind_is_a_flat_retry_in_the_recovery_band() {
        // Never zero (a hard-failing bind must not busy-spin) and never longer
        // than the recovery band, because the presence input is what tells this
        // ground station where the drone actually is.
        assert!(!LISTEN_RETRY_INTERVAL.is_zero());
        assert!(LISTEN_RETRY_INTERVAL >= Duration::from_secs(2));
        assert!(LISTEN_RETRY_INTERVAL <= Duration::from_secs(5));
    }

    #[test]
    fn listener_health_sidecar_round_trips_with_expected_keys() {
        // Write the sidecar into a temp tree via the explicit-path seam. No env
        // mutation: the temp path is threaded in directly, so this test cannot
        // race any other test under the parallel runner.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PRESENCE_LISTENER_SIDECAR_NAME);

        let health = ListenerHealth {
            starts: 3,
            restarts: 2,
            panics: 1,
            last_exit: "panic: task panicked".to_string(),
            started_at_unix: now_unix(),
        };
        write_listener_health_to(&path, &health);

        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(v["starts"], 3);
        assert_eq!(v["restarts"], 2);
        assert_eq!(v["panics"], 1);
        assert_eq!(v["last_exit"], "panic: task panicked");
        assert!(v["started_at_unix"].as_f64().unwrap() > 0.0);
    }

    #[tokio::test]
    async fn listener_survives_a_transient_recv_error_and_keeps_feeding_the_cache() {
        // The listener must keep updating the presence cache after a recv hiccup.
        // Drive a verified beacon straight at the bound listener socket through
        // the same dispatch path the loop uses; a recv error in between would, in
        // the buggy version, have ended the loop and frozen the cache. Here we
        // assert the cache is fed and stays fed across repeated dispatches,
        // proving the per-frame handler is independent of any single recv outcome.
        let cache = GsPresenceCache::new();
        let pair_key = test_key();
        let listen_sock = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let ack_recv = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let ack_addr = ack_recv.local_addr().unwrap();
        let ack_target = match ack_addr {
            std::net::SocketAddr::V4(a) => (*a.ip(), a.port()),
            _ => unreachable!("ipv4 loopback"),
        };

        // First good frame.
        let b1 = build_presence_beacon("drone-aaa", true, 149, -50, 1, &pair_key);
        handle_control_frame(
            &listen_sock,
            &b1,
            &pair_key,
            &cache,
            "",
            ack_target,
            None,
            SOLO,
        )
        .await;
        assert_eq!(cache.peer_channel(), Some(149));
        let first_seen = cache.peer_last_seen_unix();
        assert!(first_seen.is_some());

        // A garbage frame (stand-in for the kind of bad input a recv might hand
        // up) must be dropped without disturbing the established cache state.
        handle_control_frame(
            &listen_sock,
            &[0u8; 10],
            &pair_key,
            &cache,
            "",
            ack_target,
            None,
            SOLO,
        )
        .await;
        assert_eq!(cache.peer_channel(), Some(149));

        // A later good frame still updates the cache: the writer is alive.
        let b2 = build_presence_beacon("drone-aaa", true, 157, -45, 2, &pair_key);
        handle_control_frame(
            &listen_sock,
            &b2,
            &pair_key,
            &cache,
            "",
            ack_target,
            None,
            SOLO,
        )
        .await;
        assert_eq!(cache.peer_channel(), Some(157));
        assert!(cache.peer_present());
    }

    #[tokio::test]
    async fn presence_beacon_is_not_misrouted_to_the_hop_path() {
        // A 68-byte PresenceBeacon must NOT be echoed as a HopAck (no ACK lands)
        // and MUST be recorded as a peer via the presence path.
        let cache = GsPresenceCache::new();
        let pair_key = test_key();

        let listen_sock = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let ack_recv = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let ack_addr = ack_recv.local_addr().unwrap();
        let ack_target = match ack_addr {
            std::net::SocketAddr::V4(a) => (*a.ip(), a.port()),
            _ => unreachable!("ipv4 loopback"),
        };

        let beacon = build_presence_beacon("drone-xyz", true, 161, -55, 9, &pair_key);
        handle_control_frame(
            &listen_sock,
            &beacon,
            &pair_key,
            &cache,
            "",
            ack_target,
            None,
            SOLO,
        )
        .await;

        // The presence path ran: the peer is recorded.
        assert_eq!(cache.peer_channel(), Some(161));
        assert!(cache.peer_present());
        // No HopAck was emitted onto the ACK ingress.
        let mut buf = [0u8; 256];
        let echoed =
            tokio::time::timeout(Duration::from_millis(200), ack_recv.recv_from(&mut buf)).await;
        assert!(echoed.is_err(), "a presence beacon must not echo a hop ack");
    }

    /// Records the channel each retune requested, for the hop-follow assertions.
    #[derive(Default)]
    struct RecordingSetter {
        calls: std::sync::Mutex<Vec<(String, u8)>>,
        result: bool,
    }

    impl RecordingSetter {
        fn ok() -> Arc<Self> {
            Arc::new(Self {
                calls: std::sync::Mutex::new(Vec::new()),
                result: true,
            })
        }
        fn calls(&self) -> Vec<(String, u8)> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl ChannelSetter for RecordingSetter {
        fn set_channel<'a>(
            &'a self,
            interface: &'a str,
            channel: u8,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
            Box::pin(async move {
                self.calls
                    .lock()
                    .unwrap()
                    .push((interface.to_string(), channel));
                self.result
            })
        }
    }

    #[tokio::test]
    async fn a_follow_that_carries_traffic_holds() {
        let setter = RecordingSetter::ok();
        let iface = Arc::new(tokio::sync::Mutex::new(Some("wlan1".to_string())));
        let follower =
            HopFollower::new(setter.clone(), iface).with_verify_window(Duration::from_millis(50));
        let counter = SharedValidCounter::new();
        follower.set_valid_counter(counter.clone());
        let cache = GsPresenceCache::new();
        let feeding = {
            let counter = counter.clone();
            tokio::spawn(async move {
                for _ in 0..20 {
                    counter.add(10);
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
        };
        let outcome = follower
            .follow("wlan1", 157, Duration::ZERO, Some(149), &cache)
            .await;
        feeding.abort();
        assert_eq!(outcome, FollowOutcome::Followed);
        assert_eq!(setter.calls(), vec![("wlan1".to_string(), 157)]);
    }

    #[tokio::test]
    async fn a_follow_with_no_traffic_on_the_new_channel_goes_back() {
        // The drone acked path failed on its side: nothing arrives on the target.
        // The station returns to the channel it left instead of staying deaf.
        let setter = RecordingSetter::ok();
        let iface = Arc::new(tokio::sync::Mutex::new(Some("wlan1".to_string())));
        let follower =
            HopFollower::new(setter.clone(), iface).with_verify_window(Duration::from_millis(20));
        follower.set_valid_counter(SharedValidCounter::new());
        let cache = GsPresenceCache::new();
        let outcome = follower
            .follow("wlan1", 157, Duration::ZERO, Some(149), &cache)
            .await;
        assert_eq!(outcome, FollowOutcome::RevertedNoTraffic);
        assert_eq!(
            setter.calls(),
            vec![("wlan1".to_string(), 157), ("wlan1".to_string(), 149)]
        );
        assert_eq!(
            cache.peer_channel(),
            Some(149),
            "the hint follows the revert"
        );
    }

    #[test]
    fn the_follow_gate_names_each_refusal() {
        let any = std::collections::BTreeSet::new();
        let unii3: std::collections::BTreeSet<u8> = [149, 153, 157].into_iter().collect();
        assert_eq!(hop_follow_refusal(1, true, &any, 36), None);
        assert_eq!(hop_follow_refusal(0, true, &unii3, 157), None);
        assert_eq!(
            hop_follow_refusal(2, true, &any, 157),
            Some("fleet_hop_refused")
        );
        assert_eq!(
            hop_follow_refusal(1, false, &any, 157),
            Some("no_receive_iface")
        );
        assert_eq!(
            hop_follow_refusal(1, true, &unii3, 36),
            Some("channel_not_permitted")
        );
    }

    #[tokio::test]
    async fn hop_announce_drives_the_coordinated_follow() {
        // End-to-end through the dispatch: a verified HopAnnounce echoes the ack,
        // records the channel into the cache (the watchdog's hint), AND drives the
        // follower to retune the receive iface to the announced channel.
        use ados_radio::hop::{build_hop_announce, HopTrigger};
        let cache = GsPresenceCache::new();
        let pair_key = test_key();

        let listen_sock = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let ack_recv = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let ack_addr = ack_recv.local_addr().unwrap();
        let ack_target = match ack_addr {
            std::net::SocketAddr::V4(a) => (*a.ip(), a.port()),
            _ => unreachable!("ipv4 loopback"),
        };

        let setter = RecordingSetter::ok();
        let iface = Arc::new(tokio::sync::Mutex::new(Some("wlan1".to_string())));
        let follower = HopFollower::new(setter.clone(), iface);

        // A zero countdown so the spawned retune fires immediately.
        let announce = build_hop_announce(0, 161, HopTrigger::Reactive, &pair_key);
        handle_control_frame(
            &listen_sock,
            &announce,
            &pair_key,
            &cache,
            "",
            ack_target,
            Some(&follower),
            SOLO,
        )
        .await;

        // The watchdog's channel hint was updated.
        assert_eq!(cache.peer_channel(), Some(161));

        // The retune is spawned, so give it a moment to land, then assert it tuned
        // the resolved iface to the announced channel.
        for _ in 0..50 {
            if !setter.calls().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            setter.calls(),
            vec![("wlan1".to_string(), 161)],
            "the coordinated follow must retune the receive iface to the announced channel"
        );
    }

    #[test]
    fn an_aux_identity_fills_an_idless_beacon_row() {
        // The null-peer case. A node with no persistent id emits an empty one in
        // its beacon, and every downstream reader drops an id-less row, so an
        // audibly-present peer vanishes. The identity frame is the only place the
        // real id can come from.
        let cache = GsPresenceCache::new();
        cache.record_peer(String::new(), "drone".into(), 149, -55, 1);
        assert_eq!(cache.linked_peers()[0].device_id, "");

        cache.set_aux_identities(vec![("drone-a".into(), Some("Alpha".into()))]);
        let peers = cache.linked_peers();
        assert_eq!(peers[0].device_id, "drone-a");
        assert_eq!(peers[0].name.as_deref(), Some("Alpha"));
        // The measured radio values are untouched: the identity frame carries no
        // signal information and must not be allowed to invent any.
        assert_eq!(peers[0].channel, 149);
        assert_eq!(peers[0].rssi_dbm, -55);
    }

    #[test]
    fn an_aux_identity_names_a_matching_beacon_row() {
        let cache = GsPresenceCache::new();
        cache.record_peer("drone-a".into(), "drone".into(), 157, -48, 1);
        cache.set_aux_identities(vec![("drone-a".into(), Some("Alpha".into()))]);
        let peers = cache.linked_peers();
        assert_eq!(peers[0].device_id, "drone-a");
        assert_eq!(peers[0].name.as_deref(), Some("Alpha"));
    }

    #[test]
    fn an_idless_row_is_not_guessed_at_when_several_peers_are_known() {
        // With more than one identity and no id on the beacon there is no way to
        // tell which is which, and a guess would attach one node's identity to
        // another node's signal.
        let cache = GsPresenceCache::new();
        cache.record_peer(String::new(), "drone".into(), 149, -55, 1);
        cache.set_aux_identities(vec![
            ("drone-a".into(), Some("Alpha".into())),
            ("drone-b".into(), Some("Bravo".into())),
        ]);
        let peers = cache.linked_peers();
        assert_eq!(
            peers[0].device_id, "",
            "an ambiguous id must not be guessed"
        );
        assert_eq!(peers[0].name, None);
    }

    #[test]
    fn an_identity_for_an_unheard_peer_does_not_invent_a_row() {
        // An identity proves the peer is speaking, not that this receiver decoded
        // its beacon. Publishing a row would require a channel and an RSSI that
        // were never measured.
        let cache = GsPresenceCache::new();
        cache.set_aux_identities(vec![("drone-a".into(), Some("Alpha".into()))]);
        assert!(cache.linked_peers().is_empty());
    }

    #[test]
    fn a_withdrawn_identity_stops_naming_the_peer() {
        // The setter takes the current fresh set, so a peer that went quiet loses
        // its label rather than keeping it as a confident stale claim.
        let cache = GsPresenceCache::new();
        cache.record_peer("drone-a".into(), "drone".into(), 157, -48, 1);
        cache.set_aux_identities(vec![("drone-a".into(), Some("Alpha".into()))]);
        assert_eq!(cache.linked_peers()[0].name.as_deref(), Some("Alpha"));

        cache.set_aux_identities(vec![]);
        assert_eq!(cache.linked_peers()[0].name, None);
    }

    #[test]
    fn the_name_is_omitted_from_the_sidecar_when_absent() {
        // Additive by construction: a peer with no name serializes exactly the
        // shape existing readers already parse.
        let cache = GsPresenceCache::new();
        cache.record_peer("drone-a".into(), "drone".into(), 157, -48, 1);
        let payload = linked_peers_payload(&cache.linked_peers());
        let entry = &payload["peers"][0];
        assert!(entry.get("name").is_none());
        assert_eq!(entry["device_id"], "drone-a");

        cache.set_aux_identities(vec![("drone-a".into(), Some("Alpha".into()))]);
        let payload = linked_peers_payload(&cache.linked_peers());
        assert_eq!(payload["peers"][0]["name"], "Alpha");
    }
}
