use std::path::Path;
use std::sync::Arc;

use axum::http::Method;
use serde_json::Value;

use super::*;
use crate::auth::PairingState;
use crate::security_state::SecurityState;
use crate::state::PairingPaths;

const PAIRING_KEY: &str = "pairing-key-for-tests";
const OLD_OPERATOR_KEY: &str = "operator-key-before-rotation";
const ROTATE_PATH: &str = "/api/v1/security/api-key/rotate";
const CLEAR_PATH: &str = "/api/v1/security/api-key";

/// A paired node whose config carries an operator key and an unrelated
/// section, with the credential check and the routes sharing one live posture
/// that re-checks the file only at the production interval. A change seen on
/// the next check, with no sleep, is one the route made visible itself.
fn paired_node(dir: &Path) -> AppState {
    std::fs::write(
        dir.join("pairing.json"),
        format!(r#"{{"paired": true, "api_key": "{PAIRING_KEY}"}}"#),
    )
    .unwrap();
    std::fs::write(
        dir.join("config.yaml"),
        format!("agent:\n  name: bench-node\nsecurity:\n  api:\n    api_key: {OLD_OPERATOR_KEY}\n"),
    )
    .unwrap();
    node(dir)
}

fn node(dir: &Path) -> AppState {
    let security = Arc::new(SecurityState::new(dir.join("config.yaml")));
    let pairing = Arc::new(
        PairingState::with_path(dir.join("pairing.json")).with_security(Arc::clone(&security)),
    );
    AppState::new(
        pairing,
        crate::ipc::StateIpcClient::disconnected(),
        crate::ipc::MavlinkIpcClient::new(dir.join("absent-mavlink.sock")),
        crate::ipc::LogdQueryClient::new(dir.join("absent-logd.sock")),
        dir.join("board.json"),
        test_paths(dir),
        Arc::new(crate::dashboard_pin::DashboardPin::with_path(
            dir.join("dashboard-pin.json"),
        )),
        Arc::new(crate::mcp::McpTokenStore::with_path(
            dir.join("mcp-token.json"),
        )),
    )
    .with_security(security)
}

fn test_paths(dir: &Path) -> PairingPaths {
    PairingPaths {
        config: dir.join("config.yaml"),
        pairing_json: dir.join("pairing.json"),
        wfb_key_dir: dir.join("wfb"),
        bind_state: dir.join("bind-state.json"),
        profile_conf: dir.join("profile.conf"),
        mesh_role: dir.join("mesh-role"),
        relay_secret: dir.join("relay-peer-secret"),
    }
}

fn on_box() -> Option<Extension<CallerClass>> {
    Some(Extension(CallerClass::OnBox))
}

fn relayed() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(RELAYED_HEADER, "1".parse().unwrap());
    headers
}

async fn body_of(resp: Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn config_text(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("config.yaml")).unwrap()
}

#[tokio::test]
async fn rotate_returns_a_fresh_key_once_with_its_stamp() {
    let dir = tempfile::tempdir().unwrap();
    let state = paired_node(dir.path());
    let resp = rotate_api_key(State(state), on_box(), HeaderMap::new()).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_of(resp).await;
    let object = body.as_object().unwrap();
    let keys: Vec<&str> = object.keys().map(String::as_str).collect();
    assert_eq!(keys.len(), 3, "exactly ok, api_key, rotated_at: {keys:?}");
    assert_eq!(body["ok"], json!(true));
    let key = body["api_key"].as_str().unwrap();
    // `ados_` + url-safe base64 of 32 bytes, no padding: 43 characters.
    let encoded = key.strip_prefix("ados_").expect("the pairing key's prefix");
    assert_eq!(encoded.len(), 43);
    assert!(encoded
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    let stamp = body["rotated_at"].as_str().unwrap();
    let parsed = OffsetDateTime::parse(stamp, &Rfc3339).expect("an RFC 3339 stamp");
    assert_eq!(parsed.offset(), time::UtcOffset::UTC);
}

#[tokio::test]
async fn after_rotate_the_old_key_is_refused_and_the_new_one_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let state = paired_node(dir.path());
    assert!(state.pairing.credential_valid(Some(OLD_OPERATOR_KEY)));

    let resp = rotate_api_key(State(state.clone()), on_box(), HeaderMap::new()).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_of(resp).await;
    let new_key = body["api_key"].as_str().unwrap();

    assert!(!state.pairing.credential_valid(Some(OLD_OPERATOR_KEY)));
    assert!(state.pairing.credential_valid(Some(new_key)));
    assert!(
        state.pairing.credential_valid(Some(PAIRING_KEY)),
        "the pairing key is untouched"
    );
    // The write merged into the document rather than replacing it.
    let text = config_text(dir.path());
    assert!(text.contains("bench-node"), "{text}");
    assert!(text.contains(new_key));
    assert!(!text.contains(OLD_OPERATOR_KEY));
}

