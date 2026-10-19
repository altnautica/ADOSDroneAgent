use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ados_protocol::armed::ARMED_STATE_MAX_AGE;
use axum::body::Body;
use axum::http::Request as HttpRequest;
use axum::routing::post;
use axum::Router;
use tower::util::ServiceExt;

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

fn armed_drone() -> (tempfile::TempDir, AppState) {
    let (dir, state) = state_for_profile("drone");
    state.state.set_snapshot_for_test(json!({"armed": true}));
    (dir, state)
}

/// A router with the guard over one native route and a fallback standing in
/// for the proxy, each counting how often it ran.
fn guarded_router(state: AppState, hits: Arc<AtomicUsize>) -> Router {
    let native_hits = Arc::clone(&hits);
    let fallback_hits = hits;
    Router::new()
        .route(
            "/api/v1/system/restart-supervisor",
            post(move || {
                let hits = Arc::clone(&native_hits);
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    "restarted"
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
            armed_guard,
        ))
        .with_state(state)
}

async fn send(router: Router, method: &str, uri: &str, body: &str) -> (StatusCode, Value) {
    let request = HttpRequest::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = router.oneshot(request).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    (status, value)
}

/// Send to a fresh armed-drone router and return the status and handler hits.
async fn send_armed(method: &str, uri: &str, body: &str) -> (StatusCode, usize) {
    let (_dir, state) = armed_drone();
    let hits = Arc::new(AtomicUsize::new(0));
    let router = guarded_router(state, Arc::clone(&hits));
    let (status, _) = send(router, method, uri, body).await;
    (status, hits.load(Ordering::SeqCst))
}

#[tokio::test]
async fn armed_supervisor_restart_is_refused_until_forced() {
    let (_dir, state) = armed_drone();
    let hits = Arc::new(AtomicUsize::new(0));
    let router = guarded_router(state, Arc::clone(&hits));

    let (status, body) = send(
        router.clone(),
        "POST",
        "/api/v1/system/restart-supervisor",
        "",
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"], "E_ARMED");
    assert_eq!(body["override"], "force");
    assert_eq!(hits.load(Ordering::SeqCst), 0, "the handler must not run");

    let (status, _) = send(
        router.clone(),
        "POST",
        "/api/v1/system/restart-supervisor",
        r#"{"force":true}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(hits.load(Ordering::SeqCst), 1);

    let (status, _) = send(
        router,
        "POST",
        "/api/v1/system/restart-supervisor?force=1",
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(hits.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn armed_proxied_reboot_is_refused_before_the_proxy() {
    let (_dir, state) = armed_drone();
    let hits = Arc::new(AtomicUsize::new(0));
    let router = guarded_router(state, Arc::clone(&hits));

    let (status, body) = send(router.clone(), "POST", "/api/v1/setup/reboot", "").await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"], "E_ARMED");
    assert_eq!(
        hits.load(Ordering::SeqCst),
        0,
        "the proxy must not be reached"
    );

    // A percent-encoded spelling of the same path is the same action.
    let (status, _) = send(router, "POST", "/api/v1/setup/%72eboot", "").await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_stale_or_missing_state_counts_as_armed() {
    let (_dir, state) = state_for_profile("drone");
    state.state.set_aged_snapshot_for_test(
        json!({"armed": false}),
        ARMED_STATE_MAX_AGE + Duration::from_secs(1),
    );
    let hits = Arc::new(AtomicUsize::new(0));
    let router = guarded_router(state.clone(), Arc::clone(&hits));
    let (status, _) = send(router, "POST", "/api/v1/system/restart-supervisor", "").await;
    assert_eq!(status, StatusCode::CONFLICT);

    let (_dir2, empty) = state_for_profile("ground_station");
    let router = guarded_router(empty, Arc::clone(&hits));
    let (status, _) = send(router, "POST", "/api/v1/setup/reboot", "").await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_disarmed_vehicle_and_unguarded_routes_pass_through() {
    let (_dir, state) = state_for_profile("drone");
    state.state.set_snapshot_for_test(json!({"armed": false}));
    let hits = Arc::new(AtomicUsize::new(0));
    let router = guarded_router(state, Arc::clone(&hits));
    let (status, _) = send(router, "POST", "/api/v1/system/restart-supervisor", "").await;
    assert_eq!(status, StatusCode::OK);

    let (status, after) = send_armed("POST", "/api/services/ados-logd/restart", "").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a log-store restart does not drop a link"
    );
    assert_eq!(after, 1);
}

#[tokio::test]
async fn a_node_without_a_vehicle_is_not_guarded() {
    let (_dir, state) = state_for_profile("compute");
    let hits = Arc::new(AtomicUsize::new(0));
    let router = guarded_router(state, Arc::clone(&hits));
    let (status, _) = send(router, "POST", "/api/v1/setup/reboot", "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn the_real_router_refuses_an_armed_proxied_reboot() {
    let (_dir, state) = armed_drone();
    let router = crate::routes::build_router(state, false);
    let (status, body) = send(router, "POST", "/api/v1/setup/reboot", "").await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"], "E_ARMED");
}

#[tokio::test]
async fn apply_with_profile_section_is_refused_while_armed() {
    let body = r#"{"profile":{"profile":"ground_station"},"auto_restart":true}"#;
    let (status, hits) = send_armed("POST", "/api/v1/setup/apply", body).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(hits, 0);
}

#[tokio::test]
async fn apply_with_wfb_section_is_refused_while_armed() {
    let body = r#"{"wfb":{"channel":149}}"#;
    let (status, hits) = send_armed("POST", "/api/v1/setup/apply", body).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(hits, 0);
}

#[tokio::test]
async fn apply_with_network_section_is_refused_while_armed() {
    let body = r#"{"network":{"hotspot":{"enabled":false}}}"#;
    let (status, hits) = send_armed("POST", "/api/v1/setup/apply", body).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(hits, 0);
}

#[tokio::test]
async fn apply_with_regulatory_section_is_refused_while_armed() {
    let body = r#"{"regulatory":{"country":"US"}}"#;
    let (status, hits) = send_armed("POST", "/api/v1/setup/apply", body).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(hits, 0);
}

#[tokio::test]
async fn apply_with_only_ui_passes_while_armed() {
    let body = r#"{"ui":{"theme":"dark"},"profile":null}"#;
    let (status, hits) = send_armed("POST", "/api/v1/setup/apply", body).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(hits, 1, "the body is forwarded unchanged");
}

#[tokio::test]
async fn config_put_of_role_key_is_refused_while_armed() {
    let body = r#"{"key":"ground_station.role","value":"relay"}"#;
    let (status, hits) = send_armed("PUT", "/api/config", body).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(hits, 0);
}

#[tokio::test]
async fn config_put_of_theme_passes_while_armed() {
    let body = r#"{"key":"ui.theme","value":"dark"}"#;
    let (status, hits) = send_armed("PUT", "/api/config", body).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(hits, 1);
}

#[tokio::test]
async fn lora_region_change_is_refused_while_armed() {
    let (status, hits) = send_armed("PUT", "/api/v1/lora", r#"{"region":"EU"}"#).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(hits, 0);
    let (status, _) = send_armed("PUT", "/api/v1/lora", r#"{"region":null}"#).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let body = r#"{"key":"lora.region","value":"EU"}"#;
    let (status, _) = send_armed("PUT", "/api/config", body).await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn lora_power_change_without_a_region_passes_while_armed() {
    let (status, hits) = send_armed("PUT", "/api/v1/lora", r#"{"erp_mw":14}"#).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(hits, 1);
}

#[tokio::test]
async fn force_true_in_body_overrides_a_body_rule() {
    let body = r#"{"wfb":{"channel":149},"force":true}"#;
    let (status, hits) = send_armed("POST", "/api/v1/setup/apply", body).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(hits, 1);
    let body = r#"{"key":"agent.profile","value":"drone","force":true}"#;
    let (status, hits) = send_armed("PUT", "/api/config", body).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(hits, 1);
}

#[tokio::test]
async fn plugin_disable_is_refused_while_armed_and_force_passes() {
    let uri = "/api/plugins/com.example.nav/disable";
    let (status, hits) = send_armed("POST", uri, "").await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(hits, 0);
    let (status, hits) = send_armed("POST", &format!("{uri}?force=1"), "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(hits, 1);
}

#[test]
fn setup_reset_is_not_guarded() {
    assert!(!is_guarded("POST", "/api/v1/setup/reset"));
}

/// Every unit's restart verdict is its table `drops_link` value, in the file
/// name, stem and bare forms the restart route accepts.
#[test]
fn guard_verdict_per_unit() {
    for unit in ados_protocol::units::UNITS {
        let stem = unit.stem();
        let bare = stem.strip_prefix("ados-").unwrap();
        for name in [unit.name, stem, bare] {
            let path = format!("/api/services/{name}/restart");
            assert_eq!(is_guarded("POST", &path), unit.drops_link, "{name}");
        }
    }
    // The exact link-dropping set, so a table edit that drops a link from
    // the interlock is a visible test change.
    let mut dropping: Vec<&str> = ados_protocol::units::UNITS
        .iter()
        .filter(|u| u.drops_link)
        .map(|u| u.stem())
        .collect();
    dropping.sort_unstable();
    assert_eq!(
        dropping,
        [
            "ados-batman",
            "ados-cloud",
            "ados-crsf",
            "ados-dnsmasq-gs",
            "ados-hostapd",
            "ados-input",
            "ados-mavlink",
            "ados-mediamtx-gs",
            "ados-pic",
            "ados-supervisor",
            "ados-swarmbus",
            "ados-uplink-router",
            "ados-video",
            "ados-wfb",
            "ados-wfb-receiver",
            "ados-wfb-relay",
            "ados-wfb-rx",
        ]
    );
    for unit in ["ados-radio", "ados-groundlink", "ados-wfbx"] {
        assert!(!is_guarded(
            "POST",
            &format!("/api/services/{unit}/restart")
        ));
    }
}

#[test]
fn the_guarded_set_covers_each_link_dropping_action() {
    for (method, path) in [
        ("POST", "/api/v1/setup/reboot"),
        ("POST", "/api/v1/setup/reboot/"),
        ("POST", "/api/v1/system/restart-supervisor"),
        ("POST", "/api/services/ados-mavlink/restart"),
        ("POST", "/api/services/ados-wfb-rx/restart"),
        ("POST", "/api/services/ados-wfb.service/restart"),
        ("POST", "/api/services/ados-video/restart"),
        ("POST", "/api/wfb/channel"),
        ("PUT", "/api/wfb/tx-power"),
        ("PUT", "/api/v1/ground-station/wfb"),
        ("DELETE", "/api/v1/ground-station/wfb/pair"),
        ("DELETE", "/api/v1/ground-station/wfb/pair/drone-1"),
        ("POST", "/api/v1/ground-station/factory-reset"),
        ("POST", "/api/v1/setup/profile"),
        ("POST", "/api/params/FRAME_CLASS"),
        ("POST", "/api/v1/ground-station/crsf/params"),
        ("POST", "/api/wfb/pair/local-bind"),
        ("POST", "/api/mavlink/signing/enroll-fc"),
        ("POST", "/api/mavlink/signing/disable-on-fc"),
        ("DELETE", "/api/v1/network/client"),
        ("PUT", "/api/v1/network/client/join"),
        ("DELETE", "/api/v1/network/client/configured/BenchNet"),
        (
            "PUT",
            "/api/v1/network/client/configured/BenchNet/autoconnect",
        ),
        ("POST", "/api/v1/network/mac/pin"),
        ("DELETE", "/api/v1/network/mac/wlan0"),
        ("PUT", "/api/v1/ground-station/network/ap"),
        ("PUT", "/api/v1/ground-station/network/share_uplink"),
        ("PUT", "/api/v1/ground-station/network/client/join"),
        ("PUT", "/api/v1/ground-station/network/ethernet"),
        ("PUT", "/api/v1/ground-station/mesh/config"),
        ("PUT", "/api/v1/ground-station/mesh/gateway_preference"),
        ("POST", "/api/plugins/install"),
        ("POST", "/api/plugins/install_from_url"),
        ("POST", "/api/plugins/install_builtin"),
        ("POST", "/api/plugins/com.example.nav/enable"),
        ("POST", "/api/plugins/com.example.nav/disable"),
        ("DELETE", "/api/plugins/com.example.nav"),
        ("POST", "/api/v1/peripherals/ados.rtl8812eu-radio/action"),
    ] {
        assert!(is_guarded(method, path), "{method} {path} must be guarded");
    }
    for (method, path) in [
        ("GET", "/api/params/FRAME_CLASS"),
        ("GET", "/api/v1/ground-station/wfb"),
        ("POST", "/api/services/ados-logd/restart"),
        ("POST", "/api/v1/ground-station/crsf/channels"),
        ("GET", "/api/wfb/pair"),
        ("GET", "/api/v1/network/mac/adapters"),
        ("GET", "/api/v1/network/client/status"),
        ("POST", "/api/plugins/parse"),
        ("POST", "/api/plugins/com.example.nav/pin"),
        ("POST", "/api/v1/setup/reset"),
        ("POST", "/api/v1/setup/apply"),
        ("PUT", "/api/config"),
    ] {
        assert!(
            !is_guarded(method, path),
            "{method} {path} must not be guarded"
        );
    }
}
