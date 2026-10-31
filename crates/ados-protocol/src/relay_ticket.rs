//! A per-pair credential for relayed requests.
//!
//! ## What this closes
//!
//! A request that crosses the radio relay arrives on the drone's loopback and
//! is therefore treated as on-box — the highest trust level the agent has.
//! `ados_control::serve` says so in its own words: a fleet shares one radio key
//! and distributes no per-node credential, so the relay has nothing to present.
//! Radio range consequently carries a node's full authority, bounded only by
//! the `relay_forbidden` path denylist.
//!
//! This gives the relay something to present.
//!
//! ## Why the fleet key cannot be the key
//!
//! The obvious shortcut — derive the ticket key from the shared 64-byte fleet
//! keypair both ends already hold — is theatre. Every member of the fleet holds
//! that key; it is what *makes* them a member. A token derived from it proves
//! only that the caller is on the radio, which is exactly what the caller
//! already demonstrated by being on the radio. It would authenticate nothing
//! and would read, on a status page, as though it did.
//!
//! So the key is a secret generated **per pairing** by the ground station and
//! delivered to that one drone. A second ground station holding the fleet radio
//! key cannot mint a ticket the drone accepts, because it does not hold the
//! per-pair secret.
//!
//! ## Bootstrap, stated plainly
//!
//! The secret is delivered over the same relay it will later protect, at pair
//! time. That is trust-on-first-use: an attacker already positioned on the
//! radio at the moment of pairing can observe the delivery. It is a real
//! limitation and not a hidden one — the alternative is an out-of-band channel
//! that does not exist on a headless aircraft, and the window is one exchange
//! at pair time rather than every request forever, which is the situation
//! today.
//!
//! ## Shape
//!
//! An HMAC over the request it accompanies, keyed from the per-pair secret and
//! domain-separated by its own label so a ticket minted for one purpose can
//! never verify as a WebSocket ticket even though both derive from
//! HMAC-SHA256. The signed payload is
//!
//! ```text
//! v2|{scope}|{target}|{issued_at}|{expires_at}|{nonce}|{METHOD}|{path_with_query}|{sha256_hex(body)}
//! ```
//!
//! and the token on the wire is that payload followed by `|{hmac_hex}`.
//!
//! - **Bound to the request.** Every fleet member hears every ticket, because
//!   the uplink is a broadcast. A ticket that named only its target could be
//!   lifted off the air and replayed with any method, path and body against
//!   the same aircraft; binding all three makes a captured ticket good for the
//!   one request it was minted for and nothing else.
//! - **Single use.** The 16-byte random nonce is remembered by the verifier
//!   (see [`crate::nonce_cache`]) for at least [`REPLAY_RETENTION_SECONDS`],
//!   so the same request cannot be played twice either.
//! - **Tolerant of an unsynchronised clock.** A radio-only aircraft has no
//!   time source but its own RTC, which can be minutes off after a reboot. The
//!   lifetime is checked against the ticket's own `issued_at` (never longer
//!   than [`TICKET_LIFETIME_SECONDS`]), and the verifier's clock may disagree
//!   with the minter's by up to [`CLOCK_SKEW_TOLERANCE_SECONDS`] either way.
//!   The nonce cache, not the clock, is what stops a replay inside that
//!   window.

use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::nonce_cache::NonceCache;

type HmacSha256 = Hmac<Sha256>;

/// Mints and verifies relay tickets from a per-pair secret.
#[derive(Clone)]
pub struct RelayTicketIssuer {
    key: Vec<u8>,
}

mod issuer;
mod secret_store;
pub use secret_store::*;

/// Domain-separation label mixed into the per-pair secret.
///
/// Distinct from the WS ticket's label on purpose: the two credentials protect
/// different surfaces, and a token minted for one must not verify against the
/// other even if a caller obtains it.
pub const RELAY_KEY_LABEL: &[u8] = b"ados-relay-ticket-v2";

/// Where the drone keeps the secret its ground station gave it. 0600, beside
/// the plugin token secret, which is the established home for material like
/// this.
pub const RELAY_SECRET_PATH: &str = "/etc/ados/secrets/relay-peer-secret";

/// Secret length in bytes.
pub const RELAY_SECRET_LEN: usize = 32;

/// Ticket lifetime, `expires_at - issued_at`. A verifier refuses a ticket that
/// claims a longer one.
///
/// Short because a relayed request is a round trip over a radio, not a session:
/// the ticket only has to outlive the call it accompanies, including the
/// retransmit schedule on a lossy lane.
pub const TICKET_LIFETIME_SECONDS: i64 = 30;

/// How far the verifier's wall clock may disagree with the minter's, in either
/// direction, before a ticket is refused as `clock_skew` (issued too far in
/// the future) or `expired`.
pub const CLOCK_SKEW_TOLERANCE_SECONDS: i64 = 300;

/// How long a spent nonce is remembered, at minimum.
pub const REPLAY_RETENTION_SECONDS: i64 = 600;

