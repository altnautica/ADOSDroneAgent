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

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use tokio::net::UdpSocket;

use ados_radio::hop::{build_presence_beacon, derive_pair_key, now_unix, parse_presence_beacon};

use crate::watchdog::PresenceCache;

/// Beacon cadence (10 s, matching the air side).
pub const PRESENCE_CADENCE: Duration = Duration::from_secs(10);

/// GS presence emit destination: `wfb_tx_control`'s loopback ingress. NOT 5803
/// (the listener's bound port); see the module docstring for the self-pair
/// trap that asymmetry avoids.
pub const PRESENCE_EMIT_PORT: u16 = 5810;

/// The control-plane port the listener binds for inbound beacons (the same port
/// `wfb_rx_control` re-emits decoded HopAnnounce/Presence frames on).
pub const PRESENCE_LISTEN_PORT: u16 = 5803;

/// Canonical shared-key file delivered byte-for-byte to both rigs by the bind
/// protocol. AFTER a successful bind both sides have a `/etc/drone.key` with the
/// SAME 64 bytes, so it is the only shared-content key on disk and the right
/// source for a symmetric HMAC derivation.
const DRONE_KEY_PRIMARY: &str = "/etc/drone.key";
/// Forward-compatibility location if a future migration relocates the file into
/// the agent's namespace.
const DRONE_KEY_FALLBACK: &str = "/etc/ados/wfb/drone.key";

/// Resolve the symmetric pair key used to authenticate the presence beacon
/// HMAC, reusing the verified `ados_radio::hop::derive_pair_key`.
///
/// Reads the 64-byte `/etc/drone.key` (then the `/etc/ados/wfb/drone.key`
/// fallback). Cold-start (no key on disk yet) falls back to the deterministic
/// `sha256(b"ados/wfb/hop/v2/cold-start")` constant so a stray beacon still
/// parses before bind.
///
/// HARD CONSTRAINT, do not reintroduce the gs.key/tx.key divergence: an earlier
/// version hashed `/etc/ados/wfb/tx.key` on the drone and `/etc/ados/wfb/rx.key`
/// on the GS. Those are the two DIFFERENT halves of the crypto_box pair (the
/// drone keeps `drone.key`, the GS keeps `gs.key`), so the derived HMAC key
/// diverged across the rigs and every beacon was silently dropped at the
/// listener. The shared file is `/etc/drone.key`, present byte-identical on both
/// rigs after bind. Only ever derive from that.
pub fn resolve_pair_key() -> [u8; 32] {
    for path in [DRONE_KEY_PRIMARY, DRONE_KEY_FALLBACK] {
        if let Ok(key_bytes) = std::fs::read(path) {
            if key_bytes.len() == 64 {
                return derive_pair_key(Some(&key_bytes));
            }
        }
    }
    tracing::warn!("hop_supervisor_pair_key_unavailable");
    derive_pair_key(None)
}

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
    /// entries, matching the Python listener.
    fn record_peer(&self, device_id: String, role: String, channel: u8, rssi_dbm: i8) {
        let mut s = self.inner.lock().unwrap();
        let prev_channel = s.peer_channel;
        s.peer_device_id = Some(device_id);
        s.peer_role = Some(role);
        s.peer_channel = Some(channel);
        s.peer_rssi_dbm = Some(rssi_dbm);
        let now = now_unix();
        s.peer_last_seen_unix = Some(now);
        if prev_channel != Some(channel) {
            s.hop_history.push(HopFollowEntry {
                at: now,
                // The first beacon has no prior channel; record 0 (the Python
                // listener uses 0 for an unknown `from`).
                from: prev_channel.unwrap_or(0),
                to: channel,
                trigger: "periodic".to_string(),
                ok: true,
            });
            if s.hop_history.len() > HOP_HISTORY_CAP {
                let trim = s.hop_history.len() - HOP_HISTORY_CAP;
                s.hop_history.drain(0..trim);
            }
            s.last_hop_at = now;
        }
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
        let pair_key = resolve_pair_key();
        let epoch_ms = (now_unix() * 1000.0) as u64;
        let beacon = build_presence_beacon(
            &device_id,
            // GS role (role byte 0x02). `role_drone = false`.
            false,
            channel_fn(),
            0, // rssi unknown on the emit side
            epoch_ms,
            &pair_key,
        );
        if let Err(e) = sock.send_to(&beacon, target).await {
            tracing::debug!(error = %e, "presence_emit_send_failed");
        }
        tokio::time::sleep(PRESENCE_CADENCE).await;
    }
}

