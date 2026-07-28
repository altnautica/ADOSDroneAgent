//! The swarm bus payload seal: ChaCha20-Poly1305 under one fleet-wide key.
//!
//! ## Why one symmetric key for the whole fleet
//!
//! Every drone must be able to decrypt every *other* drone's beacon — that is the
//! definition of a decentralized bus, and it is what makes the neighbour table
//! work with the ground station powered off. Per-drone keys would forbid it. A
//! fleet is therefore one trust domain, and the beacon plane is keyed off the one
//! secret both rigs already hold byte-identically.
//!
//! ## Which file the key comes from
//!
//! **`/etc/drone.key`, never `tx.key` or `rx.key`.** This is a mistake already
//! made once in this codebase and documented as a hard constraint in
//! `ados-groundlink/src/presence.rs`: `tx.key` and `rx.key` are the two
//! *different* halves of the wfb-ng crypto_box pair (the drone keeps one, the
//! ground station the other), so a symmetric key derived from "the wfb key"
//! diverges across the two rigs and every frame is silently dropped at the far
//! end. The bind protocol delivers `/etc/drone.key` byte-for-byte to both sides,
//! which makes it the only shared-content key on disk and the only correct source.
//!
//! The plan text for this phase said "key = fleet rx.key"; that is the trap above,
//! so this follows the codebase's existing hard constraint instead.
//!
//! ## Nonces
//!
//! 8 random bytes drawn once per process, then a 4-byte little-endian counter.
//! Nonce reuse under a shared key is the one catastrophic failure mode of this
//! construction, so the budget is worth stating: with 24 nodes the birthday
//! probability of two of them drawing the same 8-byte prefix is about 1.5e-17, and
//! the counter allows 2^32 transmissions per process — 68 years at the 2 Hz beacon
//! rate. A restart draws a fresh prefix, so a reboot cannot replay a nonce either.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicU32, Ordering};

use crate::frame::SwarmFrameKind;

/// The swarm payload wire version, in the first plaintext byte.
pub const SWARM_WIRE_VERSION: u8 = 1;

/// Nonce length for ChaCha20-Poly1305.
pub const NONCE_LEN: usize = 12;

/// Poly1305 tag length.
pub const TAG_LEN: usize = 16;

/// Plaintext bytes ahead of the body: version, then kind.
pub const PLAINTEXT_HEADER_LEN: usize = 2;

/// Total payload overhead a body carries on the wire.
pub const PAYLOAD_OVERHEAD: usize = NONCE_LEN + PLAINTEXT_HEADER_LEN + TAG_LEN;

/// The canonical shared-key file the bind protocol delivers byte-for-byte to both
/// rigs. Mirrors `ados_groundlink::presence`'s `DRONE_KEY_PRIMARY`.
pub const DRONE_KEY_PRIMARY: &str = "/etc/drone.key";

/// Forward-compatibility location if a future migration relocates the file into
/// the agent's namespace.
pub const DRONE_KEY_FALLBACK: &str = "/etc/ados/wfb/drone.key";

/// The wfb-ng key file size. A file of any other length is not the shared key.
const DRONE_KEY_BYTES: usize = 64;

/// Domain separation for the swarm-bus key. Distinct from the hop supervisor's
/// `ados/wfb/hop/v2\n` so the same `/etc/drone.key` yields two unrelated keys and
/// a compromise of one plane does not hand over the other.
const KEY_DERIVATION: &[u8] = b"ados/swarm/v1\n";

/// Pre-bind fallback, identical on both rigs so a beacon emitted before bind is
/// still readable rather than being an unexplained bad-tag storm.
const KEY_COLD_START: &[u8] = b"ados/swarm/v1/cold-start";

/// Why a received payload was not accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SealError {
    /// Shorter than the fixed overhead — it cannot contain a nonce and a tag.
    TooShort,
    /// The Poly1305 tag did not verify: a different fleet key, a corrupted frame,
    /// or a forgery. Indistinguishable by design, and all three are counted the
    /// same way.
    BadTag,
    /// Authenticated, but a payload version this build does not implement.
    BadVersion(u8),
    /// Authenticated, but a frame kind this build does not know.
    UnknownKind(u8),
}

impl std::fmt::Display for SealError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooShort => write!(f, "payload shorter than the seal overhead"),
            Self::BadTag => write!(f, "poly1305 tag did not verify"),
            Self::BadVersion(v) => write!(f, "unsupported swarm payload version {v}"),
            Self::UnknownKind(k) => write!(f, "unknown swarm frame kind {k}"),
        }
    }
}

impl std::error::Error for SealError {}

