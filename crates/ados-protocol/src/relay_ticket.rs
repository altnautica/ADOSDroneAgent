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

/// Mints and verifies relay tickets from a per-pair secret.
#[derive(Clone)]
pub struct RelayTicketIssuer {
    key: Vec<u8>,
}

impl RelayTicketIssuer {
    /// Derive the ticket key from the per-pair secret under the relay label.
    ///
    /// Takes bytes rather than a string: the secret is random material, not
    /// text, and hex-decoding it at the boundary keeps that explicit.
    pub fn from_secret(secret: &[u8]) -> Self {
        let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
        mac.update(RELAY_KEY_LABEL);
        Self {
            key: mac.finalize().into_bytes().to_vec(),
        }
    }

    /// Mint a ticket for `request` addressed to `target`, issued at `now`,
    /// with a fresh random nonce. `None` only when the system cannot supply
    /// randomness.
    pub fn mint(&self, target: &str, request: &RequestBinding<'_>, now: i64) -> Option<String> {
        let nonce = crate::nonce_cache::random_nonce_hex()?;
        Some(self.mint_with_nonce(target, request, now, &nonce))
    }

    /// Deterministic mint core (explicit nonce) for tests.
    pub fn mint_with_nonce(
        &self,
        target: &str,
        request: &RequestBinding<'_>,
        now: i64,
        nonce: &str,
    ) -> String {
        let expires_at = now.saturating_add(TICKET_LIFETIME_SECONDS);
        let payload = sign_payload(SCOPE_RELAY, target, now, expires_at, nonce, request);
        let signature = self.sign(&payload);
        format!("{payload}|{signature}")
    }

    /// Verify a ticket against the request it arrived with: authenticity
    /// first, then what it is bound to, then time.
    ///
    /// Order matters. Checking the binding or the clock before the HMAC would
    /// answer questions about a string nobody has shown to be ours.
    ///
    /// This does NOT spend the nonce; the caller does that with
    /// [`VerifiedRelayTicket::spend`] at the point where a second arrival
    /// would re-execute the request.
    pub fn verify(
        &self,
        token: &str,
        expected_target: &str,
        request: &RequestBinding<'_>,
        now: i64,
    ) -> Result<VerifiedRelayTicket, RelayTicketError> {
        let parsed = ParsedTicket::parse(token)?;
        let sig = hex::decode(parsed.signature).map_err(|_| RelayTicketError::BadSignature)?;
        let mut mac = HmacSha256::new_from_slice(&self.key).expect("HMAC accepts any key length");
        mac.update(parsed.payload.as_bytes());
        // Constant-time.
        mac.verify_slice(&sig)
            .map_err(|_| RelayTicketError::BadSignature)?;

        if parsed.scope != SCOPE_RELAY
            || parsed.target != expected_target
            || parsed.method != request.method
            || parsed.path != request.path
            || parsed.body_sha256 != body_digest(request.body)
        {
            return Err(RelayTicketError::BindingMismatch);
        }
        let lifetime = parsed.expires_at.saturating_sub(parsed.issued_at);
        if !(1..=TICKET_LIFETIME_SECONDS).contains(&lifetime) {
            return Err(RelayTicketError::Malformed);
        }
        if parsed.issued_at > now.saturating_add(CLOCK_SKEW_TOLERANCE_SECONDS) {
            return Err(RelayTicketError::ClockSkew);
        }
        let valid_until = parsed
            .expires_at
            .saturating_add(CLOCK_SKEW_TOLERANCE_SECONDS);
        if now > valid_until {
            return Err(RelayTicketError::Expired);
        }
        Ok(VerifiedRelayTicket {
            nonce: parsed.nonce.to_owned(),
            valid_until,
        })
    }

    fn sign(&self, payload: &str) -> String {
        let mut mac = HmacSha256::new_from_slice(&self.key).expect("HMAC accepts any key length");
        mac.update(payload.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }
}

/// The fields of a token, borrowed from it.
struct ParsedTicket<'a> {
    payload: &'a str,
    signature: &'a str,
    scope: &'a str,
    target: &'a str,
    issued_at: i64,
    expires_at: i64,
    nonce: &'a str,
    method: &'a str,
    path: &'a str,
    body_sha256: &'a str,
}

