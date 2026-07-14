//! MCP-token management routes: `/api/mcp/{tokens,status,revoke}`.
//!
//! The AI-control connector (`ADOS-MCP` in agent-mode) may present a scoped,
//! revocable token instead of the full pairing key. These routes mint, list, and
//! revoke those tokens.
//!
//! Auth posture:
//! - **mint** (`POST /api/mcp/tokens`) issues a credential, so it is authorized IN
//!   THE HANDLER to on-box OR a valid `X-ADOS-Key` (the same audience that already
//!   holds the key). An MCP token can never reach mint — [`crate::mcp::route_scope`]
//!   maps `POST /api/mcp/tokens` to `None`, so the edge denies a token for it.
//! - **status** (`GET`) and **revoke** (`POST`) ride the normal edge gate; status
//!   returns only token metadata (no secret), revoke needs the `admin` scope class.
//!
//! The LAN-edge acceptance of a minted token is a separate opt-in: the
//! `mcp.token_accept_enabled` config flag (default off), which `status` reports so
//! an operator sees whether a minted token would actually be honored yet.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use ados_protocol::pairing_posture::{constant_time_eq, Pairing};

use crate::config::{ControlSecurityConfig, PairingConfig};
use crate::mcp::{MintError, MintRequest};
use crate::routes::detail;
use crate::serve::ONBOX_HEADER;
use crate::state::AppState;

/// Default token lifetime (ms) when the mint body omits `ttl_ms`: 30 days.
const DEFAULT_TTL_MS: i64 = 30 * 24 * 60 * 60 * 1000;
/// Upper bound on a requested TTL (ms): 365 days.
const MAX_TTL_MS: i64 = 365 * 24 * 60 * 60 * 1000;

/// `GET /api/mcp/status` → the accept-flag posture + the minted-token registry
/// (ids/labels/scopes/expiry + revoked/expired flags). No secret is returned.
pub async fn get_mcp_status(State(state): State<AppState>) -> Json<Value> {
    let accept_enabled = ControlSecurityConfig::load_from(&state.pairing_paths.config)
        .mcp
        .token_accept_enabled;
    let (tokens, revoked) = state.mcp_tokens.status();
    let now_ms = now_unix_ms();
    let list: Vec<Value> = tokens
        .into_iter()
        .map(|t| {
            let is_revoked = revoked.iter().any(|r| r == &t.token_id);
            json!({
                "token_id": t.token_id,
                "label": t.label,
                "scopes": t.scopes,
                "allowed_nodes": t.allowed_nodes,
                "created_at": t.created_at,
                "expires_at": t.expires_at,
                "revoked": is_revoked,
                "expired": t.expires_at <= now_ms,
            })
        })
        .collect();
    Json(json!({
        "accept_enabled": accept_enabled,
        "any_minted": state.mcp_tokens.any_minted(),
        "tokens": list,
    }))
}

/// The `POST /api/mcp/tokens` body.
#[derive(Deserialize)]
pub struct MintBody {
    #[serde(default)]
    pub label: String,
    pub scopes: Vec<String>,
    #[serde(default)]
    pub allowed_nodes: Vec<String>,
    #[serde(default)]
    pub operator_id: Option<String>,
    #[serde(default)]
    pub ttl_ms: Option<i64>,
}

/// `POST /api/mcp/tokens` → mint a scoped token. Authorized to on-box OR a valid
/// `X-ADOS-Key`. Returns `{token, expires_at}`; the token is shown ONCE.
pub async fn mint_mcp_token(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<MintBody>,
) -> Response {
    let pairing = state.pairing.current();
    // The edge stamps a trustworthy on-box header (stripped-then-set).
    let on_box = header(&headers, ONBOX_HEADER).as_deref() == Some("1");
    let key_valid = match (&pairing, header(&headers, "x-ados-key")) {
        (Pairing::Paired(k), Some(key)) => constant_time_eq(key.as_bytes(), k.as_bytes()),
        _ => false,
    };
    if !on_box && !key_valid {
        return detail(
            StatusCode::FORBIDDEN,
            "Minting an MCP token requires on-box access or a valid X-ADOS-Key.",
        );
    }
    let api_key = match &pairing {
        Pairing::Paired(k) => k.clone(),
        // Unpaired: the data plane is open, so a scoped token adds nothing and
        // there is no key to derive from. Refuse rather than mint against nothing.
        Pairing::Unpaired => {
            return detail(
                StatusCode::CONFLICT,
                "Agent is unpaired; pair first, then mint a scoped MCP token.",
            )
        }
    };
    if body.scopes.is_empty() {
        return detail(StatusCode::BAD_REQUEST, "At least one scope is required.");
    }
    let cfg = PairingConfig::load_from(&state.pairing_paths.config);
    let node_id = cfg.agent.device_id.clone();
    let operator_id = body.operator_id.unwrap_or_else(|| "local".to_string());
    let ttl_ms = body.ttl_ms.unwrap_or(DEFAULT_TTL_MS).clamp(1, MAX_TTL_MS);
    let now_ms = now_unix_ms();
    let req = MintRequest {
        api_key: &api_key,
        label: &body.label,
        operator_id: &operator_id,
        node_id: &node_id,
        scopes: &body.scopes,
        allowed_nodes: &body.allowed_nodes,
        ttl_ms,
        now_secs: now_ms as f64 / 1000.0,
        now_ms,
    };
    match state.mcp_tokens.mint(&req) {
        Ok(token) => Json(json!({
            "token": token,
            "expires_at": now_ms.saturating_add(ttl_ms),
        }))
        .into_response(),
        Err(MintError::UnknownScope(s)) => {
            detail(StatusCode::BAD_REQUEST, format!("Unknown scope group: {s}"))
        }
        Err(e) => {
            tracing::error!(error = %e, "mcp token mint failed");
            detail(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to mint MCP token",
            )
        }
    }
}

/// The `POST /api/mcp/revoke` body: revoke one id, or all.
#[derive(Deserialize)]
pub struct RevokeBody {
    #[serde(default)]
    pub token_id: Option<String>,
    #[serde(default)]
    pub all: bool,
}

/// `POST /api/mcp/revoke` → revoke one token by id, or all (salt rotation).
pub async fn revoke_mcp_token(
    State(state): State<AppState>,
    Json(body): Json<RevokeBody>,
) -> Response {
    let result = if body.all {
        state.mcp_tokens.revoke_all()
    } else if let Some(id) = body.token_id.as_deref().filter(|s| !s.is_empty()) {
        state.mcp_tokens.revoke(id)
    } else {
        return detail(
            StatusCode::BAD_REQUEST,
            "Provide a token_id to revoke, or all=true.",
        );
    };
    match result {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "mcp token revoke failed");
            detail(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to revoke MCP token",
            )
        }
    }
}

/// A header value as an owned `String`, `None` when absent or non-UTF-8.
fn header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

/// Wall-clock unix milliseconds.
fn now_unix_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
