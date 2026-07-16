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
            getrandom::getrandom(&mut salt).map_err(MintError::RandGen)?;
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
    getrandom::getrandom(&mut raw)?;
    Ok(format!("mct_{}", hex::encode(raw)))
}

/// The [`ScopeClass`] a native `(method, path)` requires of an MCP token, or
/// `None` when the route is not reachable by a token (fail-closed: the caller must
/// use the full `X-ADOS-Key`). Every native `GET` is a read; writes are classified
/// by their effect, defaulting to `None` so a new/unclassified write is denied.
///
/// Public-exempt routes (the pairing handshake, `/healthz`, the dashboard-PIN
/// status/verify/set) never reach the MCP branch (the edge admits them before
/// auth), and proxied routes (config/vision/plugins under the permanent-Python
/// prefixes) are gated by the residual surface, so this map covers only the native
/// gated routes an MCP token can present against.
pub fn route_scope(method: &Method, path: &str) -> Option<ScopeClass> {
    use ScopeClass::*;
    // A GET carries no side effect on the agent, so every native read is `read` —
    // EXCEPT a GET whose response body carries a secret, which needs the
    // `secret_read` scope so a plain `read` token cannot reach it. No native GET is
    // secret-bearing today (the pairing key / WFB bind key / WS ticket are never
    // returned by a GET); a future one MUST be listed in `SECRET_GET_ROUTES` or a
    // `read` token would reach it (fail-open for that one route).
    if method == Method::GET {
        return Some(if is_secret_get(path) {
            SecretRead
        } else {
            Read
        });
    }
    match *method {
        Method::POST => match path {
            // Raw MAVLink command passthrough can command the vehicle -> flight.
            "/api/command" => Some(Flight),
            // Param set reconfigures the FC (arming/failsafe params etc.) -> admin.
            p if is_param_write(p) => Some(Admin),
            // Service restart / supervisor restart / signing writes.
            p if is_service_restart(p) => Some(Admin),
            "/api/v1/system/restart-supervisor" => Some(Admin),
            "/api/mavlink/signing/enroll-fc" | "/api/mavlink/signing/disable-on-fc" => Some(Admin),
            // WFB radio + MAC-pin writes.
            "/api/wfb/channel" | "/api/v1/network/mac/pin" => Some(Admin),
            // Vision custom-model upload + MCP self-revoke + WS-ticket mint (mints
            // a data-plane credential) are admin actions.
            "/api/vision/models/upload" | "/api/mcp/revoke" | "/api/_ws/ticket" => Some(Admin),
            // CAN passthrough injects arbitrary CAN frames to the FC / ESCs /
            // servos — a vehicle-command write on par with /api/command -> flight.
            "/api/can/passthrough" => Some(Flight),
            // Operator-facing safe writes.
            "/api/vision/designate" | "/api/logs/push" => Some(SafeWrite),
            p if p.starts_with("/api/atlas/capture/") => Some(SafeWrite),
            // Unpairing tears down the trust relationship.
            "/api/pairing/unpair" => Some(Destructive),
            // Running a plugin's MCP tool: the edge floor is admin. The connector
            // enforces each tool's declared safety class (a flight tool needs the
            // flight scope) and the plugin's own caps bound the effect, so this is
            // a coarse floor, not the fine gate.
            p if is_plugin_tool_invoke(p) => Some(Admin),
            // Ground-station control writes.
            p if p.starts_with("/api/v1/ground-station/") => Some(Admin),
            _ => None,
        },
        Method::PUT => match path {
            "/api/wfb/tx-power" | "/api/wfb/pair/auto-pair" => Some(Admin),
            "/api/vision/detector" | "/api/mavlink/signing/require" => Some(Admin),
            "/api/atlas/config" => Some(SafeWrite),
            p if is_plugin_config(p) => Some(Admin),
            p if p.starts_with("/api/v1/network/") => Some(Admin),
            p if p.starts_with("/api/v1/ground-station/") => Some(Admin),
            _ => None,
        },
        Method::DELETE => match path {
            "/api/vision/detector" => Some(Admin),
            p if p.starts_with("/api/v1/network/") => Some(Admin),
            p if p.starts_with("/api/v1/ground-station/") => Some(Admin),
            _ => None,
        },
        _ => None,
    }
}

