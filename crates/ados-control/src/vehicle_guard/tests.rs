use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::http::Request as HttpRequest;
use axum::routing::post;
use axum::Router;
use serde_json::Value;
use tower::util::ServiceExt;

const RELAY: &str = "/api/v1/ground-station/relay-proxy/0a1b2c3d4e5f";

/// Every flight route the scope map names, bare and relay-wrapped.
fn flight_routes() -> Vec<String> {
    let bare = [
        "/api/command",
        "/api/can/passthrough",
        "/api/v1/ground-station/crsf/channels",
        "/api/v1/ground-station/pic/claim",
    ];
    bare.iter()
        .map(|p| p.to_string())
        .chain(bare.iter().map(|p| format!("{RELAY}{p}")))
        .collect()
}

/// Routes a visor must keep serving: parameter reads and writes, calibration
/// state, firmware info, signing, logs, status.
const NON_FLIGHT: &[(&str, &str)] = &[
    ("GET", "/api/params"),
    ("GET", "/api/params/INS_ACCOFFS_X"),
    ("POST", "/api/params/INS_ACCOFFS_X"),
    ("GET", "/api/status"),
    ("GET", "/api/commands"),
    ("POST", "/api/mavlink/signing/enroll-fc"),
    ("POST", "/api/logs/push"),
];

fn state_for_profile(profile: &str) -> (tempfile::TempDir, AppState) {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.yaml");
    std::fs::write(&config, format!("agent:\n  profile: {profile}\n")).unwrap();
    let pairing_json = dir.path().join("pairing.json");
    std::fs::write(&pairing_json, r#"{"paired": false}"#).unwrap();
    let paths = crate::state::PairingPaths {
        config,
        pairing_json: pairing_json.clone(),
        wfb_key_dir: dir.path().join("wfb"),
        bind_state: dir.path().join("bind-state.json"),
        profile_conf: dir.path().join("profile.conf"),
        mesh_role: dir.path().join("mesh-role"),
        relay_secret: dir.path().join("relay-peer-secret"),
    };
    let state = AppState::new(
        Arc::new(crate::auth::PairingState::with_path(pairing_json)),
        crate::ipc::StateIpcClient::disconnected(),
        crate::ipc::MavlinkIpcClient::new(dir.path().join("mavlink.sock")),
        crate::ipc::LogdQueryClient::new(dir.path().join("logd-query.sock")),
        dir.path().join("board.json"),
        paths,
        Arc::new(crate::dashboard_pin::DashboardPin::with_path(
            dir.path().join("dashboard-pin.json"),
        )),
        Arc::new(crate::mcp::McpTokenStore::with_path(
            dir.path().join("mcp-token.json"),
        )),
    );
    (dir, state)
}

/// A router with the guard over one native route and a fallback standing in
/// for the residual proxy, each counting how often it ran.
fn guarded_router(state: AppState, hits: Arc<AtomicUsize>) -> Router {
    let native_hits = Arc::clone(&hits);
    let fallback_hits = hits;
    Router::new()
        .route(
            "/api/command",
            post(move || {
                let hits = Arc::clone(&native_hits);
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    "sent"
                }
            }),
        )
        .fallback(move || {
            let hits = Arc::clone(&fallback_hits);
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
                "proxied"
            }
        })
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            vehicle_guard,
        ))
        .with_state(state)
}

/// Send one request to a fresh router for `profile`; the status, the body and
/// whether a handler ran.
async fn send(profile: &str, method: &str, uri: &str) -> (StatusCode, Value, usize) {
    let (_dir, state) = state_for_profile(profile);
    let hits = Arc::new(AtomicUsize::new(0));
    let router = guarded_router(state, Arc::clone(&hits));
    let request = HttpRequest::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(r#"{"command":"arm"}"#))
        .unwrap();
    let resp = router.oneshot(request).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    (status, value, hits.load(Ordering::SeqCst))
}

#[test]
fn the_flight_predicate_is_the_scope_maps_flight_class() {
    for path in flight_routes() {
        assert!(is_flight_action(&Method::POST, &path), "{path}");
    }
    for (method, path) in NON_FLIGHT {
        let method = Method::from_bytes(method.as_bytes()).unwrap();
        assert!(!is_flight_action(&method, path), "{method} {path}");
    }
}

#[tokio::test]
async fn a_visor_refuses_every_flight_route_before_any_handler() {
    for path in flight_routes() {
        let (status, body, hits) = send("visor", "POST", &path).await;
        assert_eq!(status, StatusCode::CONFLICT, "{path}");
        assert_eq!(body["error"], "E_NOT_A_VEHICLE", "{path}");
        assert_eq!(
            body["message"],
            "This node is a visor: its flight controller is a sensor hub, not a vehicle"
        );
        assert_eq!(hits, 0, "{path}: no handler may run");
    }
}

#[tokio::test]
async fn an_encoded_or_slash_suffixed_flight_route_is_still_refused() {
    for uri in ["/api/%63ommand", "/api/command/"] {
        let (status, _, hits) = send("visor", "POST", uri).await;
        assert_eq!(status, StatusCode::CONFLICT, "{uri}");
        assert_eq!(hits, 0, "{uri}");
    }
}

#[tokio::test]
async fn a_visor_serves_parameters_calibration_and_reads() {
    for (method, path) in NON_FLIGHT {
        let (status, _, hits) = send("visor", method, path).await;
        assert_eq!(status, StatusCode::OK, "{method} {path}");
        assert_eq!(hits, 1, "{method} {path}");
    }
}

#[tokio::test]
async fn vehicle_profiles_pass_flight_actions_through_unchanged() {
    for profile in ["drone", "ground_station", "ground-station"] {
        for path in flight_routes() {
            let (status, _, hits) = send(profile, "POST", &path).await;
            assert_eq!(status, StatusCode::OK, "{profile} {path}");
            assert_eq!(hits, 1, "{profile} {path}");
        }
    }
}

#[test]
fn the_profile_resolves_from_either_spelling() {
    let (_d, visor) = state_for_profile("visor");
    assert!(node_is_visor(&visor));
    let (_d, gs) = state_for_profile("ground_station");
    assert_eq!(node_profile(&gs), "ground-station");
    assert!(!node_is_visor(&gs));
}

fn manifest() -> ados_plugin_host::manifest::PluginManifest {
    ados_plugin_host::manifest::PluginManifest::from_yaml_text(
        "id: com.example.pod\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0\"\nagent:\n  entrypoint: agent/py/x.py\n  contributes:\n    tools:\n      - name: fire\n        safety_class: flight\n      - name: goto\n        safety_class: flight_action\n      - name: status\n        safety_class: read\n      - name: tune\n        safety_class: admin\n",
    )
    .unwrap()
}

#[test]
fn a_plugin_tool_declared_flight_is_a_flight_action() {
    let m = manifest();
    assert!(is_flight_tool(&m, "fire"));
    assert!(is_flight_tool(&m, "goto"));
    assert!(!is_flight_tool(&m, "status"));
    assert!(!is_flight_tool(&m, "tune"));
    assert!(!is_flight_tool(&m, "absent"));
}