#[tokio::test]
async fn back_to_back_rotations_each_take_effect_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let state = paired_node(dir.path());
    let mut previous = OLD_OPERATOR_KEY.to_string();
    for _ in 0..3 {
        // Prime the cached posture, then rotate within the same re-check
        // interval: the replacement key is the same length, so only the
        // route's own invalidation can surface it.
        assert!(state.pairing.credential_valid(Some(&previous)));
        let resp = rotate_api_key(State(state.clone()), on_box(), HeaderMap::new()).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let key = body_of(resp).await["api_key"].as_str().unwrap().to_string();
        assert!(
            !state.pairing.credential_valid(Some(&previous)),
            "{previous}"
        );
        assert!(state.pairing.credential_valid(Some(&key)));
        previous = key;
    }
}

#[tokio::test]
async fn clear_removes_the_operator_key_and_keeps_the_pairing_key() {
    let dir = tempfile::tempdir().unwrap();
    let state = paired_node(dir.path());
    assert!(state.pairing.credential_valid(Some(OLD_OPERATOR_KEY)));
    let resp = clear_api_key(State(state.clone()), on_box(), HeaderMap::new()).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_of(resp).await, json!({ "ok": true }));

    assert!(!state.pairing.credential_valid(Some(OLD_OPERATOR_KEY)));
    assert!(state.pairing.credential_valid(Some(PAIRING_KEY)));
    assert_eq!(state.security.config_or_default().security.api.api_key, "");
    let text = config_text(dir.path());
    assert!(!text.contains(KEY_LEAF), "{text}");
    assert!(text.contains("bench-node"), "{text}");
}

#[tokio::test]
async fn clearing_an_unset_key_writes_nothing_and_answers_ok() {
    let dir = tempfile::tempdir().unwrap();
    let state = node(dir.path());
    let resp = clear_api_key(State(state), on_box(), HeaderMap::new()).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_of(resp).await, json!({ "ok": true }));
    assert!(!dir.path().join("config.yaml").exists());
}

#[tokio::test]
async fn a_relayed_request_is_refused_and_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let state = paired_node(dir.path());
    let before = config_text(dir.path());

    // A radio-relayed request arrives on loopback, so it is classed on-box;
    // the relay marker alone must refuse it.
    let rotate = rotate_api_key(State(state.clone()), on_box(), relayed()).await;
    assert_eq!(rotate.status(), StatusCode::FORBIDDEN);
    assert!(body_of(rotate).await.get("api_key").is_none());
    let clear = clear_api_key(State(state.clone()), on_box(), relayed()).await;
    assert_eq!(clear.status(), StatusCode::FORBIDDEN);

    assert_eq!(config_text(dir.path()), before);
    assert!(state.pairing.credential_valid(Some(OLD_OPERATOR_KEY)));
}

#[tokio::test]
async fn a_remote_or_unclassified_caller_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let state = paired_node(dir.path());
    let before = config_text(dir.path());
    for caller in [Some(Extension(CallerClass::Remote)), None] {
        let rotate = rotate_api_key(State(state.clone()), caller, HeaderMap::new()).await;
        assert_eq!(rotate.status(), StatusCode::FORBIDDEN);
        let clear = clear_api_key(State(state.clone()), caller, HeaderMap::new()).await;
        assert_eq!(clear.status(), StatusCode::FORBIDDEN);
    }
    assert_eq!(config_text(dir.path()), before);
}

#[tokio::test]
async fn the_operator_lan_may_change_the_key() {
    let dir = tempfile::tempdir().unwrap();
    let state = paired_node(dir.path());
    let caller = Some(Extension(CallerClass::OperatorLan));
    let resp = clear_api_key(State(state), caller, HeaderMap::new()).await;
    assert_eq!(resp.status(), StatusCode::OK);
}

#[test]
fn both_routes_are_registered_natively() {
    let table = crate::routing::native_route_table();
    for (method, path) in [("POST", ROTATE_PATH), ("DELETE", CLEAR_PATH)] {
        assert!(
            table.iter().any(|(m, p)| m == method && *p == path),
            "{method} {path} is not registered"
        );
    }
}

#[test]
fn no_mcp_token_scope_reaches_either_route() {
    assert_eq!(crate::mcp::route_scope(&Method::POST, ROTATE_PATH), None);
    assert_eq!(crate::mcp::route_scope(&Method::DELETE, CLEAR_PATH), None);
}