impl<'a> ParsedTicket<'a> {
    /// Split a token. The path is the one field that may itself contain `|`
    /// (an unencoded query string can), so the fixed fields are taken from
    /// both ends and the path is whatever lies between.
    fn parse(token: &'a str) -> Result<Self, RelayTicketError> {
        let (payload, signature) = token.rsplit_once('|').ok_or(RelayTicketError::Malformed)?;
        let (head, body_sha256) = payload
            .rsplit_once('|')
            .ok_or(RelayTicketError::Malformed)?;
        let mut fields = head.splitn(8, '|');
        let mut next = || fields.next().ok_or(RelayTicketError::Malformed);
        let version = next()?;
        let scope = next()?;
        let target = next()?;
        let issued_at = next()?;
        let expires_at = next()?;
        let nonce = next()?;
        let method = next()?;
        let path = next()?;
        if version != "v2" || nonce.len() != 32 || !nonce.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(RelayTicketError::Malformed);
        }
        Ok(Self {
            payload,
            signature,
            scope,
            target,
            issued_at: issued_at.parse().map_err(|_| RelayTicketError::Malformed)?,
            expires_at: expires_at
                .parse()
                .map_err(|_| RelayTicketError::Malformed)?,
            nonce,
            method,
            path,
            body_sha256,
        })
    }
}

/// Lowercase hex SHA-256 of a request body.
fn body_digest(body: &[u8]) -> String {
    hex::encode(Sha256::digest(body))
}

/// The signed substring.
fn sign_payload(
    scope: &str,
    target: &str,
    issued_at: i64,
    expires_at: i64,
    nonce: &str,
    request: &RequestBinding<'_>,
) -> String {
    format!(
        "v2|{scope}|{target}|{issued_at}|{expires_at}|{nonce}|{}|{}|{}",
        request.method,
        request.path,
        body_digest(request.body)
    )
}

/// Why a secret may or may not be accepted from the relay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcceptDecision {
    /// No secret held: take this one. Trust-on-first-use.
    Accept,
    /// Already holding this exact secret. A no-op, not a conflict — the ground
    /// station restates it on every reconcile tick.
    AlreadyHeld,
    /// Holding a DIFFERENT secret. Refused.
    RefusedWouldOverwrite,
    /// The offered value is not a usable secret.
    RefusedMalformed,
}

/// Decide whether to accept a relay secret offered over the relay itself.
///
/// **This is the security decision in the whole scheme, so it is a pure
/// function with its own tests rather than a branch inside an HTTP handler.**
///
/// The delivery necessarily rides the very channel the credential will later
/// protect, which today is unauthenticated. If the drone simply took whatever
/// it was handed, anyone within radio range could overwrite the secret with
/// their own and then mint tickets the drone accepts — leaving a credential
/// that looks like protection on every status surface while granting exactly
/// the access it was built to deny. That is strictly worse than having none.
///
/// So: **first write wins.** An unset drone takes the first secret it is
/// offered; a drone already holding one refuses to replace it over the relay.
/// The exposure is the pairing moment rather than every request forever, which
/// is the honest statement of what trust-on-first-use buys.
///
/// Replacing a secret deliberately — a re-pair to a different ground station —
/// goes through unpair, which clears it locally, rather than through a write
/// that a stranger could also make.
pub fn decide_accept(held: Option<&str>, offered: &str) -> AcceptDecision {
    if offered.len() != RELAY_SECRET_LEN * 2 || !offered.chars().all(|c| c.is_ascii_hexdigit()) {
        return AcceptDecision::RefusedMalformed;
    }
    match held {
        None => AcceptDecision::Accept,
        Some(existing) if existing.eq_ignore_ascii_case(offered) => AcceptDecision::AlreadyHeld,
        Some(_) => AcceptDecision::RefusedWouldOverwrite,
    }
}

/// The secret this drone currently holds, or `None` when it holds none.
///
/// A missing file and an unreadable one are both "none", and "none" denies:
/// the drone has no credential to verify against, so the only relayed call it
/// will serve is the one that delivers the credential. An unreadable secret
/// therefore closes the relay lane rather than opening it — the opposite of a
/// permissions slip silently restoring radio-range full authority.
pub fn load_secret_at(path: &std::path::Path) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    let trimmed = raw.trim().to_string();
    (!trimmed.is_empty()).then_some(trimmed)
}

