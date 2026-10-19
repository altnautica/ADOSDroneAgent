//! The HDMI kiosk `/display` write tests.

use super::*;
use serde_json::json;

/// Read a response body as JSON.
async fn body_json(resp: Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

// ── /display ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn put_display_applies_fields_and_persists_to_config() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    std::fs::write(&cfg, "agent:\n  name: gs-1\n").unwrap();

    let update = DisplayUpdate {
        resolution: Some("1080p".to_string()),
        kiosk_enabled: Some(true),
        kiosk_target_url: Some("http://localhost:8080/hud".to_string()),
    };
    let resp = put_display_at(&cfg, &update);
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    // The wire body keeps the {resolution, kiosk_enabled, kiosk_target_url} shape.
    assert_eq!(
        body,
        json!({
            "resolution": "1080p",
            "kiosk_enabled": true,
            "kiosk_target_url": "http://localhost:8080/hud",
        })
    );
    // Persisted under ground_station.kiosk with the config field names; the
    // unrelated agent.name key survives the merge.
    let parsed: serde_norway::Value =
        serde_norway::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
    let kiosk = parsed
        .get("ground_station")
        .and_then(|g| g.get("kiosk"))
        .unwrap();
    assert_eq!(
        kiosk
            .get("resolution")
            .and_then(serde_norway::Value::as_str),
        Some("1080p")
    );
    assert_eq!(
        kiosk.get("enabled").and_then(serde_norway::Value::as_bool),
        Some(true)
    );
    assert_eq!(
        kiosk
            .get("target_url")
            .and_then(serde_norway::Value::as_str),
        Some("http://localhost:8080/hud")
    );
    assert_eq!(
        parsed
            .get("agent")
            .and_then(|a| a.get("name"))
            .and_then(|n| n.as_str()),
        Some("gs-1")
    );
}

#[tokio::test]
async fn put_display_partial_update_keeps_unset_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    let update = DisplayUpdate {
        resolution: Some("720p".to_string()),
        ..Default::default()
    };
    let resp = put_display_at(&cfg, &update);
    let body = body_json(resp).await;
    assert_eq!(body["resolution"], json!("720p"));
    // The unset fields keep their defaults.
    assert_eq!(body["kiosk_enabled"], json!(true));
    assert_eq!(body["kiosk_target_url"], json!(null));
}

#[tokio::test]
async fn put_display_seeds_from_existing_kiosk_and_preserves_other_keys() {
    // An existing kiosk section (a stored target_url + an operator-set
    // minimal_layer) seeds the response; a partial update changes only the
    // supplied field and the merge preserves minimal_layer + a sibling config key.
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    std::fs::write(
            &cfg,
            "ground_station:\n  share_uplink: true\n  kiosk:\n    target_url: http://old/hud\n    minimal_layer: true\n",
        )
        .unwrap();

    let update = DisplayUpdate {
        resolution: Some("720p".to_string()),
        ..Default::default()
    };
    let resp = put_display_at(&cfg, &update);
    let body = body_json(resp).await;
    // resolution takes the request; target_url seeds from the stored value;
    // kiosk_enabled falls to the default (the stored section had no `enabled`).
    assert_eq!(body["resolution"], json!("720p"));
    assert_eq!(body["kiosk_target_url"], json!("http://old/hud"));
    assert_eq!(body["kiosk_enabled"], json!(true));

    let parsed: serde_norway::Value =
        serde_norway::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
    let kiosk = parsed
        .get("ground_station")
        .and_then(|g| g.get("kiosk"))
        .unwrap();
    assert_eq!(
        kiosk
            .get("resolution")
            .and_then(serde_norway::Value::as_str),
        Some("720p")
    );
    assert_eq!(
        kiosk
            .get("target_url")
            .and_then(serde_norway::Value::as_str),
        Some("http://old/hud")
    );
    // The operator-set minimal_layer is NOT clobbered by the display write.
    assert_eq!(
        kiosk
            .get("minimal_layer")
            .and_then(serde_norway::Value::as_bool),
        Some(true)
    );
    // A sibling ground_station key survives the merge.
    assert_eq!(
        parsed
            .get("ground_station")
            .and_then(|g| g.get("share_uplink"))
            .and_then(serde_norway::Value::as_bool),
        Some(true)
    );
}

#[tokio::test]
async fn put_display_invalid_resolution_is_a_400() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    let update = DisplayUpdate {
        resolution: Some("4k".to_string()),
        ..Default::default()
    };
    let resp = put_display_at(&cfg, &update);
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert_eq!(
        body,
        json!({"detail": {"error": {"code": "E_INVALID_RESOLUTION"}}})
    );
    // The config was NOT written (the validation short-circuits the persist).
    assert!(!cfg.exists());
}

#[tokio::test]
async fn put_display_persist_fault_is_a_500_e_ui_save_failed() {
    // A config path whose parent cannot be created → persist fails → 500.
    let dir = tempfile::tempdir().unwrap();
    let blocker = dir.path().join("blocker");
    std::fs::write(&blocker, b"x").unwrap();
    let cfg = blocker.join("config.yaml");
    let resp = put_display_at(
        &cfg,
        &DisplayUpdate {
            kiosk_enabled: Some(true),
            ..Default::default()
        },
    );
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body = body_json(resp).await;
    assert_eq!(body["detail"]["error"]["code"], json!("E_UI_SAVE_FAILED"));
}
