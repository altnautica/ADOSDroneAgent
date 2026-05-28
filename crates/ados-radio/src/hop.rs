//! FHSS hop supervisor — HopAnnounce/HopAck/PresenceBeacon protocol.
//!
//! Mirrors `services/wfb/hop_supervisor.py`. The drone-side supervisor:
//! - Broadcasts HopAnnounce on 127.0.0.1:5803 every 100ms for 3s.
//! - Waits for a HopAck (echo on UDP 5810) before executing the hop.
//! - Executes: stop wfb_tx → iw set channel → start wfb_tx.
//! - Returns to home channel 149 when peer is stale (>25s since last beacon).
//! - Does not hop until first peer ACK is received (_was_linked gate).
//!
//! Packet formats verified from hop_supervisor.py G1 catalog.

use std::time::{Duration, Instant};

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

/// Control-plane broadcast port (wfb_tx listens on this loopback UDP port).
pub const HOP_CONTROL_PORT: u16 = 5803;
/// HopAck return port.
pub const HOP_ACK_PORT: u16 = 5810;
/// PresenceBeacon interval.
pub const PRESENCE_INTERVAL: Duration = Duration::from_secs(10);
/// Time without a peer beacon before returning to home channel.
pub const PEER_STALE_SECS: f64 = 25.0;
/// Broadcast rounds per hop announcement.
const HOP_BROADCAST_ROUNDS: u32 = 30;
/// Interval between rounds.
const HOP_BROADCAST_INTERVAL: Duration = Duration::from_millis(100);
/// How far in the future the hop epoch is set (same as HOP_COUNTDOWN_MS).
const HOP_EPOCH_ADVANCE_MS: u64 = 3000;

const HOP_MAGIC: &[u8; 8] = b"AD05HOP1";
const PRESENCE_MAGIC: &[u8; 8] = b"AD05PRES";
const HOP_VERSION: u8 = 2;
const PRESENCE_VERSION: u8 = 1;

/// Trigger byte values (hop_supervisor.py:119-130).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum HopTrigger {
    Periodic = 0,
    Reactive = 1,
}

/// Build the 32-byte pair key by SHA-256 of the HMAC derivation string + the
/// 64-byte `/etc/drone.key` shared secret.  Before bind the cold-start
/// constant is used instead.
pub fn derive_pair_key(drone_key: Option<&[u8]>) -> [u8; 32] {
    let mut h = Sha256::new();
    match drone_key {
        Some(key) => {
            h.update(b"ados/wfb/hop/v2\n");
            h.update(key);
        }
        None => {
            // Cold-start fallback — identical on both sides before bind.
            h.update(b"ados/wfb/hop/v2/cold-start");
        }
    }
    h.finalize_reset().into()
}

// Convenience alias.
type HmacSha256 = Hmac<Sha256>;

/// Sign a byte slice with the pair key.
fn sign(data: &[u8], pair_key: &[u8; 32]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(pair_key).expect("HMAC key length valid");
    mac.update(data);
    mac.finalize().into_bytes().into()
}

/// Build a 51-byte HopAnnounce packet (hop_supervisor.py:159-169).
///
/// Layout:
///   [0..8]  magic "AD05HOP1"
///   [8]     version = 2
///   [9..17] epoch_ms as big-endian u64
///   [17]    target_channel
///   [18]    trigger byte
///   [19..51] HMAC-SHA256 of bytes [0..19]
pub fn build_hop_announce(
    epoch_ms: u64,
    target_channel: u8,
    trigger: HopTrigger,
    pair_key: &[u8; 32],
) -> [u8; 51] {
    let mut pkt = [0u8; 51];
    pkt[0..8].copy_from_slice(HOP_MAGIC);
    pkt[8] = HOP_VERSION;
    pkt[9..17].copy_from_slice(&epoch_ms.to_be_bytes());
    pkt[17] = target_channel;
    pkt[18] = trigger as u8;
    let sig = sign(&pkt[0..19], pair_key);
    pkt[19..51].copy_from_slice(&sig);
    pkt
}

