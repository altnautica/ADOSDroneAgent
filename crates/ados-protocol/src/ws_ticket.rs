//! Self-contained WebSocket auth tickets.
//!
//! A browser cannot set an `X-ADOS-Key` header on a WebSocket handshake, so the
//! GCS first exchanges its pairing key (on a key-authenticated REST call) for a
//! short-lived ticket and hands it to `new WebSocket(url, ["ados-ws-ticket",
//! <ticket>])`. Unlike the older random-string-in-a-store design, this ticket is
//! **self-contained and HMAC-signed**: the minting surface and the validating
//! surface share no state, only the key, which both derive from the agent's
//! `pairing.json`. That lets the native `ados-control` front mint a ticket that
//! the native `ados-mavlink-router` (a separate process) validates with no IPC
//! and no shared ticket store.
//!
//! Wire form (pipe-delimited, like the plugin capability token):
//! `v1|<scope>|<issued_at>|<expires_at>|<sig_hex>`. The signature is
//! `HMAC-SHA256(K, "v1|<scope>|<issued_at>|<expires_at>")` where the key
//! `K = HMAC-SHA256(api_key, "ados-ws-ticket-v1")` is derived from the pairing
//! key under a fixed domain-separation label (so the ticket key is never the
//! pairing key itself, and is namespaced to this use). The Python verifier in
//! `ados.core.ws_ticket` mirrors this byte-for-byte.

use std::time::{SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use sha2::Sha256;
use thiserror::Error;

type HmacSha256 = Hmac<Sha256>;

/// Domain-separation label mixed into the pairing key to derive the ticket key.
/// MUST match `ados.core.ws_ticket._LABEL` exactly.
pub const TICKET_KEY_LABEL: &[u8] = b"ados-ws-ticket-v1";

/// Default ticket lifetime in seconds (matches the prior store's default).
pub const DEFAULT_TTL_SECONDS: i64 = 30;

/// Hard cap on a requested ticket lifetime.
pub const MAX_TTL_SECONDS: i64 = 120;

/// The MAVLink WebSocket proxy scope: a ticket minted for this scope authorizes
/// the raw `:8765` MAVLink WS for an off-box paired caller.
pub const SCOPE_MAVLINK_WS: &str = "gs.mavlink_ws";

#[derive(Debug, Error, PartialEq, Eq)]
pub enum WsTicketError {
    #[error("malformed ws ticket")]
    Malformed,
    #[error("ws ticket timestamp is not an integer")]
    BadTimestamp,
    #[error("ws ticket signature is not valid hex")]
    BadSignature,
    #[error("ws ticket HMAC mismatch")]
    HmacMismatch,
    #[error("ws ticket scope mismatch")]
    ScopeMismatch,
    #[error("ws ticket expired")]
    Expired,
}

/// A minted ticket: its compact string form plus the parsed expiry, so a mint
/// surface can return `{ ticket, scope, expires_at }` without re-parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WsTicket {
    pub token: String,
    pub scope: String,
    pub expires_at: i64,
}

/// Mints and verifies self-contained WS tickets, keyed by a pairing-derived
/// secret. Build one with [`WsTicketIssuer::from_api_key`] from the same
/// `pairing.json` `api_key` both daemons read.
#[derive(Clone)]
pub struct WsTicketIssuer {
    key: Vec<u8>,
}

impl WsTicketIssuer {
    /// Derive the ticket key from the pairing `api_key` under the fixed label.
    pub fn from_api_key(api_key: &str) -> Self {
        let mut mac =
            HmacSha256::new_from_slice(api_key.as_bytes()).expect("HMAC accepts any key length");
        mac.update(TICKET_KEY_LABEL);
        let key = mac.finalize().into_bytes().to_vec();
        Self { key }
    }

    /// Mint a ticket for `scope` valid for `ttl_seconds`, using the wall clock.
    pub fn mint(&self, scope: &str, ttl_seconds: i64) -> WsTicket {
        self.mint_at(scope, ttl_seconds, now_unix())
    }

    /// Deterministic mint core (explicit clock) for tests and cross-impl vectors.
    pub fn mint_at(&self, scope: &str, ttl_seconds: i64, now: i64) -> WsTicket {
        // Saturating add so a far-future clock or a huge ttl cannot overflow
        // (which would panic under the workspace's panic=abort).
        let expires_at = now.saturating_add(ttl_seconds);
        let payload = sign_payload(scope, now, expires_at);
        let signature = self.sign(&payload);
        WsTicket {
            token: format!("{payload}|{signature}"),
            scope: scope.to_owned(),
            expires_at,
        }
    }

    /// Verify a ticket string: HMAC authenticity first, then scope, then expiry.
    /// `now` is unix seconds.
    pub fn verify(&self, token: &str, expected_scope: &str, now: i64) -> Result<(), WsTicketError> {
        let parts: Vec<&str> = token.split('|').collect();
        if parts.len() != 5 || parts[0] != "v1" {
            return Err(WsTicketError::Malformed);
        }
        let scope = parts[1];
        let _issued: i64 = parts[2].parse().map_err(|_| WsTicketError::BadTimestamp)?;
        let expires_at: i64 = parts[3].parse().map_err(|_| WsTicketError::BadTimestamp)?;
        let sig = hex::decode(parts[4]).map_err(|_| WsTicketError::BadSignature)?;
        // Recompute over the EXACT signed substring (the first four pipe fields),
        // so reformatting never drifts from what was signed.
        let payload = parts[..4].join("|");
        let mut mac = HmacSha256::new_from_slice(&self.key).expect("HMAC accepts any key length");
        mac.update(payload.as_bytes());
        // verify_slice is a constant-time comparison.
        mac.verify_slice(&sig)
            .map_err(|_| WsTicketError::HmacMismatch)?;
        if scope != expected_scope {
            return Err(WsTicketError::ScopeMismatch);
        }
        if now >= expires_at {
            return Err(WsTicketError::Expired);
        }
        Ok(())
    }

