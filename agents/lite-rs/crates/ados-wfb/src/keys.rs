//! Key derivation + AEAD for WFB-ng broadcast payloads.
//!
//! The WFB-ng wire format pre-shares a 32-byte symmetric key between the
//! air-side and the ground-side. Operators supply a passphrase via the
//! setup webapp; both sides derive the same 32-byte key from it so a
//! ground decoder seeded with the same passphrase can authenticate the
//! frames coming from the air encoder without the key ever crossing the
//! wire.
//!
//! # Algorithm
//!
//! Matches the WFB-ng project documented passphrase-to-key flow:
//!
//! 1. `seed = SHA-256(passphrase_utf8)` — 32 bytes, deterministic.
//! 2. `secret = x25519_dalek::StaticSecret::from(seed)` — clamps the seed
//!    to a valid Curve25519 scalar.
//! 3. `shared = secret.diffie_hellman(&FIXED_PEER_PUBLIC_KEY)` — X25519
//!    against a documented peer. The peer is a build-time constant so
//!    air + ground agree without exchanging anything online.
//! 4. `key = SHA-256(shared.as_bytes())` — 32 bytes, suitable for
//!    `ChaCha20Poly1305`.
//!
//! The output is pinned by the
//! [`derive_key_kat_matches_pinned_bytes`](tests::derive_key_kat_matches_pinned_bytes)
//! known-answer test: any future regression of the KDF surface is
//! caught at `cargo test`.

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use rand_core::{OsRng, RngCore};
use sha2::{Digest, Sha256};
use thiserror::Error;
use x25519_dalek::{PublicKey, StaticSecret};

/// Length in bytes of the symmetric broadcast key.
pub const KEY_LEN: usize = 32;

/// Length in bytes of the per-frame nonce for ChaCha20-Poly1305.
pub const NONCE_LEN: usize = 12;

/// Length in bytes of a Curve25519 public key (the public component
/// returned by [`generate_keypair`] / [`regenerate_public_key_hex`]).
pub const PUBLIC_KEY_LEN: usize = 32;

/// Number of bytes generated for [`generate_passphrase`]. 24 bytes of
/// OS randomness encoded as 48 hex chars sits comfortably above the
/// 128-bit security floor without producing a passphrase the operator
/// cannot copy.
const FRESH_PASSPHRASE_BYTES: usize = 24;

