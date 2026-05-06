//! Same-origin gate coverage for the WFB-ng REST surface.
//!
//! The three WFB routes (`GET /api/v1/setup/wfb`,
//! `POST /api/v1/setup/wfb/configure`,
//! `POST /api/v1/setup/wfb/regenerate-key`) live under the same axum
//! router the rest of the universal setup surface uses, so they share
//! the same-origin middleware. This file pins the gate's behaviour on
//! the WFB routes specifically so a future refactor that hides the
//! WFB sub-router behind an unprotected merge surfaces here.
//!
//! Three permutations per route, matching the contract documented in
//! [`ados_setup::origin::check_origin`]:
//!
//! - Same-origin (`http://localhost:8080`): expected to pass through.
//! - Cross-origin (`http://evil.example.com`): expected 403 on the
//!   gated classes (POST). Reads stay 200.
//! - No `Origin` header: expected to pass through on every method.
//!
//! No `ados-setup/src/` files are touched; this test is additive.

use std::path::PathBuf;
use std::sync::Arc;

use ados_setup::{
    setup_router_with_origin_check_diag_and_wfb,
    state::StateStore,
    DiagState, OriginAllowlist, SetupState,
};
use ados_wfb::{WfbAdvancedOpts, WfbConfig, WfbManager};
use axum::body::{to_bytes, Body};
use axum::http::{Method, Request, StatusCode};
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tower::ServiceExt;

const SAME_ORIGIN: &str = "http://localhost:8080";
const FOREIGN_ORIGIN: &str = "http://evil.example.com";
const DEVICE_ID: &str = "wfb-origin-001";

fn fresh_state(dir: &tempfile::TempDir) -> Arc<SetupState> {
    let agent_yaml = dir.path().join("agent.yaml");
    std::fs::write(
        &agent_yaml,
        format!(
            "agent:\n  device_id: \"{DEVICE_ID}\"\n  name: \"WFB Origin Gate Test\"\n\
             mavlink:\n  port: \"/dev/ttyS0\"\n  baud: 115200\n\
             cloud:\n  api_key: \"\"\n\
             api:\n  bind: \"127.0.0.1:18080\"\n",
        ),
    )
    .unwrap();
    let state_path = dir.path().join("setup-state.json");
    let store = StateStore::new(state_path);
    let store_for_status = store.clone();
    Arc::new(SetupState {
        agent_yaml,
        store,
        status_builder: Box::new(move || {
            let persisted = store_for_status.load().unwrap_or_default();
            json!({
                "version": "0.1.0",
                "agent_version": "0.1.0",
                "device_id": DEVICE_ID,
                "device_name": "WFB Origin Gate Test",
                "profile": "drone",
                "ground_role": "",
                "runtime_mode": "lite",
                "setup_complete": persisted.finalized,
                "setup_finalized": persisted.finalized,
                "completion_percent": if persisted.finalized { 100 } else { 0 },
                "next_action": if persisted.finalized { "ready" } else { "pair" },
                "steps": [],
                "skipped_steps": persisted.skipped_steps.iter().cloned().collect::<Vec<_>>(),
                "access_urls": [],
                "network": { "hostname": "", "mdns_host": "", "api_port": 8080,
                             "hotspot_enabled": false, "hotspot_ssid": "", "local_ips": [] },
                "mavlink": { "connected": false, "port": "/dev/ttyS0", "baud": 115200,
                             "websocket_url": null, "public_websocket_url": null },
                "video": { "state": "not_initialized", "whep_url": null,
                           "public_whep_url": null, "recording": false },
                "remote_access": { "provider": "none", "enabled": false, "configured": false,
                                   "status": "disabled", "public_urls": [], "error": "" },
                "cloud_choice": { "mode": "cloud", "paired": false, "pair_code_required": true,
                                  "backend_url": "", "backend_reachable": false,
                                  "last_checked": null },
                "profile_suggestion": null,
                "hardware_check": null,
                "services": []
            })
        }),
    })
}

/// Build a manager with a tempdir keypair path so the configure handler
/// can persist its keypair file without touching `/etc/ados/secrets`.
fn fresh_wfb_manager(keypair_path: PathBuf) -> Arc<Mutex<WfbManager>> {
    let cfg = WfbConfig {
        channel: 161,
        mcs_index: 1,
        tx_power_dbm: 25,
        key_passphrase: "wfb-origin-gate-test".to_string(),
        wfb_tx_path: PathBuf::from(ados_wfb::DEFAULT_WFB_TX_PATH),
        interface: None,
        keypair_path,
        advanced: WfbAdvancedOpts::default(),
    };
    let mgr = WfbManager::new(cfg).expect("manager construct");
    Arc::new(Mutex::new(mgr))
}

fn build_router(state: Arc<SetupState>, mgr: Arc<Mutex<WfbManager>>) -> axum::Router {
    let allowlist = Arc::new(OriginAllowlist::new("0.0.0.0", 8080, DEVICE_ID));
    setup_router_with_origin_check_diag_and_wfb(state, allowlist, DiagState::shared(), mgr)
}

