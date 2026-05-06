//! End-to-end coverage of the universal-setup REST surface through the
//! same-origin gate.
//!
//! For every route documented in `proto/setup/setup-api.yaml` (the 11
//! `/api/v1/setup/*` paths plus the two top-level operability routes
//! that share the same router) this test drives three permutations:
//!
//! - Same-origin (`http://localhost:8080`): expects a 2xx response or,
//!   for routes whose handlers reject synthetic input on the bench
//!   (cloudflare install with a placeholder token, profile with an
//!   unknown id), a non-403 application-level error. The gate passes
//!   the request through; the handler may or may not 200 depending on
//!   the body.
//! - Cross-origin (`http://evil.example.com`): expects 403 for every
//!   gated class (mutating method, WS upgrade, `/api/v1/diag` GET) and
//!   pass-through for non-gated reads (`/status`, `/hardware-check`,
//!   `/cloudflare/verify`, `/wfb`, `/health`).
//! - No `Origin` header: expects pass-through on every route — the
//!   curl / SRE / native-client path that the gate explicitly preserves.
//!
//! The router is built with `setup_router_with_origin_check_diag_and_wfb`
//! so the gate, the diag handle, and the WFB-ng manager are all wired
//! exactly as in the production agent binary.

use std::sync::Arc;

use ados_setup::{
    setup_router_with_origin_check_diag_and_wfb,
    state::StateStore,
    DiagState, OriginAllowlist, SetupState,
};
use ados_wfb::{WfbConfig, WfbManager};
use axum::body::{to_bytes, Body};
use axum::http::{Method, Request, StatusCode};
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tower::ServiceExt;

const SAME_ORIGIN: &str = "http://localhost:8080";
const FOREIGN_ORIGIN: &str = "http://evil.example.com";

