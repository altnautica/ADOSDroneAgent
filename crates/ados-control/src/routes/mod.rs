//! The route surface: one axum `Router` served on both listeners.
//!
//! The native control surface answers the same `/api/*` (+ `/healthz`) routes
//! the FastAPI surface does, byte-identically, so the same GCS works against
//! either. This foundation chunk registers only `/healthz` and `/api/version`;
//! the pairing/status/command routes land in later chunks.
//!
//! Error bodies use FastAPI's `{"detail": "..."}` shape on 4xx/5xx, NOT the
//! logd read-API's `{"error": {...}}` envelope, because the GCS already parses
//! the agent's `{"detail"}` errors. The 404 fallback and the [`detail`] helper
//! enforce that one shape everywhere on this surface.

pub mod system;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;

use crate::state::AppState;

/// Build a FastAPI-shaped error response: `(status, {"detail": message})`. Every
/// 4xx/5xx on this surface goes through this so the body shape never drifts to
/// the logd `{"error":{...}}` envelope. Used by the routes that land in later
/// chunks (pairing 409s, command 503/400) as well as the 404 fallback.
pub fn detail(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "detail": message.into() }))).into_response()
}

/// The 404 handler returns the FastAPI `{"detail"}` shape, matching how the
/// agent's API answers an unknown path.
async fn not_found() -> Response {
    detail(StatusCode::NOT_FOUND, "Not Found")
}

/// Build the route Router for a given app state. The same Router is served on
/// both edges; the auth/rate-limit layer is added per edge by the serve loop.
/// `/healthz` sits at the root; everything else is mounted under `/api`.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(system::healthz))
        .route("/api/version", get(system::get_version))
        .fallback(not_found)
        .with_state(state)
}
