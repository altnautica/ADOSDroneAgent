//! Self-contained dashboard-access session tokens.
//!
//! A browser reaching a PAIRED agent's own web dashboard from off-box has no
//! `X-ADOS-Key` (that key lives in the GCS, not in a casual visitor's browser).
//! Rather than prompt for the raw key, the dashboard offers a short numeric PIN:
//! the operator enters it, and the agent hands back one of these session tokens,
//! which the dashboard then sends on every `/api/*` call as
//! `X-ADOS-Dashboard-Session`. The native control front accepts a valid session
//! as an alternative data-plane credential to `X-ADOS-Key`.
//!
//! Like [`crate::ws_ticket`], the token is **self-contained and HMAC-signed** —
//! the minting surface and the validating surface (the same daemon here, but the
//! shape matches the ws-ticket precedent) share no state, only the key. The key
//! is derived from BOTH the pairing `api_key` AND the PIN record's random salt:
//!
//! ```text
//! K = HMAC-SHA256(api_key, "ados-dashboard-session-v1" || salt)
//! ```
//!
//! Folding the salt into the key is what makes a PIN reset revoke every live
//! session: resetting the PIN writes a fresh salt, so `K` changes and every
//! previously-minted token fails to verify. Re-pairing (a new `api_key`) revokes
//! them too. The salt is never the pairing key itself, and is namespaced to this
//! use by the domain-separation label.
//!
//! Wire form (pipe-delimited, like the ws ticket but with no scope field):
//! `v1|<issued_at>|<expires_at>|<sig_hex>`, where the signature is
//! `HMAC-SHA256(K, "v1|<issued_at>|<expires_at>")`.

use std::time::{SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use sha2::Sha256;
use thiserror::Error;

type HmacSha256 = Hmac<Sha256>;

/// Domain-separation label mixed (with the salt) into the pairing key to derive
/// the session key.
pub const SESSION_KEY_LABEL: &[u8] = b"ados-dashboard-session-v1";

/// Default session lifetime in seconds (24 h): long enough that a dashboard is
/// not re-prompting the operator through a work session, short enough that an
/// abandoned browser tab does not stay authorized forever.
pub const DEFAULT_TTL_SECONDS: i64 = 86_400;

/// Hard cap on a requested session lifetime (7 days), the ceiling a silent
/// activity-refresh re-mints under.
pub const MAX_TTL_SECONDS: i64 = 604_800;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SessionError {
    #[error("malformed dashboard session token")]
    Malformed,
    #[error("dashboard session timestamp is not an integer")]
    BadTimestamp,
    #[error("dashboard session signature is not valid hex")]
    BadSignature,
    #[error("dashboard session HMAC mismatch")]
    HmacMismatch,
    #[error("dashboard session expired")]
    Expired,
}

/// A minted session: its compact string form plus the parsed expiry, so a mint
/// surface can return `{ session, expires_at }` without re-parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DashboardSession {
    pub token: String,
    pub expires_at: i64,
}

/// Mints and verifies self-contained dashboard-access sessions, keyed by the
/// pairing `api_key` + the PIN record's salt. Build one with
/// [`DashboardSessionIssuer::from_api_key_and_salt`].
#[derive(Clone)]
pub struct DashboardSessionIssuer {
    key: Vec<u8>,
}

impl DashboardSessionIssuer {
    /// Derive the session key from the pairing `api_key` and the PIN salt under
    /// the fixed label: `K = HMAC-SHA256(api_key, LABEL || salt)`. A fresh salt
    /// (written on every PIN set/reset) yields a fresh `K`, so a reset revokes
    /// every prior session.
    pub fn from_api_key_and_salt(api_key: &str, salt: &[u8]) -> Self {
        let mut mac =
            HmacSha256::new_from_slice(api_key.as_bytes()).expect("HMAC accepts any key length");
        mac.update(SESSION_KEY_LABEL);
        mac.update(salt);
        let key = mac.finalize().into_bytes().to_vec();
        Self { key }
    }

    /// Mint a session valid for `ttl_seconds`, using the wall clock.
    pub fn mint(&self, ttl_seconds: i64) -> DashboardSession {
        self.mint_at(ttl_seconds, now_unix())
    }

    /// Deterministic mint core (explicit clock) for tests and cross-impl vectors.
    pub fn mint_at(&self, ttl_seconds: i64, now: i64) -> DashboardSession {
        // Saturating add so a far-future clock or a huge ttl cannot overflow
        // (which would panic under the workspace's panic=abort).
        let expires_at = now.saturating_add(ttl_seconds);
        let payload = sign_payload(now, expires_at);
        let signature = self.sign(&payload);
        DashboardSession {
            token: format!("{payload}|{signature}"),
            expires_at,
        }
    }

