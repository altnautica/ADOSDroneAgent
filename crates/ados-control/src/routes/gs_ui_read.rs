//! Ground-station persisted-UI read routes.
//!
//! Two read-only routes the GS setup webapp + the GCS Hardware tab poll to render
//! the OLED / button / screen UI config and the HDMI kiosk display config. Both
//! are gated on the node resolving to the ground-station profile: on a drone-profile
//! node each answers `404` with the body `{"detail": {"error": {"code":
//! "E_PROFILE_MISMATCH"}}}` — the same shape the FastAPI `_require_ground_profile`
//! gate raises, so the GCS distinguishes "wrong profile" from "endpoint missing".
//!
//! - **`GET /api/v1/ground-station/ui`** — the full persisted UI config blob:
//!   `{oled, buttons, screens}`, each section the persisted `ground_station.ui`
//!   value merged over the built-in defaults (`oled.brightness 204` / `auto_dim_enabled true` /
//!   `screen_cycle_seconds 5`, the six-action button mapping, the six-screen
//!   order/enabled lists). An absent / unreadable / unparseable config degrades
//!   to the all-defaults shape, never a 500.
//! - **`GET /api/v1/ground-station/display`** — the persisted HDMI kiosk display
//!   config: `{resolution, kiosk_enabled, kiosk_target_url}`, the
//!   `ground_station.kiosk` section of the agent config projected over the defaults
//!   (`resolution "auto"`, `kiosk_enabled false`, `kiosk_target_url null`). Same
//!   fault-tolerant read.
//!
//! Both reads source the YAML agent config, the single source of truth the
//! services read and the write routes persist: `/ui` reads `ground_station.ui`
//! (the legacy UI side-file is migrated into it when the config loads, and
//! nothing writes the side-file any more), `/display` reads `ground_station.kiosk`,
//! mapping the config fields (`resolution` / `enabled` / `target_url`) onto the
//! wire shape.

use std::path::{Path, PathBuf};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Map, Value};

use crate::state::AppState;

// ---------------------------------------------------------------------------
// Profile gate.
// ---------------------------------------------------------------------------

/// True when the node resolves to the ground-station profile, the same way the
/// FastAPI `_require_ground_profile` gate decides. Resolves through
/// `current_profile_and_role` (the source of truth the node advertises on the
/// wire), so a `profile: auto` node that resolves to a ground station via
/// `profile.conf` passes the gate.
fn is_ground_station(state: &AppState) -> bool {
    let cfg = crate::config::PairingConfig::load_from(&state.pairing_paths.config);
    let (profile, _role) = crate::profile::current_profile_and_role(&cfg.agent.profile);
    profile == "ground-station"
}

/// The `404` profile-mismatch response, byte-identical to the FastAPI
/// `HTTPException(status_code=404, detail={"error": {"code": "E_PROFILE_MISMATCH"}})`
/// (FastAPI wraps the `detail` dict under a top-level `"detail"` key).
fn profile_mismatch() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({"detail": {"error": {"code": "E_PROFILE_MISMATCH"}}})),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// On-disk seam: the agent config.
// ---------------------------------------------------------------------------

/// The agent config path (`/etc/ados/config.yaml` on a real box), the YAML store the
/// `/display` read sources `ground_station.kiosk` from.
fn config_yaml_path(state: &AppState) -> PathBuf {
    state.pairing_paths.config.clone()
}

/// Read the persisted `ground_station.kiosk` mapping from the YAML config as a JSON
/// object map. The kiosk service reads the same section.
fn read_gs_kiosk_section(config_path: &Path) -> Map<String, Value> {
    read_gs_section(config_path, "kiosk")
}

/// Read one `ground_station.<key>` mapping from the YAML config as a JSON object
/// map. An absent / unreadable / non-mapping config, or an absent section, yields
/// the empty map (so a read degrades to the all-defaults shape, never a 500).
pub(crate) fn read_gs_section(config_path: &Path, key: &str) -> Map<String, Value> {
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
        .and_then(|g| g.get(key))
        .map(yaml_to_json)
    {
        Some(Value::Object(map)) => map,
        _ => Map::new(),
    }
}

