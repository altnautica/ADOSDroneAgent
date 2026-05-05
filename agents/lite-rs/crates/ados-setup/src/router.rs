//! Assemble the axum::Router for the universal setup REST surface.
//!
//! `setup_router(state)` returns a Router scoped under `/api/v1/setup`
//! that the agent main binary mounts onto its top-level Router. Every
//! route documented in `proto/setup/setup-api.yaml` is wired here.

use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    routing::{get, post},
    Router,
};

use crate::handlers;
use crate::state::StateStore;
use crate::webapp;

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

pub fn setup_router(state: Arc<SetupState>) -> Router {
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
        // Fallback: any non-API path serves the embedded webapp. The
        // HTML uses absolute paths (/app.js, /style.css, /brand.svg)
        // so we mount the static webapp at the root, matching the
        // Python full agent's StaticFiles behavior.
        .fallback(get(webapp::serve_request))
        .with_state(state)
}
