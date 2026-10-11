//! Side-file read + the defaults-merged UI/display blob (mirrors the read module).

use super::*;

/// Merge a persisted section over a defaults map: start from the defaults, then
/// overlay every key the persisted section carries. Mirrors the Python
/// `{**_DEFAULT_X, **(data.get("x") or {})}` spread.
pub(super) fn merge_over_defaults(
    defaults: Map<String, Value>,
    section: Option<&Value>,
) -> Map<String, Value> {
    let mut out = defaults;
    if let Some(Value::Object(overrides)) = section {
        for (k, v) in overrides {
            out.insert(k.clone(), v.clone());
        }
    }
    out
}

/// The default OLED block: the SSD1306 contrast on the native 0-255 scale.
pub(super) fn default_oled() -> Map<String, Value> {
    json_object(json!({
        "brightness": 204,
    }))
}

/// The default button mapping (the Python `_DEFAULT_BUTTONS`).
pub(super) fn default_buttons() -> Map<String, Value> {
    json_object(json!({
        "mapping": {
            "B1_short": "cycle_screen",
            "B1_long": "toggle_backlight",
            "B2_short": "show_network",
            "B2_long": "show_qr",
            "B3_short": "confirm",
            "B3_long": "pair_drone",
        }
    }))
}

/// The default screen config (the Python `_DEFAULT_SCREENS`).
pub(super) fn default_screens() -> Map<String, Value> {
    json_object(json!({
        "order": ["home", "link", "drone", "network", "system", "qr"],
        "enabled": ["home", "link", "drone", "network", "system", "qr"],
    }))
}

/// The default HDMI kiosk display config (the Python `_DEFAULT_DISPLAY`).
pub(super) fn default_display() -> Map<String, Value> {
    json_object(json!({
        "resolution": "auto",
        "kiosk_enabled": true,
        "kiosk_target_url": Value::Null,
    }))
}

/// Unwrap a `json!` object literal into its owned `Map` (the literals above are all
/// objects).
pub(super) fn json_object(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

/// The defaults-merged UI config blob `{oled, buttons, screens}` from a persisted
/// blob, byte-identical to the Python `_load_ui_config`. The base for the in-memory
/// section overlay the response body returns.
pub(super) fn load_ui_config(blob: &Map<String, Value>) -> Map<String, Value> {
    let mut out = Map::new();
    out.insert(
        "oled".to_string(),
        Value::Object(merge_over_defaults(default_oled(), blob.get("oled"))),
    );
    out.insert(
        "buttons".to_string(),
        Value::Object(merge_over_defaults(default_buttons(), blob.get("buttons"))),
    );
    out.insert(
        "screens".to_string(),
        Value::Object(merge_over_defaults(default_screens(), blob.get("screens"))),
    );
    out
}

/// Read the persisted `ground_station.kiosk` mapping from the YAML config as a JSON
/// object map. An absent / unreadable / non-mapping config, or an absent kiosk
/// section, yields the empty map. Used to seed the display write so only the
/// operator-supplied fields change.
pub(super) fn read_gs_kiosk_section(config_path: &Path) -> Map<String, Value> {
    let text = match std::fs::read_to_string(config_path) {
        Ok(t) => t,
        Err(_) => return Map::new(),
    };
    let yaml: serde_norway::Value = match serde_norway::from_str(&text) {
        Ok(v) => v,
        Err(_) => return Map::new(),
    };
    match yaml
        .get("ground_station")
        .and_then(|g| g.get("kiosk"))
        .map(yaml_to_json)
    {
        Some(Value::Object(map)) => map,
        _ => Map::new(),
    }
}

/// Project a persisted `ground_station.kiosk` section into the display-route wire
/// shape `{resolution, kiosk_enabled, kiosk_target_url}` over the built-in defaults.
/// The config keys (`resolution` / `enabled` / `target_url`) map onto the wire keys;
/// a key the section omits keeps its default.
pub(super) fn display_wire_from_kiosk(kiosk: &Map<String, Value>) -> Map<String, Value> {
    let mut out = default_display();
    if let Some(v) = kiosk.get("resolution") {
        out.insert("resolution".to_string(), v.clone());
    }
    if let Some(v) = kiosk.get("enabled") {
        out.insert("kiosk_enabled".to_string(), v.clone());
    }
    if let Some(v) = kiosk.get("target_url") {
        out.insert("kiosk_target_url".to_string(), v.clone());
    }
    out
}

/// Convert a `serde_norway::Value` into a `serde_json::Value`, preserving scalar /
/// sequence / mapping shape (the reverse of [`json_to_yaml`]). Non-string mapping
/// keys are stringified — config keys are always strings, so this only guards the
/// match's totality.
pub(super) fn yaml_to_json(value: &serde_norway::Value) -> Value {
    use serde_norway::Value as Yaml;
    match value {
        Yaml::Null => Value::Null,
        Yaml::Bool(b) => Value::Bool(*b),
        Yaml::Number(n) => {
            if let Some(i) = n.as_i64() {
                json!(i)
            } else if let Some(u) = n.as_u64() {
                json!(u)
            } else if let Some(f) = n.as_f64() {
                json!(f)
            } else {
                Value::Null
            }
        }
        Yaml::String(s) => Value::String(s.clone()),
        Yaml::Sequence(seq) => Value::Array(seq.iter().map(yaml_to_json).collect()),
        Yaml::Mapping(map) => {
            let mut out = Map::new();
            for (k, v) in map {
                let key = match k {
                    Yaml::String(s) => s.clone(),
                    other => serde_norway::to_string(other)
                        .map(|s| s.trim().to_string())
                        .unwrap_or_default(),
                };
                out.insert(key, yaml_to_json(v));
            }
            Value::Object(out)
        }
        Yaml::Tagged(t) => yaml_to_json(&t.value),
    }
}