/// How many spent nonces a verifier remembers before evicting the oldest.
pub const REPLAY_CACHE_CAPACITY: usize = 4096;

/// The scope a relayed HTTP call carries.
pub const SCOPE_RELAY: &str = "relay.http";

/// The `error` code every relay-ticket refusal answers with.
pub const RELAY_REFUSAL_CODE: &str = "E_RELAY_TICKET";

/// The method a config-tunnel ticket is bound to. Not an HTTP verb, so a
/// tunnel ticket can never authorize a relayed HTTP request or the reverse.
pub const TUNNEL_BINDING_METHOD: &str = "TUNNEL";

/// The path a config-tunnel ticket is bound to: the one surface the tunnel
/// reaches.
pub const TUNNEL_BINDING_PATH: &str = "/api/config";

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum RelayTicketError {
    /// This node holds no per-pair secret, so it cannot verify any ticket and
    /// has nothing to distinguish its own ground station from anything else
    /// holding the shared fleet radio key. Refusing is the only safe answer:
    /// admitting would hand radio range this node's full on-box authority.
    #[error("no relay peer secret on file")]
    NoSecret,
    /// Not a ticket of this version's shape.
    #[error("malformed relay ticket")]
    Malformed,
    /// The HMAC does not verify under this node's secret.
    #[error("relay ticket signature does not verify")]
    BadSignature,
    /// Authentic, but minted for a different scope, target, method, path or
    /// body than the request it arrived with.
    #[error("relay ticket is bound to a different request")]
    BindingMismatch,
    /// Its expiry is further in the past than the clock tolerance allows.
    #[error("relay ticket expired")]
    Expired,
    /// Issued further in the future than the clock tolerance allows.
    #[error("relay ticket issued in the future beyond the clock tolerance")]
    ClockSkew,
    /// Its nonce has already been spent.
    #[error("relay ticket already used")]
    Replayed,
}

impl RelayTicketError {
    /// The `reason` string a refusal body carries.
    pub fn reason(self) -> &'static str {
        match self {
            Self::NoSecret => "no_secret",
            Self::Malformed | Self::BadSignature => "bad_signature",
            Self::BindingMismatch => "binding_mismatch",
            Self::Expired => "expired",
            Self::ClockSkew => "clock_skew",
            Self::Replayed => "replayed",
        }
    }
}

/// The reason a ground station refuses to relay at all because the drone holds
/// a secret issued by a different ground station.
pub const SECRET_CONFLICT_REASON: &str = "secret_conflict";

/// The JSON body of a relay-ticket refusal:
/// `{"error":"E_RELAY_TICKET","reason":"<reason>"}`. Answered with HTTP 401.
pub fn refusal_body(reason: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "error": RELAY_REFUSAL_CODE,
        "reason": reason,
    }))
    .unwrap_or_default()
}

/// The request a ticket is bound to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestBinding<'a> {
    /// Upper-case HTTP method, or [`TUNNEL_BINDING_METHOD`].
    pub method: &'a str,
    /// The path exactly as the verifier will see it, including any query
    /// string and still percent-encoded.
    pub path: &'a str,
    /// The request body bytes.
    pub body: &'a [u8],
}

impl<'a> RequestBinding<'a> {
    pub fn new(method: &'a str, path: &'a str, body: &'a [u8]) -> Self {
        Self { method, path, body }
    }
}

/// The body a config-tunnel request's ticket binds: the op, key and value of
/// the request object, ignoring the `ticket` field itself.
///
/// Built from the parsed JSON rather than the raw bytes so the minting side
/// (which inserts the ticket into the object) and the verifying side (which
/// receives it there) agree without either depending on key order.
pub fn tunnel_binding_body(request: &serde_json::Value) -> Vec<u8> {
    let field = |name: &str| -> String {
        match request.get(name) {
            None | Some(serde_json::Value::Null) => String::new(),
            Some(v) => v.to_string(),
        }
    };
    format!("{}\n{}\n{}", field("op"), field("key"), field("value")).into_bytes()
}

/// What a verified ticket tells its verifier: the nonce to record as spent and
/// the last second at which the ticket could still have verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedRelayTicket {
    pub nonce: String,
    pub valid_until: i64,
}

impl VerifiedRelayTicket {
    /// Spend the nonce in `cache`. A second use of the same ticket is
    /// [`RelayTicketError::Replayed`].
    pub fn spend(&self, cache: &NonceCache, now: i64) -> Result<(), RelayTicketError> {
        if cache.admit(&self.nonce, now, self.valid_until) {
            Ok(())
        } else {
            Err(RelayTicketError::Replayed)
        }
    }
}

/// A replay cache sized for relay tickets.
pub const fn new_replay_cache() -> NonceCache {
    NonceCache::new(REPLAY_CACHE_CAPACITY, REPLAY_RETENTION_SECONDS)
}

#[cfg(test)]
mod tests;
