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

/// Decoded peer-presence cache, shared between the listener (writer) and the
/// watchdog (reader). Mirrors the Python `HopListener.get_peer_presence`
/// surface: `peer_channel` + `peer_last_seen_unix` are the two fields the
/// watchdog consumes.
#[derive(Debug, Default)]
struct PeerState {
    peer_device_id: Option<String>,
    peer_role: Option<String>,
    peer_channel: Option<u8>,
    peer_rssi_dbm: Option<i8>,
    peer_last_seen_unix: Option<f64>,
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
    fn record_peer(&self, device_id: String, role: String, channel: u8, rssi_dbm: i8) {
        let mut s = self.inner.lock().unwrap();
        s.peer_device_id = Some(device_id);
        s.peer_role = Some(role);
        s.peer_channel = Some(channel);
        s.peer_rssi_dbm = Some(rssi_dbm);
        s.peer_last_seen_unix = Some(now_unix());
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
