//! Assemble the axum::Router for the universal setup REST surface.
//!
//! `setup_router(state)` returns a Router scoped under `/api/v1/setup`
//! that the agent main binary mounts onto its top-level Router. Every
//! route documented in `proto/setup/setup-api.yaml` is wired here.
//!
//! `setup_router_with_origin_check(state, allowlist)` returns the same
//! Router with a same-origin gate layered onto the `/api/v1/setup/*`
//! routes only. The webapp fallback (`/`, `/setup/*`, `/app.js`,
//! `/style.css`, etc.) is not gated since static asset serving on
//! safe methods has nothing to mutate.

use std::sync::Arc;

use axum::{
    middleware,
    routing::{get, post},
    Extension, Router,
};

use crate::diag::DiagState;
use crate::handlers;
use crate::origin::{check_origin, OriginAllowlist};
use crate::state::StateStore;
use crate::webapp;

use std::path::PathBuf;

/// State carried by the axum handlers. Holds the agent.yaml path, the
/// persistent setup-state store, and a snapshot function the handlers
/// invoke to build the canonical SetupStatus response.
pub struct SetupState {
    pub agent_yaml: PathBuf,
    pub store: StateStore,
    /// Function the handlers call to build the canonical SetupStatus
    /// response. Implemented by the agent binary so the status surface
    /// can read live state (paired/unpaired, mavlink port, etc.) without
    /// this crate depending on the binary. Sync because the snapshot is
    /// a cheap read of a few fields; if a future revision needs to await
    /// (e.g. probing FC link state), wrap with `tokio::task::spawn_blocking`
    /// at the call site.
    pub status_builder:
        Box<dyn Fn() -> serde_json::Value + Send + Sync>,
}

impl SetupState {
    pub async fn snapshot_status(&self) -> serde_json::Value {
        (self.status_builder)()
    }
}

/// Build the API-only router (the 11 routes documented in
/// `proto/setup/setup-api.yaml`). Used internally by both
/// `setup_router` and `setup_router_with_origin_check` so the route
/// list stays single-source.
fn api_routes() -> Router<Arc<SetupState>> {
    Router::new()
        .route("/api/v1/setup/status", get(handlers::get_status))
        .route("/api/v1/setup/profile", post(handlers::post_profile))
        .route("/api/v1/setup/hardware-check", get(handlers::get_hardware_check))
        .route(
            "/api/v1/setup/hardware-check/refresh",
            post(handlers::post_hardware_check_refresh),
        )
        .route("/api/v1/setup/cloud-choice", post(handlers::post_cloud_choice))
        .route(
            "/api/v1/setup/remote-access/cloudflare",
            post(handlers::post_cloudflare_install),
        )
        .route(
            "/api/v1/setup/cloudflare/verify",
            get(handlers::get_cloudflare_verify),
        )
        .route(
            "/api/v1/setup/cloudflare/logs",
            get(handlers::ws_cloudflare_logs),
        )
        .route("/api/v1/setup/finish", post(handlers::post_finish))
        .route(
            "/api/v1/setup/step/:step_id/skip",
            post(handlers::post_skip),
        )
        .route("/api/v1/setup/reset", post(handlers::post_reset))
}

/// Operability routes (`/api/v1/health`, `/api/v1/diag`) that live
/// OUTSIDE `/api/v1/setup/*`. The same-origin gate is intentionally
/// not applied here so monitoring agents and SREs can hit the probes
/// from neighbouring hosts without forging an `Origin` header. Both
/// handlers are GET and return read-only state with no secrets.
fn operability_routes() -> Router<Arc<SetupState>> {
    Router::new()
        .route("/api/v1/health", get(handlers::get_health))
        .route("/api/v1/diag", get(handlers::get_diag))
}

pub fn setup_router(state: Arc<SetupState>) -> Router {
    setup_router_with_diag(state, DiagState::shared())
}

/// Same as `setup_router` but accepts an externally-constructed
/// [`DiagState`] so the agent binary can also share the handle with
/// other tasks (cloud-relay heartbeat counters, etc.).
pub fn setup_router_with_diag(state: Arc<SetupState>, diag: Arc<DiagState>) -> Router {
    let api = api_routes().merge(operability_routes());
    api
        // Fallback: any non-API path serves the embedded webapp. The
        // HTML uses absolute paths (/app.js, /style.css, /brand.svg)
        // so we mount the static webapp at the root, matching the
        // Python full agent's StaticFiles behavior.
        .fallback(get(webapp::serve_request))
        .with_state(state)
        .layer(Extension(diag))
}

/// Same as `setup_router` but layers a same-origin gate on the
/// `/api/v1/setup/*` routes. Mutating requests (POST / PUT / PATCH /
/// DELETE) whose `Origin` header is outside the allowlist are
/// rejected with HTTP 403. Read methods and missing-header requests
/// pass through unchanged.
///
/// `/api/v1/health` and `/api/v1/diag` live outside the gate so
/// monitoring agents on neighbouring hosts can hit them without
/// forging an `Origin` header.
pub fn setup_router_with_origin_check(
    state: Arc<SetupState>,
    allowlist: Arc<OriginAllowlist>,
) -> Router {
    setup_router_with_origin_check_and_diag(state, allowlist, DiagState::shared())
}

/// Variant of [`setup_router_with_origin_check`] that lets the agent
/// binary share its own [`DiagState`] with both the diag handler and
/// the cloud / MAVLink tasks that update its counters.
pub fn setup_router_with_origin_check_and_diag(
    state: Arc<SetupState>,
    allowlist: Arc<OriginAllowlist>,
    diag: Arc<DiagState>,
) -> Router {
    let gated_api =
        api_routes().layer(middleware::from_fn_with_state(allowlist, check_origin));
    // Operability routes are merged AFTER the gate is applied so the
    // gate does not extend over them.
    gated_api
        .merge(operability_routes())
        .fallback(get(webapp::serve_request))
        .with_state(state)
        .layer(Extension(diag))
}
