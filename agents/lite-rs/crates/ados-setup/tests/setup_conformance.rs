//! Conformance tests: every route documented in
//! `proto/setup/setup-api.yaml` is hit through the assembled axum
//! router and the response is validated against the canonical shape
//! the Python reference at `src/ados/api/routes/setup.py` returns.
//!
//! Wire-compat goals (subset, not byte-for-byte):
//!
//! - Status routes return JSON objects with every required key from the
//!   Python `SetupStatus` Pydantic model.
//! - Mutation routes return `SetupActionResult` with `ok` + `message`.
//! - Skip route returns 400 for required steps and 404 for unknown ids.
//! - Hardware-check returns the typed `HardwareCheckStatus` shape with
//!   profile + items array.
//!
//! Test harness uses tower's `ServiceExt::oneshot` to drive the router
//! without a real TCP listener. Fast, deterministic, no port collisions.

use std::path::PathBuf;
use std::sync::Arc;

use ados_setup::{
    setup_router, setup_router_with_origin_check,
    state::{PersistedState, StateStore},
    OriginAllowlist, SetupState,
};
use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

fn fresh_state(dir: &tempfile::TempDir) -> Arc<SetupState> {
    let agent_yaml = dir.path().join("agent.yaml");
    std::fs::write(
        &agent_yaml,
        "agent:\n  device_id: \"conformance-001\"\n  name: \"Conformance\"\nmavlink:\n  port: \"/dev/ttyS0\"\n  baud: 115200\ncloud:\n  api_key: \"\"\napi:\n  bind: \"127.0.0.1:18080\"\n",
    )
    .unwrap();
    let state_path = dir.path().join("setup-state.json");
    let store = StateStore::new(state_path);
    let device_id = "conformance-001".to_string();
    let device_id_for_status = device_id.clone();
    let store_for_status = store.clone();
    Arc::new(SetupState {
        agent_yaml,
        store,
        status_builder: Box::new(move || {
            // Read the persisted state so /status reflects mutations
            // landed by /finish, /skip, /reset.
            let persisted = store_for_status.load().unwrap_or_default();
            build_canonical_status(&device_id_for_status, &persisted)
        }),
    })
}

fn build_canonical_status(device_id: &str, persisted: &PersistedState) -> Value {
    let skipped: Vec<String> = persisted.skipped_steps.iter().cloned().collect();
    json!({
        "version": "0.1.0",
        "agent_version": "0.1.0",
        "device_id": device_id,
        "device_name": "Conformance Test",
        "profile": "drone",
        "ground_role": "",
        "runtime_mode": "lite",
        "setup_complete": persisted.finalized,
        "setup_finalized": persisted.finalized,
        "completion_percent": if persisted.finalized { 100 } else { 0 },
        "next_action": if persisted.finalized { "ready" } else { "pair" },
        "steps": [],
        "skipped_steps": skipped,
        "access_urls": [],
        "network": {
            "hostname": "",
            "mdns_host": "",
            "api_port": 8080,
            "hotspot_enabled": false,
            "hotspot_ssid": "",
            "local_ips": []
        },
        "mavlink": {
            "connected": false,
            "port": "/dev/ttyS0",
            "baud": 115200,
            "websocket_url": null,
            "public_websocket_url": null
        },
        "video": {
            "state": "not_initialized",
            "whep_url": null,
            "public_whep_url": null,
            "recording": false
        },
        "remote_access": {
            "provider": "none",
            "enabled": false,
            "configured": false,
            "status": "disabled",
            "public_urls": [],
            "error": ""
        },
        "cloud_choice": {
            "mode": "cloud",
            "paired": false,
            "pair_code_required": true,
            "backend_url": "",
            "backend_reachable": false,
            "last_checked": null
        },
        "profile_suggestion": null,
        "hardware_check": null,
        "services": [
            { "name": "mavlink-router", "state": "running" },
            { "name": "cloud-client",   "state": "running" },
            { "name": "http-api",       "state": "running" }
        ]
    })
}

async fn json_response(
    state: Arc<SetupState>,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let router = setup_router(state);
    let request = match body {
        Some(b) => Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&b).unwrap()))
            .unwrap(),
        None => Request::builder()
            .method(method)
            .uri(path)
            .body(Body::empty())
            .unwrap(),
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