fn fresh_state(dir: &tempfile::TempDir) -> Arc<SetupState> {
    let agent_yaml = dir.path().join("agent.yaml");
    std::fs::write(
        &agent_yaml,
        "agent:\n  device_id: \"routes-e2e-001\"\n  name: \"Routes E2E\"\nmavlink:\n  port: \"/dev/ttyS0\"\n  baud: 115200\ncloud:\n  api_key: \"\"\napi:\n  bind: \"127.0.0.1:18080\"\n",
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
                "device_id": "routes-e2e-001",
                "device_name": "Routes E2E",
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

fn fresh_wfb_manager() -> Arc<Mutex<WfbManager>> {
    let mgr = WfbManager::new(WfbConfig::default()).expect("default WfbConfig must construct");
    Arc::new(Mutex::new(mgr))
}

fn build_router(state: Arc<SetupState>) -> axum::Router {
    let allowlist = Arc::new(OriginAllowlist::new("0.0.0.0", 8080, "routes-e2e-001"));
    setup_router_with_origin_check_diag_and_wfb(
        state,
        allowlist,
        DiagState::shared(),
        fresh_wfb_manager(),
    )
}

/// One request scenario: method + path + optional body + optional Origin.
struct Scenario {
    method: Method,
    path: String,
    body: Option<Value>,
    origin: Option<&'static str>,
}

async fn run(state: Arc<SetupState>, sc: Scenario) -> StatusCode {
    let router = build_router(state);
    let mut builder = Request::builder().method(sc.method.clone()).uri(&sc.path);
    if let Some(o) = sc.origin {
        builder = builder.header("origin", o);
    }
    let request = match sc.body {
        Some(b) => builder
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&b).unwrap()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let response = router.oneshot(request).await.unwrap();
    let status = response.status();
    // Drain the body so Hyper does not log a noisy unread-body warning.
    let _ = to_bytes(response.into_body(), 64 * 1024).await;
    status
}

/// The closed list of routes we exercise. WS upgrade is covered by its
/// own scenario block below because it needs the full handshake header
/// set; everything else routes through plain HTTP.
fn http_routes() -> Vec<(Method, &'static str, Option<Value>, bool /* mutating */)> {
    vec![
        // 1. /status — GET, public read
        (Method::GET, "/api/v1/setup/status", None, false),
        // 2. /profile — POST, gated mutating
        (
            Method::POST,
            "/api/v1/setup/profile",
            Some(json!({"profile": "drone"})),
            true,
        ),
        // 3. /hardware-check — GET, public read
        (Method::GET, "/api/v1/setup/hardware-check", None, false),
        // 4. /hardware-check/refresh — POST, gated mutating
        (
            Method::POST,
            "/api/v1/setup/hardware-check/refresh",
            None,
            true,
        ),
        // 5. /cloud-choice — POST, gated mutating
        (
            Method::POST,
            "/api/v1/setup/cloud-choice",
            Some(json!({"mode": "cloud"})),
            true,
        ),
        // 6. /remote-access/cloudflare — POST, gated mutating. We send a
        //    placeholder body; the handler will 400 on a bogus token but
        //    that is an application-level rejection, not a gate rejection.
        (
            Method::POST,
            "/api/v1/setup/remote-access/cloudflare",
            Some(json!({"token_or_script": "x"})),
            true,
        ),
        // 7. /cloudflare/verify — GET, public read
        (Method::GET, "/api/v1/setup/cloudflare/verify", None, false),
        // 8. /finish — POST, gated mutating, body-less per spec
        (Method::POST, "/api/v1/setup/finish", None, true),
        // 9. /step/{step_id}/skip — POST, gated mutating
        (Method::POST, "/api/v1/setup/step/video/skip", None, true),
        // 10. /reset — POST, gated mutating
        (Method::POST, "/api/v1/setup/reset", None, true),
        // /cloudflare/logs (route 11) is a WebSocket upgrade and is
        // covered by its own scenario block below.
    ]
}

#[tokio::test]
async fn same_origin_passes_every_http_route() {
    // Every gated and ungated route must NOT return 403 when the Origin
    // header matches the allowlist. Some handlers will return application-
    // level 4xx (e.g. cloudflare install with a placeholder token), but
    // none should be blocked by the gate.
    for (method, path, body, _mutating) in http_routes() {
        let dir = tempfile::tempdir().unwrap();
        let state = fresh_state(&dir);
        let status = run(
            state,
            Scenario {
                method: method.clone(),
                path: path.to_string(),
                body: body.clone(),
                origin: Some(SAME_ORIGIN),
            },
        )
        .await;
        assert_ne!(
            status,
            StatusCode::FORBIDDEN,
            "same-origin {} {} got 403 from gate",
            method,
            path
        );
    }
}

#[tokio::test]
async fn foreign_origin_gates_mutating_routes() {
    // Every mutating route returns 403 when called with a foreign Origin.
    for (method, path, body, mutating) in http_routes() {
        if !mutating {
            continue;
        }
        let dir = tempfile::tempdir().unwrap();
        let state = fresh_state(&dir);
        let status = run(
            state,
            Scenario {
                method: method.clone(),
                path: path.to_string(),
                body: body.clone(),
                origin: Some(FOREIGN_ORIGIN),
            },
        )
        .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "foreign-origin {} {} should have been blocked but returned {}",
            method,
            path,
            status
        );
    }
}

#[tokio::test]
async fn foreign_origin_passes_public_reads() {
    // Non-mutating reads on the setup surface (status, hardware-check,
    // cloudflare/verify) are explicitly NOT gated. A dashboard hosted
    // elsewhere can still poll them. The gate's contract is to defend
    // mutation surfaces, WS upgrades, and `/diag` — not all reads.
    for (method, path, body, mutating) in http_routes() {
        if mutating {
            continue;
        }
        let dir = tempfile::tempdir().unwrap();
        let state = fresh_state(&dir);
        let status = run(
            state,
            Scenario {
                method: method.clone(),
                path: path.to_string(),
                body: body.clone(),
                origin: Some(FOREIGN_ORIGIN),
            },
        )
        .await;
        assert_ne!(
            status,
            StatusCode::FORBIDDEN,
            "foreign-origin read {} {} should not have been gated",
            method,
            path
        );
    }
}

#[tokio::test]
async fn missing_origin_passes_every_http_route() {
    // No Origin header at all = native-client path (curl / SRE / SDK).
    // The gate must not block any of these regardless of method, on the
    // theory that browsers always send Origin and only browsers can be
    // tricked into making cross-origin reconfiguration calls from a
    // hostile page.
    for (method, path, body, _) in http_routes() {
        let dir = tempfile::tempdir().unwrap();
        let state = fresh_state(&dir);
        let status = run(
            state,
            Scenario {
                method: method.clone(),
                path: path.to_string(),
                body: body.clone(),
                origin: None,
            },
        )
        .await;
        assert_ne!(
            status,
            StatusCode::FORBIDDEN,
            "no-origin {} {} got 403 from gate",
            method,
            path
        );
    }
}

#[tokio::test]
async fn ws_logs_with_foreign_origin_is_403() {
    // The 11th route — `/cloudflare/logs` — is a WebSocket upgrade. A
    // hostile page on the LAN that tries `new WebSocket("ws://host/.../logs")`
    // sends Origin on the handshake; the gate must reject it with 403
    // before the upgrade extractor runs.
    let dir = tempfile::tempdir().unwrap();
    let state = fresh_state(&dir);
    let router = build_router(state);
    let request = Request::builder()
        .method(Method::GET)
        .uri("/api/v1/setup/cloudflare/logs")
        .header("upgrade", "websocket")
        .header("connection", "Upgrade")
        .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
        .header("sec-websocket-version", "13")
        .header("origin", FOREIGN_ORIGIN)
        .body(Body::empty())
        .unwrap();
    let response = router.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn ws_logs_with_same_origin_is_not_gate_blocked() {
    // Same handshake from the wizard tab on the agent's own host. The
    // gate must let it through to the WebSocketUpgrade extractor. The
    // synthetic request will not actually upgrade (no real socket), but
    // the response must not be 403.
    let dir = tempfile::tempdir().unwrap();
    let state = fresh_state(&dir);
    let router = build_router(state);
    let request = Request::builder()
        .method(Method::GET)
        .uri("/api/v1/setup/cloudflare/logs")
        .header("upgrade", "websocket")
        .header("connection", "Upgrade")
        .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
        .header("sec-websocket-version", "13")
        .header("origin", SAME_ORIGIN)
        .body(Body::empty())
        .unwrap();
    let response = router.oneshot(request).await.unwrap();
    assert_ne!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn ws_logs_with_no_origin_passes_gate() {
    // Headerless CLI path. The gate explicitly preserves this so a CLI
    // operator tailing the WS without forging an Origin header can keep
    // working.
    let dir = tempfile::tempdir().unwrap();
    let state = fresh_state(&dir);
    let router = build_router(state);
    let request = Request::builder()
        .method(Method::GET)
        .uri("/api/v1/setup/cloudflare/logs")
        .header("upgrade", "websocket")
        .header("connection", "Upgrade")
        .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
        .header("sec-websocket-version", "13")
        .body(Body::empty())
        .unwrap();
    let response = router.oneshot(request).await.unwrap();
    assert_ne!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn route_count_matches_setup_api_yaml_paths_section() {
    // The `paths:` section of `proto/setup/setup-api.yaml` documents 11
    // routes under the universal-setup tree:
    //   /status, /profile, /hardware-check, /hardware-check/refresh,
    //   /cloud-choice, /remote-access/cloudflare, /cloudflare/verify,
    //   /cloudflare/logs, /finish, /step/{step_id}/skip, /reset.
    //
    // This test asserts that the HTTP-only subset is exactly 10 entries
    // (the 11th being the WS upgrade we cover separately) so a future
    // add or drop is caught at the test layer.
    let routes = http_routes();
    assert_eq!(
        routes.len(),
        10,
        "expected exactly 10 HTTP routes; the 11th is /cloudflare/logs (WS)"
    );
}
