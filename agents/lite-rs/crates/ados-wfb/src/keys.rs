//! Key derivation + AEAD for WFB-ng broadcast payloads.
//!
//! The WFB-ng wire format pre-shares a 32-byte symmetric key between the
//! air-side and the ground-side. Operators supply a passphrase via the
//! setup webapp; both sides derive the same 32-byte key from it so a
//! ground decoder seeded with the same passphrase can authenticate the
//! frames coming from the air encoder without the key ever crossing the
//! wire.
//!
//! # Algorithm (current)
//!
//! 1. `seed = SHA-256(passphrase_utf8)` — 32 bytes, deterministic.
//! 2. `secret = x25519_dalek::StaticSecret::from(seed)` — clamps the seed
//!    to a valid Curve25519 scalar.
//! 3. `shared = secret.diffie_hellman(&FIXED_AGENT_PUBLIC_KEY)` — the
//!    fixed peer public key is a build-time constant so air + ground
//!    agree without exchanging anything.
//! 4. `key = SHA-256(shared.as_bytes())` — 32 bytes, suitable for
//!    `ChaCha20Poly1305`.
//!
//! # Compatibility note
//!
//! A future hardware-validation pass will replay this against the
//! upstream `wfb-ng-cli keygen` output and pin whichever variant the C
//! tooling actually ships. Any divergence at that point is a
//! key-byte-for-key-byte fixup, not an algorithm rethink — the SHA-256
//! pre-image / Curve25519 / SHA-256 finalize structure is the standard
//! recipe for "passphrase to authenticated symmetric key" and the
//! upstream choice is one of: this exact construction, an Argon2id
//! pre-hash instead of SHA-256, or a raw 32-byte clamp without the X25519
//! step. All three are testable as drop-in alternatives.
//
// TODO: validate against wfb-ng-cli reference output.

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use sha2::{Digest, Sha256};
use thiserror::Error;
use x25519_dalek::{PublicKey, StaticSecret};

/// Length in bytes of the symmetric broadcast key.
pub const KEY_LEN: usize = 32;

/// Length in bytes of the per-frame nonce for ChaCha20-Poly1305.
pub const NONCE_LEN: usize = 12;

/// Fixed agent-side Curve25519 public key. The bytes here are a
/// deterministic placeholder (`SHA-256("ados-wfb-v0-fixed-peer")`) so
/// the constant carries no operator data and is easy to rotate. The
/// hardware-validation pass pins this to whatever value the upstream
/// `wfb-ng-cli` actually expects.
const FIXED_PEER_PUBLIC_KEY: [u8; 32] = [
    // SHA-256("ados-wfb-v0-fixed-peer") — round-tripped in the
    // `fixed_peer_constant_matches_documented_seed` test below.
    0x1b, 0x81, 0x4e, 0x49, 0x69, 0x1b, 0xe0, 0xd8, 0x23, 0xe0, 0x6f, 0xa1, 0xee, 0x75, 0x88, 0x07,
    0x78, 0x6c, 0x51, 0xde, 0xfc, 0x30, 0xa8, 0x67, 0x78, 0x01, 0x27, 0x28, 0x1f, 0xb9, 0x48, 0x78,
];

/// Errors that can surface from the key + AEAD layer.
#[derive(Debug, Error)]
pub enum KeyError {
    /// The supplied passphrase was empty. Distinct from the AEAD failures
    /// so the wizard can show "passphrase is required" vs "passphrase did
    /// not authenticate."
    #[error("passphrase must not be empty")]
    EmptyPassphrase,
    /// AEAD seal failed. ChaCha20-Poly1305 cannot fail on encrypt under
    /// normal conditions; a failure here means an out-of-memory or other
    /// host-level fault.
    #[error("aead seal failed")]
    SealFailed,
    /// AEAD open failed. The wrapper key did not authenticate, or the
    /// nonce was reused, or the ciphertext was truncated.
    #[error("aead open failed (key mismatch or tampered ciphertext)")]
    OpenFailed,
}

