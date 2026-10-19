use super::*;
use serde_json::json;

const GS: &str = "ground_station";
const GS_SCREENS: [&str; 9] = ados_protocol::panel_ui::GROUND_STATION_SCREENS;

/// Read a response body as JSON.
async fn body_json(resp: Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[test]
fn ui_reloads_name_units_that_are_actually_installed() {
    // A reload signalled at a unit the installer does not ship is a silent
    // no-op: the write returns 200 and the running daemon keeps its old
    // mapping until it next restarts. This previously pointed at
    // `ados-buttons.service`, which the installer always tore down, while
    // the daemon that owns the mapping (`ados-pic`) was never signalled.
    //
    // Reading the source keeps the assertion honest without a systemd stub:
    // the check is which unit name the call sites carry.
    let src = include_str!("../gs_ui_write.rs");
    for unit in ["ados-pic.service", "ados-oled.service"] {
        assert!(
            src.contains(&format!("signal_reload(\"{unit}\")")),
            "expected a reload signalled at {unit}"
        );
        let shipped = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../data/systemd")
            .join(unit);
        assert!(
            shipped.exists(),
            "{unit} is signalled for a UI reload but is not a shipped unit"
        );
    }
    assert!(
        !src.contains("signal_reload(\"ados-buttons.service\")"),
        "ados-buttons.service is no longer shipped; signalling it is a no-op"
    );
}

// ── profile-mismatch + error envelopes ────────────────────────────────────

#[tokio::test]
async fn profile_mismatch_is_the_fastapi_404_shape() {
    let resp = profile_mismatch();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body = body_json(resp).await;
    assert_eq!(
        body,
        json!({"detail": {"error": {"code": "E_PROFILE_MISMATCH"}}})
    );
}

#[tokio::test]
async fn brightness_over_range_is_the_fastapi_422_envelope() {
    // The exact FastAPI/Pydantic-v2 422 body for brightness=999 (le=255).
    let resp = check_brightness(Some(999)).unwrap();
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = body_json(resp).await;
    assert_eq!(
        body,
        json!({
            "detail": [{
                "type": "less_than_equal",
                "loc": ["body", "brightness"],
                "msg": "Input should be less than or equal to 255",
                "input": 999,
                "ctx": {"le": 255},
                "url": "https://errors.pydantic.dev/2.11/v/less_than_equal",
            }]
        })
    );
}

#[tokio::test]
async fn gs_ui_write_rejects_auto_dim_key() {
    // The retired keys are no longer part of the body: the axum extractor the
    // route uses answers 422 before the handler runs.
    use axum::extract::FromRequest;
    for body in [
        r#"{"auto_dim_enabled": true}"#,
        r#"{"brightness": 100, "screen_cycle_seconds": 5}"#,
    ] {
        let req = axum::http::Request::builder()
            .method("PUT")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body))
            .unwrap();
        let rejection = Json::<OledUpdate>::from_request(req, &())
            .await
            .expect_err("retired key must be rejected");
        assert_eq!(
            rejection.into_response().status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }
    let ok: OledUpdate = serde_json::from_str(r#"{"brightness": 100}"#).unwrap();
    assert_eq!(ok.brightness, Some(100));
}

#[test]
fn bounds_accept_in_range_and_absent_values() {
    assert!(check_brightness(None).is_none());
    assert!(check_brightness(Some(0)).is_none());
    assert!(check_brightness(Some(255)).is_none());
}

// ── /ui/oled ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn put_oled_applies_fields_persists_and_echoes_full_blob() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    std::fs::write(&cfg, "agent:\n  name: gs-1\n").unwrap();

    let update = OledUpdate {
        brightness: Some(120),
    };
    let resp = put_ui_oled_at(&cfg, GS, &update);
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    // The full UI blob, with the oled section mutated, and nothing but the
    // contrast in it.
    assert_eq!(body["oled"], json!({"brightness": 120}));
    // The untouched sections are the full defaults.
    assert_eq!(
        body["buttons"]["mapping"]["B1_short"],
        json!("cycle_screen")
    );
    assert_eq!(body["screens"]["order"], json!(GS_SCREENS));

    // The mutated section landed under ground_station.ui.oled, agent.name survived.
    let parsed: serde_norway::Value =
        serde_norway::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
    let oled = parsed
        .get("ground_station")
        .and_then(|g| g.get("ui"))
        .and_then(|u| u.get("oled"))
        .unwrap();
    assert_eq!(
        oled.get("brightness").and_then(serde_norway::Value::as_i64),
        Some(120)
    );
    assert!(oled.get("auto_dim_enabled").is_none());
    assert!(oled.get("screen_cycle_seconds").is_none());
    assert_eq!(
        parsed
            .get("agent")
            .and_then(|a| a.get("name"))
            .and_then(|n| n.as_str()),
        Some("gs-1")
    );
}

