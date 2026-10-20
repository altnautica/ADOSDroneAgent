//! The agent-side MCP-token record + the route->scope map for the AI-control
//! surface.
//!
//! An MCP client that reaches this agent directly (the `ADOS-MCP` connector in
//! agent-mode) may present a **scoped, revocable** token instead of the full
//! pairing key. This module owns:
//!
//! 1. [`McpTokenStore`] — the on-disk record (`/etc/ados/mcp-token.json`, `0600`)
//!    holding the bulk-revocation salt, a per-token denylist, and a registry of
//!    minted tokens for the status surface. Mint derives the key from the pairing
//!    `api_key` + the salt via [`ados_protocol::mcp_token`]; verify re-derives the
//!    identical key with no shared state and additionally rejects a denylisted
//!    token id.
//! 2. [`route_scope`] — the map from a native `(method, path)` to the
//!    [`ScopeClass`] a token must hold to reach it. It is **fail-closed**: every
//!    read (`GET`) needs `read`; a write with no explicit class returns `None`, so
//!    the edge denies the MCP token for it (the caller must fall back to the full
//!    `X-ADOS-Key`). A new route is therefore unreachable-by-token until it is
//!    classified here — never silently over-granted.
//!
//! The store is consulted at the auth edge ONLY on a would-be-401 (a request with
//! no valid `X-ADOS-Key` and no valid dashboard session), so an authenticated
//! request never stats the record. The whole surface is inert until the
//! `mcp_token_accept_enabled` flag flips (default off).

use std::path::{Path, PathBuf};

use ados_protocol::mcp_token::{McpClaims, McpTokenIssuer, ScopeClass, SCOPE_GROUPS};
use ados_protocol::pairing_posture::Pairing;
use http::Method;
use serde::{Deserialize, Serialize};

mod scope;

pub(crate) use scope::relay_proxy_inner_path;
pub use scope::route_decides_scope;
pub use scope::route_scope;
pub use scope::tool_safety_class;
pub use scope::tool_scope_class;
#[cfg(test)]
use scope::*;

/// Canonical MCP-token record path. Overridable via `ADOS_MCP_TOKEN_JSON` for
/// tests, mirroring the sibling `ADOS_DASHBOARD_PIN_JSON` / `ADOS_PAIRING_JSON`
/// override convention.
pub const DEFAULT_MCP_TOKEN_PATH: &str = "/etc/ados/mcp-token.json";

/// The header an MCP client sends its scoped token on, accepted by the front's
/// auth edge as an alternative to `X-ADOS-Key` (behind the accept flag).
pub const MCP_TOKEN_HEADER: &str = "x-ados-mcp-token";

/// The header the front stamps with the verified token's granted scope groups
/// (comma-joined) after admitting an MCP token. Like [`crate::serve::ONBOX_HEADER`]
/// it is STRIPPED from every inbound request first, then set only on a verified
/// token, so a client-supplied value can never be spoofed in.
pub const MCP_SCOPES_HEADER: &str = "x-ados-mcp-scopes";

/// Salt length in bytes (128-bit): folded into the token key so a rotation
/// (a "revoke all") makes every previously-minted token fail verification.
const SALT_LEN: usize = 16;

/// Serializes store MUTATIONS (mint / revoke / revoke_all) across this process so
/// a load-modify-write of the record cannot interleave with another writer (a
/// mint racing a revoke_all must not re-write the pre-revoke salt and resurrect
/// revoked tokens). The read path (verify / status / any_minted) is intentionally
/// unguarded — it only reads, and a self-contained token's validity never depends
/// on a concurrent write finishing.
static STORE_WRITE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// One minted token's registry entry. The secret is NOT stored (the token is
/// self-contained + shown once at mint); this is the operator-facing record so
/// `status` can list + `revoke` can target a specific token id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpTokenRecord {
    pub token_id: String,
    #[serde(default)]
    pub label: String,
    pub scopes: Vec<String>,
    #[serde(default)]
    pub allowed_nodes: Vec<String>,
    /// Unix seconds (fractional) the token was minted.
    #[serde(default)]
    pub created_at: f64,
    /// Milliseconds since the epoch the token expires (matches the claim field).
    pub expires_at: i64,
}