async fn dispatch(
    router: axum::Router,
    method: Method,
    path: &str,
    origin: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(path);
    if let Some(o) = origin {
        builder = builder.header("origin", o);
    }
    let request = match body {
        Some(b) => builder
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&b).unwrap()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let response = router.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    let value: Value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

#[tokio::test]
async fn wfb_get_passes_with_same_origin() {
    let dir = tempfile::tempdir().unwrap();
    let state = fresh_state(&dir);
    let mgr = fresh_wfb_manager(dir.path().join("kp"));
    let router = build_router(state, mgr);
    let (status, body) =
        dispatch(router, Method::GET, "/api/v1/setup/wfb", Some(SAME_ORIGIN), None).await;
    assert_eq!(status, StatusCode::OK);
    // Snapshot body shape sanity: must carry a `state` discriminator.
    assert!(body.get("state").is_some(), "snapshot body missing state, got {body}");
}

#[tokio::test]
async fn wfb_get_passes_without_origin_header() {
    // Reads are not gated. SREs / native clients without an Origin
    // header keep working.
    let dir = tempfile::tempdir().unwrap();
    let state = fresh_state(&dir);
    let mgr = fresh_wfb_manager(dir.path().join("kp"));
    let router = build_router(state, mgr);
    let (status, _body) = dispatch(router, Method::GET, "/api/v1/setup/wfb", None, None).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn wfb_get_passes_with_foreign_origin() {
    // GETs are not in the gated class. A read of `/wfb` from a
    // different origin returns 200 — public state, no secret leakage.
    let dir = tempfile::tempdir().unwrap();
    let state = fresh_state(&dir);
    let mgr = fresh_wfb_manager(dir.path().join("kp"));
    let router = build_router(state, mgr);
    let (status, _body) = dispatch(
        router,
        Method::GET,
        "/api/v1/setup/wfb",
        Some(FOREIGN_ORIGIN),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn wfb_configure_post_passes_with_same_origin() {
    let dir = tempfile::tempdir().unwrap();
    let state = fresh_state(&dir);
    // Use a writable keypair path under the tempdir so the handler's
    // persist step does not race with /etc.
    let keypair = dir.path().join("kp");
    let mgr = fresh_wfb_manager(keypair);
    let router = build_router(state, mgr);
    let body = json!({
        "channel": 161,
        "mcs_index": 1,
        "tx_power_dbm": 25,
        "key_passphrase": "same-origin-pass-2026",
    });
    let (status, _resp) = dispatch(
        router,
        Method::POST,
        "/api/v1/setup/wfb/configure",
        Some(SAME_ORIGIN),
        Some(body),
    )
    .await;
    // The handler may surface a non-201 response (e.g., the keypair
    // path defaults to /etc/ados/secrets which the test cannot create
    // permissions for from the manager's persist step). What we care
    // about here is the gate did NOT 403 — anything other than 403 is
    // a pass.
    assert_ne!(status, StatusCode::FORBIDDEN, "same-origin POST must NOT be gated");
}

#[tokio::test]
async fn wfb_configure_post_blocked_with_foreign_origin() {
    let dir = tempfile::tempdir().unwrap();
    let state = fresh_state(&dir);
    let mgr = fresh_wfb_manager(dir.path().join("kp"));
    let router = build_router(state, mgr);
    let body = json!({
        "channel": 161,
        "mcs_index": 1,
        "tx_power_dbm": 25,
        "key_passphrase": "foreign-origin-pass-2026",
    });
    let (status, resp) = dispatch(
        router,
        Method::POST,
        "/api/v1/setup/wfb/configure",
        Some(FOREIGN_ORIGIN),
        Some(body),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(resp["ok"], false);
    assert!(
        resp["error"]
            .as_str()
            .unwrap_or("")
            .contains("origin"),
        "403 body must call out the origin: got {resp}"
    );
}

#[tokio::test]
async fn wfb_configure_post_passes_without_origin_header() {
    // Curl / native CLI path: no Origin header -> gate passes through
    // even on a mutating method. The handler then validates the body
    // and may 200 / 400 depending on payload health; we only assert
    // the gate does not 403.
    let dir = tempfile::tempdir().unwrap();
    let state = fresh_state(&dir);
    let mgr = fresh_wfb_manager(dir.path().join("kp"));
    let router = build_router(state, mgr);
    let body = json!({
        "channel": 161,
        "mcs_index": 1,
        "tx_power_dbm": 25,
        "key_passphrase": "headerless-pass-2026",
    });
    let (status, _resp) = dispatch(
        router,
        Method::POST,
        "/api/v1/setup/wfb/configure",
        None,
        Some(body),
    )
    .await;
    assert_ne!(
        status,
        StatusCode::FORBIDDEN,
        "POST without an Origin header must NOT be gated"
    );
}

#[tokio::test]
async fn wfb_regenerate_key_post_blocked_with_foreign_origin() {
    let dir = tempfile::tempdir().unwrap();
    let state = fresh_state(&dir);
    let mgr = fresh_wfb_manager(dir.path().join("kp"));
    let router = build_router(state, mgr);
    let (status, resp) = dispatch(
        router,
        Method::POST,
        "/api/v1/setup/wfb/regenerate-key",
        Some(FOREIGN_ORIGIN),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(resp["ok"], false);
}

#[tokio::test]
async fn wfb_regenerate_key_post_passes_with_same_origin() {
    let dir = tempfile::tempdir().unwrap();
    let state = fresh_state(&dir);
    let mgr = fresh_wfb_manager(dir.path().join("kp"));
    let router = build_router(state, mgr);
    let (status, _resp) = dispatch(
        router,
        Method::POST,
        "/api/v1/setup/wfb/regenerate-key",
        Some(SAME_ORIGIN),
        None,
    )
    .await;
    assert_ne!(
        status,
        StatusCode::FORBIDDEN,
        "regenerate-key POST from same origin must NOT be gated"
    );
}

#[tokio::test]
async fn wfb_regenerate_key_post_passes_without_origin_header() {
    let dir = tempfile::tempdir().unwrap();
    let state = fresh_state(&dir);
    let mgr = fresh_wfb_manager(dir.path().join("kp"));
    let router = build_router(state, mgr);
    let (status, _resp) =
        dispatch(router, Method::POST, "/api/v1/setup/wfb/regenerate-key", None, None).await;
    assert_ne!(status, StatusCode::FORBIDDEN);
}