/// Fixed peer Curve25519 public key. The bytes here are derived from a
/// documented seed (`SHA-256("ados-wfb-v0-fixed-peer")`) so the constant
/// carries no operator data and is reproducible without source
/// archaeology. Any future rotation lands as a single constant change
/// plus a re-pinned KAT in this module.
const FIXED_PEER_PUBLIC_KEY: [u8; 32] = [
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

/// Derive both the public and the symmetric broadcast key for a given
/// passphrase. The public component is what an operator records for
/// ground-side decoder seeding; the secret stays inside the agent
/// process. Returns `(public_key_bytes, broadcast_key_bytes)`.
pub fn derive_keypair(
    passphrase: &str,
) -> Result<([u8; PUBLIC_KEY_LEN], [u8; KEY_LEN]), KeyError> {
    if passphrase.is_empty() {
        return Err(KeyError::EmptyPassphrase);
    }
    let seed = sha256_bytes(passphrase.as_bytes());
    let secret = StaticSecret::from(seed);
    let public = PublicKey::from(&secret);
    let peer = PublicKey::from(FIXED_PEER_PUBLIC_KEY);
    let shared = secret.diffie_hellman(&peer);
    let broadcast = sha256_bytes(shared.as_bytes());
    Ok((public.to_bytes(), broadcast))
}

/// Generate a fresh, OS-entropy-backed passphrase suitable for handing
/// to [`derive_key`]. Used by the `regenerate-key` REST handler so an
/// operator can rotate the broadcast key from the wizard without typing
/// a new passphrase by hand.
///
/// Output is hex-encoded so it round-trips through any text channel an
/// operator may use to ferry it to the ground decoder.
pub fn generate_passphrase() -> String {
    let mut bytes = [0u8; FRESH_PASSPHRASE_BYTES];
    OsRng.fill_bytes(&mut bytes);
    hex_lower(&bytes)
}

/// Compute a fingerprint over a 32-byte key. Returns the first 8 bytes
/// of `SHA-256(key)` hex-encoded — short enough for a wizard banner,
/// long enough to disambiguate two keys at a glance. Used by the
/// regenerate-key REST surface so the operator sees a stable "key
/// fingerprint" without the secret leaking.
pub fn key_fingerprint(key: &[u8; KEY_LEN]) -> String {
    let digest = sha256_bytes(key);
    hex_lower(&digest[..8])
}

/// Return the public key for a passphrase as a hex string. Convenience
/// wrapper for the REST handler that mints fresh keys; never persists
/// the passphrase, only the derived public bytes.
pub fn regenerate_public_key_hex(passphrase: &str) -> Result<String, KeyError> {
    let (public, _broadcast) = derive_keypair(passphrase)?;
    Ok(hex_lower(&public))
}

/// Build a fresh keypair with random scalar (NOT derived from a
/// passphrase). Used when the operator wants the agent to mint and
/// persist the secret rather than supply a passphrase.
pub fn generate_keypair() -> ([u8; PUBLIC_KEY_LEN], [u8; KEY_LEN]) {
    let mut seed = [0u8; 32];
    OsRng.fill_bytes(&mut seed);
    let secret = StaticSecret::from(seed);
    let public = PublicKey::from(&secret);
    let peer = PublicKey::from(FIXED_PEER_PUBLIC_KEY);
    let shared = secret.diffie_hellman(&peer);
    let broadcast = sha256_bytes(shared.as_bytes());
    (public.to_bytes(), broadcast)
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

fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
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

    /// Known-answer test pinning the documented KDF output for a fixed
    /// passphrase. Any change to the key derivation algorithm — algorithm
    /// substitution, peer-constant rotation, hash function swap — surfaces
    /// here as a hard failure so the air + ground halves cannot drift
    /// silently.
    #[test]
    fn derive_key_kat_matches_pinned_bytes() {
        // The fixture passphrase was picked once and is stable forever.
        // The expected key bytes were generated by running the documented
        // algorithm against this passphrase and pinned here. To re-pin
        // (e.g., after a documented peer rotation), set ADOS_WFB_KAT_PRINT=1
        // in the env, run the test once, copy the hex into the EXPECTED
        // constant, and re-run.
        let passphrase = "ados-wfb-kat-fixture-2026";
        let key = derive_key(passphrase).expect("derive_key against fixture");
        if std::env::var_os("ADOS_WFB_KAT_PRINT").is_some() {
            eprintln!("ADOS_WFB_KAT = {}", hex_lower(&key));
        }
        const EXPECTED: [u8; 32] = KAT_EXPECTED;
        assert_eq!(key, EXPECTED, "KDF output drifted from pinned KAT bytes");
    }

    /// Pinned bytes for the KAT fixture passphrase. Generated by the
    /// documented KDF (SHA-256 → Curve25519 → ECDH against
    /// FIXED_PEER_PUBLIC_KEY → SHA-256 finalize) over the passphrase
    /// `ados-wfb-kat-fixture-2026`. See
    /// [`derive_key_kat_matches_pinned_bytes`].
    const KAT_EXPECTED: [u8; 32] = [
        0xf2, 0x4a, 0xeb, 0x2b, 0x03, 0xb9, 0x4e, 0x31, 0xd3, 0xc1, 0xfb, 0x67, 0x5c, 0x0b, 0x51,
        0x15, 0xaf, 0x6d, 0x2c, 0x6c, 0x70, 0x02, 0x6e, 0xe2, 0x8b, 0x81, 0xa2, 0x15, 0xeb, 0xfc,
        0x32, 0x40,
    ];

    /// Generated passphrases are non-empty, hex-encoded, and unique
    /// across calls. Enough rigor that an operator triggering
    /// `regenerate-key` twice cannot land on the same key.
    #[test]
    fn generate_passphrase_is_random_hex() {
        let a = generate_passphrase();
        let b = generate_passphrase();
        assert_ne!(a, b, "generate_passphrase must not produce duplicates");
        assert!(!a.is_empty());
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(a.len(), FRESH_PASSPHRASE_BYTES * 2);
    }

    /// Fingerprint output is exactly 16 hex chars (8 bytes) and stable.
    #[test]
    fn key_fingerprint_is_eight_bytes_hex() {
        let key = derive_key("operator-passphrase").unwrap();
        let fp = key_fingerprint(&key);
        assert_eq!(fp.len(), 16);
        assert!(fp.chars().all(|c| c.is_ascii_hexdigit()));
        // Stable across calls.
        assert_eq!(fp, key_fingerprint(&key));
    }

    /// Public key extraction round-trips via the dedicated keypair path.
    #[test]
    fn derive_keypair_returns_public_and_broadcast() {
        let (pub_bytes, broadcast) = derive_keypair("hello-world").unwrap();
        assert_eq!(pub_bytes.len(), 32);
        assert_eq!(broadcast.len(), 32);
        // The same passphrase fed through the symmetric path produces
        // the same broadcast key.
        let only_broadcast = derive_key("hello-world").unwrap();
        assert_eq!(broadcast, only_broadcast);
    }

    /// Hex helper output is lowercase + correctly padded.
    #[test]
    fn hex_lower_pads_single_byte() {
        assert_eq!(hex_lower(&[0x0a, 0x05]), "0a05");
    }
}