/// The persisted MCP-token record. Absent file = no tokens minted.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct McpTokenDoc {
    /// Hex-encoded bulk-revocation salt, folded into the token key.
    #[serde(default)]
    pub salt: String,
    /// Token ids revoked individually (a self-contained token can only be revoked
    /// by denylisting its id or rotating the salt).
    #[serde(default)]
    pub revoked: Vec<String>,
    /// The minted-token registry (for the status surface + targeted revoke).
    #[serde(default)]
    pub tokens: Vec<McpTokenRecord>,
}

/// The inputs to a single mint. Groups the caller-supplied fields (identity,
/// scopes, TTL, wall clock) so the mint entry point stays a one-argument call.
#[derive(Debug, Clone)]
pub struct MintRequest<'a> {
    /// The pairing `api_key` the token key is derived from.
    pub api_key: &'a str,
    /// The human label shown in the status surface.
    pub label: &'a str,
    /// The minting operator's id (carried in the claim, not enforced here).
    pub operator_id: &'a str,
    /// This node's id, stamped as the `agent:<node_id>` issuer subject.
    pub node_id: &'a str,
    /// The scope groups granted (each must be a known group).
    pub scopes: &'a [String],
    /// The node allowlist carried in the claim (empty = the minting node only).
    pub allowed_nodes: &'a [String],
    /// Lifetime in milliseconds from `now_ms`.
    pub ttl_ms: i64,
    /// Mint timestamp in unix seconds (fractional), stored in the registry record.
    pub now_secs: f64,
    /// Mint timestamp in milliseconds, added to `ttl_ms` for the claim expiry.
    pub now_ms: i64,
}

/// Why a mint failed.
#[derive(Debug)]
pub enum MintError {
    /// A requested scope is not one of the known groups.
    UnknownScope(String),
    /// `getrandom` failed while minting the salt / token id — fail closed rather
    /// than use a predictable value (which would weaken the key or collide ids).
    RandGen(getrandom::Error),
    /// Serializing or atomically writing the record failed.
    Persist(std::io::Error),
    /// No pairing key is present, so no key can be derived.
    Unpaired,
}

impl std::fmt::Display for MintError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MintError::UnknownScope(s) => write!(f, "unknown scope group: {s}"),
            MintError::RandGen(e) => write!(f, "random generation failed: {e}"),
            MintError::Persist(e) => write!(f, "{e}"),
            MintError::Unpaired => write!(f, "agent is unpaired; no key to mint against"),
        }
    }
}

impl std::error::Error for MintError {}

/// The MCP-token store: a path plus read/write operations. Every op reads or
/// writes the file fresh (the record is small + ops are infrequent), so there is
/// no cache to keep coherent across the routes + the auth edge.
#[derive(Debug, Clone)]
pub struct McpTokenStore {
    path: PathBuf,
}

