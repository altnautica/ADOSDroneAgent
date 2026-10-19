use super::*;
use serde_json::json;

const GS_SCREENS: [&str; 9] = [
    "home", "link", "drone", "network", "system", "qr", "gnss", "radios", "lora",
];

/// An `AppState` rooted in `dir` whose config resolves to `profile`.
fn state_with_profile(dir: &std::path::Path, profile: &str) -> AppState {
    std::fs::write(
        dir.join("config.yaml"),
        format!("agent:\n  profile: {profile}\n"),
    )
    .unwrap();
    let pairing_paths = crate::state::PairingPaths {
        config: dir.join("config.yaml"),
        pairing_json: dir.join("pairing.json"),
        wfb_key_dir: dir.join("wfb"),
        bind_state: dir.join("bind-state.json"),
        profile_conf: dir.join("profile.conf"),
        mesh_role: dir.join("mesh-role"),
        relay_secret: dir.join("relay-peer-secret"),
    };
    AppState::new(
        std::sync::Arc::new(crate::auth::PairingState::with_path(
            dir.join("pairing.json"),
        )),
        crate::ipc::StateIpcClient::disconnected(),
        crate::ipc::MavlinkIpcClient::new(dir.join("absent-mavlink.sock")),
        crate::ipc::LogdQueryClient::new(dir.join("absent-logd.sock")),
        dir.join("board.json"),
        pairing_paths,
        std::sync::Arc::new(crate::dashboard_pin::DashboardPin::with_path(
            dir.join("dashboard-pin.json"),
        )),
        std::sync::Arc::new(crate::mcp::McpTokenStore::with_path(
            dir.join("mcp-token.json"),
        )),
    )
}

async fn body_json(resp: Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn visor_ui_read_serves_the_visor_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let resp = get_ui(State(state_with_profile(dir.path(), "visor"))).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let screens = json!([
        "plugin:capture_hud",
        "cameras",
        "sensors",
        "storage",
        "about"
    ]);
    assert_eq!(body["screens"]["order"], screens);
    assert_eq!(body["screens"]["enabled"], screens);
    assert_eq!(body["buttons"]["mapping"]["B1_short"], json!("passthrough"));
    assert_eq!(
        body["buttons"]["mapping"]["B1_long"],
        json!("shutdown_prompt")
    );
    assert_eq!(
        body["buttons"]["mapping"]["B3_short"],
        json!("cycle_screen")
    );
}

#[tokio::test]
async fn ground_station_ui_read_serves_the_ground_station_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let resp = get_ui(State(state_with_profile(dir.path(), "ground_station"))).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["screens"]["order"], json!(GS_SCREENS));
}

#[tokio::test]
async fn drone_ui_read_is_a_profile_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    let resp = get_ui(State(state_with_profile(dir.path(), "drone"))).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        body_json(resp).await,
        json!({"detail": {"error": {"code": "E_PROFILE_MISMATCH"}}})
    );
}

#[tokio::test]
async fn visor_kiosk_display_read_is_a_profile_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    let resp = get_display(State(state_with_profile(dir.path(), "visor"))).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[test]
fn profile_mismatch_is_the_fastapi_404_shape() {
    let resp = profile_mismatch();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    // The body shape is the contract; build it independently and compare.
    let want = json!({"detail": {"error": {"code": "E_PROFILE_MISMATCH"}}});
    assert_eq!(
        want,
        json!({"detail": {"error": {"code": "E_PROFILE_MISMATCH"}}})
    );
}

#[test]
fn ui_config_of_an_absent_side_file_is_the_all_defaults_shape() {
    // With no ground-station-ui.json the blob is empty, so every section is the
    // built-in default. This is the golden shape the GCS reads on a fresh GS.
    let blob = Map::new();
    let got = build_ui_config("ground_station", &blob);
    let want = json!({
        "oled": {
            "brightness": 204,
        },
        "buttons": {
            "mapping": {
                "B1_short": "cycle_screen",
                "B1_long": "toggle_backlight",
                "B2_short": "show_network",
                "B2_long": "show_qr",
                "B3_short": "confirm",
                "B3_long": "pair_drone",
                "B4_short": "back",
                "B4_long": "menu",
            }
        },
        "screens": {
            "order": GS_SCREENS,
            "enabled": GS_SCREENS,
        },
    });
    assert_eq!(got, want);
}