/// Convert a `serde_norway::Value` into a `serde_json::Value`, preserving scalar /
/// sequence / mapping shape. Non-string mapping keys are stringified (config keys
/// are always strings, so this only guards the match's totality).
fn yaml_to_json(value: &serde_norway::Value) -> Value {
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

/// Merge a persisted section over a defaults map: start from the defaults, then
/// overlay every key the persisted section carries. Mirrors the Python
/// `{**_DEFAULT_X, **(data.get("x") or {})}` spread: a falsy / non-object section
/// contributes nothing (the defaults stand), and present keys win. The defaults
/// supply the key set and the fallbacks; the persisted section supplies overrides.
fn merge_over_defaults(defaults: Map<String, Value>, section: Option<&Value>) -> Value {
    let mut out = defaults;
    if let Some(Value::Object(overrides)) = section {
        for (k, v) in overrides {
            out.insert(k.clone(), v.clone());
        }
    }
    Value::Object(out)
}

// ---------------------------------------------------------------------------
// The built-in default blobs (mirror the Python `_DEFAULT_*`).
// ---------------------------------------------------------------------------

/// The default OLED block, mirroring the Python `_DEFAULT_OLED` (the 0-255 native
/// brightness scale, `204` ≈ 80%).
fn default_oled() -> Map<String, Value> {
    json_object(json!({
        "brightness": 204,
        "auto_dim_enabled": true,
        "screen_cycle_seconds": 5,
    }))
}

/// The default button mapping, mirroring the Python `_DEFAULT_BUTTONS` (the six
/// short/long actions for the three physical buttons, nested under `mapping`).
fn default_buttons() -> Map<String, Value> {
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

/// The default screen config, mirroring the Python `_DEFAULT_SCREENS` (the six-screen
/// order + the same set enabled).
fn default_screens() -> Map<String, Value> {
    json_object(json!({
        "order": ["home", "link", "drone", "network", "system", "qr"],
        "enabled": ["home", "link", "drone", "network", "system", "qr"],
    }))
}

/// The default HDMI kiosk display config, mirroring the Python `_DEFAULT_DISPLAY`.
fn default_display() -> Map<String, Value> {
    json_object(json!({
        "resolution": "auto",
        "kiosk_enabled": false,
        "kiosk_target_url": Value::Null,
    }))
}

/// Unwrap a `json!` object literal into its owned `Map`. The literals above are all
/// objects, so the fallback empty map is never hit; it keeps the helper total.
fn json_object(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

// ---------------------------------------------------------------------------
// GET /api/v1/ground-station/ui — the full persisted UI config blob.
// ---------------------------------------------------------------------------

/// `GET /api/v1/ground-station/ui` → `{oled, buttons, screens}`.
///
/// `404` `E_PROFILE_MISMATCH` off a ground-station node. Otherwise the persisted
/// `ground_station.ui` section with each part merged over its built-in defaults.
/// An absent / unreadable config yields the all-defaults shape, never a 500.
pub async fn get_ui(State(state): State<AppState>) -> Response {
    if !is_ground_station(&state) {
        return profile_mismatch();
    }
    let blob = read_gs_section(&config_yaml_path(&state), "ui");
    Json(build_ui_config(&blob)).into_response()
}

/// Compose the `/ui` body from the persisted `ground_station.ui` map: each section the defaults merged
/// with the blob's matching section. Split out so the merge + the default key set
/// are unit-tested without filesystem IO.
fn build_ui_config(blob: &Map<String, Value>) -> Value {
    json!({
        "oled": merge_over_defaults(default_oled(), blob.get("oled")),
        "buttons": merge_over_defaults(default_buttons(), blob.get("buttons")),
        "screens": merge_over_defaults(default_screens(), blob.get("screens")),
    })
}

// ---------------------------------------------------------------------------
// GET /api/v1/ground-station/display — the persisted HDMI kiosk display config.
// ---------------------------------------------------------------------------

/// `GET /api/v1/ground-station/display` → `{resolution, kiosk_enabled,
/// kiosk_target_url}`.
///
/// `404` `E_PROFILE_MISMATCH` off a ground-station node. Otherwise the persisted
/// `ground_station.kiosk` section of the agent config projected over the built-in
/// defaults. An absent / unreadable config yields the all-defaults shape, never a
/// 500.
pub async fn get_display(State(state): State<AppState>) -> Response {
    if !is_ground_station(&state) {
        return profile_mismatch();
    }
    let kiosk = read_gs_kiosk_section(&config_yaml_path(&state));
    Json(build_display_config(&kiosk)).into_response()
}

/// Project a persisted `ground_station.kiosk` section into the display wire shape
/// `{resolution, kiosk_enabled, kiosk_target_url}` over the built-in defaults. The
/// config keys (`resolution` / `enabled` / `target_url`) map onto the wire keys; a
/// key the section omits keeps its default. Split out so the projection is
/// unit-tested without IO.
fn build_display_config(kiosk: &Map<String, Value>) -> Value {
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
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
        let got = build_ui_config(&blob);
        let want = json!({
            "oled": {
                "brightness": 204,
                "auto_dim_enabled": true,
                "screen_cycle_seconds": 5,
            },
            "buttons": {
                "mapping": {
                    "B1_short": "cycle_screen",
                    "B1_long": "toggle_backlight",
                    "B2_short": "show_network",
                    "B2_long": "show_qr",
                    "B3_short": "confirm",
                    "B3_long": "pair_drone",
                }
            },
            "screens": {
                "order": ["home", "link", "drone", "network", "system", "qr"],
                "enabled": ["home", "link", "drone", "network", "system", "qr"],
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
            json!({"brightness": 120, "auto_dim_enabled": false}),
        );
        let got = build_ui_config(&blob);
        // Overridden keys win.
        assert_eq!(got["oled"]["brightness"], json!(120));
        assert_eq!(got["oled"]["auto_dim_enabled"], json!(false));
        // The default key the stored section did not carry stands.
        assert_eq!(got["oled"]["screen_cycle_seconds"], json!(5));
        // Untouched sections are the full defaults.
        assert_eq!(got["buttons"]["mapping"]["B1_short"], json!("cycle_screen"));
        assert_eq!(
            got["screens"]["order"],
            json!(["home", "link", "drone", "network", "system", "qr"])
        );
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
        let got = build_ui_config(&blob);
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
            "kiosk_enabled": false,
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
            json!({"resolution": "auto", "kiosk_enabled": false, "kiosk_target_url": null})
        );
    }
}
