//! Field-only mesh tap-to-pair primitives (relay ↔ receiver invite exchange).
//!
//! Disambiguation: this is the mesh-fabric tap-to-pair manager (relay ↔ receiver
//! invite-bundle exchange over UDP/bat0), NOT the WFB radio-link pair manager.
//! Different concern, different transport.
//!
//! This crate provides the library-level crypto + bundle + revocation
//! primitives that the Python REST router and OLED screens drive. The
//! `PairingManager` UDP state machine (accept-window timers, the asyncio
//! datagram protocol) stays in Python for now; only the deterministic,
//! safety-critical crypto + persistence is ported here, where the borrow
//! checker and the byte-for-byte KDF parity matter most.
//!
//! Crypto wire contract (matches `pairing_manager.py`):
//!   - X25519 ECDH, raw 32-byte public keys.
//!   - Session key: a CUSTOM 2-step HMAC-SHA256 KDF (NOT RFC 5869).
//!   - AEAD: ChaCha20Poly1305, wire `32B receiver_pub ‖ 12B nonce ‖ ct‖tag`,
//!     context `b"ados-mesh-invite"`.
//!   - Invite bundle: `json.dumps(sort_keys=True)` with hex-encoded binaries.
//!   - Revocations: sorted JSON list at `/etc/ados/mesh/revocations.json`, 0600.

pub mod crypto;
pub mod invite;
pub mod revocations;

pub use crypto::{decrypt_invite, encrypt_invite, generate_keypair, session_key, KeyPair};
pub use invite::InviteBundle;

/// The UDP port the receiver binds on `bat0` for relay join requests.
pub const PAIR_UDP_PORT: u16 = 5801;
/// Default accept-window duration (seconds).
pub const DEFAULT_ACCEPT_WINDOW_S: u64 = 60;
/// Invite time-to-live (seconds).
pub const INVITE_TTL_S: i64 = 120;
