//! The route surface: one axum `Router` served on both listeners.
//!
//! The native control surface answers the same `/api/*` (+ `/healthz`) routes
//! the FastAPI surface does, byte-identically, so the same GCS works against
//! either. This surface registers `/healthz`, `/api/version`, `/api/status`,
//! `/api/telemetry`, `/api/time`, the four `/api/pairing/*` routes, and the two
//! `/api/command{,s}` routes.
//!
//! Error bodies use FastAPI's `{"detail": "..."}` shape on 4xx/5xx, NOT the
//! logd read-API's `{"error": {...}}` envelope, because the GCS already parses
//! the agent's `{"detail"}` errors. The 404 fallback and the [`detail`] helper
//! enforce that one shape everywhere on this surface.

pub mod command;
pub mod pairing;
pub mod status;
pub mod system;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::json;

use crate::proxy::proxy_to_residual;
use crate::state::AppState;

/// Build a FastAPI-shaped error response: `(status, {"detail": message})`. Every
/// 4xx/5xx on this surface goes through this so the body shape never drifts to
/// the logd `{"error":{...}}` envelope. Used by the routes that land in later
/// chunks (pairing 409s, command 503/400) as well as the proxy's
/// graceful-degradation reply.
pub fn detail(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "detail": message.into() }))).into_response()
}

/// Build the route Router for a given app state. The same Router is served on
/// both edges; the auth/rate-limit layer is added per edge by the serve loop.
/// `/healthz` sits at the root; everything else is mounted under `/api`.
///
/// Any path not registered here falls through to the reverse-proxy fallback,
/// which forwards it to the residual Python over its internal Unix socket (and
/// degrades cleanly to a FastAPI-shaped `{"detail"}` when that upstream is
/// absent), so the front serves the migrated routes natively and proxies the
/// rest while the migration is in flight.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(system::healthz))
        .route("/api/version", get(system::get_version))
        .route("/api/status", get(status::get_status))
        .route("/api/telemetry", get(status::get_telemetry))
        .route("/api/time", get(system::get_time))
        // Pairing: the node-identity probe + the local pairing handshake. info /
        // code / claim are public (the auth-exempt set); unpair requires the key.
        .route("/api/pairing/info", get(pairing::get_pairing_info))
        .route("/api/pairing/code", get(pairing::get_pairing_code))
        .route("/api/pairing/claim", post(pairing::claim_pairing))
        .route("/api/pairing/unpair", post(pairing::unpair))
        // Command: the fire-and-forget text-command executor (auth-gated when
        // paired) + the catalog. The executor builds a MAVLink frame and writes
        // it to the mavlink socket; the catalog is the static command list.
        .route("/api/command", post(command::execute_command))
        .route("/api/commands", get(command::list_commands))
        // Everything else: reverse-proxy to the residual Python.
        .fallback(proxy_to_residual)
        .with_state(state)
}
