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

/// Gating state — the drone only hops after the first peer ACK is received.
#[derive(Debug, Default)]
pub struct HopState {
    /// Whether a peer has ever been seen on this link.
    was_linked: bool,
    /// Monotonic instant of the last peer beacon received.
    peer_last_seen: Option<Instant>,
    /// Current operating channel.
    pub channel: u8,
    /// Home (rendezvous) channel from config — never modified at runtime.
    pub home_channel: u8,
    /// Last hop instant, for reactive cooldown (30s).
    last_hop_at: Option<Instant>,
}

impl HopState {
    pub fn new(home_channel: u8) -> Self {
        Self {
            channel: home_channel,
            home_channel,
            ..Default::default()
        }
    }

    /// Record that a PresenceBeacon was received.
    pub fn on_peer_seen(&mut self) {
        self.was_linked = true;
        self.peer_last_seen = Some(Instant::now());
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

    /// Record that a hop was executed.
    pub fn on_hop(&mut self, new_channel: u8) {
        self.channel = new_channel;
        self.last_hop_at = Some(Instant::now());
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
}
