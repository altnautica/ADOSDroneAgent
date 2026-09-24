//! Owner routes on the job API for the credentials this node issues:
//!
//! - `POST /api/compute/node-credentials` `{ peer_device_id, lanes? }` issues a
//!   credential for another node (every lane by default), replacing any earlier
//!   one for that peer, and returns the secret once.
//! - `GET /api/compute/node-credentials` lists what was issued (no secrets).
//! - `POST /api/compute/node-credentials/:id/revoke` revokes one.
//! - `POST /api/compute/ws-ticket` `{ scope }` mints the short-lived ticket a
//!   browser offers to open the world-model stream.
//!
//! All four sit behind [`crate::auth::require_job_api`] with no node lane, so
//! only the owner (or the on-box operator) reaches them: a node credential can
//! never issue another.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::Deserialize;

use ados_protocol::node_credential::NodeLane;
use ados_protocol::pairing_posture::Pairing;
use ados_protocol::ws_ticket::{WsTicketIssuer, DEFAULT_TTL_SECONDS, SCOPE_ATLAS_WORLD_WS};

use crate::auth::ComputeAuth;
use crate::node_credentials::CredentialError;

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(serde_json::json!({ "error": message }))).into_response()
}

/// The owner key a credential or ticket is bound to, or the status and reason
/// there is none.
fn owner_key(auth: &ComputeAuth) -> Result<String, (StatusCode, &'static str)> {
    match auth.gate.current() {
        Pairing::Paired(key) => Ok(key),
        Pairing::Unpaired => Err((
            StatusCode::CONFLICT,
            "this node is not paired; pair it before issuing credentials",
        )),
        Pairing::Unreadable => Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "the pairing state on this node is unreadable",
        )),
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct MintRequest {
    peer_device_id: String,
    #[serde(default)]
    lanes: Option<Vec<NodeLane>>,
}

pub(crate) async fn mint(
    Extension(auth): Extension<Arc<ComputeAuth>>,
    Json(req): Json<MintRequest>,
) -> Response {
    let key = match owner_key(&auth) {
        Ok(k) => k,
        Err((status, why)) => return error(status, why),
    };
    let lanes = req.lanes.unwrap_or_else(|| NodeLane::ALL.to_vec());
    // The store fsyncs its file; keep that off the async workers.
    let minted = tokio::task::spawn_blocking(move || {
        auth.credentials
            .mint(&req.peer_device_id, &lanes, &key, now_ms())
    })
    .await;
    match minted {
        Ok(Ok(m)) => (StatusCode::CREATED, Json(m)).into_response(),
        Ok(Err(e @ (CredentialError::BadPeer | CredentialError::NoLanes))) => {
            error(StatusCode::BAD_REQUEST, &e.to_string())
        }
        Ok(Err(e)) => {
            tracing::error!(error = %e, "node credential issue failed");
            error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string())
        }
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

pub(crate) async fn list(Extension(auth): Extension<Arc<ComputeAuth>>) -> Response {
    let key = match auth.gate.current() {
        Pairing::Paired(k) => Some(k),
        Pairing::Unpaired | Pairing::Unreadable => None,
    };
    Json(serde_json::json!({
        "workstation_node_id": auth.credentials.node_id(),
        "credentials": auth.credentials.list(key.as_deref()),
    }))
    .into_response()
}

pub(crate) async fn revoke(
    Extension(auth): Extension<Arc<ComputeAuth>>,
    Path(id): Path<String>,
) -> Response {
    match tokio::task::spawn_blocking(move || auth.credentials.revoke(&id)).await {
        Ok(Ok(revoked)) => Json(serde_json::json!({ "revoked": revoked })).into_response(),
        Ok(Err(e)) => {
            // Revoked in memory, but it would come back after a restart.
            tracing::error!(error = %e, "node credential revoke not persisted");
            error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string())
        }
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct TicketRequest {
    scope: String,
}

pub(crate) async fn mint_ws_ticket(
    Extension(auth): Extension<Arc<ComputeAuth>>,
    Json(req): Json<TicketRequest>,
) -> Response {
    if req.scope != SCOPE_ATLAS_WORLD_WS {
        return error(StatusCode::BAD_REQUEST, "unknown ticket scope");
    }
    let key = match owner_key(&auth) {
        Ok(k) => k,
        Err((status, why)) => return error(status, why),
    };
    let t = WsTicketIssuer::from_api_key(&key).mint(&req.scope, DEFAULT_TTL_SECONDS);
    Json(serde_json::json!({
        "ticket": t.token,
        "scope": t.scope,
        "expires_at": t.expires_at,
    }))
    .into_response()
}
