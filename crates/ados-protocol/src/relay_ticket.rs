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
//! Deliberately the same self-contained HMAC as [`crate::ws_ticket`]: no store,
//! no lookup, no IPC between the minting process and the verifying one. They
//! share only the secret. Domain-separated by its own label, so a ticket minted
//! for one purpose can never be replayed as the other even though both derive
//! from HMAC-SHA256.

use hmac::{Hmac, Mac};
use sha2::Sha256;
use thiserror::Error;

type HmacSha256 = Hmac<Sha256>;

/// Domain-separation label mixed into the per-pair secret.
///
/// Distinct from the WS ticket's label on purpose: the two credentials protect
/// different surfaces, and a token minted for one must not verify against the
/// other even if a caller obtains it.
pub const RELAY_KEY_LABEL: &[u8] = b"ados-relay-ticket-v1";

/// Where the drone keeps the secret its ground station gave it. 0600, beside
/// the plugin token secret, which is the established home for material like
/// this.
pub const RELAY_SECRET_PATH: &str = "/etc/ados/secrets/relay-peer-secret";

/// Secret length in bytes.
pub const RELAY_SECRET_LEN: usize = 32;

/// Default ticket lifetime.
///
/// Short because a relayed request is a round trip over a radio, not a session:
/// the ticket only has to outlive the call it accompanies. Long enough to
/// tolerate the retransmit schedule, which can carry a request for several
/// seconds on a lossy lane.
pub const DEFAULT_TTL_SECONDS: i64 = 30;

/// Hard cap on a requested lifetime, so a caller cannot mint a long-lived
/// bearer by asking for one.
pub const MAX_TTL_SECONDS: i64 = 120;

/// The scope a relayed HTTP call carries.
pub const SCOPE_RELAY: &str = "relay.http";

#[derive(Debug, Error, PartialEq, Eq)]
pub enum RelayTicketError {
    #[error("malformed relay ticket")]
    Malformed,
    #[error("relay ticket timestamp is not an integer")]
    BadTimestamp,
    #[error("relay ticket signature is not valid hex")]
    BadSignature,
    #[error("relay ticket HMAC mismatch")]
    HmacMismatch,
    #[error("relay ticket scope mismatch")]
    ScopeMismatch,
    #[error("relay ticket expired")]
    Expired,
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

    /// Mint a ticket naming the drone it is for, valid for `ttl_seconds`.
    ///
    /// The target device id is signed, not merely carried, so a ticket minted
    /// for one drone cannot be lifted and replayed against another — which
    /// matters precisely because the uplink is a broadcast every drone hears.
    pub fn mint_at(&self, target: &str, ttl_seconds: i64, now: i64) -> String {
        let ttl = ttl_seconds.clamp(1, MAX_TTL_SECONDS);
        let expires_at = now.saturating_add(ttl);
        let payload = sign_payload(SCOPE_RELAY, target, now, expires_at);
        let signature = self.sign(&payload);
        format!("{payload}|{signature}")
    }

    /// Verify a ticket: authenticity first, then who it names, then expiry.
    ///
    /// Order matters. Checking the target or the clock before the HMAC would
    /// answer questions about a string nobody has shown to be ours.
    pub fn verify(
        &self,
        token: &str,
        expected_target: &str,
        now: i64,
    ) -> Result<(), RelayTicketError> {
        let parts: Vec<&str> = token.split('|').collect();
        if parts.len() != 6 || parts[0] != "v1" {
            return Err(RelayTicketError::Malformed);
        }
        let scope = parts[1];
        let target = parts[2];
        let _issued: i64 = parts[3]
            .parse()
            .map_err(|_| RelayTicketError::BadTimestamp)?;
        let expires_at: i64 = parts[4]
            .parse()
            .map_err(|_| RelayTicketError::BadTimestamp)?;
        let sig = hex::decode(parts[5]).map_err(|_| RelayTicketError::BadSignature)?;

        // Recompute over the exact signed substring, so reformatting can never
        // drift from what was signed.
        let payload = parts[..5].join("|");
        let mut mac = HmacSha256::new_from_slice(&self.key).expect("HMAC accepts any key length");
        mac.update(payload.as_bytes());
        // Constant-time.
        mac.verify_slice(&sig)
            .map_err(|_| RelayTicketError::HmacMismatch)?;

        if scope != SCOPE_RELAY {
            return Err(RelayTicketError::ScopeMismatch);
        }
        if target != expected_target {
            return Err(RelayTicketError::ScopeMismatch);
        }
        if now >= expires_at {
            return Err(RelayTicketError::Expired);
        }
        Ok(())
    }

