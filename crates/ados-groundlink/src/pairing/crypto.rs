//! Mesh-invite crypto: X25519 ECDH + a custom 2-step HMAC-SHA256 session-key
//! derivation + ChaCha20Poly1305 AEAD.
//!
//! Wire-compatible with the agent's Python `invite_crypto` module. The
//! session-key derivation is NOT RFC 5869 HKDF: it is a bespoke 2-step
//! HMAC-SHA256 construction (`HMAC(salt=0x00*32, shared) -> prk;
//! HMAC(prk, context || 0x01)`), built here from `hmac` + `sha2`. The `hkdf`
//! crate is deliberately NOT used because RFC 5869 differs in the info/counter
//! framing and would not interoperate with a Python-produced bundle.
//!
//! The context binds the accept window's six-digit code:
//! `b"ados-mesh-invite" ‖ 0x00 ‖ code`. ECDH alone does not tell the relay who
//! answered its join request (any node that heard it holds the relay's public
//! key); only a receiver that showed the code to its operator can seal an
//! invite that opens under it.
//!
//! Wire format of an encrypted invite (matches `encrypt_invite`):
//!   `32B receiver_pubkey ‖ 12B nonce ‖ N ciphertext‖tag`

use chacha20poly1305::aead::Aead;
use chacha20poly1305::{ChaCha20Poly1305, Key, KeyInit, Nonce};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use x25519_dalek::{PublicKey, StaticSecret};

use super::invite::InviteBundle;

type HmacSha256 = Hmac<Sha256>;

/// The fixed prefix of the session-key context. Load-bearing for interop:
/// changing it makes a Python-produced bundle undecryptable.
pub const INVITE_CONTEXT: &[u8] = b"ados-mesh-invite";

/// Length of the accept window's code, in decimal digits.
pub const INVITE_CODE_DIGITS: usize = 6;

/// The session-key context for an accept window's `code`: `INVITE_CONTEXT ‖
/// 0x00 ‖ code`. Refuses anything but six ASCII digits.
pub fn invite_context(code: &str) -> Result<Vec<u8>, CryptoError> {
    if code.len() != INVITE_CODE_DIGITS || !code.bytes().all(|b| b.is_ascii_digit()) {
        return Err(CryptoError::BadCode);
    }
    let mut context = Vec::with_capacity(INVITE_CONTEXT.len() + 1 + code.len());
    context.extend_from_slice(INVITE_CONTEXT);
    context.push(0x00);
    context.extend_from_slice(code.as_bytes());
    Ok(context)
}

/// Errors from the invite crypto path.
#[derive(Debug)]
pub enum CryptoError {
    /// The encrypted blob is shorter than `32 + 12 + 16` bytes.
    BlobTooShort,
    /// The receiver public-key field was not 32 bytes.
    BadPublicKey,
    /// AEAD decrypt failed (wrong key, tampered ciphertext, or bad nonce).
    DecryptFailed,
    /// The decrypted bundle JSON did not parse.
    BadBundle(String),
    /// The bundle's `expires_at_ms` is in the past.
    Expired,
    /// The accept-window code was not six ASCII digits.
    BadCode,
}

impl std::fmt::Display for CryptoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CryptoError::BlobTooShort => write!(f, "invite blob too short"),
            CryptoError::BadPublicKey => write!(f, "invite public key not 32 bytes"),
            CryptoError::DecryptFailed => write!(f, "invite decrypt failed"),
            CryptoError::BadBundle(e) => write!(f, "invite bundle parse failed: {e}"),
            CryptoError::Expired => write!(f, "invite expired"),
            CryptoError::BadCode => write!(f, "invite code must be six digits"),
        }
    }
}

impl std::error::Error for CryptoError {}

/// An X25519 keypair: the owned secret + its raw 32-byte public half.
pub struct KeyPair {
    pub secret: StaticSecret,
    pub public: [u8; 32],
}

/// Generate an X25519 keypair. The public half is the raw 32-byte encoding
/// (matching Python's `public_bytes(Encoding.Raw, PublicFormat.Raw)`).
pub fn generate_keypair() -> KeyPair {
    let mut seed = [0u8; 32];
    getrandom::getrandom(&mut seed).expect("OS RNG for X25519 keygen");
    let secret = StaticSecret::from(seed);
    let public = PublicKey::from(&secret).to_bytes();
    KeyPair { secret, public }
}

/// Derive the 32-byte ChaCha20Poly1305 session key from the ECDH shared secret.
///
/// This is the CUSTOM 2-step construction, NOT RFC 5869, identical to the
/// Python `invite_crypto.session_key`:
///
/// 1. `prk = HMAC-SHA256(key = 0x00 * 32, msg = shared)`
/// 2. `okm = HMAC-SHA256(key = prk, msg = context || 0x01)`
pub fn session_key(shared: &[u8], context: &[u8]) -> [u8; 32] {
    // Step 1: extract with an all-zero 32-byte salt. The fully-qualified
    // `Mac::` disambiguates from chacha's `KeyInit::new_from_slice`.
    let mut mac =
        <HmacSha256 as Mac>::new_from_slice(&[0u8; 32]).expect("HMAC accepts any key length");
    mac.update(shared);
    let prk = mac.finalize().into_bytes();

    // Step 2: expand with the context and a single 0x01 counter byte.
    let mut mac2 = <HmacSha256 as Mac>::new_from_slice(&prk).expect("HMAC accepts any key length");
    mac2.update(context);
    mac2.update(&[0x01]);
    mac2.finalize().into_bytes().into()
}