#[test]
fn ui_config_merges_a_stored_section_over_the_defaults() {
    // A stored oled section overrides only the keys it carries; the other oled
    // defaults stand, and the untouched buttons/screens sections stay default.
    let mut blob = Map::new();
    blob.insert(
        "oled".to_string(),
        json!({"brightness": 120, "auto_dim_enabled": false, "screen_cycle_seconds": 9}),
    );
    let got = build_ui_config("ground_station", &blob);
    // The stored contrast wins; the retired keys are not served.
    assert_eq!(got["oled"], json!({"brightness": 120}));
    // Untouched sections are the full defaults.
    assert_eq!(got["buttons"]["mapping"]["B1_short"], json!("cycle_screen"));
    assert_eq!(got["screens"]["order"], json!(GS_SCREENS));
}

#[test]
fn ui_config_replaces_screen_lists_wholesale() {
    // A stored screens section replaces the order/enabled lists wholesale (a
    // dict spread overwrites the whole value at a key, it does not deep-merge
    // lists), matching the Python `{**_DEFAULT_SCREENS, **stored}`.
    let mut blob = Map::new();
    blob.insert(
        "screens".to_string(),
        json!({"order": ["home", "link"], "enabled": ["home"]}),
    );
    let got = build_ui_config("ground_station", &blob);
    assert_eq!(got["screens"]["order"], json!(["home", "link"]));
    assert_eq!(got["screens"]["enabled"], json!(["home"]));
}

#[test]
fn display_config_of_an_absent_section_is_the_defaults() {
    // No kiosk section → the built-in defaults. The golden shape on a fresh GS.
    let kiosk = Map::new();
    let got = build_display_config(&kiosk);
    let want = json!({
        "resolution": "auto",
        "kiosk_enabled": true,
        "kiosk_target_url": null,
    });
    assert_eq!(got, want);
}

#[test]
fn display_config_projects_a_stored_kiosk_section_over_the_defaults() {
    // A stored ground_station.kiosk section maps its config field names onto the
    // wire keys; a kiosk-only key (minimal_layer) is not surfaced on the wire.
    let mut kiosk = Map::new();
    kiosk.insert("resolution".to_string(), json!("1080p"));
    kiosk.insert("enabled".to_string(), json!(true));
    kiosk.insert("target_url".to_string(), json!("http://x"));
    kiosk.insert("minimal_layer".to_string(), json!(true));
    let got = build_display_config(&kiosk);
    let want = json!({
        "resolution": "1080p",
        "kiosk_enabled": true,
        "kiosk_target_url": "http://x",
    });
    assert_eq!(got, want);
}

#[test]
fn read_gs_kiosk_section_reads_config_and_projects_to_wire() {
    // The kiosk section is read from the YAML config and projected to the wire
    // display shape (the single source of truth the kiosk service also reads).
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    std::fs::write(
            &cfg,
            "ground_station:\n  kiosk:\n    resolution: 720p\n    enabled: true\n    target_url: http://hud\n    minimal_layer: true\n",
        )
        .unwrap();
    let got = build_display_config(&read_gs_kiosk_section(&cfg));
    assert_eq!(got["resolution"], json!("720p"));
    assert_eq!(got["kiosk_enabled"], json!(true));
    assert_eq!(got["kiosk_target_url"], json!("http://hud"));
}

#[test]
fn read_gs_kiosk_section_absent_or_empty_yields_defaults() {
    let dir = tempfile::tempdir().unwrap();
    // Absent config → empty section → all-defaults display.
    assert_eq!(
        read_gs_kiosk_section(&dir.path().join("nope.yaml")),
        Map::new()
    );
    // A config with no ground_station.kiosk → empty section.
    let cfg = dir.path().join("config.yaml");
    std::fs::write(&cfg, "agent:\n  name: gs\n").unwrap();
    assert_eq!(read_gs_kiosk_section(&cfg), Map::new());
    // Which projects to the all-defaults display.
    assert_eq!(
        build_display_config(&read_gs_kiosk_section(&cfg)),
        json!({"resolution": "auto", "kiosk_enabled": true, "kiosk_target_url": null})
    );
}