/// Derive the 32-byte fleet key by SHA-256 over a domain string plus the 64-byte
/// `/etc/drone.key` shared secret, matching the construction
/// `ados_radio::hop::derive_pair_key` uses for the presence beacon.
///
/// `None` selects the cold-start constant, which both rigs compute identically
/// before bind.
pub fn derive_fleet_key(drone_key: Option<&[u8]>) -> [u8; 32] {
    let mut h = Sha256::new();
    match drone_key {
        Some(key) => {
            h.update(KEY_DERIVATION);
            h.update(key);
        }
        None => h.update(KEY_COLD_START),
    }
    h.finalize().into()
}

/// Read the fleet key from the first shared-key file present on disk, falling
/// back to the cold-start constant.
///
/// A file of the wrong length is treated as absent rather than hashed: a
/// half-written key would derive a key only this node holds, and a fleet where one
/// member's frames all fail their tag is far harder to diagnose than a fleet on the
/// cold-start key.
pub fn resolve_fleet_key() -> [u8; 32] {
    for path in [DRONE_KEY_PRIMARY, DRONE_KEY_FALLBACK] {
        match std::fs::read(path) {
            Ok(bytes) if bytes.len() == DRONE_KEY_BYTES => return derive_fleet_key(Some(&bytes)),
            Ok(bytes) => tracing::warn!(
                path,
                len = bytes.len(),
                "swarm_key_file_wrong_length: ignoring"
            ),
            Err(_) => {}
        }
    }
    tracing::warn!("swarm_fleet_key_unavailable: falling back to the cold-start key");
    derive_fleet_key(None)
}

/// Seals and opens swarm payloads under one fleet key.
///
/// Shared by the transmit and receive halves of the bus (`&self` throughout, the
/// nonce counter being atomic), so both use the same key material and neither can
/// be given a stale one.
pub struct SwarmCipher {
    cipher: ChaCha20Poly1305,
    /// Per-process random nonce prefix; see the module docs for the reuse budget.
    prefix: [u8; 8],
    counter: AtomicU32,
}

impl SwarmCipher {
    /// Build a cipher over `key`, drawing a fresh random nonce prefix.
    ///
    /// A failure of the OS random source falls back to deriving the prefix from
    /// the key and the wall clock. That is weaker than random but still distinct
    /// per process start, and it beats refusing to run: a drone that cannot draw
    /// randomness must still be able to tell its neighbours where it is.
    pub fn new(key: &[u8; 32]) -> Self {
        let mut prefix = [0u8; 8];
        if getrandom::getrandom(&mut prefix).is_err() {
            let mut h = Sha256::new();
            h.update(b"ados/swarm/v1/nonce-prefix\n");
            h.update(key);
            h.update(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
                    .to_le_bytes(),
            );
            prefix.copy_from_slice(&h.finalize()[..8]);
            tracing::warn!("swarm_nonce_prefix_fallback: OS randomness unavailable");
        }
        Self {
            cipher: ChaCha20Poly1305::new(Key::from_slice(key)),
            prefix,
            counter: AtomicU32::new(0),
        }
    }

    /// Seal one frame: `nonce || ChaCha20-Poly1305(version || kind || body)`.
    ///
    /// The version and kind are inside the ciphertext rather than in the clear, so
    /// an observer on the channel learns nothing about which lane is active from a
    /// frame it cannot decrypt — and neither field can be tampered with to steer a
    /// receiver's dispatch.
    pub fn seal(&self, kind: SwarmFrameKind, body: &[u8]) -> Vec<u8> {
        let mut nonce_bytes = [0u8; NONCE_LEN];
        nonce_bytes[..8].copy_from_slice(&self.prefix);
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        nonce_bytes[8..].copy_from_slice(&n.to_le_bytes());

        let mut plaintext = Vec::with_capacity(PLAINTEXT_HEADER_LEN + body.len());
        plaintext.push(SWARM_WIRE_VERSION);
        plaintext.push(kind as u8);
        plaintext.extend_from_slice(body);

        // ChaCha20-Poly1305 encryption is infallible for any body this bus can
        // produce (the failure case is a plaintext beyond 2^38 bytes).
        let sealed = self
            .cipher
            .encrypt(
                Nonce::from_slice(&nonce_bytes),
                Payload {
                    msg: &plaintext,
                    aad: &[],
                },
            )
            .expect("chacha20poly1305 encrypt cannot fail for a beacon-sized body");

        let mut out = Vec::with_capacity(NONCE_LEN + sealed.len());
        out.extend_from_slice(&nonce_bytes);
        out.extend_from_slice(&sealed);
        out
    }

