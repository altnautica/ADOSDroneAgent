//! The recordings surface as reached through the REAL router.
//!
//! The per-handler tests cover each route's own logic; what only the router can
//! answer is whether the requests ARRIVE. `DELETE .../recording/{segment}` is
//! registered beside five literal siblings (`/list`, `/segments`, `/clip`,
//! `/start`, `/stop`), and a param route that swallowed one of them, or a
//! matchit insertion conflict that panicked at startup, would only show at the
//! bench.

use super::*;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::util::ServiceExt;

/// A state whose config resolves to `profile`, under a private tempdir.
/// Shared with the other route modules' router-level tests.
pub(crate) fn state_for_profile(profile: &str) -> (tempfile::TempDir, AppState) {
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
        std::sync::Arc::new(crate::auth::PairingState::with_path(pairing_json)),
        crate::ipc::StateIpcClient::disconnected(),
        crate::ipc::MavlinkIpcClient::new(dir.path().join("mavlink.sock")),
        crate::ipc::LogdQueryClient::new(dir.path().join("logd-query.sock")),
        dir.path().join("board.json"),
        paths,
        std::sync::Arc::new(crate::dashboard_pin::DashboardPin::with_path(
            dir.path().join("dashboard-pin.json"),
        )),
        std::sync::Arc::new(crate::mcp::McpTokenStore::with_path(
            dir.path().join("mcp-token.json"),
        )),
    )
    .with_run_dir(dir.path().join("run"));
    // A disarmed vehicle: the armed guard fails closed on a drone with no
    // fresh state, and these tests exercise routing, not the interlock.
    state
        .state
        .set_snapshot_for_test(serde_json::json!({"armed": false}));
    (dir, state)
}

async fn body_text(resp: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8_lossy(&bytes).to_string()
}

/// Send one request through a freshly built router.
async fn send(method: &str, uri: &str) -> (StatusCode, String) {
    // A drone-profile node: every recordings route answers the profile gate's
    // `404 E_PROFILE_MISMATCH`, which is a body only the HANDLER produces. A
    // router miss answers an EMPTY 404 and a wrong-method match answers 405,
    // so the body is what tells "the request arrived" from "it did not".
    let (_dir, state) = state_for_profile("drone");
    let router = build_router(state, false);
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    let resp = router.oneshot(request).await.unwrap();
    let status = resp.status();
    (status, body_text(resp).await)
}

#[tokio::test]
async fn every_recordings_route_is_reachable_through_the_router() {
    for (method, uri) in [
        ("GET", "/api/v1/ground-station/recording/list"),
        ("GET", "/api/v1/ground-station/recording/segments?path=main"),
        (
            "GET",
            "/api/v1/ground-station/recording/clip?path=main&start=2026-09-03T12:00:00Z&duration=30",
        ),
        ("DELETE", "/api/v1/ground-station/recording/seg.mp4"),
    ] {
        let (status, body) = send(method, uri).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "{method} {uri} should hit the profile gate"
        );
        assert!(
            body.contains("E_PROFILE_MISMATCH"),
            "{method} {uri} did not reach its handler; got body {body:?}"
        );
    }
}

#[tokio::test]
async fn a_percent_encoded_traversal_reaches_the_delete_handler_not_a_router_miss() {
    // `%2e%2e%2f` is how a traversal attempt arrives: axum percent-decodes the
    // path parameter, so the handler is what must refuse it. This pins that
    // the request is routed rather than 404'd by matchit, which is why the
    // handler's own traversal gate is load-bearing.
    let (status, body) = send(
        "DELETE",
        "/api/v1/ground-station/recording/%2e%2e%2fconfig.yaml",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        body.contains("E_PROFILE_MISMATCH"),
        "the traversal attempt must reach the handler; got {body:?}"
    );
}

#[tokio::test]
async fn the_delete_param_route_does_not_shadow_its_literal_siblings() {
    // matchit resolves a literal segment ahead of a parameter, so
    // `/recording/list` stays the listing route. The consequence, pinned here
    // so it is a decision rather than a surprise: a `DELETE` of a file
    // literally named `list` is a 405, not a delete.
    let (status, _) = send("DELETE", "/api/v1/ground-station/recording/list").await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    // And the start/stop writes keep their own methods.
    let (status, _) = send("DELETE", "/api/v1/ground-station/recording/start").await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn a_drone_serves_the_wifi_client_routes_natively() {
    // These were registered only on a ground station, so a drone's request
    // fell through to a residual with no handler (405). Every profile now
    // routes them to the native handler, which answers from the Wi-Fi
    // command socket (absent in this test, so an explicit 503).
    for (method, uri) in [
        ("GET", "/api/v1/network/client/status"),
        ("DELETE", "/api/v1/network/client"),
        ("DELETE", "/api/v1/network/client/configured/BenchNet"),
    ] {
        let (status, body) = send(method, uri).await;
        assert_eq!(
            status,
            StatusCode::SERVICE_UNAVAILABLE,
            "{method} {uri}: {body}"
        );
        assert!(
            body.contains("E_WIFI_"),
            "{method} {uri} missed its handler: {body}"
        );
    }
}
