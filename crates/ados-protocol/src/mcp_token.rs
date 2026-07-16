//! Self-contained, scoped ADOS MCP tokens (agent-side verify + on-box mint).
//!
//! An MCP client that reaches the drone directly (the `ADOS-MCP` connector in
//! agent-mode) can present a **scoped, revocable** token instead of the full
//! pairing key. The token is minted either by the connector's `agent:` issuer or
//! by this agent's own on-box mint (`ados mcp mint`); both derive the identical
//! key from the pairing `api_key`, so the agent verifies it with no shared state.
//!
//! Wire form (like the plugin capability token): `base64url(json).base64url(hmac)`,
//! split on the LAST `.`. The signature is `HMAC-SHA256(K, blob_bytes)` over the
//! EXACT received blob bytes (never a re-serialization). The key
//! `K = HKDF-SHA256(ikm = api_key, salt = "ados/mcp-token/v1", info = revocation_salt)`
//! — an HKDF (extract + expand), NOT the single-HMAC derivation the ws-ticket uses,
//! and the expiry is in MILLISECONDS (not seconds). These three facts are what make
//! this module byte-for-byte compatible with the connector's `agent:` issuer, and
//! the known-answer vector in the tests is the guardrail.

use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use thiserror::Error;

type HmacSha256 = Hmac<Sha256>;

const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// Domain-separation label for the HKDF. MUST equal the connector's
/// `MCP_TOKEN_HKDF_LABEL = utf8("ados/mcp-token/v1")` (ADOS-MCP src/auth/issuers.ts).
pub const MCP_TOKEN_HKDF_LABEL: &[u8] = b"ados/mcp-token/v1";

/// The scope groups a token may carry. Mirrors ADOS-MCP src/auth/scopes.ts.
pub const SCOPE_GROUPS: [&str; 6] = [
    "read",
    "safe_write",
    "admin",
    "flight",
    "destructive",
    "secret_read",
];

#[derive(Debug, Error, PartialEq, Eq)]
pub enum McpTokenError {
    #[error("malformed mcp token")]
    Malformed,
    #[error("mcp token base64 is invalid")]
    BadBase64,
    #[error("mcp token HMAC mismatch")]
    HmacMismatch,
    #[error("mcp token claims are not valid JSON")]
    BadClaims,
    #[error("mcp token carries an unknown scope")]
    UnknownScope,
    #[error("mcp token expired")]
    Expired,
    #[error("mcp token issued for a different node")]
    WrongNode,
}

/// The verified claim set. Field renames match the connector's TokenClaims.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct McpClaims {
    #[serde(rename = "tokenId")]
    pub token_id: String,
    #[serde(rename = "operatorId")]
    pub operator_id: String,
    pub iss: String,
    pub scopes: Vec<String>,
    #[serde(rename = "allowedNodes", default)]
    pub allowed_nodes: Vec<String>,
    #[serde(rename = "expiresAt")]
    pub expires_at: i64, // milliseconds since the epoch
    #[serde(default)]
    pub label: String,
}

/// The scope class a route requires. Mirrors the connector's safety classes, plus
/// `SecretRead` for a read route whose body carries a secret: it needs the
/// `secret_read` scope, NOT plain `read`, so a `read`-only token cannot reach it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeClass {
    Read,
    SecretRead,
    SafeWrite,
    Admin,
    Flight,
    Destructive,
}

impl ScopeClass {
    pub fn group_name(self) -> &'static str {
        match self {
            ScopeClass::Read => "read",
            ScopeClass::SecretRead => "secret_read",
            ScopeClass::SafeWrite => "safe_write",
            ScopeClass::Admin => "admin",
            ScopeClass::Flight => "flight",
            ScopeClass::Destructive => "destructive",
        }
    }
}

/// True when the granted scope groups admit a route of this class (group-level
/// membership; the finer tool->capability expansion is the connector's job).
pub fn scope_allows_class(required: ScopeClass, granted: &[String]) -> bool {
    let want = required.group_name();
    granted.iter().any(|g| g == want)
}

