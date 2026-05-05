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
use crate::wfb_handlers::{self, SharedWfbManager};

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

/// WFB-ng broadcast-config sub-router (3 routes). Mounted alongside
/// the universal setup routes so the same-origin gate + body limit
/// cover them uniformly. Stays in its own `Router::new()` because the
/// shared manager is delivered via `Extension`, not `with_state`.
fn wfb_routes() -> Router<Arc<SetupState>> {
    Router::new()
        .route("/api/v1/setup/wfb", get(wfb_handlers::get_wfb))
        .route(
            "/api/v1/setup/wfb/configure",
            post(wfb_handlers::post_wfb_configure),
        )
        .route(
            "/api/v1/setup/wfb/regenerate-key",
            post(wfb_handlers::post_wfb_regenerate_key),
        )
}

/// Unauthenticated probe (`/api/v1/health`) that lives OUTSIDE the
/// same-origin gate so monitoring agents and SREs can hit it from
/// neighbouring hosts without forging an `Origin` header. The handler
/// is GET-only and returns `{status, version}` with no operator state.
fn operability_routes() -> Router<Arc<SetupState>> {
    Router::new().route("/api/v1/health", get(handlers::get_health))
}

/// Diagnostic dump (`/api/v1/diag`) — kept in its own router block so
/// it can be merged INTO the same-origin gated branch alongside the
/// `/api/v1/setup/*` routes. The endpoint surfaces broker URL, Convex
/// URL, device_id, paired bool, MAVLink port, RSS, uptime, and
/// `consecutive_failures`. Each field is non-secret per spec but
/// collectively they are reconnaissance for a targeted attack, so a
/// browser on the same LAN should not be able to scrape them via a
/// hostile page. Native callers without an `Origin` header (curl, SRE
/// scripts, monitoring agents) still pass through — see
/// `check_origin` for the pass-through contract.
fn diag_routes() -> Router<Arc<SetupState>> {
    Router::new().route("/api/v1/diag", get(handlers::get_diag))
}

pub fn setup_router(state: Arc<SetupState>) -> Router {
    setup_router_with_diag(state, DiagState::shared())
}

/// Variant of [`setup_router`] that includes the WFB-ng routes. The
/// production wiring uses this when the agent has a `WfbManager` to
/// share; tests can call [`setup_router`] alone when they want the
/// pre-MSN-056 surface.
pub fn setup_router_with_wfb(
    state: Arc<SetupState>,
    wfb_manager: SharedWfbManager,
) -> Router {
    setup_router_with_wfb_and_diag(state, wfb_manager, DiagState::shared())
}

/// Variant accepting both the diag handle and the wfb manager. Wires
/// the `Extension(SharedWfbManager)` layer onto the wfb sub-router
/// before merging it with the rest so the wfb handlers see the right
/// extractor.
pub fn setup_router_with_wfb_and_diag(
    state: Arc<SetupState>,
    wfb_manager: SharedWfbManager,
    diag: Arc<DiagState>,
) -> Router {
    let api = api_routes()
        .merge(wfb_routes().layer(Extension(wfb_manager)))
        .merge(operability_routes())
        .merge(diag_routes());
    api.fallback(get(webapp::serve_request))
        .with_state(state)
        .layer(Extension(diag))
}

/// Same as `setup_router` but accepts an externally-constructed
/// [`DiagState`] so the agent binary can also share the handle with
/// other tasks (cloud-relay heartbeat counters, etc.).
pub fn setup_router_with_diag(state: Arc<SetupState>, diag: Arc<DiagState>) -> Router {
    // No origin gate variant: every route is exposed without a same-
    // origin check. `/api/v1/diag` is merged in here alongside the
    // setup routes and `/api/v1/health` so callers see no behavioural
    // difference from the pre-gate world.
    let api = api_routes()
        .merge(operability_routes())
        .merge(diag_routes());
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
/// `/api/v1/setup/*` routes plus `/api/v1/diag`. The middleware also
/// recognizes WebSocket upgrade handshakes (HTTP GET +
/// `Upgrade: websocket`) and gates them on the same allowlist so a
/// hostile page on the LAN cannot open
/// `/api/v1/setup/cloudflare/logs` from a foreign origin.
///
/// Pass-through (no allowlist check):
/// - All other reads under `/api/v1/setup/*` (`status`, etc.).
/// - Any request with no `Origin` header — curl, native SDKs, the
///   wizard webapp's own no-CORS fetches.
/// - `/api/v1/health` — the SRE liveness probe lives outside the gate
///   so monitoring agents on neighbouring hosts can poll it without
///   forging an `Origin` header.
///
/// Reject (HTTP 403):
/// - Any of the gated classes (mutating method, WebSocket upgrade, or
///   GET on `/api/v1/diag`) with an `Origin` header that is not in
///   the allowlist.
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
    // Setup routes + diag share the same-origin gate. The middleware
    // is aware of three gated classes (mutating methods, WebSocket
    // upgrades, GET on /api/v1/diag) and the diag handler lives inside
    // this layered group so the path-based check in the middleware
    // matches a real route.
    let gated = api_routes()
        .merge(diag_routes())
        .layer(middleware::from_fn_with_state(allowlist, check_origin));
    // Health stays UNGATED so SRE liveness probes from neighbouring
    // hosts work without an Origin header.
    gated
        .merge(operability_routes())
        .fallback(get(webapp::serve_request))
        .with_state(state)
        .layer(Extension(diag))
}

/// Variant that bundles the same-origin gate, the diag handle, and
/// the WFB-ng manager. This is the production wiring the agent binary
/// uses once the MSN-056 wfb crate is wired in.
pub fn setup_router_with_origin_check_diag_and_wfb(
    state: Arc<SetupState>,
    allowlist: Arc<OriginAllowlist>,
    diag: Arc<DiagState>,
    wfb_manager: SharedWfbManager,
) -> Router {
    // Wfb routes ride alongside the rest of the setup routes inside
    // the gated branch, so a hostile page on the LAN cannot POST a
    // fresh passphrase from a foreign origin. The handlers consume the
    // shared manager via `Extension(SharedWfbManager)`.
    let gated = api_routes()
        .merge(wfb_routes().layer(Extension(wfb_manager)))
        .merge(diag_routes())
        .layer(middleware::from_fn_with_state(allowlist, check_origin));
    gated
        .merge(operability_routes())
        .fallback(get(webapp::serve_request))
        .with_state(state)
        .layer(Extension(diag))
}