#[tokio::test]
async fn put_oled_strips_retired_keys_from_the_persisted_section() {
    // A config written by an earlier release still carries the retired keys;
    // the next write keeps the stored brightness and drops them on disk too.
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    std::fs::write(
            &cfg,
            "ground_station:\n  ui:\n    oled:\n      brightness: 40\n      screen_cycle_seconds: 9\n      auto_dim_enabled: false\n",
        )
        .unwrap();

    let resp = put_ui_oled_at(&cfg, GS, &OledUpdate::default());
    let body = body_json(resp).await;
    assert_eq!(body["oled"], json!({"brightness": 40}));
    let read = crate::routes::gs_ui_read::read_gs_section(&cfg, "ui");
    assert_eq!(read["oled"], json!({"brightness": 40}));
}

#[tokio::test]
async fn a_second_write_keeps_what_the_first_one_persisted() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    std::fs::write(&cfg, "agent:\n  name: gs-1\n").unwrap();
    let first = OledUpdate {
        brightness: Some(90),
    };
    assert_eq!(put_ui_oled_at(&cfg, GS, &first).status(), StatusCode::OK);
    let second = OledUpdate::default();
    let body = body_json(put_ui_oled_at(&cfg, GS, &second)).await;
    assert_eq!(body["oled"]["brightness"], json!(90));
    // The read route serves the same persisted section.
    let read = crate::routes::gs_ui_read::read_gs_section(&cfg, "ui");
    assert_eq!(read["oled"]["brightness"], json!(90));
}

#[tokio::test]
async fn put_oled_persist_fault_is_a_500_e_ui_save_failed() {
    // Point the config at a path whose parent cannot be created (a file stands
    // where a directory would need to be) so the YAML persist fails → 500.
    let dir = tempfile::tempdir().unwrap();
    let blocker = dir.path().join("blocker");
    std::fs::write(&blocker, b"x").unwrap();
    let cfg = blocker.join("config.yaml"); // parent "blocker" is a file

    let resp = put_ui_oled_at(
        &cfg,
        GS,
        &OledUpdate {
            brightness: Some(50),
        },
    );
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body = body_json(resp).await;
    assert_eq!(body["detail"]["error"]["code"], json!("E_UI_SAVE_FAILED"));
    assert!(body["detail"]["error"]["message"].as_str().is_some());
}

// ── /ui/buttons ───────────────────────────────────────────────────────────

#[tokio::test]
async fn put_buttons_replaces_the_mapping_wholesale() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");

    let mut mapping = std::collections::BTreeMap::new();
    mapping.insert("B1_short".to_string(), "show_qr".to_string());
    mapping.insert("B2_short".to_string(), "confirm".to_string());
    let update = ButtonsUpdate {
        mapping: Some(mapping),
    };
    let resp = put_ui_buttons_at(&cfg, GS, &update);
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    // The whole buttons section is now exactly {"mapping": <supplied>}, NOT a
    // merge over the six-action default.
    assert_eq!(
        body["buttons"],
        json!({"mapping": {"B1_short": "show_qr", "B2_short": "confirm"}})
    );
    // The other sections stay default.
    assert_eq!(body["oled"]["brightness"], json!(204));

    // The section persisted under ground_station.ui.buttons.
    let parsed: serde_norway::Value =
        serde_norway::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
    let mapping_b1 = parsed
        .get("ground_station")
        .and_then(|g| g.get("ui"))
        .and_then(|u| u.get("buttons"))
        .and_then(|b| b.get("mapping"))
        .and_then(|m| m.get("B1_short"))
        .and_then(|v| v.as_str());
    assert_eq!(mapping_b1, Some("show_qr"));
}

