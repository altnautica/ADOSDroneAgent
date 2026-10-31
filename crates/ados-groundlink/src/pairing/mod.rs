//! Field-only mesh tap-to-pair (relay ↔ receiver invite exchange).
//!
//! Disambiguation: this is the mesh-fabric tap-to-pair (relay ↔ receiver
//! invite-bundle exchange over UDP/bat0), NOT the WFB radio-link pair manager.
//! Different concern, different transport.
//!
//! The whole flow runs in this crate as the `ados-mesh-pairing` service
//! (`ados-groundlink --pairing`), the single owner of pairing state:
//!   - [`manager`]: the receiver's accept window (code, key pair, UDP join
//!     listener, expiry and countdown tick, pending list, approvals).
//!   - [`client`]: the relay's join (resolve the receiver, send the request,
//!     open the invite under the window code, persist the mesh identity).
//!   - [`rekey`] / [`relay_rekey`]: revocation re-keys the mesh and re-seals the
//!     rotated bundle to every relay still paired; relays apply it.
//!   - [`server`]: the newline-JSON RPC on `/run/ados/pairing.sock` that the
//!     native REST front forwards to.
//!   - [`journal`]: the `pair-events.jsonl` event journal.
//!
//! Crypto wire contract:
//!   - X25519 ECDH, raw 32-byte public keys.
//!   - Session key: a CUSTOM 2-step HMAC-SHA256 KDF (NOT RFC 5869), binding
//!     the window's six-digit code.
//!   - AEAD: ChaCha20Poly1305, wire `32B receiver_pub ‖ 12B nonce ‖ ct‖tag`,
//!     context `b"ados-mesh-invite"`.
//!   - Invite bundle: JSON with sorted keys and hex-encoded binaries.
//!   - Revocations: sorted JSON list at `/etc/ados/mesh/revocations.json`, 0600.

pub mod client;
pub mod crypto;
pub mod env;
pub mod invite;
pub mod journal;
pub mod manager;
pub mod rekey;
pub mod relay_rekey;
pub mod revocations;
pub mod server;
pub mod window;

pub use crypto::{
    decrypt_invite, encrypt_invite, generate_keypair, open_from_peer, seal_to_peer,
    seal_to_peer_with, session_key, KeyPair, SealedToPeer,
};
pub use env::PairingEnv;
pub use invite::InviteBundle;
pub use manager::MeshPairingManager;
pub use server::{serve, PairingService};

/// The UDP port the receiver binds on `bat0` for relay join requests.
pub const PAIR_UDP_PORT: u16 = 5801;
/// Default accept-window duration (seconds).
pub const DEFAULT_ACCEPT_WINDOW_S: u64 = 60;
/// Invite time-to-live (seconds).
pub const INVITE_TTL_S: i64 = 120;

#[cfg(test)]
mod tests;