/// Derive a 32-byte symmetric key from `passphrase` using the algorithm
/// documented in this module's preamble.
pub fn derive_key(passphrase: &str) -> Result<[u8; KEY_LEN], KeyError> {
    if passphrase.is_empty() {
        return Err(KeyError::EmptyPassphrase);
    }

    // Step 1: seed = SHA-256(passphrase).
    let seed = sha256_bytes(passphrase.as_bytes());

    // Step 2: clamp the seed into a Curve25519 scalar.
    let secret = StaticSecret::from(seed);

    // Step 3: ECDH against the fixed peer.
    let peer = PublicKey::from(FIXED_PEER_PUBLIC_KEY);
    let shared = secret.diffie_hellman(&peer);

    // Step 4: SHA-256 the shared secret to produce the broadcast key.
    let key = sha256_bytes(shared.as_bytes());
    Ok(key)
}

/// Seal `plaintext` under `key` with a per-call `nonce`. The caller is
/// responsible for ensuring the nonce is unique across calls under the
/// same key — WFB-ng generates per-frame nonces from a sequence number,
/// which the orchestration layer threads through here.
pub fn seal(key: &[u8; KEY_LEN], nonce: &[u8; NONCE_LEN], plaintext: &[u8]) -> Result<Vec<u8>, KeyError> {
    let cipher = ChaCha20Poly1305::new(key.into());
    let nonce = Nonce::from_slice(nonce);
    cipher
        .encrypt(nonce, plaintext)
        .map_err(|_| KeyError::SealFailed)
}

/// Open a ciphertext produced by [`seal`].
pub fn unseal(key: &[u8; KEY_LEN], nonce: &[u8; NONCE_LEN], ciphertext: &[u8]) -> Result<Vec<u8>, KeyError> {
    let cipher = ChaCha20Poly1305::new(key.into());
    let nonce = Nonce::from_slice(nonce);
    cipher
        .decrypt(nonce, ciphertext)
        .map_err(|_| KeyError::OpenFailed)
}

fn sha256_bytes(input: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(input);
    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Same passphrase always produces the same key bytes. The whole
    /// air-ground key sharing scheme depends on this.
    #[test]
    fn derive_key_is_deterministic() {
        let a = derive_key("hunter2-correct-horse").unwrap();
        let b = derive_key("hunter2-correct-horse").unwrap();
        assert_eq!(a, b);
    }

    /// Different passphrases must yield different keys. A collision here
    /// would let an attacker who knew any common passphrase decrypt
    /// traffic from a different operator.
    #[test]
    fn derive_key_differs_per_passphrase() {
        let a = derive_key("alpha").unwrap();
        let b = derive_key("bravo").unwrap();
        assert_ne!(a, b);
    }

    /// Output is exactly 32 bytes — the contract `ChaCha20Poly1305` and
    /// the WFB-ng wire format both pin.
    #[test]
    fn derive_key_outputs_32_bytes() {
        let k = derive_key("any-non-empty-string").unwrap();
        assert_eq!(k.len(), 32);
    }

    /// Empty passphrase is a typed error, not a silent success that
    /// would derive a known key.
    #[test]
    fn derive_key_rejects_empty_passphrase() {
        match derive_key("") {
            Err(KeyError::EmptyPassphrase) => {}
            other => panic!("expected EmptyPassphrase, got {other:?}"),
        }
    }

    /// Round-trip: seal then unseal returns the original bytes.
    #[test]
    fn chacha_seal_unseal_roundtrip() {
        let key = derive_key("operator-passphrase").unwrap();
        let nonce = [0u8; NONCE_LEN];
        let payload = b"a sample h.264 NAL unit";
        let ct = seal(&key, &nonce, payload).unwrap();
        let pt = unseal(&key, &nonce, &ct).unwrap();
        assert_eq!(pt, payload);
    }

    /// Wrong-key open must fail. This is the property that makes the
    /// broadcast confidential against anyone without the passphrase.
    #[test]
    fn chacha_with_wrong_key_fails() {
        let key_a = derive_key("operator-a").unwrap();
        let key_b = derive_key("operator-b").unwrap();
        let nonce = [0u8; NONCE_LEN];
        let ct = seal(&key_a, &nonce, b"payload").unwrap();
        match unseal(&key_b, &nonce, &ct) {
            Err(KeyError::OpenFailed) => {}
            other => panic!("expected OpenFailed, got {other:?}"),
        }
    }

    /// Documents how the FIXED_PEER_PUBLIC_KEY constant was generated so
    /// any future rotation is reproducible without source archaeology.
    #[test]
    fn fixed_peer_constant_matches_documented_seed() {
        let expected = sha256_bytes(b"ados-wfb-v0-fixed-peer");
        assert_eq!(FIXED_PEER_PUBLIC_KEY, expected);
    }
}
