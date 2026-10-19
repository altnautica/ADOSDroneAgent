//! The OLED, buttons and screens write routes.

use super::*;

/// The `PUT .../ui/oled` request body: the optional OLED contrast. Unknown keys
/// are a deserialization error, which the axum `Json` extractor answers `422`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OledUpdate {
    #[serde(default)]
    pub brightness: Option<i64>,
}

/// `PUT .../ui/oled` → the full UI config blob with the OLED section mutated.
///
/// `404` off a panel profile; `422` on an out-of-range bound (the GCS never
/// sends those). Applies the supplied OLED fields over the persisted value,
/// persists the merged section into the YAML config, signals the OLED service to
/// reload, and echoes the full UI config blob. A persist fault is a
/// `500 E_UI_SAVE_FAILED`.
pub async fn put_ui_oled(
    State(state): State<AppState>,
    Json(update): Json<OledUpdate>,
) -> Response {
    // FastAPI validates the request body (the Pydantic `OledUpdate` bounds) during
    // request parsing, BEFORE the handler runs its `_require_ground_profile()`
    // gate. So an out-of-range bound is a `422` even on a wrong-profile node; the
    // bound checks therefore precede the profile gate here to match that order.
    if let Some(resp) = check_brightness(update.brightness) {
        return resp;
    }
    // `deny_unknown_fields` already rejected any retired key during extraction.
    let Some(profile) = panel_profile(&state) else {
        return profile_mismatch();
    };
    let resp = put_ui_oled_at(&config_yaml_path(&state), &profile, &update);
    if resp.status().is_success() {
        signal_oled_reload().await;
    }
    resp
}

/// The OLED-write logic against an explicit config path. The public
/// handler resolves both from the app state; this takes them directly so a test can
/// point them at temp paths.
pub(super) fn put_ui_oled_at(config_path: &Path, profile: &str, update: &OledUpdate) -> Response {
    let mut data = load_ui_config(profile, &read_gs_section(config_path, "ui"));
    // The mutated section starts from the loaded `oled` block (defaults⊕persisted).
    let mut oled = match data.get("oled") {
        Some(Value::Object(m)) => m.clone(),
        _ => Map::new(),
    };
    for key in RETIRED_OLED_KEYS {
        oled.remove(key);
    }
    if let Some(b) = update.brightness {
        oled.insert("brightness".to_string(), json!(b));
    }
    let oled_value = Value::Object(oled);
    data.insert("oled".to_string(), oled_value.clone());

    if let Err(e) = persist_gs_ui_section(config_path, "oled", &oled_value) {
        return error_message(StatusCode::INTERNAL_SERVER_ERROR, "E_UI_SAVE_FAILED", e);
    }
    Json(Value::Object(data)).into_response()
}

// ---------------------------------------------------------------------------
// PUT /api/v1/ground-station/ui/buttons
// ---------------------------------------------------------------------------

/// The `PUT .../ui/buttons` request body (the FastAPI `ButtonsUpdate`): an optional
/// opaque mapping of action bindings.
#[derive(Debug, Default, Deserialize)]
pub struct ButtonsUpdate {
    #[serde(default)]
    pub mapping: Option<std::collections::BTreeMap<String, String>>,
}

/// `PUT .../ui/buttons` → the full UI config blob with the button section replaced.
///
/// `404` off a panel profile. When `mapping` is supplied, the WHOLE buttons
/// section becomes `{"mapping": <mapping>}` (wholesale replace, matching the
/// FastAPI handler — NOT a merge); when omitted, the buttons section keeps its
/// loaded value. Persists the section into the YAML config, signals the button
/// service to reload, and echoes the full UI config blob. A persist fault is a
/// `500 E_UI_SAVE_FAILED`.
pub async fn put_ui_buttons(
    State(state): State<AppState>,
    Json(update): Json<ButtonsUpdate>,
) -> Response {
    let Some(profile) = panel_profile(&state) else {
        return profile_mismatch();
    };
    let resp = put_ui_buttons_at(&config_yaml_path(&state), &profile, &update);
    if resp.status().is_success() {
        signal_buttons_reload().await;
    }
    resp
}