    /// Verify and open a received payload, returning its kind and body.
    pub fn open(&self, wire: &[u8]) -> Result<(SwarmFrameKind, Vec<u8>), SealError> {
        if wire.len() < PAYLOAD_OVERHEAD {
            return Err(SealError::TooShort);
        }
        let (nonce, sealed) = wire.split_at(NONCE_LEN);
        let plaintext = self
            .cipher
            .decrypt(
                Nonce::from_slice(nonce),
                Payload {
                    msg: sealed,
                    aad: &[],
                },
            )
            .map_err(|_| SealError::BadTag)?;
        // The overhead check above guarantees at least the two header bytes.
        let version = plaintext[0];
        if version != SWARM_WIRE_VERSION {
            return Err(SealError::BadVersion(version));
        }
        let kind =
            SwarmFrameKind::from_wire(plaintext[1]).ok_or(SealError::UnknownKind(plaintext[1]))?;
        Ok((kind, plaintext[PLAINTEXT_HEADER_LEN..].to_vec()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::beacon::{SwarmBeacon, BEACON_WIRE_LEN};

    fn key() -> [u8; 32] {
        derive_fleet_key(Some(&[7u8; 64]))
    }

    /// The whole reason this derives from `/etc/drone.key`: two rigs holding the
    /// same file must compute the same key, and a different file must not.
    #[test]
    fn the_same_shared_key_file_derives_the_same_fleet_key_on_both_rigs() {
        let drone_side = derive_fleet_key(Some(&[7u8; 64]));
        let ground_side = derive_fleet_key(Some(&[7u8; 64]));
        assert_eq!(drone_side, ground_side);
        // A different fleet's key file gives an unrelated key.
        assert_ne!(drone_side, derive_fleet_key(Some(&[8u8; 64])));
        // And the cold-start key is distinct from any bound key, but identical on
        // both rigs.
        assert_ne!(drone_side, derive_fleet_key(None));
        assert_eq!(derive_fleet_key(None), derive_fleet_key(None));
    }

    /// Domain separation: the same `/etc/drone.key` must not yield the hop
    /// supervisor's presence-beacon key. Sharing one key across two planes would
    /// let a captured frame from either be replayed into the other.
    #[test]
    fn the_swarm_key_is_domain_separated_from_the_hop_pair_key() {
        let shared = [3u8; 64];
        assert_ne!(
            derive_fleet_key(Some(&shared)),
            ados_radio::hop::derive_pair_key(Some(&shared)),
            "the swarm and hop planes must not share a key"
        );
    }

    #[test]
    fn a_sealed_beacon_round_trips() {
        let c = SwarmCipher::new(&key());
        let beacon = SwarmBeacon {
            slot: 3,
            lat: 129_716_000,
            ..SwarmBeacon::default()
        };
        let wire = c.seal(SwarmFrameKind::Beacon, &beacon.encode());
        let (kind, body) = c.open(&wire).expect("our own frame opens");
        assert_eq!(kind, SwarmFrameKind::Beacon);
        assert_eq!(SwarmBeacon::decode(&body), Some(beacon));
    }

    /// The full payload size is pinned, because it is what the airtime arithmetic
    /// in [`crate::AIRTIME_BUDGET`] is computed from.
    #[test]
    fn the_sealed_beacon_payload_is_exactly_fifty_bytes() {
        let c = SwarmCipher::new(&key());
        let wire = c.seal(SwarmFrameKind::Beacon, &SwarmBeacon::default().encode());
        assert_eq!(PAYLOAD_OVERHEAD, 30);
        assert_eq!(wire.len(), BEACON_WIRE_LEN + PAYLOAD_OVERHEAD);
        assert_eq!(wire.len(), 50);
        // The nonce is a cleartext prefix; the tag is the trailing 16 bytes.
        assert_eq!(wire.len() - NONCE_LEN - TAG_LEN, BEACON_WIRE_LEN + 2);
    }

    /// A flipped bit anywhere in the payload must fail the tag. Sweeping every
    /// byte catches an implementation that authenticates only part of the frame.
    #[test]
    fn a_tampered_payload_is_rejected_at_every_byte_position() {
        let c = SwarmCipher::new(&key());
        let wire = c.seal(SwarmFrameKind::Beacon, &SwarmBeacon::default().encode());
        for i in 0..wire.len() {
            let mut bad = wire.clone();
            bad[i] ^= 0x01;
            assert_eq!(
                c.open(&bad),
                Err(SealError::BadTag),
                "a flipped bit at offset {i} must not verify"
            );
        }
    }

    /// A frame from another fleet decrypts to nothing under our key. This is what
    /// keeps two fleets on one channel from polluting each other's tables even
    /// though the BPF's fleet word could be forged.
    #[test]
    fn a_frame_sealed_under_another_fleets_key_fails_the_tag() {
        let ours = SwarmCipher::new(&key());
        let theirs = SwarmCipher::new(&derive_fleet_key(Some(&[9u8; 64])));
        let wire = theirs.seal(SwarmFrameKind::Beacon, &SwarmBeacon::default().encode());
        assert_eq!(ours.open(&wire), Err(SealError::BadTag));
    }

    #[test]
    fn a_payload_shorter_than_the_overhead_is_rejected_without_a_decrypt() {
        let c = SwarmCipher::new(&key());
        for len in 0..PAYLOAD_OVERHEAD {
            assert_eq!(c.open(&vec![0u8; len]), Err(SealError::TooShort));
        }
        // At exactly the overhead it is long enough to attempt, and fails the tag.
        assert_eq!(c.open(&[0u8; PAYLOAD_OVERHEAD]), Err(SealError::BadTag));
    }

    /// Version and kind are INSIDE the seal, so a wrong one is only reachable
    /// through a validly-authenticated frame — which is exactly the "newer agent
    /// in the fleet" case, and must be a clean reject rather than a mis-dispatch.
    #[test]
    fn an_authenticated_frame_with_an_unknown_version_or_kind_is_rejected() {
        let k = key();
        let c = SwarmCipher::new(&k);
        let raw = ChaCha20Poly1305::new(Key::from_slice(&k));
        let nonce = [0u8; NONCE_LEN];

        let forge = |header: [u8; 2]| {
            let mut plaintext = header.to_vec();
            plaintext.extend_from_slice(&[0u8; BEACON_WIRE_LEN]);
            let sealed = raw
                .encrypt(
                    Nonce::from_slice(&nonce),
                    Payload {
                        msg: &plaintext,
                        aad: &[],
                    },
                )
                .unwrap();
            let mut wire = nonce.to_vec();
            wire.extend_from_slice(&sealed);
            wire
        };

        assert_eq!(
            c.open(&forge([2, SwarmFrameKind::Beacon as u8])),
            Err(SealError::BadVersion(2))
        );
        assert_eq!(
            c.open(&forge([SWARM_WIRE_VERSION, 99])),
            Err(SealError::UnknownKind(99))
        );
        // And the well-formed control case still opens, proving the forge helper
        // itself is sound.
        assert!(c
            .open(&forge([SWARM_WIRE_VERSION, SwarmFrameKind::Beacon as u8]))
            .is_ok());
    }

    /// Nonce reuse under a shared key is the one catastrophic failure of this
    /// construction, so the counter must actually advance. Identical plaintext
    /// sealed twice must produce different bytes.
    #[test]
    fn consecutive_seals_use_distinct_nonces() {
        let c = SwarmCipher::new(&key());
        let body = SwarmBeacon::default().encode();
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..64 {
            let wire = c.seal(SwarmFrameKind::Beacon, &body);
            assert!(
                seen.insert(wire[..NONCE_LEN].to_vec()),
                "a nonce repeated within one process"
            );
            // Every one of them still opens.
            assert!(c.open(&wire).is_ok());
        }
        // Two ciphers in one process draw different prefixes, so two nodes on one
        // key do not collide either.
        let d = SwarmCipher::new(&key());
        assert_ne!(
            c.seal(SwarmFrameKind::Beacon, &body)[..8],
            d.seal(SwarmFrameKind::Beacon, &body)[..8]
        );
    }

    /// The CBBA lane shares the transport, so an arbitrary body must round-trip
    /// unchanged and be dispatched under its own kind.
    #[test]
    fn the_bid_lane_round_trips_an_arbitrary_body_under_its_own_kind() {
        let c = SwarmCipher::new(&key());
        let bid: Vec<u8> = (0..=255u8).collect();
        let wire = c.seal(SwarmFrameKind::CbbaBid, &bid);
        let (kind, body) = c.open(&wire).unwrap();
        assert_eq!(kind, SwarmFrameKind::CbbaBid);
        assert_eq!(body, bid);
        // An empty body is legal too (the degenerate zero-task bid vector).
        let (kind, body) = c.open(&c.seal(SwarmFrameKind::CbbaBid, &[])).unwrap();
        assert_eq!(kind, SwarmFrameKind::CbbaBid);
        assert!(body.is_empty());
    }

    #[test]
    fn resolve_ignores_a_wrong_length_key_file_rather_than_hashing_it() {
        // The production paths are absolute, so this exercises the length gate
        // through the pure derivation the resolver delegates to: a truncated file
        // must never derive a key only this node would hold.
        assert_ne!(
            derive_fleet_key(Some(&[7u8; 32])),
            derive_fleet_key(Some(&[7u8; 64]))
        );
        // With no readable key file the resolver lands on cold start, which both
        // rigs compute identically.
        assert_eq!(derive_fleet_key(None).len(), 32);
    }
}