/// `POST /api/params/{name}` — a single-segment param name after `/api/params/`.
fn is_param_write(path: &str) -> bool {
    match path.strip_prefix("/api/params/") {
        Some(rest) => !rest.is_empty() && !rest.contains('/'),
        None => false,
    }
}

/// `POST /api/services/{name}/restart`.
fn is_service_restart(path: &str) -> bool {
    match path
        .strip_prefix("/api/services/")
        .and_then(|r| r.strip_suffix("/restart"))
    {
        Some(name) => !name.is_empty() && !name.contains('/'),
        None => false,
    }
}

/// `PUT /api/plugins/{plugin_id}/config`.
fn is_plugin_config(path: &str) -> bool {
    match path
        .strip_prefix("/api/plugins/")
        .and_then(|r| r.strip_suffix("/config"))
    {
        Some(id) => !id.is_empty() && !id.contains('/'),
        None => false,
    }
}

/// Native `GET` routes whose response body carries a secret and therefore need the
/// `secret_read` scope rather than plain `read`. Empty today: no native GET returns
/// the pairing key, the WFB bind key, or a WS ticket (`/api/pairing/code` is public
/// and 409s when paired, `wfb/pair` returns only a fingerprint, `signing/counters`
/// returns zeros). Add any future secret-bearing GET here, or a plain `read` token
/// would reach it; this is a deliberate security surface, not a convenience list.
const SECRET_GET_ROUTES: &[&str] = &[];

/// Whether a native GET route returns a secret (needs `secret_read`, not `read`).
fn is_secret_get(path: &str) -> bool {
    SECRET_GET_ROUTES.contains(&path)
}

