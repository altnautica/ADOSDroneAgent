//! Front-panel and ground-station persisted-UI read routes.
//!
//! Two read-only routes the setup webapp + the GCS Hardware tab poll to render
//! the OLED / button / screen panel config and the HDMI kiosk display config.
//! The panel read is served on every profile that carries a front panel
//! (`ground_station` and `visor`, see [`ados_protocol::panel_ui::has_panel`]);
//! the kiosk read is ground-station only. Off those profiles each answers `404`
//! with the body `{"detail": {"error": {"code": "E_PROFILE_MISMATCH"}}}` — the
//! same shape the FastAPI `_require_ground_profile` gate raises, so the GCS
//! distinguishes "wrong profile" from "endpoint missing".
//!
//! - **`GET /api/v1/ground-station/ui`** — the full persisted UI config blob:
//!   `{oled, buttons, screens}`, each section the persisted `ground_station.ui`
//!   value merged over the built-in defaults (`oled.brightness 204`, the SSD1306
//!   contrast; the retired `auto_dim_enabled` / `screen_cycle_seconds` keys are
//!   never served, even from an older persisted section), the profile's default
//!   button mapping and screen order/enabled lists from
//!   [`ados_protocol::panel_ui`]). An absent / unreadable / unparseable config
//!   degrades to the all-defaults shape, never a 500.
//! - **`GET /api/v1/ground-station/display`** — the persisted HDMI kiosk display
//!   config: `{resolution, kiosk_enabled, kiosk_target_url}`, the
//!   `ground_station.kiosk` section of the agent config projected over the defaults
//!   (`resolution "auto"`, `kiosk_enabled true`, `kiosk_target_url null`). Same
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

/// The node's resolved profile in its underscore wire spelling
/// (`ground_station`, `visor`, ...). Resolves through `current_profile_and_role`
/// (the source of truth the node advertises on the wire), so a `profile: auto`
/// node that resolves via `profile.conf` passes the gates below.
fn resolved_profile(state: &AppState) -> String {
    let cfg = crate::config::PairingConfig::load_from(&state.pairing_paths.config);
    let (profile, _role) = crate::profile::current_profile_and_role(&cfg.agent.profile);
    profile.replace('-', "_")
}

/// The resolved profile when it carries a front panel, else `None`. Gates the
/// panel (`/ui`) read and writes.
pub(crate) fn panel_profile(state: &AppState) -> Option<String> {
    let profile = resolved_profile(state);
    ados_protocol::panel_ui::has_panel(&profile).then_some(profile)
}

/// True when the node resolves to the ground-station profile. Gates the kiosk
/// display routes (a visor has no kiosk).
pub(crate) fn is_ground_station(state: &AppState) -> bool {
    resolved_profile(state) == "ground_station"
}

/// The `404` profile-mismatch response, byte-identical to the FastAPI
/// `HTTPException(status_code=404, detail={"error": {"code": "E_PROFILE_MISMATCH"}})`
/// (FastAPI wraps the `detail` dict under a top-level `"detail"` key).
pub(crate) fn profile_mismatch() -> Response {
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

/// The default OLED block: the SSD1306 contrast on the native 0-255 scale
/// (`204` ≈ 80%).
fn default_oled() -> Map<String, Value> {
    json_object(json!({
        "brightness": 204,
    }))
}

/// OLED keys earlier releases persisted although no renderer applied them. A
/// stale config may still carry them; the read strips them.
const RETIRED_OLED_KEYS: [&str; 2] = ["auto_dim_enabled", "screen_cycle_seconds"];

/// The OLED section: the defaults merged with the stored section, minus the
/// retired keys.
fn oled_section(stored: Option<&Value>) -> Value {
    let mut oled = merge_over_defaults(default_oled(), stored);
    if let Value::Object(map) = &mut oled {
        for key in RETIRED_OLED_KEYS {
            map.remove(key);
        }
    }
    oled
}

/// The default button section for `profile`: the shared panel table nested
/// under `mapping`, plus the rotary-encoder keys (`ENC_*`) when the board
/// declares an encoder.
pub(crate) fn default_buttons(profile: &str, encoder: bool) -> Map<String, Value> {
    let encoder_keys: &[(&str, &str)] = if encoder {
        &ados_protocol::panel_ui::ENCODER_BUTTONS
    } else {
        &[]
    };
    let mapping: Map<String, Value> = ados_protocol::panel_ui::default_buttons(profile)
        .iter()
        .chain(encoder_keys.iter())
        .map(|(k, v)| ((*k).to_string(), Value::String((*v).to_string())))
        .collect();
    let mut out = Map::new();
    out.insert("mapping".to_string(), Value::Object(mapping));
    out
}

/// Whether the HAL board sidecar (`board.json`) declares a rotary encoder. An
/// absent or unreadable sidecar means no encoder.
pub(crate) fn board_has_encoder(board_path: &Path) -> bool {
    std::fs::read_to_string(board_path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .is_some_and(|board| board.get("encoder").is_some_and(Value::is_object))
}

/// The default screen section for `profile`: the shared screen list as both the
/// order and the enabled set.
pub(crate) fn default_screens(profile: &str) -> Map<String, Value> {
    let list: Vec<Value> = ados_protocol::panel_ui::default_screens(profile)
        .iter()
        .map(|s| Value::String((*s).to_string()))
        .collect();
    let mut out = Map::new();
    out.insert("order".to_string(), Value::Array(list.clone()));
    out.insert("enabled".to_string(), Value::Array(list));
    out
}

/// The default HDMI kiosk display config, mirroring the Python `_DEFAULT_DISPLAY`.
fn default_display() -> Map<String, Value> {
    json_object(json!({
        "resolution": "auto",
        "kiosk_enabled": true,
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
/// `404` `E_PROFILE_MISMATCH` off a panel profile. Otherwise the persisted
/// `ground_station.ui` section with each part merged over the profile's built-in
/// defaults. An absent / unreadable config yields the all-defaults shape, never
/// a 500.
pub async fn get_ui(State(state): State<AppState>) -> Response {
    let Some(profile) = panel_profile(&state) else {
        return profile_mismatch();
    };
    let encoder = board_has_encoder(&state.board_path);
    let blob = read_gs_section(&config_yaml_path(&state), "ui");
    Json(build_ui_config(&profile, encoder, &blob)).into_response()
}

/// Compose the `/ui` body from the persisted `ground_station.ui` map: each section the defaults merged
/// with the blob's matching section. Split out so the merge + the default key set
/// are unit-tested without filesystem IO.
fn build_ui_config(profile: &str, encoder: bool, blob: &Map<String, Value>) -> Value {
    json!({
        "oled": oled_section(blob.get("oled")),
        "buttons": merge_over_defaults(default_buttons(profile, encoder), blob.get("buttons")),
        "screens": merge_over_defaults(default_screens(profile), blob.get("screens")),
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
mod tests;