/// Verify a received HopAnnounce or HopAck packet.
pub fn verify_hop_packet(pkt: &[u8], pair_key: &[u8; 32]) -> bool {
    if pkt.len() != 51 {
        return false;
    }
    if &pkt[0..8] != HOP_MAGIC {
        return false;
    }
    let expected = sign(&pkt[0..19], pair_key);
    expected == pkt[19..51]
}

/// Build a 68-byte PresenceBeacon (hop_supervisor.py:235-250).
///
/// Layout:
///   [0..8]  magic "AD05PRES"
///   [8]     version = 1
///   [9..25] device_id (16B ASCII, zero-padded)
///   [25]    role: 0x01=drone, 0x02=gs
///   [26]    current channel
///   [27]    rssi_dbm (i8)
///   [28..36] epoch_ms as big-endian u64
///   [36..68] HMAC-SHA256 of bytes [0..36]
pub fn build_presence_beacon(
    device_id: &str,
    role_drone: bool,
    channel: u8,
    rssi_dbm: i8,
    epoch_ms: u64,
    pair_key: &[u8; 32],
) -> [u8; 68] {
    let mut pkt = [0u8; 68];
    pkt[0..8].copy_from_slice(PRESENCE_MAGIC);
    pkt[8] = PRESENCE_VERSION;
    // Device id: up to 16 ASCII bytes, zero-padded.
    let id_bytes = device_id.as_bytes();
    let id_len = id_bytes.len().min(16);
    pkt[9..9 + id_len].copy_from_slice(&id_bytes[..id_len]);
    pkt[25] = if role_drone { 0x01 } else { 0x02 };
    pkt[26] = channel;
    pkt[27] = rssi_dbm as u8;
    pkt[28..36].copy_from_slice(&epoch_ms.to_be_bytes());
    let sig = sign(&pkt[0..36], pair_key);
    pkt[36..68].copy_from_slice(&sig);
    pkt
}

/// Wall-clock unix seconds, for the sidecar timestamps the cross-process
/// heartbeat reader compares against `time.time()`. Gating uses monotonic
/// `Instant` separately so a wall-clock step never breaks staleness logic.
pub fn now_unix() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Decoded peer info from a PresenceBeacon (after HMAC verification).
#[derive(Debug, Clone)]
pub struct PeerPresence {
    pub device_id: String,
    pub role: String, // "drone" | "gs" | "unknown"
    pub channel: u8,
    pub rssi_dbm: i8,
}

/// Verify + parse a 68-byte PresenceBeacon. `None` on bad length/magic/HMAC.
/// Layout matches [`build_presence_beacon`].
pub fn parse_presence_beacon(pkt: &[u8], pair_key: &[u8; 32]) -> Option<PeerPresence> {
    if pkt.len() != 68 || &pkt[0..8] != PRESENCE_MAGIC {
        return None;
    }
    if sign(&pkt[0..36], pair_key) != pkt[36..68] {
        return None;
    }
    let end = pkt[9..25].iter().position(|&b| b == 0).unwrap_or(16);
    let device_id = String::from_utf8_lossy(&pkt[9..9 + end]).into_owned();
    let role = match pkt[25] {
        0x01 => "drone",
        0x02 => "gs",
        _ => "unknown",
    }
    .to_string();
    Some(PeerPresence {
        device_id,
        role,
        channel: pkt[26],
        rssi_dbm: pkt[27] as i8,
    })
}

/// Verify a 51-byte HopAck and return its target channel (byte 17), so the
/// drone can match the ACK to the hop it announced. `None` if invalid.
pub fn parse_hop_ack(pkt: &[u8], pair_key: &[u8; 32]) -> Option<u8> {
    if verify_hop_packet(pkt, pair_key) {
        Some(pkt[17])
    } else {
        None
    }
}

/// One entry in the hop history ring (the `hop-supervisor.json` `history` list).
#[derive(Debug, Clone, serde::Serialize)]
pub struct HopHistoryEntry {
    pub at: f64,
    pub from: u8,
    pub to: u8,
    pub trigger: String,
    pub ok: bool,
}

