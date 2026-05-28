//! Plugin RPC envelope and capability tokens.
//!
//! Wire format: length-prefixed msgpack frames over a Unix domain socket (see
//! [`crate::frame`]). Each frame is a [`Envelope`] serialized as a msgpack map
//! with short keys. At plugin start the supervisor mints a per-process
//! HMAC-SHA256 capability token bound to `(plugin_id, granted_caps, session,
//! exp)`; the token rides in every envelope and the supervisor verifies it
//! before routing.
//!
//! This mirrors `ADOSDroneAgent/src/ados/plugins/rpc.py` exactly, so a Rust
//! plugin or a Rust core speaks the identical wire as the Python supervisor.

use std::collections::BTreeSet;
use std::time::{SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use thiserror::Error;

use crate::frame::{self, PLUGIN_MAX_FRAME};

/// Protocol version carried in every envelope (`PROTOCOL_VERSION`).
pub const PROTOCOL_VERSION: i64 = 1;

/// Default capability-token lifetime in seconds (`TOKEN_TTL_SECONDS`).
pub const TOKEN_TTL_SECONDS: i64 = 600;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Error)]
pub enum PluginError {
    #[error("framing error: {0}")]
    Frame(#[from] frame::FrameError),
    #[error("msgpack encode error: {0}")]
    Encode(#[from] rmp_serde::encode::Error),
    #[error("msgpack decode error: {0}")]
    Decode(#[from] rmp_serde::decode::Error),
}

fn empty_args() -> rmpv::Value {
    rmpv::Value::Map(Vec::new())
}

fn default_version() -> i64 {
    PROTOCOL_VERSION
}

/// One RPC message. Serializes to a msgpack map with the short keys the Python
/// side uses: `v`, `t`, `m`, `c`, `a`, `id`, `tok`, `err`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    #[serde(rename = "v", default = "default_version")]
    pub version: i64,
    /// "request" | "response" | "event".
    #[serde(rename = "t")]
    pub kind: String,
    #[serde(rename = "m")]
    pub method: String,
    #[serde(rename = "c", default)]
    pub capability: String,
    #[serde(rename = "a", default = "empty_args")]
    pub args: rmpv::Value,
    #[serde(rename = "id")]
    pub request_id: String,
    #[serde(rename = "tok", default)]
    pub token: String,
    #[serde(rename = "err", default)]
    pub error: Option<String>,
}

impl Envelope {
    /// Serialize the envelope to a msgpack map (no length prefix).
    pub fn to_msgpack(&self) -> Result<Vec<u8>, PluginError> {
        // `to_vec_named` encodes the struct as a map keyed by the serde field
        // names, matching Python's `msgpack.packb(dict, use_bin_type=True)`.
        Ok(rmp_serde::to_vec_named(self)?)
    }

    /// Deserialize an envelope from a msgpack map body (no length prefix).
    pub fn from_msgpack(body: &[u8]) -> Result<Self, PluginError> {
        Ok(rmp_serde::from_slice(body)?)
    }

    /// Encode the envelope as a complete length-prefixed frame, ready to write
    /// to the socket. Equivalent to Python `encode_frame`.
    pub fn encode_frame(&self) -> Result<Vec<u8>, PluginError> {
        let body = self.to_msgpack()?;
        Ok(frame::encode_frame(&body, PLUGIN_MAX_FRAME)?)
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum TokenError {
    #[error("malformed capability token")]
    Malformed,
    #[error("capability blob is not valid hex")]
    BadCapsBlob,
    #[error("token timestamp is not an integer")]
    BadTimestamp,
    #[error("capability token HMAC mismatch")]
    HmacMismatch,
    #[error("capability token expired")]
    Expired,
}

/// A minted, signed capability token. The string form is pipe-delimited:
/// `v1|<plugin_id>|<session_id>|<issued_at>|<expires_at>|<caps_hex>|<sig_hex>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityToken {
    pub plugin_id: String,
    pub session_id: String,
    /// Sorted set so the canonical `,`-joined form matches Python's
    /// `",".join(sorted(caps))`.
    pub granted_caps: BTreeSet<String>,
    pub issued_at: i64,
    pub expires_at: i64,
    /// Hex-encoded HMAC-SHA256.
    pub signature: String,
}

impl CapabilityToken {
    fn caps_csv(&self) -> String {
        // BTreeSet iterates in sorted order.
        self.granted_caps
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join(",")
    }

    /// Compact pipe-delimited string form.
    pub fn to_token_string(&self) -> String {
        let caps_hex = hex::encode(self.caps_csv().as_bytes());
        [
            "v1",
            &self.plugin_id,
            &self.session_id,
            &self.issued_at.to_string(),
            &self.expires_at.to_string(),
            &caps_hex,
            &self.signature,
        ]
        .join("|")
    }

    /// Parse the pipe-delimited string form.
    pub fn from_token_string(encoded: &str) -> Result<Self, TokenError> {
        let parts: Vec<&str> = encoded.split('|').collect();
        if parts.len() != 7 || parts[0] != "v1" {
            return Err(TokenError::Malformed);
        }
        let caps_bytes = hex::decode(parts[5]).map_err(|_| TokenError::BadCapsBlob)?;
        let caps_csv = String::from_utf8(caps_bytes).map_err(|_| TokenError::BadCapsBlob)?;
        let granted_caps: BTreeSet<String> = if caps_csv.is_empty() {
            BTreeSet::new()
        } else {
            caps_csv
                .split(',')
                .filter(|c| !c.is_empty())
                .map(str::to_owned)
                .collect()
        };
        let issued_at: i64 = parts[3].parse().map_err(|_| TokenError::BadTimestamp)?;
        let expires_at: i64 = parts[4].parse().map_err(|_| TokenError::BadTimestamp)?;
        Ok(Self {
            plugin_id: parts[1].to_owned(),
            session_id: parts[2].to_owned(),
            granted_caps,
            issued_at,
            expires_at,
            signature: parts[6].to_owned(),
        })
    }

    pub fn is_expired(&self, now: i64) -> bool {
        now >= self.expires_at
    }
}

/// Mints and verifies capability tokens. The supervisor holds one instance;
/// the secret is generated once per process and never written to disk.
pub struct TokenIssuer {
    secret: Vec<u8>,
}

impl TokenIssuer {
    pub fn new(secret: Vec<u8>) -> Self {
        Self { secret }
    }

    /// New issuer with a fresh 32-byte random secret (`secrets.token_bytes(32)`).
    pub fn new_random() -> Self {
        let mut secret = vec![0u8; 32];
        getrandom::getrandom(&mut secret).expect("OS RNG unavailable");
        Self { secret }
    }

    /// Mint a token using the current wall clock and a fresh random session id.
    pub fn mint(
        &self,
        plugin_id: &str,
        granted_caps: &BTreeSet<String>,
        ttl_seconds: i64,
    ) -> CapabilityToken {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_secs() as i64;
        let mut session_bytes = [0u8; 8];
        getrandom::getrandom(&mut session_bytes).expect("OS RNG unavailable");
        let session_id = hex::encode(session_bytes);
        self.mint_at(plugin_id, granted_caps, ttl_seconds, now, &session_id)
    }

    /// Deterministic mint core (explicit clock + session) for tests and replay.
    pub fn mint_at(
        &self,
        plugin_id: &str,
        granted_caps: &BTreeSet<String>,
        ttl_seconds: i64,
        now: i64,
        session_id: &str,
    ) -> CapabilityToken {
        let expires_at = now + ttl_seconds;
        let signature = self.sign(plugin_id, session_id, now, expires_at, granted_caps);
        CapabilityToken {
            plugin_id: plugin_id.to_owned(),
            session_id: session_id.to_owned(),
            granted_caps: granted_caps.clone(),
            issued_at: now,
            expires_at,
            signature,
        }
    }

    /// Verify the HMAC and expiry. `now` is unix seconds.
    pub fn verify(&self, token: &CapabilityToken, now: i64) -> Result<(), TokenError> {
        let actual = hex::decode(&token.signature).map_err(|_| TokenError::HmacMismatch)?;
        let payload = self.sign_payload(
            &token.plugin_id,
            &token.session_id,
            token.issued_at,
            token.expires_at,
            &token.granted_caps,
        );
        // verify_slice does a constant-time comparison internally.
        let mut mac =
            HmacSha256::new_from_slice(&self.secret).expect("HMAC accepts any key length");
        mac.update(payload.as_bytes());
        mac.verify_slice(&actual)
            .map_err(|_| TokenError::HmacMismatch)?;
        if token.is_expired(now) {
            return Err(TokenError::Expired);
        }
        Ok(())
    }

    fn sign_payload(
        &self,
        plugin_id: &str,
        session_id: &str,
        issued_at: i64,
        expires_at: i64,
        caps: &BTreeSet<String>,
    ) -> String {
        let caps_csv = caps.iter().cloned().collect::<Vec<_>>().join(",");
        [
            plugin_id,
            session_id,
            &issued_at.to_string(),
            &expires_at.to_string(),
            &caps_csv,
        ]
        .join("|")
    }

    fn sign(
        &self,
        plugin_id: &str,
        session_id: &str,
        issued_at: i64,
        expires_at: i64,
        caps: &BTreeSet<String>,
    ) -> String {
        let payload = self.sign_payload(plugin_id, session_id, issued_at, expires_at, caps);
        let mut mac =
            HmacSha256::new_from_slice(&self.secret).expect("HMAC accepts any key length");
        mac.update(payload.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn envelope_round_trips_through_msgpack() {
        let env = Envelope {
            version: PROTOCOL_VERSION,
            kind: "request".into(),
            method: "event.publish".into(),
            capability: "event.publish".into(),
            args: rmpv::Value::Map(vec![(
                rmpv::Value::String("topic".into()),
                rmpv::Value::String("demo".into()),
            )]),
            request_id: "abc123".into(),
            token: "v1|p|s|0|600||sig".into(),
            error: None,
        };
        let body = env.to_msgpack().unwrap();
        let back = Envelope::from_msgpack(&body).unwrap();
        assert_eq!(env, back);
    }

    #[test]
    fn envelope_uses_short_keys_on_the_wire() {
        let env = Envelope {
            version: 1,
            kind: "event".into(),
            method: "m".into(),
            capability: "c".into(),
            args: empty_args(),
            request_id: "id".into(),
            token: "tok".into(),
            error: Some("boom".into()),
        };
        let body = env.to_msgpack().unwrap();
        // Decode as a generic msgpack value and assert the map keys are the
        // short forms the Python side reads.
        let value: rmpv::Value = rmp_serde::from_slice(&body).unwrap();
        let map = match value {
            rmpv::Value::Map(m) => m,
            other => panic!("expected map, got {other:?}"),
        };
        let keys: BTreeSet<String> = map
            .iter()
            .filter_map(|(k, _)| k.as_str().map(str::to_owned))
            .collect();
        assert_eq!(keys, caps(&["v", "t", "m", "c", "a", "id", "tok", "err"]));
    }

    #[test]
    fn token_string_round_trips() {
        let issuer = TokenIssuer::new(b"test-secret-key".to_vec());
        let token = issuer.mint_at(
            "com.example.plugin",
            &caps(&["mavlink.read", "event.publish"]),
            600,
            1000,
            "deadbeef",
        );
        let s = token.to_token_string();
        // v1|plugin|session|issued|exp|caps_hex|sig
        assert!(s.starts_with("v1|com.example.plugin|deadbeef|1000|1600|"));
        let parsed = CapabilityToken::from_token_string(&s).unwrap();
        assert_eq!(parsed, token);
    }

    #[test]
    fn caps_are_canonically_sorted_in_the_signed_payload() {
        let issuer = TokenIssuer::new(b"k".to_vec());
        // Same caps, different insertion order, must yield the same signature.
        let a = issuer.mint_at("p", &caps(&["b", "a", "c"]), 600, 0, "s");
        let b = issuer.mint_at("p", &caps(&["c", "b", "a"]), 600, 0, "s");
        assert_eq!(a.signature, b.signature);
    }

    #[test]
    fn verify_accepts_a_good_token_and_rejects_tampering() {
        let issuer = TokenIssuer::new(b"secret".to_vec());
        let token = issuer.mint_at("p", &caps(&["mavlink.read"]), 600, 1000, "sess");
        assert!(issuer.verify(&token, 1500).is_ok());

        // Tamper with granted caps -> signature no longer matches.
        let mut tampered = token.clone();
        tampered.granted_caps.insert("mavlink.write".into());
        assert_eq!(
            issuer.verify(&tampered, 1500),
            Err(TokenError::HmacMismatch)
        );

        // Wrong secret -> mismatch.
        let other = TokenIssuer::new(b"different".to_vec());
        assert_eq!(other.verify(&token, 1500), Err(TokenError::HmacMismatch));
    }

    #[test]
    fn verify_rejects_expired_token() {
        let issuer = TokenIssuer::new(b"secret".to_vec());
        let token = issuer.mint_at("p", &caps(&["x"]), 600, 1000, "sess");
        // now == expires_at -> expired (Python: ts >= expires_at).
        assert_eq!(issuer.verify(&token, 1600), Err(TokenError::Expired));
        assert!(issuer.verify(&token, 1599).is_ok());
    }

    #[test]
    fn empty_caps_token_round_trips() {
        let issuer = TokenIssuer::new(b"k".to_vec());
        let token = issuer.mint_at("p", &BTreeSet::new(), 600, 0, "s");
        let parsed = CapabilityToken::from_token_string(&token.to_token_string()).unwrap();
        assert!(parsed.granted_caps.is_empty());
        assert_eq!(parsed, token);
    }
}