/// `POST /api/plugins/{plugin_id}/tools/{tool}/invoke` — a two-param template.
fn is_plugin_tool_invoke(path: &str) -> bool {
    let Some(rest) = path.strip_prefix("/api/plugins/") else {
        return false;
    };
    let Some(rest) = rest.strip_suffix("/invoke") else {
        return false;
    };
    // rest is `{plugin_id}/tools/{tool}`: exactly three non-empty segments with
    // the middle literal `tools`.
    let parts: Vec<&str> = rest.split('/').collect();
    parts.len() == 3 && parts[1] == "tools" && !parts[0].is_empty() && !parts[2].is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(dir: &Path) -> McpTokenStore {
        McpTokenStore::with_path(dir.join("mcp-token.json"))
    }

    fn paired() -> Pairing {
        Pairing::Paired("ados_secret".to_string())
    }

    /// A default mint request (`api_key = ados_secret`, node `node-1`), overriding
    /// only the scopes / TTL / mint-time a given test cares about.
    fn req(scopes: &[String], ttl_ms: i64, now_ms: i64) -> MintRequest<'_> {
        MintRequest {
            api_key: "ados_secret",
            label: "claude",
            operator_id: "cloud:usr_1",
            node_id: "node-1",
            scopes,
            allowed_nodes: &[],
            ttl_ms,
            now_secs: now_ms as f64 / 1000.0,
            now_ms,
        }
    }

    #[test]
    fn absent_file_mints_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        assert!(!s.any_minted());
        // Verify against an empty store is None (no salt to key with).
        assert!(s.verify(&paired(), "anything.here", 1, "node-1").is_none());
        let (tokens, revoked) = s.status();
        assert!(tokens.is_empty() && revoked.is_empty());
    }

    #[test]
    fn mint_then_verify_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        let scopes = ["read".to_string(), "admin".to_string()];
        let tok = s.mint(&req(&scopes, 3_600_000, 1_000_000_000_000)).unwrap();
        assert!(s.any_minted());
        let claims = s
            .verify(&paired(), &tok, 1_000_000_100_000, "node-1")
            .expect("a fresh token verifies");
        assert_eq!(claims.scopes, vec!["read", "admin"]);
        assert_eq!(claims.iss, "agent:node-1");
        // The registry records the mint (sans secret).
        let (tokens, _) = s.status();
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].label, "claude");
    }

    #[test]
    fn wrong_node_and_wrong_key_do_not_verify() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        let scopes = ["read".to_string()];
        let tok = s.mint(&req(&scopes, 3_600_000, 0)).unwrap();
        // A different node subject is rejected.
        assert!(s.verify(&paired(), &tok, 1_000, "node-2").is_none());
        // A different pairing key is rejected.
        assert!(s
            .verify(&Pairing::Paired("other".into()), &tok, 1_000, "node-1")
            .is_none());
        // Unpaired never verifies (data plane is open anyway).
        assert!(s
            .verify(&Pairing::Unpaired, &tok, 1_000, "node-1")
            .is_none());
    }

    #[test]
    fn expired_token_does_not_verify() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        // TTL 1000 ms from mint-time 0 -> expires at 1000.
        let scopes = ["read".to_string()];
        let tok = s.mint(&req(&scopes, 1_000, 0)).unwrap();
        assert!(s.verify(&paired(), &tok, 999, "node-1").is_some());
        assert!(s.verify(&paired(), &tok, 1_000, "node-1").is_none());
    }

    #[test]
    fn revoke_denylists_one_token() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        let scopes = ["read".to_string()];
        let tok = s.mint(&req(&scopes, 3_600_000, 0)).unwrap();
        let (tokens, _) = s.status();
        let id = tokens[0].token_id.clone();
        assert!(s.verify(&paired(), &tok, 1_000, "node-1").is_some());
        s.revoke(&id).unwrap();
        assert!(
            s.verify(&paired(), &tok, 1_000, "node-1").is_none(),
            "a revoked token id no longer verifies"
        );
        // Revoking an unknown id is a no-op success.
        s.revoke("mct_unknown").unwrap();
    }

    #[test]
    fn revoke_all_rotates_and_kills_prior_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        let scopes = ["read".to_string()];
        let tok = s.mint(&req(&scopes, 3_600_000, 0)).unwrap();
        assert!(s.verify(&paired(), &tok, 1_000, "node-1").is_some());
        s.revoke_all().unwrap();
        assert!(!s.any_minted());
        assert!(
            s.verify(&paired(), &tok, 1_000, "node-1").is_none(),
            "revoke_all rotates the salt so the prior token dies"
        );
        // A fresh mint installs a new salt + verifies again.
        let tok2 = s.mint(&req(&scopes, 3_600_000, 0)).unwrap();
        assert!(s.verify(&paired(), &tok2, 1_000, "node-1").is_some());
        assert_ne!(tok, tok2, "a new salt yields a different token");
    }

    #[test]
    fn concurrent_mints_do_not_lose_writes() {
        // Serialized load-modify-write + a unique temp file per write means every
        // concurrent mint is recorded (an unlocked RMW sharing one temp name would
        // drop writes). 8 threads mint against one store; all 8 must land.
        use std::sync::Arc;
        let dir = tempfile::tempdir().unwrap();
        let s = Arc::new(store(dir.path()));
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let s = s.clone();
                std::thread::spawn(move || {
                    let scopes = ["read".to_string()];
                    s.mint(&req(&scopes, 3_600_000, 1_000_000 + i)).unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let (tokens, _) = s.status();
        assert_eq!(tokens.len(), 8, "every concurrent mint is recorded");
    }

    #[test]
    fn mint_rejects_unknown_scope() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        let scopes = ["read".to_string(), "root".to_string()];
        let err = s.mint(&req(&scopes, 1, 0)).unwrap_err();
        assert!(matches!(err, MintError::UnknownScope(s) if s == "root"));
    }

    #[cfg(unix)]
    #[test]
    fn record_is_0600() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        let scopes = ["read".to_string()];
        s.mint(&req(&scopes, 1, 0)).unwrap();
        let mode = std::fs::metadata(s.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "mcp-token.json must be 0600");
    }

    // ---- route_scope ----

    #[test]
    fn reads_require_read() {
        for p in [
            "/api/status",
            "/api/telemetry",
            "/api/params",
            "/api/params/RC1_MIN",
            "/api/services",
            "/api/wfb",
            "/api/fleet/peers",
            "/api/mcp/status",
        ] {
            assert_eq!(route_scope(&Method::GET, p), Some(ScopeClass::Read), "{p}");
        }
    }

    #[test]
    fn command_route_requires_flight() {
        // The raw command passthrough can arm/fly the vehicle -> flight scope.
        assert_eq!(
            route_scope(&Method::POST, "/api/command"),
            Some(ScopeClass::Flight)
        );
        // CAN passthrough injects frames to the FC/ESCs/servos -> also flight.
        assert_eq!(
            route_scope(&Method::POST, "/api/can/passthrough"),
            Some(ScopeClass::Flight)
        );
    }

    #[test]
    fn secret_read_class_gates_on_the_secret_read_scope() {
        // A read-only token cannot reach a secret-read route; a secret_read token can.
        assert_eq!(ScopeClass::SecretRead.group_name(), "secret_read");
        assert!(!ados_protocol::mcp_token::scope_allows_class(
            ScopeClass::SecretRead,
            &["read".to_string()]
        ));
        assert!(ados_protocol::mcp_token::scope_allows_class(
            ScopeClass::SecretRead,
            &["read".to_string(), "secret_read".to_string()]
        ));
    }

    #[test]
    fn no_native_get_is_secret_bearing_today() {
        // GET->Read is a blanket, so any GET that returns a secret MUST be listed
        // as secret_read or a plain `read` token reaches it. This guards the
        // invariant: adding a secret-bearing GET without classifying it fails here.
        assert!(
            SECRET_GET_ROUTES.is_empty(),
            "classify any secret-bearing native GET as secret_read"
        );
        // A representative native GET stays plain read.
        assert_eq!(
            route_scope(&Method::GET, "/api/status"),
            Some(ScopeClass::Read)
        );
    }

    #[test]
    fn writes_map_to_their_class() {
        assert_eq!(
            route_scope(&Method::POST, "/api/params/ARMING_CHECK"),
            Some(ScopeClass::Admin)
        );
        assert_eq!(
            route_scope(&Method::POST, "/api/services/ados-mavlink/restart"),
            Some(ScopeClass::Admin)
        );
        assert_eq!(
            route_scope(&Method::POST, "/api/pairing/unpair"),
            Some(ScopeClass::Destructive)
        );
        assert_eq!(
            route_scope(&Method::POST, "/api/vision/designate"),
            Some(ScopeClass::SafeWrite)
        );
        assert_eq!(
            route_scope(&Method::POST, "/api/atlas/capture/start"),
            Some(ScopeClass::SafeWrite)
        );
        assert_eq!(
            route_scope(&Method::PUT, "/api/atlas/config"),
            Some(ScopeClass::SafeWrite)
        );
        assert_eq!(
            route_scope(&Method::PUT, "/api/plugins/battery-panel/config"),
            Some(ScopeClass::Admin)
        );
        assert_eq!(
            route_scope(&Method::PUT, "/api/v1/ground-station/wfb"),
            Some(ScopeClass::Admin)
        );
        assert_eq!(
            route_scope(&Method::POST, "/api/_ws/ticket"),
            Some(ScopeClass::Admin)
        );
        // A plugin tool invoke floors at admin (the connector gates the fine class).
        assert_eq!(
            route_scope(&Method::POST, "/api/plugins/com.x.p/tools/greet/invoke"),
            Some(ScopeClass::Admin)
        );
        // A plugin path that is NOT the tool-invoke shape stays fail-closed.
        assert_eq!(
            route_scope(&Method::POST, "/api/plugins/com.x.p/install"),
            None
        );
    }

    #[test]
    fn unclassified_writes_fail_closed() {
        // An unknown write path is DENIED to a token (must use the full key).
        assert_eq!(route_scope(&Method::POST, "/api/does-not-exist"), None);
        assert_eq!(route_scope(&Method::POST, "/api/mcp/tokens"), None); // mint is on-box/key only
        assert_eq!(route_scope(&Method::PUT, "/api/does-not-exist"), None);
        assert_eq!(route_scope(&Method::DELETE, "/api/does-not-exist"), None);
        // A trailing-slash / multi-segment name is not a param write.
        assert_eq!(route_scope(&Method::POST, "/api/params/"), None);
        assert_eq!(route_scope(&Method::POST, "/api/params/a/b"), None);
    }
}