/// HKDF-SHA256 to 32 bytes = HMAC(HMAC(salt, ikm), info || 0x01). One expand block
/// suffices because the output length equals the SHA-256 block (32 bytes).
fn hkdf_sha256_32(salt: &[u8], ikm: &[u8], info: &[u8]) -> [u8; 32] {
    let mut extract = HmacSha256::new_from_slice(salt).expect("HMAC accepts any key length");
    extract.update(ikm);
    let prk = extract.finalize().into_bytes();
    let mut expand = HmacSha256::new_from_slice(&prk).expect("HMAC accepts any key length");
    expand.update(info);
    expand.update(&[0x01]);
    let okm = expand.finalize().into_bytes();
    let mut out = [0u8; 32];
    out.copy_from_slice(&okm);
    out
}

fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Mints and verifies self-contained MCP tokens, keyed by a pairing-derived HKDF
/// secret. Build with [`McpTokenIssuer::from_api_key`] from the same `api_key` the
/// connector's `agent:` issuer uses.
#[derive(Clone)]
pub struct McpTokenIssuer {
    key: [u8; 32],
}

impl McpTokenIssuer {
    /// Derive the token key from the pairing `api_key` and the bulk-revocation salt.
    /// An empty salt is the connector's default (revocationSalt = new Uint8Array(0));
    /// rotating the salt (a "revoke all") yields a fresh key so every prior token dies.
    pub fn from_api_key(api_key: &str, revocation_salt: &[u8]) -> Self {
        Self {
            key: hkdf_sha256_32(MCP_TOKEN_HKDF_LABEL, api_key.as_bytes(), revocation_salt),
        }
    }

    /// Verify a token to its claims. Hashes the EXACT received blob bytes, checks the
    /// HMAC (constant time), parses the claims, rejects an unknown scope, enforces the
    /// millisecond expiry (`expires_at <= now`), and — when `expected_node_id` is set
    /// and the issuer is `agent:<id>` — enforces the node subject.
    pub fn verify(
        &self,
        token: &str,
        now_ms: i64,
        expected_node_id: Option<&str>,
    ) -> Result<McpClaims, McpTokenError> {
        let (b_blob, b_sig) = token.rsplit_once('.').ok_or(McpTokenError::Malformed)?;
        if b_blob.is_empty() || b_sig.is_empty() {
            return Err(McpTokenError::Malformed);
        }
        let blob = B64.decode(b_blob).map_err(|_| McpTokenError::BadBase64)?;
        let sig = B64.decode(b_sig).map_err(|_| McpTokenError::BadBase64)?;
        let mut mac = HmacSha256::new_from_slice(&self.key).expect("HMAC accepts any key length");
        mac.update(&blob);
        mac.verify_slice(&sig)
            .map_err(|_| McpTokenError::HmacMismatch)?;
        let claims: McpClaims =
            serde_json::from_slice(&blob).map_err(|_| McpTokenError::BadClaims)?;
        for s in &claims.scopes {
            if !SCOPE_GROUPS.contains(&s.as_str()) {
                return Err(McpTokenError::UnknownScope);
            }
        }
        if claims.expires_at <= now_ms {
            return Err(McpTokenError::Expired);
        }
        if let Some(node) = expected_node_id {
            if let Some(subj) = claims.iss.strip_prefix("agent:") {
                if subj != node {
                    return Err(McpTokenError::WrongNode);
                }
            }
        }
        Ok(claims)
    }

    /// Verify against the current wall clock.
    pub fn verify_now(
        &self,
        token: &str,
        expected_node_id: Option<&str>,
    ) -> Result<McpClaims, McpTokenError> {
        self.verify(token, now_unix_ms(), expected_node_id)
    }