#[tokio::test]
async fn put_buttons_with_no_mapping_keeps_the_loaded_section() {
    // An omitted mapping leaves the buttons section as loaded (the default
    // six-action mapping), still persisted + echoed.
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");

    let resp = put_ui_buttons_at(&cfg, GS, &ButtonsUpdate::default());
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(
        body["buttons"]["mapping"]["B1_short"],
        json!("cycle_screen")
    );
}

#[tokio::test]
async fn visor_buttons_default_and_write_persist() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    let body = body_json(put_ui_buttons_at(&cfg, "visor", &ButtonsUpdate::default())).await;
    assert_eq!(body["buttons"]["mapping"]["B1_short"], json!("passthrough"));
    assert_eq!(
        body["buttons"]["mapping"]["B1_long"],
        json!("shutdown_prompt")
    );

    let mut mapping = std::collections::BTreeMap::new();
    mapping.insert("B2_short".to_string(), "back".to_string());
    let update = ButtonsUpdate {
        mapping: Some(mapping),
    };
    assert_eq!(
        put_ui_buttons_at(&cfg, "visor", &update).status(),
        StatusCode::OK
    );
    let read = crate::routes::gs_ui_read::read_gs_section(&cfg, "ui");
    assert_eq!(read["buttons"], json!({"mapping": {"B2_short": "back"}}));
}

// ── /ui/screens ───────────────────────────────────────────────────────────

#[tokio::test]
async fn put_screens_applies_order_and_enabled() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");

    let update = ScreensUpdate {
        order: Some(vec!["home".to_string(), "link".to_string()]),
        enabled: Some(vec!["home".to_string()]),
    };
    let resp = put_ui_screens_at(&cfg, GS, &update);
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["screens"]["order"], json!(["home", "link"]));
    assert_eq!(body["screens"]["enabled"], json!(["home"]));
    // The other sections stay default.
    assert_eq!(body["oled"]["brightness"], json!(204));

    // Persisted under ground_station.ui.screens.
    let parsed: serde_norway::Value =
        serde_norway::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
    let order = parsed
        .get("ground_station")
        .and_then(|g| g.get("ui"))
        .and_then(|u| u.get("screens"))
        .and_then(|s| s.get("order"))
        .and_then(serde_norway::Value::as_sequence)
        .map(|seq| seq.len());
    assert_eq!(order, Some(2));
}

#[tokio::test]
async fn put_screens_with_only_order_keeps_default_enabled() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    let update = ScreensUpdate {
        order: Some(vec!["home".to_string()]),
        enabled: None,
    };
    let resp = put_ui_screens_at(&cfg, GS, &update);
    let body = body_json(resp).await;
    assert_eq!(body["screens"]["order"], json!(["home"]));
    // enabled keeps the full default list.
    assert_eq!(body["screens"]["enabled"], json!(GS_SCREENS));
}

#[test]
fn json_to_yaml_preserves_shapes() {
    let v = json!({"a": 1, "b": [true, "x"], "c": null});
    let y = json_to_yaml(&v);
    assert_eq!(y.get("a").and_then(serde_norway::Value::as_i64), Some(1));
    assert!(y
        .get("c")
        .map(serde_norway::Value::is_null)
        .unwrap_or(false));
    let seq = y
        .get("b")
        .and_then(serde_norway::Value::as_sequence)
        .unwrap();
    assert_eq!(seq.len(), 2);
}