    fn sign(&self, payload: &str) -> String {
        let mut mac = HmacSha256::new_from_slice(&self.key).expect("HMAC accepts any key length");
        mac.update(payload.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }
}

/// The signed substring: `v1|<scope>|<target>|<issued_at>|<expires_at>`.
fn sign_payload(scope: &str, target: &str, issued_at: i64, expires_at: i64) -> String {
    format!("v1|{scope}|{target}|{issued_at}|{expires_at}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &[u8] = b"0123456789abcdef0123456789abcdef";
    const DRONE: &str = "40bb1a5a";

    fn issuer() -> RelayTicketIssuer {
        RelayTicketIssuer::from_secret(SECRET)
    }

    #[test]
    fn a_freshly_minted_ticket_verifies_for_its_target() {
        let t = issuer().mint_at(DRONE, 30, 1_000);
        assert_eq!(issuer().verify(&t, DRONE, 1_005), Ok(()));
    }

    #[test]
    fn a_ticket_for_one_drone_does_not_verify_at_another() {
        // The uplink is a broadcast every drone hears, so a ticket that named
        // its target only in an unsigned field could be lifted off the air and
        // replayed against a different aircraft.
        let t = issuer().mint_at(DRONE, 30, 1_000);
        assert_eq!(
            issuer().verify(&t, "f6aa0aa4", 1_005),
            Err(RelayTicketError::ScopeMismatch)
        );
    }

    #[test]
    fn a_different_pair_secret_does_not_verify() {
        // The whole point: a second ground station holding the shared fleet
        // radio key still cannot mint a ticket this drone accepts.
        let t = issuer().mint_at(DRONE, 30, 1_000);
        let other = RelayTicketIssuer::from_secret(b"ffffffffffffffffffffffffffffffff");
        assert_eq!(
            other.verify(&t, DRONE, 1_005),
            Err(RelayTicketError::HmacMismatch)
        );
    }

    #[test]
    fn a_ws_ticket_key_derivation_does_not_verify_a_relay_ticket() {
        // Domain separation. Both credentials are HMAC-SHA256 over the same
        // shape; only the label keeps one from being replayed as the other.
        let relay = RelayTicketIssuer::from_secret(SECRET);
        let t = relay.mint_at(DRONE, 30, 1_000);

        // Derive a key the WS way from the same material and check it rejects.
        let mut mac = HmacSha256::new_from_slice(SECRET).unwrap();
        mac.update(crate::ws_ticket::TICKET_KEY_LABEL);
        let ws_keyed = RelayTicketIssuer {
            key: mac.finalize().into_bytes().to_vec(),
        };
        assert_eq!(
            ws_keyed.verify(&t, DRONE, 1_005),
            Err(RelayTicketError::HmacMismatch)
        );
        assert_ne!(RELAY_KEY_LABEL, crate::ws_ticket::TICKET_KEY_LABEL);
    }

    #[test]
    fn an_expired_ticket_is_refused() {
        let t = issuer().mint_at(DRONE, 30, 1_000);
        assert_eq!(
            issuer().verify(&t, DRONE, 1_030),
            Err(RelayTicketError::Expired)
        );
    }

    #[test]
    fn a_tampered_field_is_refused_before_anything_else_is_read() {
        let t = issuer().mint_at(DRONE, 30, 1_000);
        // Push the expiry far out. Without an HMAC over the exact substring
        // this would simply extend the ticket's life.
        let parts: Vec<&str> = t.split('|').collect();
        let forged = format!(
            "{}|{}|{}|{}|{}|{}",
            parts[0], parts[1], parts[2], parts[3], "99999999999", parts[5]
        );
        assert_eq!(
            issuer().verify(&forged, DRONE, 1_005),
            Err(RelayTicketError::HmacMismatch)
        );
    }

    #[test]
    fn a_malformed_token_is_refused_rather_than_panicking() {
        for bad in [
            "",
            "v1",
            "v2|relay.http|d|1|2|ff",
            "not-a-ticket",
            "v1|a|b|c|d|e",
        ] {
            assert!(issuer().verify(bad, DRONE, 1_000).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_requested_lifetime_cannot_exceed_the_cap() {
        // Otherwise a caller mints a long-lived bearer just by asking.
        let t = issuer().mint_at(DRONE, 86_400, 1_000);
        let expires: i64 = t.split('|').nth(4).unwrap().parse().unwrap();
        assert_eq!(expires, 1_000 + MAX_TTL_SECONDS);
    }

    // Checked at compile time: a shorter secret would weaken every ticket,
    // and a runtime assertion on a constant is not a test.
    const _: () = assert!(RELAY_SECRET_LEN >= 32);
}