/// Gating state — the drone only hops after the first peer ACK is received.
#[derive(Debug, Default)]
pub struct HopState {
    /// Whether a peer has ever been seen on this link.
    was_linked: bool,
    /// Monotonic instant of the last peer beacon received.
    peer_last_seen: Option<Instant>,
    /// Wall-clock unix of the last peer beacon (for `peer-presence.json`).
    peer_last_seen_unix: Option<f64>,
    /// Decoded peer (device-id / role / channel / rssi) from the last beacon.
    peer: Option<PeerPresence>,
    /// Current operating channel.
    pub channel: u8,
    /// Home (rendezvous) channel from config — never modified at runtime.
    pub home_channel: u8,
    /// Last hop instant, for reactive cooldown (30s).
    last_hop_at: Option<Instant>,
    /// Wall-clock unix of the last hop (for `hop-supervisor.json`).
    last_hop_at_unix: Option<f64>,
    /// Hop history ring (last 32 kept on read).
    history: Vec<HopHistoryEntry>,
}

impl HopState {
    pub fn new(home_channel: u8) -> Self {
        Self {
            channel: home_channel,
            home_channel,
            ..Default::default()
        }
    }

    /// Record a verified PresenceBeacon from the peer (drives the link gate +
    /// the `peer-presence.json` sidecar).
    pub fn on_peer_beacon(&mut self, presence: PeerPresence) {
        self.was_linked = true;
        self.peer_last_seen = Some(Instant::now());
        self.peer_last_seen_unix = Some(now_unix());
        self.peer = Some(presence);
    }

    /// The decoded peer, if one has been seen.
    pub fn peer(&self) -> Option<&PeerPresence> {
        self.peer.as_ref()
    }

    /// Wall-clock unix of the last peer beacon (for the sidecar).
    pub fn peer_last_seen_unix(&self) -> Option<f64> {
        self.peer_last_seen_unix
    }

    /// Wall-clock unix of the last hop (for the sidecar).
    pub fn last_hop_at_unix(&self) -> Option<f64> {
        self.last_hop_at_unix
    }

    /// The last 32 hop-history entries (the sidecar `history` list).
    pub fn history(&self) -> &[HopHistoryEntry] {
        let n = self.history.len();
        &self.history[n.saturating_sub(32)..]
    }

    /// Test/back-compat shim: record a peer beacon with no decoded payload.
    pub fn on_peer_seen(&mut self) {
        self.was_linked = true;
        self.peer_last_seen = Some(Instant::now());
        self.peer_last_seen_unix = Some(now_unix());
    }

    /// True if the peer has been silent for more than PEER_STALE_SECS.
    pub fn peer_is_stale(&self) -> bool {
        self.peer_last_seen
            .map(|t| t.elapsed().as_secs_f64() > PEER_STALE_SECS)
            .unwrap_or(false)
    }

    /// True if a periodic hop is allowed (link established, peer fresh <60s,
    /// 30s reactive cooldown met).
    pub fn can_hop(&self) -> bool {
        if !self.was_linked {
            return false;
        }
        if self.peer_is_stale() {
            return false;
        }
        if let Some(t) = self.last_hop_at {
            if t.elapsed().as_secs() < 30 {
                return false;
            }
        }
        true
    }

    /// Should we return to home channel? (peer gone >25s after ever being linked)
    pub fn should_return_home(&self) -> bool {
        self.was_linked && self.peer_is_stale() && self.channel != self.home_channel
    }

    /// Record that a hop was executed, appending a history entry.
    pub fn on_hop(&mut self, new_channel: u8) {
        self.record_hop(new_channel, "periodic", true);
    }

