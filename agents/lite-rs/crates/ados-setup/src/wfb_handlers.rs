//! Axum handlers for the WFB-ng broadcast configuration surface.
//!
//! Three routes from `proto/setup/setup-api.yaml`:
//!
//! - `GET  /api/v1/setup/wfb`               — current state + config
//! - `POST /api/v1/setup/wfb/configure`     — channel/MCS/power/passphrase
//! - `POST /api/v1/setup/wfb/regenerate-key` — fresh keypair, public hex
//!
//! All three respect the same-origin gate the rest of the setup surface
//! uses. The shared `WfbManager` lives behind an `Arc<Mutex<...>>` so
//! the agent's orchestration loop and the REST handlers can both
//! consume it; the manager itself uses fine-grained internal locking
//! so a long-held outer lock is not required.
//!
//! Test pattern: each handler accepts the manager handle as an axum
//! `Extension` so the conformance tests can build a bare `Router` from
//! `wfb_router_only(manager)` without standing up the full
//! `SetupState`. The production wiring in `router.rs` merges this
//! sub-router into the main setup surface so the same-origin gate
//! covers the routes uniformly.

use std::sync::Arc;

use ados_wfb::{
    derive_key, key_fingerprint, regenerate_public_key_hex, WfbConfig, WfbError, WfbManager,
};
use axum::extract::Extension;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::Router;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::atomic::ensure_secret_dir;

/// Type alias for the shared manager handle. Same shape across the
/// agent binary and the REST handlers so a future refactor that adds
/// instrumentation is a one-line change.
pub type SharedWfbManager = Arc<Mutex<WfbManager>>;

/// Request body for `POST /api/v1/setup/wfb/configure`. Mirrors the
/// `WfbConfigureRequest` schema in the OpenAPI spec.
#[derive(Debug, Deserialize)]
pub struct WfbConfigureRequest {
    pub channel: u8,
    pub mcs_index: u8,
    pub tx_power_dbm: i8,
    pub key_passphrase: String,
}

/// Response body for `POST /api/v1/setup/wfb/configure`. Mirrors the
/// `WfbConfigureResponse` schema in the OpenAPI spec.
#[derive(Debug, Serialize)]
pub struct WfbConfigureResponse {
    pub ok: bool,
    pub message: Option<String>,
    pub state: Value,
}

/// Response body for `POST /api/v1/setup/wfb/regenerate-key`.
#[derive(Debug, Serialize)]
pub struct WfbRegenerateKeyResponse {
    pub ok: bool,
    pub public_key_hex: String,
    pub key_fingerprint: String,
}

/// `GET /api/v1/setup/wfb` — read the current state + config summary.
pub async fn get_wfb(Extension(mgr): Extension<SharedWfbManager>) -> Json<Value> {
    let snapshot = mgr.lock().await.state_snapshot().await;
    Json(serde_json::to_value(snapshot).unwrap_or_else(|_| json!({})))
}

/// `POST /api/v1/setup/wfb/configure` — apply a new config.
pub async fn post_wfb_configure(
    Extension(mgr): Extension<SharedWfbManager>,
    Json(req): Json<WfbConfigureRequest>,
) -> Response {
    if req.key_passphrase.trim().is_empty() {
        return error_response("key_passphrase must not be empty");
    }
    // Validate envelopes up-front so the caller sees a 400 rather than
    // a vaguely-typed Invariant later.
    if !((1..=13).contains(&req.channel) || (36..=165).contains(&req.channel)) {
        return error_response(&format!(
            "channel {} outside the allowed 2.4 / 5 GHz ranges",
            req.channel
        ));
    }
    if req.mcs_index > 7 {
        return error_response(&format!(
            "mcs_index {} exceeds 0..=7 single-stream ceiling",
            req.mcs_index
        ));
    }
    if req.tx_power_dbm > 30 {
        return error_response(&format!(
            "tx_power_dbm {} outside 0..=30 envelope",
            req.tx_power_dbm
        ));
    }
    // Build a fresh config inheriting interface (set by udev) from
    // the live snapshot. Only the four operator-controlled fields land
    // from the request body; the keypair path + binary path + advanced
    // opts default to their compile-time constants.
    let new_cfg = {
        let snap = mgr.lock().await.state_snapshot().await;
        WfbConfig {
            channel: req.channel,
            mcs_index: req.mcs_index,
            tx_power_dbm: req.tx_power_dbm,
            key_passphrase: req.key_passphrase.clone(),
            wfb_tx_path: std::path::PathBuf::from(ados_wfb::DEFAULT_WFB_TX_PATH),
            interface: snap.config_summary.interface.clone(),
            keypair_path: std::path::PathBuf::from(ados_wfb::DEFAULT_KEYPAIR_PATH),
            advanced: ados_wfb::WfbAdvancedOpts::default(),
        }
    };
    // Tighten the secret directory before any subsequent write touches
    // it (regenerate-key + manager keypair persistence both land under
    // /etc/ados/secrets/ in production).
    if let Some(parent) = new_cfg.keypair_path.parent() {
        let _ = ensure_secret_dir(parent);
    }
    // Apply through the manager so the orchestration loop sees the
    // change and respawns wfb_tx with the new arguments on its next
    // tick.
    let apply = mgr.lock().await.apply_config(new_cfg.clone()).await;
    match apply {
        Ok(()) => {
            // Materialise the keypair file so the next wfb_tx spawn
            // has the bytes on disk. Best-effort — a failure here is
            // recoverable (the operator can rerun configure) but we
            // surface it in the message.
            let kp_msg = match mgr.lock().await.persist_keypair_file().await {
                Ok(_) => None,
                Err(e) => Some(format!("(keypair file not persisted: {e})")),
            };
            let snap = mgr.lock().await.state_snapshot().await;
            let body = WfbConfigureResponse {
                ok: true,
                message: kp_msg.or_else(|| Some("config applied".to_string())),
                state: serde_json::to_value(snap).unwrap_or_else(|_| json!({})),
            };
            Json(body).into_response()
        }
        Err(e) => match e {
            WfbError::InvalidChannel(_)
            | WfbError::InvalidMcs(_)
            | WfbError::InvalidPower(_)
            | WfbError::Key(_) => error_response(&e.to_string()),
            other => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "ok": false, "error": other.to_string() })),
            )
                .into_response(),
        },
    }
}