/// Encrypt an invite bundle for `relay_pubkey` using `receiver_secret`, under
/// the accept window's `code`.
///
/// Returns the wire blob `receiver_pub ‖ nonce ‖ ct‖tag`. AEAD AD is empty
/// (matching Python's `associated_data=None`).
pub fn encrypt_invite(
    bundle: &InviteBundle,
    receiver_secret: &StaticSecret,
    relay_pubkey: &[u8],
    code: &str,
) -> Result<Vec<u8>, CryptoError> {
    let context = invite_context(code)?;
    let peer = pubkey_from_bytes(relay_pubkey)?;
    let shared = receiver_secret.diffie_hellman(&peer);
    let key = session_key(shared.as_bytes(), &context);

    let mut nonce_bytes = [0u8; 12];
    getrandom::getrandom(&mut nonce_bytes).expect("OS RNG for nonce");
    let cipher = ChaCha20Poly1305::new(Key::from_slice(&key));
    let ct = cipher
        .encrypt(Nonce::from_slice(&nonce_bytes), bundle.pack().as_slice())
        .map_err(|_| CryptoError::DecryptFailed)?;

    let receiver_pub = PublicKey::from(receiver_secret).to_bytes();
    let mut blob = Vec::with_capacity(32 + 12 + ct.len());
    blob.extend_from_slice(&receiver_pub);
    blob.extend_from_slice(&nonce_bytes);
    blob.extend_from_slice(&ct);
    Ok(blob)
}

/// Decrypt an invite received from the receiver using `relay_secret` and the
/// accept-window `code` the relay's operator entered.
///
/// Verifies the wire framing, runs the ECDH + AEAD decrypt, parses the bundle,
/// and enforces the `expires_at_ms` freshness check against wall-clock now. An
/// invite sealed under another code fails as [`CryptoError::DecryptFailed`].
pub fn decrypt_invite(
    blob: &[u8],
    relay_secret: &StaticSecret,
    code: &str,
) -> Result<InviteBundle, CryptoError> {
    let context = invite_context(code)?;
    if blob.len() < 32 + 12 + 16 {
        return Err(CryptoError::BlobTooShort);
    }
    let receiver_pub = pubkey_from_bytes(&blob[..32])?;
    let nonce = &blob[32..44];
    let ct = &blob[44..];

    let shared = relay_secret.diffie_hellman(&receiver_pub);
    let key = session_key(shared.as_bytes(), &context);
    let cipher = ChaCha20Poly1305::new(Key::from_slice(&key));
    let plaintext = cipher
        .decrypt(Nonce::from_slice(nonce), ct)
        .map_err(|_| CryptoError::DecryptFailed)?;

    let bundle = InviteBundle::unpack(&plaintext).map_err(CryptoError::BadBundle)?;
    let now_ms = now_ms();
    if now_ms > bundle.expires_at_ms {
        return Err(CryptoError::Expired);
    }
    Ok(bundle)
}

/// Build an X25519 `PublicKey` from a 32-byte slice (Raw encoding).
fn pubkey_from_bytes(bytes: &[u8]) -> Result<PublicKey, CryptoError> {
    let arr: [u8; 32] = bytes.try_into().map_err(|_| CryptoError::BadPublicKey)?;
    Ok(PublicKey::from(arr))
}