/// Listen for inbound PresenceBeacons on the control port, verify the HMAC, and
/// update `cache`. A frame whose device-id is our own is dropped (the self-pair
/// guard the Python listener applies). Returns only on a fatal bind error or
/// cancellation.
pub async fn listen_loop(cache: GsPresenceCache) -> std::io::Result<()> {
    let own_device_id = read_device_id();
    let sock = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, PRESENCE_LISTEN_PORT)).await?;
    tracing::info!(
        port = PRESENCE_LISTEN_PORT,
        "ground_presence_listen_started"
    );

    let mut buf = [0u8; 256];
    loop {
        let (len, _addr) = sock.recv_from(&mut buf).await?;
        let pair_key = resolve_pair_key();
        let Some(peer) = parse_presence_beacon(&buf[..len], &pair_key) else {
            continue;
        };
        // Self-pair guard: skip a beacon that carries our own device-id (the
        // emit loop's frame can loop back via wfb_rx_control's re-emit). The
        // Python listener compares against the first 16 chars (the beacon
        // device-id field is 16 bytes).
        if !own_device_id.is_empty() {
            let own_trunc: String = own_device_id.chars().take(16).collect();
            if peer.device_id == own_trunc {
                continue;
            }
        }
        cache.record_peer(peer.device_id, peer.role, peer.channel, peer.rssi_dbm);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presence_ports_are_asymmetric() {
        // The trap: emit to 5810 (tx_control ingress), listen on 5803. They must
        // never be the same value or the GS self-pairs over loopback.
        assert_eq!(PRESENCE_EMIT_PORT, 5810);
        assert_eq!(PRESENCE_LISTEN_PORT, 5803);
        assert_ne!(PRESENCE_EMIT_PORT, PRESENCE_LISTEN_PORT);
    }

    #[test]
    fn cold_start_pair_key_matches_radio_crate() {
        // With no key file on disk the resolver must produce the same cold-start
        // key the radio crate derives, so a pre-bind beacon round-trips.
        let resolved = resolve_pair_key();
        let cold = derive_pair_key(None);
        // On a dev host /etc/drone.key is absent, so this is the cold path.
        if !std::path::Path::new(DRONE_KEY_PRIMARY).exists()
            && !std::path::Path::new(DRONE_KEY_FALLBACK).exists()
        {
            assert_eq!(resolved, cold);
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
        cache.record_peer("drone-abc".into(), "drone".into(), 157, -48);
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
        cache.record_peer("drone-1".into(), "drone".into(), 157, -50);
        let s = cache.hop_snapshot("u-nii-3");
        assert_eq!(s.history.len(), 1);
        assert_eq!(s.history[0].from, 0);
        assert_eq!(s.history[0].to, 157);
        assert_eq!(s.history[0].trigger, "periodic");
        assert!(s.history[0].ok);
        assert!(s.last_hop_at > 0.0);

        // Same channel again: no new entry.
        cache.record_peer("drone-1".into(), "drone".into(), 157, -47);
        assert_eq!(cache.hop_snapshot("u-nii-3").history.len(), 1);

        // New channel: a follow from 157 to 149.
        cache.record_peer("drone-1".into(), "drone".into(), 149, -45);
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
            cache.record_peer("drone-1".into(), "drone".into(), ch, -50);
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
        let pair_key = resolve_pair_key();
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
        listener_cache.record_peer(peer.device_id, peer.role, peer.channel, peer.rssi_dbm);

        assert_eq!(cache.announced_channel(), Some(161));
        assert_eq!(cache.peer_channel(), Some(161));
    }
}