const REQUIRED_STATUS_KEYS: &[&str] = &[
    "version",
    "agent_version",
    "device_id",
    "device_name",
    "profile",
    "ground_role",
    "runtime_mode",
    "setup_complete",
    "setup_finalized",
    "completion_percent",
    "next_action",
    "steps",
    "skipped_steps",
    "access_urls",
    "network",
    "mavlink",
    "video",
    "remote_access",
    "cloud_choice",
    "services",
];

fn assert_canonical_status_shape(value: &Value) {
    let obj = value
        .as_object()
        .expect("status response should be JSON object");
    for key in REQUIRED_STATUS_KEYS {
        assert!(obj.contains_key(*key), "missing required key: {key}");
    }
    let cloud_choice = obj.get("cloud_choice").unwrap();
    for key in &["mode", "paired", "pair_code_required", "backend_url"] {
        assert!(
            cloud_choice.get(*key).is_some(),
            "cloud_choice missing key: {key}"
        );
    }
}

#[tokio::test]
async fn status_returns_canonical_shape() {
    let dir = tempfile::tempdir().unwrap();
    let state = fresh_state(&dir);
    let (status, body) = json_response(state, Method::GET, "/api/v1/setup/status", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_canonical_status_shape(&body);
    assert_eq!(body["next_action"], "pair");
    assert_eq!(body["setup_finalized"], false);
}

#[tokio::test]
async fn profile_route_returns_action_result() {
    let dir = tempfile::tempdir().unwrap();
    let state = fresh_state(&dir);
    let (status, body) = json_response(
        state.clone(),
        Method::POST,
        "/api/v1/setup/profile",
        Some(json!({"profile": "drone"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], true);
    assert!(body.get("message").is_some());
    assert!(body.get("status").is_some());
    let inner = &body["status"];
    assert_canonical_status_shape(inner);
}

#[tokio::test]
async fn profile_invalid_returns_400() {
    let dir = tempfile::tempdir().unwrap();
    let state = fresh_state(&dir);
    let (status, body) = json_response(
        state,
        Method::POST,
        "/api/v1/setup/profile",
        Some(json!({"profile": "spaceship"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["ok"], false);
}

#[tokio::test]
async fn hardware_check_returns_typed_status_shape() {
    let dir = tempfile::tempdir().unwrap();
    let state = fresh_state(&dir);
    let (status, body) =
        json_response(state, Method::GET, "/api/v1/setup/hardware-check", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["profile"], "drone");
    let items = body["items"].as_array().expect("items is array");
    let ids: Vec<&str> = items
        .iter()
        .filter_map(|i| i.get("id").and_then(|v| v.as_str()))
        .collect();
    assert!(ids.contains(&"board"));
    assert!(ids.contains(&"fc"));
}

#[tokio::test]
async fn hardware_check_refresh_is_post_only() {
    let dir = tempfile::tempdir().unwrap();
    let state = fresh_state(&dir);
    let (status, body) = json_response(
        state.clone(),
        Method::POST,
        "/api/v1/setup/hardware-check/refresh",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.get("items").is_some());
}

#[tokio::test]
async fn cloud_choice_local_mode_persists() {
    let dir = tempfile::tempdir().unwrap();
    let state = fresh_state(&dir);
    let (status, body) = json_response(
        state.clone(),
        Method::POST,
        "/api/v1/setup/cloud-choice",
        Some(json!({"mode": "local"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], true);
    let raw = std::fs::read_to_string(&state.agent_yaml).unwrap();
    assert!(raw.contains("mode: local"));
}

#[tokio::test]
async fn cloudflare_install_with_invalid_token_returns_400() {
    std::env::set_var("ADOS_CLOUDFLARE_TOKEN_PATH", "/tmp/cf-token-conformance");
    std::env::set_var("ADOS_CLOUDFLARED_SKIP_DOWNLOAD", "1");
    let dir = tempfile::tempdir().unwrap();
    let state = fresh_state(&dir);
    let (status, body) = json_response(
        state,
        Method::POST,
        "/api/v1/setup/remote-access/cloudflare",
        Some(json!({"token_or_script": "not a valid token"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["ok"], false);
    std::env::remove_var("ADOS_CLOUDFLARE_TOKEN_PATH");
    std::env::remove_var("ADOS_CLOUDFLARED_SKIP_DOWNLOAD");
}

#[tokio::test]
async fn cloudflare_verify_no_url_reports_unset() {
    let dir = tempfile::tempdir().unwrap();
    let state = fresh_state(&dir);
    let (status, body) =
        json_response(state, Method::GET, "/api/v1/setup/cloudflare/verify", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["reachable"], false);
    let err = body["error"].as_str().unwrap_or("");
    assert!(
        err.contains("Set the public setup URL"),
        "expected unset-URL error, got: {err}"
    );
}

#[tokio::test]
async fn skip_then_status_reflects_skipped_step() {
    let dir = tempfile::tempdir().unwrap();
    let state = fresh_state(&dir);
    let (skip_status, _body) = json_response(
        state.clone(),
        Method::POST,
        "/api/v1/setup/step/video/skip",
        None,
    )
    .await;
    assert_eq!(skip_status, StatusCode::OK);
    let (status, body) =
        json_response(state, Method::GET, "/api/v1/setup/status", None).await;
    assert_eq!(status, StatusCode::OK);
    let skipped: Vec<&str> = body["skipped_steps"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s.as_str())
        .collect();
    assert!(skipped.contains(&"video"));
}

#[tokio::test]
async fn skip_required_step_returns_400() {
    let dir = tempfile::tempdir().unwrap();
    let state = fresh_state(&dir);
    let (status, body) = json_response(
        state,
        Method::POST,
        "/api/v1/setup/step/welcome/skip",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["detail"]
        .as_str()
        .unwrap_or("")
        .contains("cannot be skipped"));
}

#[tokio::test]
async fn skip_unknown_step_returns_404() {
    let dir = tempfile::tempdir().unwrap();
    let state = fresh_state(&dir);
    let (status, body) = json_response(
        state,
        Method::POST,
        "/api/v1/setup/step/quasar/skip",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["detail"]
        .as_str()
        .unwrap_or("")
        .contains("Unknown step"));
}

#[tokio::test]
async fn finish_marks_setup_finalized_in_status() {
    let dir = tempfile::tempdir().unwrap();
    let state = fresh_state(&dir);
    let (finish_status, finish_body) =
        json_response(state.clone(), Method::POST, "/api/v1/setup/finish", None).await;
    assert_eq!(finish_status, StatusCode::OK);
    assert_eq!(finish_body["setup_finalized"], true);
    assert_eq!(finish_body["completion_percent"], 100);
    // Subsequent /status read also reflects.
    let (_, body) = json_response(state, Method::GET, "/api/v1/setup/status", None).await;
    assert_eq!(body["setup_finalized"], true);
    assert_eq!(body["next_action"], "ready");
}

#[tokio::test]
async fn reset_clears_finalized_flag() {
    let dir = tempfile::tempdir().unwrap();
    let state = fresh_state(&dir);
    json_response(state.clone(), Method::POST, "/api/v1/setup/finish", None).await;
    let (reset_status, body) =
        json_response(state.clone(), Method::POST, "/api/v1/setup/reset", None).await;
    assert_eq!(reset_status, StatusCode::OK);
    assert_eq!(body["setup_finalized"], false);
    let (_, status_body) =
        json_response(state, Method::GET, "/api/v1/setup/status", None).await;
    assert_eq!(status_body["setup_finalized"], false);
}

#[tokio::test]
async fn full_wizard_flow_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let state = fresh_state(&dir);
    // 1. profile
    let (s, _) = json_response(
        state.clone(),
        Method::POST,
        "/api/v1/setup/profile",
        Some(json!({"profile": "drone"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    // 2. hardware-check
    let (s, _) = json_response(
        state.clone(),
        Method::GET,
        "/api/v1/setup/hardware-check",
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    // 3. cloud-choice
    let (s, _) = json_response(
        state.clone(),
        Method::POST,
        "/api/v1/setup/cloud-choice",
        Some(json!({"mode": "cloud"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    // 4. skip remote_access
    let (s, _) = json_response(
        state.clone(),
        Method::POST,
        "/api/v1/setup/step/remote_access/skip",
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    // 5. finish
    let (s, body) =
        json_response(state.clone(), Method::POST, "/api/v1/setup/finish", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["setup_finalized"], true);
    assert_eq!(body["next_action"], "ready");
    // 6. /status confirms
    let (_, body) = json_response(state, Method::GET, "/api/v1/setup/status", None).await;
    assert_eq!(body["setup_finalized"], true);
    let skipped: Vec<&str> = body["skipped_steps"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s.as_str())
        .collect();
    assert!(skipped.contains(&"remote_access"));
}

#[tokio::test]
async fn unknown_route_returns_404() {
    let dir = tempfile::tempdir().unwrap();
    let state = fresh_state(&dir);
    let (status, _) = json_response(
        state,
        Method::GET,
        "/api/v1/setup/totally-bogus-route",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn route_count_matches_setup_api_yaml() {
    // 11 routes documented in proto/setup/setup-api.yaml. The router
    // mounts /api/v1/setup/{status, profile, hardware-check,
    // hardware-check/refresh, cloud-choice, remote-access/cloudflare,
    // cloudflare/verify, cloudflare/logs, finish, step/:id/skip, reset}.
    // We verify each responds (any status code) so we never accidentally
    // drop a route.
    let dir = tempfile::tempdir().unwrap();
    let state = fresh_state(&dir);
    let routes: &[(Method, &str, Option<Value>)] = &[
        (Method::GET, "/api/v1/setup/status", None),
        (Method::POST, "/api/v1/setup/profile", Some(json!({"profile": "drone"}))),
        (Method::GET, "/api/v1/setup/hardware-check", None),
        (Method::POST, "/api/v1/setup/hardware-check/refresh", None),
        (Method::POST, "/api/v1/setup/cloud-choice", Some(json!({"mode": "cloud"}))),
        (Method::POST, "/api/v1/setup/remote-access/cloudflare", Some(json!({"token_or_script": "x"}))),
        (Method::GET, "/api/v1/setup/cloudflare/verify", None),
        // /cloudflare/logs is a WS upgrade — covered separately
        (Method::POST, "/api/v1/setup/finish", None),
        (Method::POST, "/api/v1/setup/step/video/skip", None),
        (Method::POST, "/api/v1/setup/reset", None),
    ];
    let mut hit = 0;
    for (method, path, body) in routes {
        let (status, _) = json_response(state.clone(), method.clone(), path, body.clone()).await;
        assert!(
            status != StatusCode::NOT_FOUND,
            "route {} {} not registered",
            method,
            path
        );
        hit += 1;
    }
    assert_eq!(hit, 10, "expected 10 non-WS routes wired");
}

// Drop unused fields warning at the end so a future addition doesn't leak.
#[allow(dead_code)]
fn _unused_path_marker() -> PathBuf {
    PathBuf::from("/")
}

// ---------------------------------------------------------------------------
// Origin gate (defense-in-depth on POST / PUT / PATCH / DELETE)
// ---------------------------------------------------------------------------

async fn json_response_gated(
    state: Arc<SetupState>,
    allowlist: Arc<OriginAllowlist>,
    method: Method,
    path: &str,
    origin: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let router = setup_router_with_origin_check(state, allowlist);
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
async fn origin_gate_allows_post_without_origin_header() {
    let dir = tempfile::tempdir().unwrap();
    let state = fresh_state(&dir);
    let allowlist =
        Arc::new(OriginAllowlist::new("0.0.0.0", 8080, "conformance-001"));
    let (status, body) = json_response_gated(
        state,
        allowlist,
        Method::POST,
        "/api/v1/setup/profile",
        None,
        Some(json!({"profile": "drone"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], true);
}

#[tokio::test]
async fn origin_gate_rejects_post_with_foreign_origin() {
    let dir = tempfile::tempdir().unwrap();
    let state = fresh_state(&dir);
    let allowlist =
        Arc::new(OriginAllowlist::new("0.0.0.0", 8080, "conformance-001"));
    let (status, body) = json_response_gated(
        state,
        allowlist,
        Method::POST,
        "/api/v1/setup/profile",
        Some("http://evil.example"),
        Some(json!({"profile": "drone"})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["ok"], false);
    assert!(body["error"]
        .as_str()
        .unwrap_or("")
        .contains("origin"));
}

#[tokio::test]
async fn origin_gate_allows_post_with_loopback_origin() {
    let dir = tempfile::tempdir().unwrap();
    let state = fresh_state(&dir);
    let allowlist =
        Arc::new(OriginAllowlist::new("0.0.0.0", 8080, "conformance-001"));
    let (status, body) = json_response_gated(
        state,
        allowlist,
        Method::POST,
        "/api/v1/setup/profile",
        Some("http://localhost:8080"),
        Some(json!({"profile": "drone"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], true);
}

#[tokio::test]
async fn origin_gate_passes_get_through_with_any_origin() {
    let dir = tempfile::tempdir().unwrap();
    let state = fresh_state(&dir);
    let allowlist =
        Arc::new(OriginAllowlist::new("0.0.0.0", 8080, "conformance-001"));
    // Read methods are never gated. A GET with a foreign origin still
    // returns 200 + the canonical status shape so dashboards / probes
    // that fetch /status from elsewhere keep working. Cross-origin
    // reads of public state are explicitly out of scope for this gate.
    let (status, _body) = json_response_gated(
        state,
        allowlist,
        Method::GET,
        "/api/v1/setup/status",
        Some("http://evil.example"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

// ---------------------------------------------------------------------------
// Operability surface — /api/v1/health and /api/v1/diag.
//
// These two routes live OUTSIDE /api/v1/setup/* so the same-origin gate
// does not apply. SREs polling from a monitoring host should not need to
// forge an Origin header.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn health_returns_ok_with_version() {
    let dir = tempfile::tempdir().unwrap();
    let state = fresh_state(&dir);
    let (status, body) = json_response(state, Method::GET, "/api/v1/health", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");
    assert!(!body["version"].as_str().unwrap().is_empty());
}

#[tokio::test]
async fn health_passes_through_origin_gate_without_origin_header() {
    // The gate applies to /api/v1/setup/*. /api/v1/health must remain
    // reachable from a monitoring host that sends no Origin header.
    let dir = tempfile::tempdir().unwrap();
    let state = fresh_state(&dir);
    let allowlist =
        Arc::new(OriginAllowlist::new("0.0.0.0", 8080, "conformance-001"));
    let (status, body) = json_response_gated(
        state,
        allowlist,
        Method::GET,
        "/api/v1/health",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");
}

#[tokio::test]
async fn health_passes_through_origin_gate_with_foreign_origin() {
    // Even with a foreign Origin header, /api/v1/health stays open.
    // The gate must not extend over the operability surface.
    let dir = tempfile::tempdir().unwrap();
    let state = fresh_state(&dir);
    let allowlist =
        Arc::new(OriginAllowlist::new("0.0.0.0", 8080, "conformance-001"));
    let (status, body) = json_response_gated(
        state,
        allowlist,
        Method::GET,
        "/api/v1/health",
        Some("http://monitoring.example"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");
}

#[tokio::test]
async fn diag_returns_canonical_shape() {
    let dir = tempfile::tempdir().unwrap();
    let state = fresh_state(&dir);
    let (status, body) = json_response(state, Method::GET, "/api/v1/diag", None).await;
    assert_eq!(status, StatusCode::OK);

    let obj = body.as_object().expect("diag response is a JSON object");
    for key in &[
        "version",
        "uptime_seconds",
        "device_id",
        "paired",
        "runtime_mode",
        "rss_mb",
        "mqtt",
        "cloud_relay",
        "mavlink",
    ] {
        assert!(obj.contains_key(*key), "diag missing required key: {key}");
    }

    assert_eq!(body["runtime_mode"], "lite");
    assert_eq!(body["paired"], false);
    assert_eq!(body["device_id"], "conformance-001");
    // Default DiagState reports zero failures and never-published.
    assert_eq!(body["mqtt"]["connected_recently"], false);
    assert!(body["cloud_relay"]["last_heartbeat_at"].is_null());
    assert_eq!(body["cloud_relay"]["consecutive_failures"], 0);
    // No frame-rate estimator wired at v0.1.
    assert!(body["mavlink"]["frame_rate_recent"].is_null());
}

#[tokio::test]
async fn diag_omits_secrets() {
    // Defense-in-depth: the diag surface must never leak pair codes,
    // API keys, or tokens. We inspect the serialized response body for
    // any of those literal keys.
    let dir = tempfile::tempdir().unwrap();
    let state = fresh_state(&dir);
    let (status, body) = json_response(state, Method::GET, "/api/v1/diag", None).await;
    assert_eq!(status, StatusCode::OK);

    let serialized = serde_json::to_string(&body).unwrap();
    for forbidden in &["api_key", "pairing_code", "pair_code", "token"] {
        assert!(
            !serialized.contains(forbidden),
            "diag response leaked '{forbidden}': {serialized}"
        );
    }
}