/// Wall-clock unix milliseconds (the invite issued/expiry timeline).
pub fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CODE: &str = "482910";

    fn bundle(expires_at_ms: i64) -> InviteBundle {
        InviteBundle {
            mesh_id: "m".into(),
            mesh_psk: vec![1; 32],
            drone_channel: 36,
            wfb_rx_key: vec![2; 64],
            receiver_mdns_host: "h.local".into(),
            receiver_mdns_port: 5800,
            issued_at_ms: expires_at_ms - 120_000,
            expires_at_ms,
        }
    }

    /// The same vector the Python invite crypto test pins: both sides must
    /// derive this key for this shared secret and code, or an invite sealed by
    /// one never opens on the other.
    #[test]
    fn the_code_bound_session_key_matches_the_python_vector() {
        let context = invite_context("123456").unwrap();
        assert_eq!(context, b"ados-mesh-invite\x00123456");
        assert_eq!(
            hex::encode(session_key(&[0x11u8; 32], &context)),
            "b286a016663b9570232bda114a1f86cced6679ca9e7357ca17b118ea9a71c200"
        );
        for bad in ["12345", "1234567", "12345a", "", "１２３４５６"] {
            assert!(matches!(invite_context(bad), Err(CryptoError::BadCode)));
        }
    }

    /// The KDF is the exact 2-step construction, reconstructed independently.
    #[test]
    fn session_key_matches_custom_two_step_construction() {
        let shared = [0x11u8; 32];
        let key = session_key(&shared, INVITE_CONTEXT);

        // Recompute the 2-step construction independently here to lock the exact
        // wiring (salt = 0x00*32, then HMAC(prk, context || 0x01)). The
        // fully-qualified `Mac::` disambiguates from the chacha `KeyInit` trait,
        // which also exposes a `new_from_slice` once both are glob-imported.
        let mut m1 = <HmacSha256 as Mac>::new_from_slice(&[0u8; 32]).unwrap();
        m1.update(&shared);
        let prk = m1.finalize().into_bytes();
        let mut m2 = <HmacSha256 as Mac>::new_from_slice(&prk).unwrap();
        m2.update(INVITE_CONTEXT);
        m2.update(&[0x01]);
        let expected: [u8; 32] = m2.finalize().into_bytes().into();

        assert_eq!(key, expected);
        // Sanity: the key is deterministic for a fixed shared secret + context.
        assert_eq!(key, session_key(&shared, INVITE_CONTEXT));
        // A different context yields a different key (the 0x01 counter + context
        // are bound in).
        assert_ne!(key, session_key(&shared, b"other-context"));
    }

    #[test]
    fn encrypt_decrypt_round_trip() {
        let receiver = generate_keypair();
        let relay = generate_keypair();
        let bundle = InviteBundle {
            mesh_id: "ados-abc123def4".into(),
            mesh_psk: vec![0xAB; 32],
            drone_channel: 149,
            wfb_rx_key: vec![0xCD; 64],
            receiver_mdns_host: "gs-recv.local".into(),
            receiver_mdns_port: 5800,
            issued_at_ms: now_ms(),
            expires_at_ms: now_ms() + 120_000,
        };

        let blob = encrypt_invite(&bundle, &receiver.secret, &relay.public, CODE).unwrap();
        // Wire framing: 32 (pub) + 12 (nonce) + ct(>=16 tag).
        assert!(blob.len() >= 32 + 12 + 16);
        assert_eq!(&blob[..32], &PublicKey::from(&receiver.secret).to_bytes());

        let decrypted = decrypt_invite(&blob, &relay.secret, CODE).unwrap();
        assert_eq!(decrypted.mesh_id, bundle.mesh_id);
        assert_eq!(decrypted.mesh_psk, bundle.mesh_psk);
        assert_eq!(decrypted.drone_channel, 149);
        assert_eq!(decrypted.wfb_rx_key, bundle.wfb_rx_key);
        assert_eq!(decrypted.receiver_mdns_host, "gs-recv.local");
        assert_eq!(decrypted.receiver_mdns_port, 5800);
    }

    #[test]
    fn decrypt_rejects_wrong_relay_key() {
        let receiver = generate_keypair();
        let relay = generate_keypair();
        let attacker = generate_keypair();
        let blob = encrypt_invite(
            &bundle(now_ms() + 120_000),
            &receiver.secret,
            &relay.public,
            CODE,
        )
        .unwrap();
        // The attacker's key derives a different shared secret → AEAD fails.
        let err = decrypt_invite(&blob, &attacker.secret, CODE).unwrap_err();
        assert!(matches!(err, CryptoError::DecryptFailed));
    }

    /// Anyone who heard the join request holds the relay's public key and can
    /// seal an invite to it; without the window code it does not open.
    #[test]
    fn an_invite_sealed_under_another_code_does_not_open() {
        let hostile = generate_keypair();
        let relay = generate_keypair();
        let blob = encrypt_invite(
            &bundle(now_ms() + 120_000),
            &hostile.secret,
            &relay.public,
            "000000",
        )
        .unwrap();
        let err = decrypt_invite(&blob, &relay.secret, CODE).unwrap_err();
        assert!(matches!(err, CryptoError::DecryptFailed));
    }

    #[test]
    fn decrypt_rejects_expired_bundle() {
        let receiver = generate_keypair();
        let relay = generate_keypair();
        let blob = encrypt_invite(
            &bundle(now_ms() - 100_000),
            &receiver.secret,
            &relay.public,
            CODE,
        )
        .unwrap();
        let err = decrypt_invite(&blob, &relay.secret, CODE).unwrap_err();
        assert!(matches!(err, CryptoError::Expired));
    }

    #[test]
    fn decrypt_rejects_short_blob() {
        let relay = generate_keypair();
        let err = decrypt_invite(&[0u8; 40], &relay.secret, CODE).unwrap_err();
        assert!(matches!(err, CryptoError::BlobTooShort));
    }

    #[test]
    fn public_key_is_raw_32_bytes() {
        let kp = generate_keypair();
        assert_eq!(kp.public.len(), 32);
    }
}