/// The button-write logic against an explicit config path.
pub(super) fn put_ui_buttons_at(
    config_path: &Path,
    profile: &str,
    update: &ButtonsUpdate,
) -> Response {
    let mut data = load_ui_config(profile, &read_gs_section(config_path, "ui"));
    if let Some(mapping) = &update.mapping {
        // Wholesale replace: the FastAPI handler sets
        // `data["buttons"] = {"mapping": dict(update.mapping)}`. The mapping is a
        // BTreeMap so the serialized key order is deterministic; the FastAPI
        // handler preserves the request dict's order (Python dicts are ordered),
        // but the GCS sends the canonical action set and the conformance check is
        // on the response shape, so a stable sort is the faithful deterministic
        // choice.
        let mut m = Map::new();
        for (k, v) in mapping {
            m.insert(k.clone(), Value::String(v.clone()));
        }
        let mut buttons = Map::new();
        buttons.insert("mapping".to_string(), Value::Object(m));
        data.insert("buttons".to_string(), Value::Object(buttons));
    }
    // The persisted section is whatever `data["buttons"]` is now (the loaded value
    // when `mapping` was omitted, the wholesale replacement when supplied).
    let buttons_value = data.get("buttons").cloned().unwrap_or(Value::Null);

    if let Err(e) = persist_gs_ui_section(config_path, "buttons", &buttons_value) {
        return error_message(StatusCode::INTERNAL_SERVER_ERROR, "E_UI_SAVE_FAILED", e);
    }
    Json(Value::Object(data)).into_response()
}

// ---------------------------------------------------------------------------
// PUT /api/v1/ground-station/ui/screens
// ---------------------------------------------------------------------------

/// The `PUT .../ui/screens` request body (the FastAPI `ScreensUpdate`): an optional
/// screen `order` and an optional `enabled` set.
#[derive(Debug, Default, Deserialize)]
pub struct ScreensUpdate {
    #[serde(default)]
    pub order: Option<Vec<String>>,
    #[serde(default)]
    pub enabled: Option<Vec<String>>,
}

/// `PUT .../ui/screens` → the full UI config blob with the screens section mutated.
///
/// `404` off a panel profile. Applies the supplied `order` / `enabled` lists
/// over the loaded section (each replaces the whole list when present), persists
/// the merged section into the YAML config, signals the OLED service to reload, and
/// echoes the full UI config blob. A persist fault is a `500 E_UI_SAVE_FAILED`.
pub async fn put_ui_screens(
    State(state): State<AppState>,
    Json(update): Json<ScreensUpdate>,
) -> Response {
    let Some(profile) = panel_profile(&state) else {
        return profile_mismatch();
    };
    let resp = put_ui_screens_at(&config_yaml_path(&state), &profile, &update);
    if resp.status().is_success() {
        signal_oled_reload().await;
    }
    resp
}

/// The screen-write logic against an explicit config path.
pub(super) fn put_ui_screens_at(
    config_path: &Path,
    profile: &str,
    update: &ScreensUpdate,
) -> Response {
    let mut data = load_ui_config(profile, &read_gs_section(config_path, "ui"));
    let mut screens = match data.get("screens") {
        Some(Value::Object(m)) => m.clone(),
        _ => Map::new(),
    };
    if let Some(order) = &update.order {
        screens.insert(
            "order".to_string(),
            Value::Array(order.iter().map(|s| Value::String(s.clone())).collect()),
        );
    }
    if let Some(enabled) = &update.enabled {
        screens.insert(
            "enabled".to_string(),
            Value::Array(enabled.iter().map(|s| Value::String(s.clone())).collect()),
        );
    }
    let screens_value = Value::Object(screens);
    data.insert("screens".to_string(), screens_value.clone());

    if let Err(e) = persist_gs_ui_section(config_path, "screens", &screens_value) {
        return error_message(StatusCode::INTERNAL_SERVER_ERROR, "E_UI_SAVE_FAILED", e);
    }
    Json(Value::Object(data)).into_response()
}