/// Store a relay secret at `path` with owner-only permissions.
///
/// The mode is set explicitly AFTER writing as well as at open time, because
/// the open-time mode applies only on creation: an existing file left
/// group-readable by an earlier build would otherwise keep that mode and leave
/// the material readable to anything on the box. Mirrors the plugin token
/// secret's write, which is owner-only for the same reason.
pub fn store_secret_at(path: &std::path::Path, secret: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = std::fs::metadata(parent) {
                let mut perms = meta.permissions();
                perms.set_mode(0o700);
                let _ = std::fs::set_permissions(parent, perms);
            }
        }
    }
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(secret.as_bytes())?;
        f.flush()?;
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, secret.as_bytes())
    }
}

/// Apply an offered secret under the [`decide_accept`] rule, storing it only on
/// [`AcceptDecision::Accept`].
///
/// The decision and the write are joined here so no caller can reach the write
/// without going through the rule -- the whole scheme rests on first-write-wins,
/// and a handler that stored first and asked afterwards would defeat it.
pub fn apply_offered_secret(
    path: &std::path::Path,
    offered: &str,
) -> Result<AcceptDecision, std::io::Error> {
    let decision = decide_accept(load_secret_at(path).as_deref(), offered);
    if decision == AcceptDecision::Accept {
        store_secret_at(path, offered)?;
    }
    Ok(decision)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &[u8] = b"0123456789abcdef0123456789abcdef";
    const DRONE: &str = "40bb1a5a";

    fn issuer() -> RelayTicketIssuer {
        RelayTicketIssuer::from_secret(SECRET)
    }

    const HEX32: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn an_unset_drone_takes_the_first_secret_it_is_offered() {
        // Trust-on-first-use: at pair time there is nothing better available on
        // a headless aircraft.
        assert_eq!(decide_accept(None, HEX32), AcceptDecision::Accept);
    }

    #[test]
    fn a_drone_already_holding_a_secret_refuses_to_be_re_keyed_over_the_air() {
        // The delivery rides the channel the credential protects, and that
        // channel is unauthenticated. Without this, anyone in radio range
        // overwrites the secret and then mints tickets the drone accepts — a
        // credential that reads as protection while granting the access it
        // exists to deny. Deliberate replacement goes through unpair.
        let other = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
        assert_eq!(
            decide_accept(Some(HEX32), other),
            AcceptDecision::RefusedWouldOverwrite
        );
    }

    #[test]
    fn restating_the_same_secret_is_a_no_op_not_a_conflict() {
        // The ground station restates it on every reconcile tick, so the
        // steady state must not look like an attack.
        assert_eq!(
            decide_accept(Some(HEX32), HEX32),
            AcceptDecision::AlreadyHeld
        );
        // Case differences in hex are the same secret.
        assert_eq!(
            decide_accept(Some(&HEX32.to_uppercase()), HEX32),
            AcceptDecision::AlreadyHeld
        );
    }

    #[test]
    fn a_malformed_offer_is_refused_before_anything_is_stored() {
        // Storing a short or non-hex value would leave the drone holding
        // something credential-shaped that authenticates nothing, and would
        // then block the real secret via first-write-wins.
        for bad in ["", "abc", "zzzz", &"a".repeat(63), &"a".repeat(65)] {
            assert_eq!(
                decide_accept(None, bad),
                AcceptDecision::RefusedMalformed,
                "{bad:?}"
            );
        }
    }

    #[test]
    fn a_malformed_offer_cannot_displace_a_held_secret() {
        assert_eq!(
            decide_accept(Some(HEX32), "nope"),
            AcceptDecision::RefusedMalformed
        );
    }

    const NONCE: &str = "00112233445566778899aabbccddeeff";

    fn status_get() -> RequestBinding<'static> {
        RequestBinding::new("GET", "/api/status", b"")
    }

    fn mint(now: i64) -> String {
        issuer().mint_with_nonce(DRONE, &status_get(), now, NONCE)
    }

    #[test]
    fn a_freshly_minted_ticket_verifies_for_its_target_and_request() {
        let t = mint(1_000);
        let v = issuer().verify(&t, DRONE, &status_get(), 1_005).unwrap();
        assert_eq!(v.nonce, NONCE);
        assert_eq!(
            v.valid_until,
            1_000 + TICKET_LIFETIME_SECONDS + CLOCK_SKEW_TOLERANCE_SECONDS
        );
    }

    #[test]
    fn a_ticket_presented_with_another_request_is_a_binding_mismatch() {
        // A ticket lifted off the broadcast uplink must not authorize any
        // request but the one it was minted for.
        let t = mint(1_000);
        for other in [
            RequestBinding::new("POST", "/api/command", b"{}"),
            RequestBinding::new("POST", "/api/status", b""),
            RequestBinding::new("GET", "/api/status?x=1", b""),
            RequestBinding::new("GET", "/api/status", b"x"),
        ] {
            assert_eq!(
                issuer().verify(&t, DRONE, &other, 1_005),
                Err(RelayTicketError::BindingMismatch),
                "{other:?}"
            );
        }
        assert_eq!(
            RelayTicketError::BindingMismatch.reason(),
            "binding_mismatch"
        );
    }

    #[test]
    fn a_ticket_for_one_drone_does_not_verify_at_another() {
        let t = mint(1_000);
        assert_eq!(
            issuer().verify(&t, "f6aa0aa4", &status_get(), 1_005),
            Err(RelayTicketError::BindingMismatch)
        );
    }

    #[test]
    fn a_different_pair_secret_does_not_verify() {
        // A second ground station holding the shared fleet radio key still
        // cannot mint a ticket this drone accepts.
        let t = mint(1_000);
        let other = RelayTicketIssuer::from_secret(b"ffffffffffffffffffffffffffffffff");
        assert_eq!(
            other.verify(&t, DRONE, &status_get(), 1_005),
            Err(RelayTicketError::BadSignature)
        );
    }

    #[test]
    fn a_ws_ticket_key_derivation_does_not_verify_a_relay_ticket() {
        // Domain separation: only the label keeps one credential from being
        // replayed as the other.
        let t = mint(1_000);
        let mut mac = HmacSha256::new_from_slice(SECRET).unwrap();
        mac.update(crate::ws_ticket::TICKET_KEY_LABEL);
        let ws_keyed = RelayTicketIssuer {
            key: mac.finalize().into_bytes().to_vec(),
        };
        assert_eq!(
            ws_keyed.verify(&t, DRONE, &status_get(), 1_005),
            Err(RelayTicketError::BadSignature)
        );
        assert_ne!(RELAY_KEY_LABEL, crate::ws_ticket::TICKET_KEY_LABEL);
    }

    #[test]
    fn clock_skew_inside_the_tolerance_is_accepted_and_beyond_it_is_refused() {
        let now = 10_000;
        // Minter's clock 200 s ahead of ours: inside the tolerance.
        let ahead = mint(now + 200);
        assert!(issuer().verify(&ahead, DRONE, &status_get(), now).is_ok());
        // 400 s ahead: refused, and said why.
        let far_ahead = mint(now + 400);
        assert_eq!(
            issuer().verify(&far_ahead, DRONE, &status_get(), now),
            Err(RelayTicketError::ClockSkew)
        );
        // Minter's clock behind ours: accepted until the expiry is more than
        // the tolerance in the past.
        let behind = mint(now - 300);
        assert!(issuer().verify(&behind, DRONE, &status_get(), now).is_ok());
        let stale = mint(now - 400);
        assert_eq!(
            issuer().verify(&stale, DRONE, &status_get(), now),
            Err(RelayTicketError::Expired)
        );
    }

    #[test]
    fn a_ticket_is_spent_once() {
        let cache = new_replay_cache();
        let t = mint(1_000);
        let v = issuer().verify(&t, DRONE, &status_get(), 1_001).unwrap();
        assert_eq!(v.spend(&cache, 1_001), Ok(()));
        let again = issuer().verify(&t, DRONE, &status_get(), 1_002).unwrap();
        assert_eq!(again.spend(&cache, 1_002), Err(RelayTicketError::Replayed));
        assert_eq!(RelayTicketError::Replayed.reason(), "replayed");
    }

    #[test]
    fn every_mint_carries_a_fresh_nonce() {
        let a = issuer().mint(DRONE, &status_get(), 1_000).unwrap();
        let b = issuer().mint(DRONE, &status_get(), 1_000).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn a_tampered_field_is_refused_before_anything_else_is_read() {
        let t = mint(1_000);
        // Push the expiry out. Without an HMAC over the exact substring this
        // would simply extend the ticket's life.
        let forged = t.replacen("|1030|", "|99999999999|", 1);
        assert_ne!(forged, t);
        assert_eq!(
            issuer().verify(&forged, DRONE, &status_get(), 1_005),
            Err(RelayTicketError::BadSignature)
        );
    }

    #[test]
    fn a_path_containing_a_pipe_still_binds_exactly() {
        let req = RequestBinding::new("GET", "/api/logs?q=a|b", b"");
        let t = issuer().mint_with_nonce(DRONE, &req, 1_000, NONCE);
        assert!(issuer().verify(&t, DRONE, &req, 1_001).is_ok());
        let other = RequestBinding::new("GET", "/api/logs?q=a", b"");
        assert_eq!(
            issuer().verify(&t, DRONE, &other, 1_001),
            Err(RelayTicketError::BindingMismatch)
        );
    }

    #[test]
    fn a_malformed_token_is_refused_rather_than_panicking() {
        for bad in [
            "",
            "v2",
            "v1|relay.http|d|1|2|ff",
            "not-a-ticket",
            "v2|relay.http|d|1|2|short|GET|/x|ab|ff",
            "v2|relay.http|d|x|2|00112233445566778899aabbccddeeff|GET|/x|ab|ff",
        ] {
            assert!(
                issuer().verify(bad, DRONE, &status_get(), 1_000).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn a_refusal_body_names_the_code_and_reason() {
        let body: serde_json::Value = serde_json::from_slice(&refusal_body("clock_skew")).unwrap();
        assert_eq!(body["error"], RELAY_REFUSAL_CODE);
        assert_eq!(body["reason"], "clock_skew");
    }

    #[test]
    fn the_tunnel_binding_ignores_the_ticket_and_key_order() {
        let a: serde_json::Value =
            serde_json::from_str(r#"{"op":"put","key":"video.bitrate","value":4}"#).unwrap();
        let b: serde_json::Value = serde_json::from_str(
            r#"{"value":4,"ticket":"anything","key":"video.bitrate","op":"put"}"#,
        )
        .unwrap();
        assert_eq!(tunnel_binding_body(&a), tunnel_binding_body(&b));
        let c: serde_json::Value =
            serde_json::from_str(r#"{"op":"put","key":"video.bitrate","value":5}"#).unwrap();
        assert_ne!(tunnel_binding_body(&a), tunnel_binding_body(&c));
    }

    // Checked at compile time: a shorter secret would weaken every ticket,
    // and a runtime assertion on a constant is not a test.
    const _: () = assert!(RELAY_SECRET_LEN >= 32);

    #[test]
    fn applying_an_offer_to_an_unset_drone_stores_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets/relay-peer-secret");

        assert_eq!(
            apply_offered_secret(&path, HEX32).unwrap(),
            AcceptDecision::Accept
        );
        assert_eq!(load_secret_at(&path).as_deref(), Some(HEX32));
    }

    #[test]
    fn applying_a_restated_secret_leaves_the_stored_value_alone() {
        // The ground station restates on every reconcile tick, so the ordinary
        // steady state must not read as a refusal.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets/relay-peer-secret");
        apply_offered_secret(&path, HEX32).unwrap();

        assert_eq!(
            apply_offered_secret(&path, HEX32).unwrap(),
            AcceptDecision::AlreadyHeld
        );
    }

    #[test]
    fn applying_a_stranger_offer_leaves_the_stored_secret_intact() {
        // The attack the first-write-wins rule exists to stop: anyone in radio
        // range replacing the credential with their own would leave something
        // that reads as protection while granting exactly what it denies.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets/relay-peer-secret");
        apply_offered_secret(&path, HEX32).unwrap();

        let attacker = "f".repeat(64);
        assert_eq!(
            apply_offered_secret(&path, &attacker).unwrap(),
            AcceptDecision::RefusedWouldOverwrite
        );
        assert_eq!(
            load_secret_at(&path).as_deref(),
            Some(HEX32),
            "the held secret is untouched"
        );
    }

    #[test]
    fn applying_a_malformed_offer_reaches_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets/relay-peer-secret");

        assert_eq!(
            apply_offered_secret(&path, "nonsense").unwrap(),
            AcceptDecision::RefusedMalformed
        );
        assert!(load_secret_at(&path).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn a_stored_secret_is_owner_only_even_over_a_looser_existing_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("relay-peer-secret");
        // An earlier build could have left this world-readable; the open-time
        // mode alone would not repair it.
        std::fs::write(&path, "old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        store_secret_at(&path, HEX32).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "got {mode:o}");
    }

    #[test]
    fn an_absent_secret_reads_as_none_rather_than_an_error() {
        // Which is what keeps the drone inert until one is delivered.
        let dir = tempfile::tempdir().unwrap();
        assert!(load_secret_at(&dir.path().join("nope")).is_none());
    }
}