impl McpTokenStore {
    /// Build a store against the standard path (honouring `ADOS_MCP_TOKEN_JSON`).
    pub fn new() -> Self {
        let path = std::env::var("ADOS_MCP_TOKEN_JSON")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(DEFAULT_MCP_TOKEN_PATH));
        Self { path }
    }

    /// Build a store against an explicit path (tests + the daemon's injectable path).
    pub fn with_path(path: PathBuf) -> Self {
        Self { path }
    }

    /// The record path this store reads + writes.
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn load(&self) -> McpTokenDoc {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    /// The decoded salt, or `None` when no salt is set yet (no token ever minted).
    fn salt_bytes(&self, doc: &McpTokenDoc) -> Option<Vec<u8>> {
        if doc.salt.is_empty() {
            None
        } else {
            hex::decode(&doc.salt).ok()
        }
    }

    /// Whether any (non-revoked, unexpired) token could be presented. Used by the
    /// heartbeat/status surface; the salt existing means at least one mint happened.
    pub fn any_minted(&self) -> bool {
        !self.load().salt.is_empty()
    }

    /// The minted-token registry (for the `status` route), plus the revoked-id set
    /// so the surface can badge each entry.
    pub fn status(&self) -> (Vec<McpTokenRecord>, Vec<String>) {
        let doc = self.load();
        (doc.tokens, doc.revoked)
    }

    /// Mint a new token for the given pairing key. Validates every scope is a known
    /// group, mints the salt on first use, derives the issuer, appends the record,
    /// and returns the one-time token string. `now_secs`/`now_ms` are the wall clock.
    pub fn mint(&self, req: &MintRequest) -> Result<String, MintError> {
        let _guard = STORE_WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        if req.api_key.is_empty() {
            return Err(MintError::Unpaired);
        }
        for s in req.scopes {
            if !SCOPE_GROUPS.contains(&s.as_str()) {
                return Err(MintError::UnknownScope(s.clone()));
            }
        }
        let mut doc = self.load();
        // Mint the salt on first use (a fresh 16-byte random), so an existing salt
        // is preserved (a salt rotation is an explicit `revoke_all`, not a re-mint).
        if doc.salt.is_empty() {
            let mut salt = [0u8; SALT_LEN];
            getrandom::fill(&mut salt).map_err(MintError::RandGen)?;
            doc.salt = hex::encode(salt);
        }
        let salt = hex::decode(&doc.salt).unwrap_or_default();
        let token_id = new_token_id().map_err(MintError::RandGen)?;
        let claims = McpClaims {
            token_id: token_id.clone(),
            operator_id: req.operator_id.to_string(),
            iss: format!("agent:{}", req.node_id),
            scopes: req.scopes.to_vec(),
            allowed_nodes: req.allowed_nodes.to_vec(),
            expires_at: req.now_ms.saturating_add(req.ttl_ms),
            label: req.label.to_string(),
        };
        let token = McpTokenIssuer::from_api_key(req.api_key, &salt).mint(&claims);
        doc.tokens.push(McpTokenRecord {
            token_id,
            label: req.label.to_string(),
            scopes: req.scopes.to_vec(),
            allowed_nodes: req.allowed_nodes.to_vec(),
            created_at: req.now_secs,
            expires_at: claims.expires_at,
        });
        self.persist(&doc).map_err(MintError::Persist)?;
        Ok(token)
    }

    /// Verify a presented token against the current pairing key + stored salt.
    /// Returns the claims only when the HMAC is authentic, the token is unexpired,
    /// the node subject matches (for an `agent:` issuer), AND the token id is not on
    /// the revocation denylist. `None` when unpaired, no salt is set, or any check
    /// fails — the edge treats `None` as "not an MCP-token-authorized request".
    pub fn verify(
        &self,
        pairing: &Pairing,
        token: &str,
        now_ms: i64,
        node_id: &str,
    ) -> Option<McpClaims> {
        let Pairing::Paired(key) = pairing else {
            return None;
        };
        let doc = self.load();
        let salt = self.salt_bytes(&doc)?;
        let claims = McpTokenIssuer::from_api_key(key, &salt)
            .verify(token, now_ms, Some(node_id))
            .ok()?;
        if doc.revoked.iter().any(|r| r == &claims.token_id) {
            return None;
        }
        Some(claims)
    }

    /// Revoke one token by id (denylist). A no-op success if the id is unknown or
    /// already revoked. Absent record = nothing to revoke (success).
    pub fn revoke(&self, token_id: &str) -> std::io::Result<()> {
        let _guard = STORE_WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut doc = self.load();
        if !doc.revoked.iter().any(|r| r == token_id) {
            doc.revoked.push(token_id.to_string());
        }
        self.persist(&doc)
    }

    /// Revoke ALL tokens by rotating the salt (every prior token's key dies) and
    /// clearing the registry + denylist. A subsequent mint installs a fresh salt.
    pub fn revoke_all(&self) -> std::io::Result<()> {
        let _guard = STORE_WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Removing the record is the cleanest bulk revoke: the salt is gone, so no
        // prior token can verify, and the next mint re-mints a fresh salt.
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    fn persist(&self, doc: &McpTokenDoc) -> std::io::Result<()> {
        let body =
            serde_json::to_vec_pretty(doc).map_err(|e| std::io::Error::other(e.to_string()))?;
        crate::pairing_store::atomic_write_0600(&self.path, &body)
    }
}

impl Default for McpTokenStore {
    fn default() -> Self {
        Self::new()
    }
}

/// A fresh, unguessable token id (`mct_<hex>`), used both as the registry key and
/// the revocation-denylist key.
fn new_token_id() -> Result<String, getrandom::Error> {
    let mut raw = [0u8; 12];
    getrandom::fill(&mut raw)?;
    Ok(format!("mct_{}", hex::encode(raw)))
}

#[cfg(test)]
mod tests;
