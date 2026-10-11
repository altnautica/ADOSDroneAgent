use super::*;

#[test]
fn fc_triple_is_disconnected_with_nulls_when_the_snapshot_is_absent() {
    let (c, p, b) = fc_from_snapshot(None);
    assert_eq!(c, json!(false));
    // Pairing-info uses null (not the status route's "" / 0).
    assert_eq!(p, Value::Null);
    assert_eq!(b, Value::Null);
}

#[test]
fn fc_triple_reads_a_connected_snapshot() {
    let snap = json!({
        "fc_connected": true,
        "fc_port": "/dev/ttyACM0",
        "fc_baud": 115200,
    });
    let (c, p, b) = fc_from_snapshot(Some(&snap));
    assert_eq!(c, json!(true));
    assert_eq!(p, json!("/dev/ttyACM0"));
    assert_eq!(b, json!(115200));
}

#[test]
fn fc_triple_treats_empty_port_and_zero_baud_as_null() {
    let snap = json!({
        "fc_connected": false,
        "fc_port": "",
        "fc_baud": 0,
    });
    let (c, p, b) = fc_from_snapshot(Some(&snap));
    assert_eq!(c, json!(false));
    assert_eq!(p, Value::Null);
    assert_eq!(b, Value::Null);
}

#[test]
fn bind_state_is_null_for_an_absent_sentinel() {
    let dir = tempfile::tempdir().unwrap();
    let paths = test_paths(dir.path());
    assert_eq!(read_bind_state(&paths), Value::Null);
}

#[test]
fn bind_state_is_null_when_the_sentinel_has_no_state() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("bind-state.json"), r#"{"phase":"x"}"#).unwrap();
    let paths = test_paths(dir.path());
    assert_eq!(read_bind_state(&paths), Value::Null);
}

#[test]
fn bind_state_folds_the_six_fields_when_state_is_present() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
            dir.path().join("bind-state.json"),
            r#"{"state":"binding","phase":"key_transfer","active":true,"error":null,"finished_at":123.0,"fingerprint":"ab"}"#,
        )
        .unwrap();
    let paths = test_paths(dir.path());
    let bs = read_bind_state(&paths);
    let obj = bs.as_object().expect("bind_state object");
    let keys: std::collections::BTreeSet<_> = obj.keys().cloned().collect();
    let want: std::collections::BTreeSet<_> = [
        "state",
        "phase",
        "active",
        "error",
        "finished_at",
        "fingerprint",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    assert_eq!(keys, want, "bind_state folds exactly the six FastAPI keys");
    assert_eq!(bs["state"], json!("binding"));
    assert_eq!(bs["active"], json!(true));
    assert_eq!(bs["error"], Value::Null);
}

#[test]
fn bind_state_missing_active_folds_to_false() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("bind-state.json"), r#"{"state":"done"}"#).unwrap();
    let paths = test_paths(dir.path());
    let bs = read_bind_state(&paths);
    assert_eq!(bs["active"], json!(false));
    assert_eq!(bs["phase"], Value::Null);
}

#[cfg(unix)]
#[tokio::test]
async fn a_code_that_cannot_be_persisted_is_refused_not_served() {
    // No code on file and the pairing directory is read-only: the route must
    // not hand the operator a code the device does not hold.
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let ro = dir.path().join("ro");
    std::fs::create_dir(&ro).unwrap();
    std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o555)).unwrap();
    let state = AppState::new(
        std::sync::Arc::new(crate::auth::PairingState::with_path(
            ro.join("pairing.json"),
        )),
        crate::ipc::StateIpcClient::disconnected(),
        crate::ipc::MavlinkIpcClient::new(dir.path().join("absent-mavlink.sock")),
        crate::ipc::LogdQueryClient::new(dir.path().join("absent-logd.sock")),
        dir.path().join("board.json"),
        test_paths(&ro),
        std::sync::Arc::new(crate::dashboard_pin::DashboardPin::with_path(
            dir.path().join("dashboard-pin.json"),
        )),
        std::sync::Arc::new(crate::mcp::McpTokenStore::with_path(
            dir.path().join("mcp-token.json"),
        )),
    );
    let resp = get_pairing_code(State(state), Some(Extension(CallerClass::OnBox))).await;
    std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(!ro.join("pairing.json").exists());
}

