//! Ticket minting and verification.

use super::*;

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

    pub(super) fn sign(&self, payload: &str) -> String {
        let mut mac = HmacSha256::new_from_slice(&self.key).expect("HMAC accepts any key length");
        mac.update(payload.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }
}

/// The fields of a token, borrowed from it.
pub(super) struct ParsedTicket<'a> {
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
    pub(super) fn parse(token: &'a str) -> Result<Self, RelayTicketError> {
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
pub(super) fn body_digest(body: &[u8]) -> String {
    hex::encode(Sha256::digest(body))
}

/// The signed substring.
pub(super) fn sign_payload(
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