/// `POST /api/v1/setup/wfb/regenerate-key` — mint a fresh passphrase
/// + keypair, persist the keypair file, and return the public bytes.
pub async fn post_wfb_regenerate_key(
    Extension(mgr): Extension<SharedWfbManager>,
) -> Response {
    // Mint a fresh OS-entropy passphrase. The agent never persists the
    // passphrase itself — only the derived 32-byte broadcast key + 32-byte
    // public are written to the keypair file at 0600.
    let new_passphrase = ados_wfb::generate_passphrase();
    // Update the manager's config with the fresh passphrase, preserving
    // every other field. We can't construct a fresh `WfbConfig` from
    // the snapshot because the snapshot deliberately omits the secret
    // surface. Instead we mutate via the manager's `apply_config` that
    // copies forward whatever we pass + validates.
    let snap_summary = mgr.lock().await.state_snapshot().await.config_summary;
    let new_cfg = WfbConfig {
        channel: snap_summary.channel,
        mcs_index: snap_summary.mcs_index,
        tx_power_dbm: snap_summary.tx_power_dbm,
        key_passphrase: new_passphrase.clone(),
        wfb_tx_path: std::path::PathBuf::from(ados_wfb::DEFAULT_WFB_TX_PATH),
        interface: snap_summary.interface.clone(),
        keypair_path: std::path::PathBuf::from(ados_wfb::DEFAULT_KEYPAIR_PATH),
        advanced: ados_wfb::WfbAdvancedOpts::default(),
    };
    if let Some(parent) = new_cfg.keypair_path.parent() {
        let _ = ensure_secret_dir(parent);
    }
    if let Err(e) = mgr.lock().await.apply_config(new_cfg).await {
        return error_response(&format!("could not apply fresh keypair config: {e}"));
    }
    // Compute public hex and fingerprint outside the manager lock.
    let public_hex = match regenerate_public_key_hex(&new_passphrase) {
        Ok(h) => h,
        Err(e) => return error_response(&format!("derive_keypair: {e}")),
    };
    let broadcast = match derive_key(&new_passphrase) {
        Ok(b) => b,
        Err(e) => return error_response(&format!("derive_key: {e}")),
    };
    let fp = key_fingerprint(&broadcast);
    // Persist the keypair file via the manager so we share the same
    // atomic-write pathway with the configure handler.
    if let Err(e) = mgr.lock().await.persist_keypair_file().await {
        return error_response(&format!("could not persist keypair file: {e}"));
    }
    Json(WfbRegenerateKeyResponse {
        ok: true,
        public_key_hex: public_hex,
        key_fingerprint: fp,
    })
    .into_response()
}

/// Build a bare router carrying just the three WFB routes plus the
/// shared manager `Extension`. Production wiring merges this into the
/// gated setup router; tests use it directly.
pub fn wfb_router_only(mgr: SharedWfbManager) -> Router {
    Router::new()
        .route("/api/v1/setup/wfb", get(get_wfb))
        .route("/api/v1/setup/wfb/configure", post(post_wfb_configure))
        .route(
            "/api/v1/setup/wfb/regenerate-key",
            post(post_wfb_regenerate_key),
        )
        .layer(Extension(mgr))
}

fn error_response(msg: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "ok": false, "error": msg })),
    )
        .into_response()
}