fn test_state(dir: &std::path::Path) -> AppState {
    AppState::new(
        std::sync::Arc::new(crate::auth::PairingState::with_path(
            dir.join("pairing.json"),
        )),
        crate::ipc::StateIpcClient::disconnected(),
        crate::ipc::MavlinkIpcClient::new(dir.join("absent-mavlink.sock")),
        crate::ipc::LogdQueryClient::new(dir.join("absent-logd.sock")),
        dir.join("board.json"),
        test_paths(dir),
        std::sync::Arc::new(crate::dashboard_pin::DashboardPin::with_path(
            dir.join("dashboard-pin.json"),
        )),
        std::sync::Arc::new(crate::mcp::McpTokenStore::with_path(
            dir.join("mcp-token.json"),
        )),
    )
}

fn claim_body(user: &str) -> Json<ClaimRequest> {
    Json(ClaimRequest {
        user_id: user.to_string(),
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_claims_hand_out_one_key() {
    // Two browsers claiming at once used to both get 200 with different
    // keys; the loser's key then failed every request.
    let dir = tempfile::tempdir().unwrap();
    let state = test_state(dir.path());
    let calls: Vec<_> = (0..8)
        .map(|i| {
            let state = state.clone();
            tokio::spawn(async move {
                claim_pairing(State(state), HeaderMap::new(), claim_body(&format!("u{i}")))
                    .await
                    .status()
            })
        })
        .collect();
    let mut ok = 0;
    for call in calls {
        match call.await.unwrap() {
            StatusCode::OK => ok += 1,
            StatusCode::CONFLICT => {}
            other => panic!("unexpected {other}"),
        }
    }
    assert_eq!(ok, 1, "exactly one claim succeeds");
}

#[tokio::test]
async fn a_relayed_claim_is_refused_even_on_an_unpaired_node() {
    let dir = tempfile::tempdir().unwrap();
    let state = test_state(dir.path());
    let mut headers = HeaderMap::new();
    headers.insert(crate::auth::RELAYED_HEADER, "1".parse().unwrap());
    let resp = claim_pairing(State(state), headers, claim_body("radio")).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert!(
        !dir.path().join("pairing.json").exists(),
        "nothing was minted"
    );
}

#[tokio::test]
async fn the_gate_sees_a_claim_and_an_unpair_at_once() {
    // The edge gate's pairing posture is TTL-cached. Primed as unpaired, it
    // used to answer the claiming browser's first keyed request (sent
    // straight after the claim) with the unpaired 403.
    use ados_protocol::pairing_posture::Pairing;
    let dir = tempfile::tempdir().unwrap();
    let state = test_state(dir.path());
    assert_eq!(state.pairing.current(), Pairing::Unpaired);
    let resp = claim_pairing(State(state.clone()), HeaderMap::new(), claim_body("u")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let key = serde_json::from_slice::<Value>(&body).unwrap()["api_key"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(state.pairing.current(), Pairing::Paired(key.clone()));
    assert!(state.pairing.credential_valid(Some(&key)));

    let resp = unpair(State(state.clone())).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(state.pairing.current(), Pairing::Unpaired);
    assert!(!state.pairing.credential_valid(Some(&key)));
}

#[tokio::test]
async fn an_expired_code_is_replaced_and_a_live_one_is_kept() {
    let dir = tempfile::tempdir().unwrap();
    let state = test_state(dir.path());
    let pairing = dir.path().join("pairing.json");
    std::fs::write(
        &pairing,
        r#"{"pairing_code":"OLD234","code_created_at":1.0,"pending_api_key":"ados_k"}"#,
    )
    .unwrap();
    let caller = || Some(Extension(CallerClass::OnBox));
    let resp = get_pairing_code(State(state.clone()), caller()).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let first = PairingDoc::read(&pairing).unwrap().pairing_code.unwrap();
    assert_ne!(first, "OLD234", "a day-old code is regenerated");
    assert_eq!(
        PairingDoc::read(&pairing)
            .unwrap()
            .pending_api_key
            .as_deref(),
        Some("ados_k"),
        "the pending key the beacon advertised survives a new code"
    );
    let _ = get_pairing_code(State(state), caller()).await;
    assert_eq!(
        PairingDoc::read(&pairing).unwrap().pairing_code.unwrap(),
        first,
        "a live code is stable"
    );
}

#[tokio::test]
async fn a_live_code_without_a_pending_key_gets_one_and_keeps_the_code() {
    let dir = tempfile::tempdir().unwrap();
    let state = test_state(dir.path());
    let pairing = dir.path().join("pairing.json");
    let fresh = now_unix_seconds();
    std::fs::write(
        &pairing,
        format!(r#"{{"pairing_code":"LIVE23","code_created_at":{fresh}}}"#),
    )
    .unwrap();
    let resp = get_pairing_code(State(state), Some(Extension(CallerClass::OnBox))).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let doc = PairingDoc::read(&pairing).unwrap();
    assert_eq!(doc.pairing_code.as_deref(), Some("LIVE23"));
    assert!(doc.pending_api_key.unwrap().starts_with("ados_"));
}

async fn accept_body(resp: Response) -> (StatusCode, Value) {
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

fn accept_req(code: &str) -> Json<AcceptCodeRequest> {
    Json(AcceptCodeRequest {
        code: code.to_string(),
    })
}

#[tokio::test]
async fn a_malformed_accept_code_is_refused_with_every_field_and_no_write() {
    let dir = tempfile::tempdir().unwrap();
    let state = test_state(dir.path());
    // Separators are dropped before the length check, so a 5-character
    // code is refused however it is punctuated.
    let resp = accept_pairing_code(State(state), HeaderMap::new(), accept_req("ab-c 23")).await;
    let (status, body) = accept_body(resp).await;
    assert_eq!(status, StatusCode::OK, "the outcome travels in `ok`");
    assert_eq!(
        body,
        json!({
            "ok": false,
            "error": "invalid_code",
            "message": "Pairing code must be 6 characters.",
            "owner_id": null,
            "paired_at": null,
            "device_id": null,
        })
    );
    assert!(!dir.path().join("pairing.json").exists());
}

#[tokio::test]
async fn a_relayed_accept_is_refused_before_anything_is_minted() {
    let dir = tempfile::tempdir().unwrap();
    let state = test_state(dir.path());
    let mut headers = HeaderMap::new();
    headers.insert(crate::auth::RELAYED_HEADER, "1".parse().unwrap());
    let resp = accept_pairing_code(State(state), headers, accept_req("ABC234")).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert!(!dir.path().join("pairing.json").exists());
}

#[tokio::test]
async fn a_paired_node_refuses_an_accept_and_keeps_its_key() {
    let dir = tempfile::tempdir().unwrap();
    let state = test_state(dir.path());
    let pairing = dir.path().join("pairing.json");
    let before = r#"{"paired":true,"api_key":"ados_k","owner_id":"u1","paired_at":1.0}"#;
    std::fs::write(&pairing, before).unwrap();
    let resp = accept_pairing_code(State(state), HeaderMap::new(), accept_req("ABC234")).await;
    let (_status, body) = accept_body(resp).await;
    assert_eq!(body["ok"], json!(false));
    assert_eq!(body["error"], json!("already_paired"));
    assert_eq!(std::fs::read_to_string(&pairing).unwrap(), before);
}

#[test]
fn a_backend_url_is_mapped_to_its_site_origin() {
    assert_eq!(
        convex_site_url("http://192.168.1.50:3210/"),
        "http://192.168.1.50:3211"
    );
    assert_eq!(
        convex_site_url("https://convex.altnautica.com"),
        "https://convex-site.altnautica.com"
    );
    assert_eq!(
        convex_site_url("https://convex-site.altnautica.com"),
        "https://convex-site.altnautica.com"
    );
    assert_eq!(convex_site_url("   "), "");
}

fn test_paths(dir: &std::path::Path) -> PairingPaths {
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