    /// Verify a session token: HMAC authenticity first, then expiry. `now` is
    /// unix seconds.
    pub fn verify(&self, token: &str, now: i64) -> Result<(), SessionError> {
        let parts: Vec<&str> = token.split('|').collect();
        if parts.len() != 4 || parts[0] != "v1" {
            return Err(SessionError::Malformed);
        }
        let _issued: i64 = parts[1].parse().map_err(|_| SessionError::BadTimestamp)?;
        let expires_at: i64 = parts[2].parse().map_err(|_| SessionError::BadTimestamp)?;
        let sig = hex::decode(parts[3]).map_err(|_| SessionError::BadSignature)?;
        // Recompute over the EXACT signed substring (the first three pipe fields),
        // so reformatting never drifts from what was signed.
        let payload = parts[..3].join("|");
        let mut mac = HmacSha256::new_from_slice(&self.key).expect("HMAC accepts any key length");
        mac.update(payload.as_bytes());
        // verify_slice is a constant-time comparison.
        mac.verify_slice(&sig)
            .map_err(|_| SessionError::HmacMismatch)?;
        if now >= expires_at {
            return Err(SessionError::Expired);
        }
        Ok(())
    }

    fn sign(&self, payload: &str) -> String {
        let mut mac = HmacSha256::new_from_slice(&self.key).expect("HMAC accepts any key length");
        mac.update(payload.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }
}

/// The signed substring: `v1|<issued_at>|<expires_at>`.
fn sign_payload(issued_at: i64, expires_at: i64) -> String {
    format!("v1|{issued_at}|{expires_at}")
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

    const SALT: &[u8] = b"a-random-salt-16";

    #[test]
    fn mint_round_trips_and_verifies() {
        let issuer = DashboardSessionIssuer::from_api_key_and_salt("ados_secret", SALT);
        let s = issuer.mint_at(86_400, 1000);
        assert_eq!(s.expires_at, 87_400);
        assert!(s.token.starts_with("v1|1000|87400|"));
        assert!(issuer.verify(&s.token, 1000).is_ok());
        assert!(issuer.verify(&s.token, 87_399).is_ok());
    }

    #[test]
    fn rejects_expired_at_the_boundary() {
        let issuer = DashboardSessionIssuer::from_api_key_and_salt("k", SALT);
        let s = issuer.mint_at(30, 1000);
        // now == expires_at is expired (matches the ws-ticket / plugin token `>=`).
        assert_eq!(issuer.verify(&s.token, 1030), Err(SessionError::Expired));
    }

    #[test]
    fn rejects_wrong_key() {
        let a = DashboardSessionIssuer::from_api_key_and_salt("key-a", SALT);
        let b = DashboardSessionIssuer::from_api_key_and_salt("key-b", SALT);
        let s = a.mint_at(30, 1000);
        assert_eq!(b.verify(&s.token, 1000), Err(SessionError::HmacMismatch));
    }

    #[test]
    fn a_fresh_salt_revokes_prior_sessions() {
        // The reset-kills-sessions invariant: the same api_key with a DIFFERENT
        // salt (what a PIN reset writes) cannot verify a token minted under the
        // old salt.
        let old = DashboardSessionIssuer::from_api_key_and_salt("ados_secret", b"old-salt");
        let new = DashboardSessionIssuer::from_api_key_and_salt("ados_secret", b"new-salt");
        let s = old.mint_at(86_400, 1000);
        assert!(
            old.verify(&s.token, 1000).is_ok(),
            "the old salt still verifies its own token"
        );
        assert_eq!(
            new.verify(&s.token, 1000),
            Err(SessionError::HmacMismatch),
            "a reset (new salt) must revoke the old session"
        );
    }

    #[test]
    fn rejects_tampered_expiry() {
        let issuer = DashboardSessionIssuer::from_api_key_and_salt("k", SALT);
        let s = issuer.mint_at(30, 1000);
        // Forge a longer expiry while keeping the original signature.
        let parts: Vec<&str> = s.token.split('|').collect();
        let forged = format!("v1|{}|99999|{}", parts[1], parts[3]);
        assert_eq!(
            issuer.verify(&forged, 1000),
            Err(SessionError::HmacMismatch)
        );
    }

    #[test]
    fn rejects_malformed() {
        let issuer = DashboardSessionIssuer::from_api_key_and_salt("k", SALT);
        assert_eq!(issuer.verify("", 0), Err(SessionError::Malformed));
        // A ws-ticket-shaped 5-field token is malformed here (no scope field).
        assert_eq!(
            issuer.verify("v1|s|1|2|ff", 0),
            Err(SessionError::Malformed)
        );
        assert_eq!(issuer.verify("v2|1|2|ff", 0), Err(SessionError::Malformed));
        assert_eq!(
            issuer.verify("v1|notanint|2|ff", 0),
            Err(SessionError::BadTimestamp)
        );
        assert_eq!(
            issuer.verify("v1|1|2|nothex", 0),
            Err(SessionError::BadSignature)
        );
    }

    #[test]
    fn ttl_saturates_rather_than_overflows() {
        let issuer = DashboardSessionIssuer::from_api_key_and_salt("k", SALT);
        let s = issuer.mint_at(i64::MAX, i64::MAX - 1);
        assert_eq!(s.expires_at, i64::MAX);
    }
}