    /// Record a hop with an explicit trigger label + outcome (for the history
    /// ring). `from` is the channel before the hop.
    pub fn record_hop(&mut self, new_channel: u8, trigger: &str, ok: bool) {
        let from = self.channel;
        let now = now_unix();
        if ok {
            self.channel = new_channel;
            self.last_hop_at = Some(Instant::now());
            self.last_hop_at_unix = Some(now);
        }
        self.history.push(HopHistoryEntry {
            at: now,
            from,
            to: new_channel,
            trigger: trigger.to_string(),
            ok,
        });
        // Bound the ring so it cannot grow without limit (we only ever expose
        // the last 32, but keep the Vec from ballooning over a long flight).
        if self.history.len() > 128 {
            let drop = self.history.len() - 64;
            self.history.drain(..drop);
        }
    }
}

/// Compute the hop announce epoch: current wall-clock ms + HOP_COUNTDOWN_MS.
pub fn hop_epoch_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    now_ms + HOP_EPOCH_ADVANCE_MS
}

/// Broadcast a HopAnnounce on 127.0.0.1:5803 up to 30 times, 100ms apart.
/// Returns the epoch_ms that was embedded (to be used by `_execute_hop`).
///
/// In the full async manager this runs as a tokio task; here exposed as a pure
/// computation for testability. The caller handles the actual UDP sends.
pub fn hop_announce_rounds() -> u32 {
    HOP_BROADCAST_ROUNDS
}
pub fn hop_announce_interval() -> Duration {
    HOP_BROADCAST_INTERVAL
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_key() -> [u8; 32] {
        derive_pair_key(None)
    }

    #[test]
    fn hop_announce_is_51_bytes() {
        let pkt = build_hop_announce(12345678, 149, HopTrigger::Periodic, &test_key());
        assert_eq!(pkt.len(), 51);
    }

    #[test]
    fn hop_announce_magic_and_version() {
        let pkt = build_hop_announce(0, 36, HopTrigger::Reactive, &test_key());
        assert_eq!(&pkt[0..8], b"AD05HOP1");
        assert_eq!(pkt[8], 2); // version
        assert_eq!(pkt[17], 36); // channel
        assert_eq!(pkt[18], 1); // reactive trigger
    }

    #[test]
    fn hop_announce_verifies_with_same_key() {
        let key = test_key();
        let epoch = 999_000u64;
        let pkt = build_hop_announce(epoch, 149, HopTrigger::Periodic, &key);
        assert!(verify_hop_packet(&pkt, &key));
    }

    #[test]
    fn hop_announce_fails_with_wrong_key() {
        let key = test_key();
        let other_key = derive_pair_key(Some(&[0xABu8; 64]));
        let pkt = build_hop_announce(0, 149, HopTrigger::Periodic, &key);
        assert!(!verify_hop_packet(&pkt, &other_key));
    }

    #[test]
    fn hop_announce_epoch_is_big_endian() {
        let epoch: u64 = 0x0102_0304_0506_0708;
        let pkt = build_hop_announce(epoch, 0, HopTrigger::Periodic, &test_key());
        assert_eq!(
            &pkt[9..17],
            &[0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08]
        );
    }

    #[test]
    fn presence_beacon_is_68_bytes() {
        let pkt = build_presence_beacon("abc-123", true, 149, -48, 0, &test_key());
        assert_eq!(pkt.len(), 68);
    }

    #[test]
    fn presence_beacon_magic_and_role() {
        let pkt = build_presence_beacon("dev1", true, 149, -60, 0, &test_key());
        assert_eq!(&pkt[0..8], b"AD05PRES");
        assert_eq!(pkt[8], 1); // version
        assert_eq!(pkt[25], 0x01); // drone role
        assert_eq!(pkt[26], 149); // channel
    }

    #[test]
    fn presence_beacon_device_id_zero_padded() {
        let pkt = build_presence_beacon("x", false, 0, 0, 0, &test_key());
        assert_eq!(pkt[9], b'x');
        assert_eq!(pkt[10], 0); // zero-padded
    }

    #[test]
    fn cold_start_key_is_deterministic() {
        let k1 = derive_pair_key(None);
        let k2 = derive_pair_key(None);
        assert_eq!(k1, k2);
    }

    #[test]
    fn bound_key_differs_from_cold_start() {
        let cold = derive_pair_key(None);
        let bound = derive_pair_key(Some(&[0u8; 64]));
        assert_ne!(cold, bound);
    }

    #[test]
    fn hop_state_blocks_hop_before_link() {
        let state = HopState::new(149);
        assert!(!state.can_hop()); // _was_linked = false
    }

    #[test]
    fn hop_state_allows_hop_after_peer_seen() {
        let mut state = HopState::new(149);
        state.on_peer_seen();
        // Fresh peer (< 25s): can hop.
        assert!(state.can_hop());
    }

    #[test]
    fn hop_state_should_return_home_when_stale_and_off_home() {
        // Manually set was_linked + stale peer + wrong channel.
        let mut state = HopState::new(149);
        state.was_linked = true;
        state.peer_last_seen = Some(Instant::now() - Duration::from_secs(30));
        state.channel = 36; // not home
        assert!(state.should_return_home());
    }

    #[test]
    fn hop_state_no_return_home_if_on_home() {
        let mut state = HopState::new(149);
        state.was_linked = true;
        state.peer_last_seen = Some(Instant::now() - Duration::from_secs(30));
        state.channel = 149; // already home
        assert!(!state.should_return_home());
    }

    #[test]
    fn hop_announce_roundtrip_constants() {
        assert_eq!(hop_announce_rounds(), 30);
        assert_eq!(hop_announce_interval(), Duration::from_millis(100));
    }

    #[test]
    fn presence_beacon_round_trips_through_parse() {
        let key = test_key();
        let pkt = build_presence_beacon("gs-abc123", false, 149, -52, 12345, &key);
        let p = parse_presence_beacon(&pkt, &key).expect("valid beacon parses");
        assert_eq!(p.device_id, "gs-abc123");
        assert_eq!(p.role, "gs");
        assert_eq!(p.channel, 149);
        assert_eq!(p.rssi_dbm, -52);
    }

    #[test]
    fn presence_beacon_rejects_wrong_key() {
        let key = test_key();
        let other = derive_pair_key(Some(&[9u8; 64]));
        let pkt = build_presence_beacon("x", true, 149, -40, 0, &key);
        assert!(parse_presence_beacon(&pkt, &other).is_none());
    }

    #[test]
    fn hop_ack_parse_returns_target_channel() {
        let key = test_key();
        let pkt = build_hop_announce(0, 157, HopTrigger::Periodic, &key);
        assert_eq!(parse_hop_ack(&pkt, &key), Some(157));
        let other = derive_pair_key(Some(&[1u8; 64]));
        assert_eq!(parse_hop_ack(&pkt, &other), None);
    }

    #[test]
    fn on_peer_beacon_drives_link_gate_and_peer_cache() {
        let mut s = HopState::new(149);
        assert!(!s.can_hop()); // not linked yet
        s.on_peer_beacon(PeerPresence {
            device_id: "gs1".into(),
            role: "gs".into(),
            channel: 149,
            rssi_dbm: -48,
        });
        assert!(s.can_hop());
        assert_eq!(s.peer().unwrap().device_id, "gs1");
        assert!(s.peer_last_seen_unix().is_some());
    }

    #[test]
    fn record_hop_appends_history_and_caps_exposure_at_32() {
        let mut s = HopState::new(149);
        for i in 0..40u8 {
            s.record_hop(36 + (i % 5), "periodic", true);
        }
        // Only the last 32 are exposed even though 40 were recorded.
        assert_eq!(s.history().len(), 32);
        assert!(s.last_hop_at_unix().is_some());
    }

    #[test]
    fn record_hop_failure_keeps_channel_but_logs_history() {
        let mut s = HopState::new(149);
        s.record_hop(157, "reactive", false);
        assert_eq!(s.channel, 149); // failed hop does not change channel
        assert_eq!(s.history().len(), 1);
        assert!(!s.history()[0].ok);
        assert_eq!(s.history()[0].trigger, "reactive");
    }
}