    fn sign(&self, payload: &str) -> String {
        let mut mac = HmacSha256::new_from_slice(&self.key).expect("HMAC accepts any key length");
        mac.update(payload.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }
}

/// The signed substring: `v1|<scope>|<issued_at>|<expires_at>`.
fn sign_payload(scope: &str, issued_at: i64, expires_at: i64) -> String {
    format!("v1|{scope}|{issued_at}|{expires_at}")
}

/// Unix seconds, clamped to 0 if the clock predates the epoch (a freshly booted
/// SBC before NTP sync) so minting never aborts the process.
pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mint_round_trips_and_verifies() {
        let issuer = WsTicketIssuer::from_api_key("ados_secret");
        let t = issuer.mint_at(SCOPE_MAVLINK_WS, 30, 1000);
        assert_eq!(t.expires_at, 1030);
        assert!(t.token.starts_with("v1|gs.mavlink_ws|1000|1030|"));
        assert!(issuer.verify(&t.token, SCOPE_MAVLINK_WS, 1000).is_ok());
        assert!(issuer.verify(&t.token, SCOPE_MAVLINK_WS, 1029).is_ok());
    }

    #[test]
    fn rejects_expired_at_the_boundary() {
        let issuer = WsTicketIssuer::from_api_key("k");
        let t = issuer.mint_at(SCOPE_MAVLINK_WS, 30, 1000);
        // now == expires_at is expired (matches the plugin token's `>=`).
        assert_eq!(
            issuer.verify(&t.token, SCOPE_MAVLINK_WS, 1030),
            Err(WsTicketError::Expired)
        );
    }

    #[test]
    fn rejects_wrong_scope() {
        let issuer = WsTicketIssuer::from_api_key("k");
        let t = issuer.mint_at("gs.pic_events", 30, 1000);
        assert_eq!(
            issuer.verify(&t.token, SCOPE_MAVLINK_WS, 1000),
            Err(WsTicketError::ScopeMismatch)
        );
        // ...and accepts the scope it was minted for.
        assert!(issuer.verify(&t.token, "gs.pic_events", 1000).is_ok());
    }

    #[test]
    fn rejects_wrong_key() {
        let a = WsTicketIssuer::from_api_key("key-a");
        let b = WsTicketIssuer::from_api_key("key-b");
        let t = a.mint_at(SCOPE_MAVLINK_WS, 30, 1000);
        assert_eq!(
            b.verify(&t.token, SCOPE_MAVLINK_WS, 1000),
            Err(WsTicketError::HmacMismatch)
        );
    }

    #[test]
    fn rejects_tampered_expiry() {
        let issuer = WsTicketIssuer::from_api_key("k");
        let t = issuer.mint_at(SCOPE_MAVLINK_WS, 30, 1000);
        // Forge a longer expiry while keeping the original signature.
        let parts: Vec<&str> = t.token.split('|').collect();
        let forged = format!("v1|{}|{}|99999|{}", parts[1], parts[2], parts[4]);
        assert_eq!(
            issuer.verify(&forged, SCOPE_MAVLINK_WS, 1000),
            Err(WsTicketError::HmacMismatch)
        );
    }

    #[test]
    fn rejects_malformed() {
        let issuer = WsTicketIssuer::from_api_key("k");
        assert_eq!(
            issuer.verify("", SCOPE_MAVLINK_WS, 0),
            Err(WsTicketError::Malformed)
        );
        assert_eq!(
            issuer.verify("v2|s|1|2|ff", SCOPE_MAVLINK_WS, 0),
            Err(WsTicketError::Malformed)
        );
        assert_eq!(
            issuer.verify("v1|s|notanint|2|ff", "s", 0),
            Err(WsTicketError::BadTimestamp)
        );
        assert_eq!(
            issuer.verify("v1|s|1|2|nothex", "s", 0),
            Err(WsTicketError::BadSignature)
        );
    }

    // The exact vector the Python verifier (`ados.core.ws_ticket`) must agree
    // with. If this signature ever changes, the Python mirror must change in
    // lockstep or cross-language tickets silently stop verifying.
    #[test]
    fn known_answer_vector_for_python_interop() {
        let issuer = WsTicketIssuer::from_api_key("ados_secret");
        let t = issuer.mint_at(SCOPE_MAVLINK_WS, 30, 1_000_000);
        // Pin the full token so the Python test can assert the identical string.
        assert_eq!(
            t.token,
            "v1|gs.mavlink_ws|1000000|1000030|\
655a695c0b38fa07b830a7ca3534a4cd6ef95831fb5e523cc98871bbef191413",
            "if this fails, regenerate the vector and update ados.core.ws_ticket"
        );
    }
}