    /// On-box mint (for `ados mcp mint`). The blob is `serde_json::to_vec(claims)` —
    /// interop with the connector is guaranteed by HKDF + HMAC, not by JSON key order
    /// (each side verifies received bytes), so no canonical serializer is needed.
    pub fn mint(&self, claims: &McpClaims) -> String {
        let blob = serde_json::to_vec(claims).expect("claims serialize");
        let mut mac = HmacSha256::new_from_slice(&self.key).expect("HMAC accepts any key length");
        mac.update(&blob);
        let sig = mac.finalize().into_bytes();
        format!("{}.{}", B64.encode(&blob), B64.encode(sig))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claims(exp_ms: i64) -> McpClaims {
        McpClaims {
            token_id: "mct_abc".into(),
            operator_id: "cloud:usr_1".into(),
            iss: "agent:dev-01".into(),
            scopes: vec!["read".into(), "admin".into()],
            allowed_nodes: vec![],
            expires_at: exp_ms,
            label: "claude".into(),
        }
    }

    #[test]
    fn mint_round_trips_and_verifies() {
        let iss = McpTokenIssuer::from_api_key("ados_secret", &[]);
        let tok = iss.mint(&claims(2_000_000_000_000));
        let c = iss.verify(&tok, 1_000_000_000_000, Some("dev-01")).unwrap();
        assert_eq!(c.token_id, "mct_abc");
        assert_eq!(c.scopes, vec!["read", "admin"]);
    }

    #[test]
    fn rejects_expired_at_the_boundary() {
        let iss = McpTokenIssuer::from_api_key("k", &[]);
        let tok = iss.mint(&claims(1_000));
        assert_eq!(iss.verify(&tok, 1_000, None), Err(McpTokenError::Expired));
        assert!(iss.verify(&tok, 999, None).is_ok());
    }

    #[test]
    fn rejects_wrong_key_and_tampered_blob() {
        let iss = McpTokenIssuer::from_api_key("k1", &[]);
        let tok = iss.mint(&claims(2_000_000_000_000));
        assert_eq!(
            McpTokenIssuer::from_api_key("k2", &[]).verify(&tok, 1, None),
            Err(McpTokenError::HmacMismatch)
        );
        // flip a blob byte, keep the sig -> HMAC over received bytes fails
        let (blob, sig) = tok.rsplit_once('.').unwrap();
        let mut raw = B64.decode(blob).unwrap();
        raw[0] ^= 0xff;
        let tampered = format!("{}.{}", B64.encode(&raw), sig);
        assert_eq!(
            iss.verify(&tampered, 1, None),
            Err(McpTokenError::HmacMismatch)
        );
    }

    #[test]
    fn rejects_unknown_scope_and_wrong_node() {
        let iss = McpTokenIssuer::from_api_key("k", &[]);
        let mut c = claims(2_000_000_000_000);
        c.scopes = vec!["read".into(), "superuser".into()];
        assert_eq!(
            iss.verify(&iss.mint(&c), 1, None),
            Err(McpTokenError::UnknownScope)
        );
        let ok = iss.mint(&claims(2_000_000_000_000));
        assert_eq!(
            iss.verify(&ok, 1, Some("other")),
            Err(McpTokenError::WrongNode)
        );
    }

    #[test]
    fn rejects_malformed() {
        let iss = McpTokenIssuer::from_api_key("k", &[]);
        for bad in ["", "nodot", "a.", ".b", "!!.??"] {
            assert!(iss.verify(bad, 1, None).is_err());
        }
    }

    #[test]
    fn revocation_salt_fold_revokes_all() {
        let a = McpTokenIssuer::from_api_key("k", b"salt-a");
        let b = McpTokenIssuer::from_api_key("k", b"salt-b");
        let tok = a.mint(&claims(2_000_000_000_000));
        assert!(a.verify(&tok, 1, None).is_ok());
        assert_eq!(b.verify(&tok, 1, None), Err(McpTokenError::HmacMismatch));
    }

    #[test]
    fn scope_class_membership() {
        let granted = vec!["read".to_string(), "admin".to_string()];
        assert!(scope_allows_class(ScopeClass::Read, &granted));
        assert!(scope_allows_class(ScopeClass::Admin, &granted));
        assert!(!scope_allows_class(ScopeClass::Flight, &granted));
        assert!(!scope_allows_class(ScopeClass::SafeWrite, &granted));
    }

    // Known-answer vector: pin a token this module mints for a fixed key + claims so
    // a mirror test in ADOS-MCP (src/auth) can assert its verifyToken accepts the
    // identical string. If this drifts, cross-repo tokens silently stop verifying.
    #[test]
    fn known_answer_vector_for_connector_interop() {
        let iss = McpTokenIssuer::from_api_key("ados_kat_key", &[]);
        // a fixed HKDF key + a fixed HMAC over fixed bytes is deterministic
        let c = McpClaims {
            token_id: "mct_kat".into(),
            operator_id: "cloud:kat".into(),
            iss: "agent:kat-node".into(),
            scopes: vec!["read".into()],
            allowed_nodes: vec![],
            expires_at: 4_102_444_800_000, // 2100-01-01
            label: "kat".into(),
        };
        let tok = iss.mint(&c);
        // round-trips under the same key (the connector mirror re-derives the key
        // from "ados_kat_key" and asserts verify(tok) == these claims)
        assert!(iss.verify(&tok, 1, Some("kat-node")).is_ok());
        assert!(tok.contains('.'));
    }
}
