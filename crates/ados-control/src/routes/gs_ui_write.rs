//! Ground-station persisted-UI write routes.
//!
//! The OLED / button / screen UI knobs and the HDMI kiosk display config are
//! written from the GS setup webapp + the GCS Hardware tab. The read views live
//! in [`crate::routes::gs_ui_read`]; this module serves the four writes the front
//! can reproduce faithfully:
//!
//! - **`PUT /api/v1/ground-station/ui/oled`** — apply the supplied OLED fields
//!   (`brightness` / `auto_dim_enabled` / `screen_cycle_seconds`), persist the
//!   merged section into `ground_station.ui.oled` of the agent config, signal the
//!   OLED display service to reload, and echo the full UI config blob.
//! - **`PUT /api/v1/ground-station/ui/buttons`** — replace the button mapping
//!   wholesale (when supplied), persist it into `ground_station.ui.buttons`,
//!   signal the button service to reload, and echo the full UI config blob.
//! - **`PUT /api/v1/ground-station/ui/screens`** — apply the supplied screen
//!   `order` / `enabled` lists, persist the merged section into
//!   `ground_station.ui.screens`, signal the OLED display service to reload, and
//!   echo the full UI config blob.
//! - **`PUT /api/v1/ground-station/display`** — apply the supplied HDMI kiosk
//!   fields (`resolution` / `kiosk_enabled` / `kiosk_target_url`), persist the
//!   merged config into `ground_station.kiosk` of the agent config, and echo the
//!   display config.
//!
//! ## Two persistence targets, mirroring the FastAPI handlers
//!
//! The three `/ui/*` writes persist their section into the YAML-backed agent
//! config under `ground_station.ui.<section>` (the authoritative path the live
//! services read), while the RESPONSE body is the legacy side-file UI blob
//! (`/etc/ados/ground-station-ui.json`) merged over the built-in defaults — with
//! the just-mutated section overlaid in memory. That split exactly mirrors the
//! FastAPI handlers, which mutate the in-memory `_load_ui_config()` dict (sourced
//! from the side-file), call `_persist_gs_ui_section(...)` (which writes the YAML
//! config), and return the mutated in-memory dict. The front reproduces both legs:
//! the YAML-config merge (the same atomic `serde_norway` tmp+rename the MAC-pin /
//! WFB writes use) for the persist, and the side-file read + in-memory section
//! overlay for the response body.
//!
//! The `/display` write is the single-source-of-truth path: it seeds from, and
//! persists into, `ground_station.kiosk` of the YAML config — the same section the
//! kiosk service reads (`target_url` / `minimal_layer`) and `PUT /api/config`
//! writes. The wire fields (`resolution` / `kiosk_enabled` / `kiosk_target_url`)
//! map onto the config fields (`resolution` / `enabled` / `target_url`); a merge
//! preserves any other kiosk key (e.g. an operator-set `minimal_layer`).
//!
//! ## Persist-failure is a 500, not a degraded body
//!
//! Unlike the WFB-config write (which degrades to `persisted: false` on a write
//! fault), the FastAPI UI handlers wrap the persist in a `try/except OSError` and
//! raise `500 {"detail": {"error": {"code": "E_UI_SAVE_FAILED", "message":
//! "<io error>"}}}`. The YAML-config persist also fails (the FastAPI
//! `_persist_gs_ui_section` raises `OSError`) when the writer is not root, because
//! the config is 0600 root-owned — so a non-root front lands on the same 500. The
//! front reproduces that 500 envelope on any persist fault.
//!
//! ## Service reload signal
//!
//! After persisting, the FastAPI handlers SIGHUP the live display service so it
//! reloads its config without a restart (`signal_oled_reload` →
//! `ados-oled.service`, `signal_buttons_reload` → `ados-buttons.service`). The
//! front does the same via `systemctl kill -s HUP <unit>`, best-effort: a failure
//! (the unit inactive, systemd unavailable on a dev tree) is swallowed, matching
//! the FastAPI helper that degrades silently. The reload is fire-and-forget and
//! never affects the response.
//!
//! ## The profile gate
//!
//! Like every ground-station route, each first gates on the resolved profile being
//! a ground station and returns the FastAPI
//! `404 {"detail":{"error":{"code":"E_PROFILE_MISMATCH"}}}` on a drone, the same
//! body the read module serves.

use std::path::{Path, PathBuf};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::state::AppState;

// ---------------------------------------------------------------------------
// Profile gate (mirrors the read module + the Python `_require_ground_profile`).
// ---------------------------------------------------------------------------

/// True when the node resolves to the ground-station profile, the same way the
/// FastAPI `_require_ground_profile` gate decides (and the sibling read module).
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
// Error envelopes (matched to the FastAPI handlers' error-object detail shape).
// ---------------------------------------------------------------------------

/// A FastAPI error-object body: `(status, {"detail": {"error": {"code": <code>}}})`.
/// Mirrors the Python `HTTPException(detail={"error": {"code": ...}})` shape (with
/// no message), used by the display 400.
fn error_code(status: StatusCode, code: &str) -> Response {
    (status, Json(json!({"detail": {"error": {"code": code}}}))).into_response()
}

/// A FastAPI error-object body with a message:
/// `(status, {"detail": {"error": {"code": <code>, "message": <message>}}})`.
/// Mirrors the Python `HTTPException(detail={"error": {"code": ..., "message":
/// ...}})` the UI persist failure raises.
fn error_message(status: StatusCode, code: &str, message: impl Into<String>) -> Response {
    (
        status,
        Json(json!({"detail": {"error": {"code": code, "message": message.into()}}})),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Pydantic version coupling for the 422 validation envelope.
// ---------------------------------------------------------------------------

/// The Pydantic version baked into the `url` field of a FastAPI 422 validation
/// error. FastAPI serializes Pydantic's per-error `url`
/// (`https://errors.pydantic.dev/<ver>/v/<type>`). Kept as one constant so the
/// coupling to the pinned Pydantic minor is explicit and updated in one place when
/// the dependency bumps. The GCS clamps its sliders to the in-range values so this
/// 422 path is never hit on the real wire; it exists for faithfulness on a raw
/// out-of-range request.
const PYDANTIC_VERSION: &str = "2.11";

/// Build the FastAPI 422 validation-error body for one numeric bound violation on
/// a request-body field. Mirrors the FastAPI `RequestValidationError` shape:
/// `{"detail": [{"type", "loc": ["body", <field>], "msg", "input", "ctx", "url"}]}`,
/// with `ctx` carrying the single bound key (`ge` / `le`) and `url` the
/// version-pinned Pydantic docs link.
fn validation_error_422(
    field: &str,
    error_type: &str,
    msg: &str,
    bound_key: &str,
    bound: i64,
    input: i64,
) -> Response {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(json!({
            "detail": [{
                "type": error_type,
                "loc": ["body", field],
                "msg": msg,
                "input": input,
                "ctx": { bound_key: bound },
                "url": format!("https://errors.pydantic.dev/{PYDANTIC_VERSION}/v/{error_type}"),
            }],
        })),
    )
        .into_response()
}

/// Validate an OLED `brightness` (Pydantic `ge=0, le=255`). `None` when in range or
/// absent; `Some(response)` carries the FastAPI 422 for the violated bound. (Returns
/// an `Option` rather than a `Result<(), Response>` because the axum `Response` is a
/// large type and a large `Err` variant is a clippy lint.)
fn check_brightness(v: Option<i64>) -> Option<Response> {
    match v {
        Some(n) if n < 0 => Some(validation_error_422(
            "brightness",
            "greater_than_equal",
            "Input should be greater than or equal to 0",
            "ge",
            0,
            n,
        )),
        Some(n) if n > 255 => Some(validation_error_422(
            "brightness",
            "less_than_equal",
            "Input should be less than or equal to 255",
            "le",
            255,
            n,
        )),
        _ => None,
    }
}

/// Validate an OLED `screen_cycle_seconds` (Pydantic `ge=1, le=60`). `None` when in
/// range or absent; `Some(response)` carries the FastAPI 422 for the bound.
fn check_screen_cycle(v: Option<i64>) -> Option<Response> {
    match v {
        Some(n) if n < 1 => Some(validation_error_422(
            "screen_cycle_seconds",
            "greater_than_equal",
            "Input should be greater than or equal to 1",
            "ge",
            1,
            n,
        )),
        Some(n) if n > 60 => Some(validation_error_422(
            "screen_cycle_seconds",
            "less_than_equal",
            "Input should be less than or equal to 60",
            "le",
            60,
            n,
        )),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// On-disk seams: the legacy side-file + the agent config.
// ---------------------------------------------------------------------------

/// The persisted UI config side-file (`/etc/ados/ground-station-ui.json`, the
/// `GS_UI_JSON` path), resolved as a sibling of the agent config so the write
/// shares the config-path injection the rest of the ground-station routes use, the
/// same resolution the read module performs.
fn ui_config_path(state: &AppState) -> PathBuf {
    state
        .pairing_paths
        .config
        .parent()
        .map(|dir| dir.join("ground-station-ui.json"))
        .unwrap_or_else(|| PathBuf::from("/etc/ados/ground-station-ui.json"))
}

/// The agent config path (`/etc/ados/config.yaml` on a real box), the YAML store
/// the `/ui/*` sections persist into under `ground_station.ui.<section>`.
fn config_yaml_path(state: &AppState) -> PathBuf {
    state.pairing_paths.config.clone()
}

// ---------------------------------------------------------------------------
// Side-file read + the defaults-merged UI/display blob (mirrors the read module).
// ---------------------------------------------------------------------------

/// Read the side-file into an object map, returning the empty map on absence / a
/// read error / a parse error / a falsy or non-object body. Mirrors the Python
/// `json.loads(...) or {}` guarded by `except (OSError, ValueError)`.
fn read_ui_blob(path: &Path) -> Map<String, Value> {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(map)) => map,
            _ => Map::new(),
        },
        Err(_) => Map::new(),
    }
}

/// Merge a side-file section over a defaults map: start from the defaults, then
/// overlay every key the side-file section carries. Mirrors the Python
/// `{**_DEFAULT_X, **(data.get("x") or {})}` spread.
fn merge_over_defaults(
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

/// The default OLED block (the Python `_DEFAULT_OLED`).
fn default_oled() -> Map<String, Value> {
    json_object(json!({
        "brightness": 204,
        "auto_dim_enabled": true,
        "screen_cycle_seconds": 5,
    }))
}

/// The default button mapping (the Python `_DEFAULT_BUTTONS`).
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

/// The default screen config (the Python `_DEFAULT_SCREENS`).
fn default_screens() -> Map<String, Value> {
    json_object(json!({
        "order": ["home", "link", "drone", "network", "system", "qr"],
        "enabled": ["home", "link", "drone", "network", "system", "qr"],
    }))
}

/// The default HDMI kiosk display config (the Python `_DEFAULT_DISPLAY`).
fn default_display() -> Map<String, Value> {
    json_object(json!({
        "resolution": "auto",
        "kiosk_enabled": false,
        "kiosk_target_url": Value::Null,
    }))
}

/// Unwrap a `json!` object literal into its owned `Map` (the literals above are all
/// objects).
fn json_object(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

/// The defaults-merged UI config blob `{oled, buttons, screens}` from a side-file
/// blob, byte-identical to the Python `_load_ui_config`. The base for the in-memory
/// section overlay the response body returns.
fn load_ui_config(blob: &Map<String, Value>) -> Map<String, Value> {
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
fn read_gs_kiosk_section(config_path: &Path) -> Map<String, Value> {
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
fn display_wire_from_kiosk(kiosk: &Map<String, Value>) -> Map<String, Value> {
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

// ---------------------------------------------------------------------------
// YAML config persist: ground_station.ui.<section> (the authoritative path).
// ---------------------------------------------------------------------------

/// Merge `value` into `ground_station.ui.<section>` of the on-disk YAML config,
/// atomically (tmp + rename), preserving every other key and the mapping insertion
/// order (the Python `yaml.safe_dump(sort_keys=False)`). Returns `Ok(())` on
/// success, `Err(message)` on any read/parse/serialize/write fault so the caller
/// can map it to the `E_UI_SAVE_FAILED` 500 — including the EPERM a non-root front
/// gets on the 0600 root-owned config, matching the FastAPI `_persist_gs_ui_section`
/// raising `OSError` when `_save_config_dict` returns `False`.
fn persist_gs_ui_section(config_path: &Path, section: &str, value: &Value) -> Result<(), String> {
    use serde_norway::{Mapping, Value as Yaml};

    // An absent / non-mapping config starts from an empty mapping (the Python
    // `_load_config_dict()` returns `{}` when the file is absent / unparseable).
    let mut data: Yaml = match std::fs::read_to_string(config_path) {
        Ok(text) => match serde_norway::from_str::<Yaml>(&text) {
            Ok(v) if v.is_mapping() => v,
            _ => Yaml::Mapping(Mapping::new()),
        },
        Err(_) => Yaml::Mapping(Mapping::new()),
    };

    {
        let root = data
            .as_mapping_mut()
            .ok_or_else(|| "config root is not a mapping".to_string())?;
        let gs = root
            .entry(Yaml::String("ground_station".to_string()))
            .or_insert_with(|| Yaml::Mapping(Mapping::new()));
        if !gs.is_mapping() {
            *gs = Yaml::Mapping(Mapping::new());
        }
        let gs_map = gs
            .as_mapping_mut()
            .ok_or_else(|| "ground_station section is not a mapping".to_string())?;
        let ui = gs_map
            .entry(Yaml::String("ui".to_string()))
            .or_insert_with(|| Yaml::Mapping(Mapping::new()));
        if !ui.is_mapping() {
            *ui = Yaml::Mapping(Mapping::new());
        }
        let ui_map = ui
            .as_mapping_mut()
            .ok_or_else(|| "ui section is not a mapping".to_string())?;
        // Convert the JSON section value into a YAML value so it nests under the
        // config tree (the Python writes the raw dict; the round-trip through YAML
        // preserves the same scalar/list/map shape).
        let yaml_value: Yaml = json_to_yaml(value);
        ui_map.insert(Yaml::String(section.to_string()), yaml_value);
    }

    let body = serde_norway::to_string(&data).map_err(|e| e.to_string())?;
    write_atomic_bytes(config_path, body.as_bytes())
}

/// Convert a `serde_json::Value` into a `serde_norway::Value`, preserving the
/// scalar / array / object shape. Used to nest a UI section under the YAML config
/// tree without going through a string round-trip.
fn json_to_yaml(value: &Value) -> serde_norway::Value {
    use serde_norway::{Mapping, Value as Yaml};
    match value {
        Value::Null => Yaml::Null,
        Value::Bool(b) => Yaml::Bool(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Yaml::Number(i.into())
            } else if let Some(u) = n.as_u64() {
                Yaml::Number(u.into())
            } else if let Some(f) = n.as_f64() {
                Yaml::Number(f.into())
            } else {
                Yaml::Null
            }
        }
        Value::String(s) => Yaml::String(s.clone()),
        Value::Array(arr) => Yaml::Sequence(arr.iter().map(json_to_yaml).collect()),
        Value::Object(map) => {
            let mut m = Mapping::new();
            for (k, v) in map {
                m.insert(Yaml::String(k.clone()), json_to_yaml(v));
            }
            Yaml::Mapping(m)
        }
    }
}

// ---------------------------------------------------------------------------
// Kiosk config persist: merge fields into ground_station.kiosk (the single source
// of truth the kiosk service + PUT /api/config also use).
// ---------------------------------------------------------------------------

/// Merge the supplied kiosk fields into `ground_station.kiosk` of the on-disk YAML
/// config, atomically (tmp + rename), preserving every other key (e.g. an
/// operator-set `minimal_layer`) and the mapping insertion order. Returns `Ok(())`
/// on success, `Err(message)` on any read/parse/serialize/write fault so the caller
/// maps it to the `E_UI_SAVE_FAILED` 500 — including the EPERM a non-root front gets
/// on the 0600 root-owned config.
fn persist_gs_kiosk_fields(config_path: &Path, fields: &[(&str, Value)]) -> Result<(), String> {
    use serde_norway::{Mapping, Value as Yaml};

    // An absent / non-mapping config starts from an empty mapping (the same seed the
    // sibling `persist_gs_ui_section` uses).
    let mut data: Yaml = match std::fs::read_to_string(config_path) {
        Ok(text) => match serde_norway::from_str::<Yaml>(&text) {
            Ok(v) if v.is_mapping() => v,
            _ => Yaml::Mapping(Mapping::new()),
        },
        Err(_) => Yaml::Mapping(Mapping::new()),
    };

    {
        let root = data
            .as_mapping_mut()
            .ok_or_else(|| "config root is not a mapping".to_string())?;
        let gs = root
            .entry(Yaml::String("ground_station".to_string()))
            .or_insert_with(|| Yaml::Mapping(Mapping::new()));
        if !gs.is_mapping() {
            *gs = Yaml::Mapping(Mapping::new());
        }
        let gs_map = gs
            .as_mapping_mut()
            .ok_or_else(|| "ground_station section is not a mapping".to_string())?;
        let kiosk = gs_map
            .entry(Yaml::String("kiosk".to_string()))
            .or_insert_with(|| Yaml::Mapping(Mapping::new()));
        if !kiosk.is_mapping() {
            *kiosk = Yaml::Mapping(Mapping::new());
        }
        let kiosk_map = kiosk
            .as_mapping_mut()
            .ok_or_else(|| "kiosk section is not a mapping".to_string())?;
        for (k, v) in fields {
            kiosk_map.insert(Yaml::String((*k).to_string()), json_to_yaml(v));
        }
    }

    let body = serde_norway::to_string(&data).map_err(|e| e.to_string())?;
    write_atomic_bytes(config_path, body.as_bytes())
}

/// Write `bytes` to `path` atomically: ensure the parent dir, write a `.tmp`
/// sibling, then rename over the target. Mirrors the Python tmp-write +
/// `os.replace` / `tmp.replace` idiom. Returns `Err(message)` on any I/O fault.
fn write_atomic_bytes(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let tmp = {
        let mut ext = path
            .extension()
            .map(|e| e.to_os_string())
            .unwrap_or_default();
        ext.push(".tmp");
        path.with_extension(ext)
    };
    std::fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// SIGHUP the live display service (best-effort, matches the Python reload helper).
// ---------------------------------------------------------------------------

/// SIGHUP the named systemd unit so it reloads its UI config without a restart.
/// Best-effort: a failure (the unit inactive, systemd unavailable on a dev tree /
/// non-Linux host) is swallowed, matching the FastAPI `signal_sighup` helper which
/// degrades silently. `systemctl kill -s HUP <unit>` is the unit-targeted
/// equivalent of the Python's MainPID-resolve + `os.kill(pid, SIGHUP)`.
#[cfg(target_os = "linux")]
fn signal_reload(unit: &str) {
    let _ = std::process::Command::new("systemctl")
        .args(["kill", "-s", "HUP", unit])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

#[cfg(not(target_os = "linux"))]
fn signal_reload(_unit: &str) {
    // No systemd on the dev host; the reload is a no-op, matching the Python
    // helper's silent degrade when the unit/PID cannot be resolved.
}

/// SIGHUP the OLED display service (the Python `signal_oled_reload`).
fn signal_oled_reload() {
    signal_reload("ados-oled.service");
}

/// SIGHUP the button service (the Python `signal_buttons_reload`).
fn signal_buttons_reload() {
    signal_reload("ados-buttons.service");
}

// ---------------------------------------------------------------------------
// PUT /api/v1/ground-station/ui/oled
// ---------------------------------------------------------------------------

/// The `PUT .../ui/oled` request body (the FastAPI `OledUpdate`): three optional
/// fields, each applied only when present.
#[derive(Debug, Default, Deserialize)]
pub struct OledUpdate {
    #[serde(default)]
    pub brightness: Option<i64>,
    #[serde(default)]
    pub auto_dim_enabled: Option<bool>,
    #[serde(default)]
    pub screen_cycle_seconds: Option<i64>,
}

/// `PUT .../ui/oled` → the full UI config blob with the OLED section mutated.
///
/// `404` off a ground-station node; `422` on an out-of-range bound (the GCS never
/// sends those). Applies the supplied OLED fields over the side-file value,
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
    if let Some(resp) = check_screen_cycle(update.screen_cycle_seconds) {
        return resp;
    }
    if !is_ground_station(&state) {
        return profile_mismatch();
    }
    put_ui_oled_at(&config_yaml_path(&state), &ui_config_path(&state), &update)
}

/// The OLED-write logic against explicit config + side-file paths. The public
/// handler resolves both from the app state; this takes them directly so a test can
/// point them at temp paths.
fn put_ui_oled_at(config_path: &Path, ui_path: &Path, update: &OledUpdate) -> Response {
    let mut data = load_ui_config(&read_ui_blob(ui_path));
    // The mutated section starts from the loaded `oled` block (defaults⊕side-file).
    let mut oled = match data.get("oled") {
        Some(Value::Object(m)) => m.clone(),
        _ => Map::new(),
    };
    if let Some(b) = update.brightness {
        oled.insert("brightness".to_string(), json!(b));
    }
    if let Some(d) = update.auto_dim_enabled {
        oled.insert("auto_dim_enabled".to_string(), json!(d));
    }
    if let Some(s) = update.screen_cycle_seconds {
        oled.insert("screen_cycle_seconds".to_string(), json!(s));
    }
    let oled_value = Value::Object(oled);
    data.insert("oled".to_string(), oled_value.clone());

    if let Err(e) = persist_gs_ui_section(config_path, "oled", &oled_value) {
        return error_message(StatusCode::INTERNAL_SERVER_ERROR, "E_UI_SAVE_FAILED", e);
    }
    signal_oled_reload();
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
/// `404` off a ground-station node. When `mapping` is supplied, the WHOLE buttons
/// section becomes `{"mapping": <mapping>}` (wholesale replace, matching the
/// FastAPI handler — NOT a merge); when omitted, the buttons section keeps its
/// loaded value. Persists the section into the YAML config, signals the button
/// service to reload, and echoes the full UI config blob. A persist fault is a
/// `500 E_UI_SAVE_FAILED`.
pub async fn put_ui_buttons(
    State(state): State<AppState>,
    Json(update): Json<ButtonsUpdate>,
) -> Response {
    if !is_ground_station(&state) {
        return profile_mismatch();
    }
    put_ui_buttons_at(&config_yaml_path(&state), &ui_config_path(&state), &update)
}

/// The button-write logic against explicit config + side-file paths.
fn put_ui_buttons_at(config_path: &Path, ui_path: &Path, update: &ButtonsUpdate) -> Response {
    let mut data = load_ui_config(&read_ui_blob(ui_path));
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
    signal_buttons_reload();
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
/// `404` off a ground-station node. Applies the supplied `order` / `enabled` lists
/// over the loaded section (each replaces the whole list when present), persists
/// the merged section into the YAML config, signals the OLED service to reload, and
/// echoes the full UI config blob. A persist fault is a `500 E_UI_SAVE_FAILED`.
pub async fn put_ui_screens(
    State(state): State<AppState>,
    Json(update): Json<ScreensUpdate>,
) -> Response {
    if !is_ground_station(&state) {
        return profile_mismatch();
    }
    put_ui_screens_at(&config_yaml_path(&state), &ui_config_path(&state), &update)
}

/// The screen-write logic against explicit config + side-file paths.
fn put_ui_screens_at(config_path: &Path, ui_path: &Path, update: &ScreensUpdate) -> Response {
    let mut data = load_ui_config(&read_ui_blob(ui_path));
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
    signal_oled_reload();
    Json(Value::Object(data)).into_response()
}

// ---------------------------------------------------------------------------
// PUT /api/v1/ground-station/display
// ---------------------------------------------------------------------------

/// The `PUT .../display` request body (the FastAPI `DisplayUpdate`): an optional
/// `resolution`, `kiosk_enabled`, and `kiosk_target_url`.
#[derive(Debug, Default, Deserialize)]
pub struct DisplayUpdate {
    #[serde(default)]
    pub resolution: Option<String>,
    #[serde(default)]
    pub kiosk_enabled: Option<bool>,
    #[serde(default)]
    pub kiosk_target_url: Option<String>,
}

/// The accepted HDMI kiosk resolutions (the Python `allowed_res`).
const ALLOWED_RESOLUTIONS: [&str; 3] = ["auto", "720p", "1080p"];

/// `PUT .../display` → the merged HDMI kiosk display config.
///
/// `404` off a ground-station node; `400 E_INVALID_RESOLUTION` when `resolution` is
/// supplied and not one of `auto` / `720p` / `1080p`. Seeds from the persisted
/// `ground_station.kiosk` section, applies the supplied fields, persists the merged
/// config back into `ground_station.kiosk`, and echoes the merged config. A persist
/// fault is a `500 E_UI_SAVE_FAILED`.
pub async fn put_display(
    State(state): State<AppState>,
    Json(update): Json<DisplayUpdate>,
) -> Response {
    if !is_ground_station(&state) {
        return profile_mismatch();
    }
    put_display_at(&config_yaml_path(&state), &update)
}

/// The display-write logic against an explicit YAML config path. Seeds the current
/// value from the persisted `ground_station.kiosk` section, applies the supplied
/// fields, persists the owned fields back into `ground_station.kiosk` (wire →
/// config field-name mapping; a merge preserves any other kiosk key such as
/// `minimal_layer`), and echoes the wire display config.
fn put_display_at(config_path: &Path, update: &DisplayUpdate) -> Response {
    let mut current = display_wire_from_kiosk(&read_gs_kiosk_section(config_path));

    if let Some(res) = &update.resolution {
        if !ALLOWED_RESOLUTIONS.contains(&res.as_str()) {
            return error_code(StatusCode::BAD_REQUEST, "E_INVALID_RESOLUTION");
        }
        current.insert("resolution".to_string(), Value::String(res.clone()));
    }
    if let Some(k) = update.kiosk_enabled {
        current.insert("kiosk_enabled".to_string(), Value::Bool(k));
    }
    if let Some(url) = &update.kiosk_target_url {
        current.insert("kiosk_target_url".to_string(), Value::String(url.clone()));
    }

    // Map the wire fields onto the config field names and merge them into
    // ground_station.kiosk. minimal_layer + any other kiosk key is preserved.
    let fields = [
        (
            "resolution",
            current
                .get("resolution")
                .cloned()
                .unwrap_or_else(|| Value::String("auto".to_string())),
        ),
        (
            "enabled",
            current
                .get("kiosk_enabled")
                .cloned()
                .unwrap_or(Value::Bool(false)),
        ),
        (
            "target_url",
            current
                .get("kiosk_target_url")
                .cloned()
                .unwrap_or(Value::Null),
        ),
    ];
    if let Err(e) = persist_gs_kiosk_fields(config_path, &fields) {
        return error_message(StatusCode::INTERNAL_SERVER_ERROR, "E_UI_SAVE_FAILED", e);
    }
    Json(Value::Object(current)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Read a response body as JSON.
    async fn body_json(resp: Response) -> Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
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
    async fn screen_cycle_under_range_is_the_fastapi_422_envelope() {
        let resp = check_screen_cycle(Some(0)).unwrap();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body = body_json(resp).await;
        assert_eq!(
            body,
            json!({
                "detail": [{
                    "type": "greater_than_equal",
                    "loc": ["body", "screen_cycle_seconds"],
                    "msg": "Input should be greater than or equal to 1",
                    "input": 0,
                    "ctx": {"ge": 1},
                    "url": "https://errors.pydantic.dev/2.11/v/greater_than_equal",
                }]
            })
        );
    }

    #[test]
    fn bounds_accept_in_range_and_absent_values() {
        assert!(check_brightness(None).is_none());
        assert!(check_brightness(Some(0)).is_none());
        assert!(check_brightness(Some(255)).is_none());
        assert!(check_screen_cycle(None).is_none());
        assert!(check_screen_cycle(Some(1)).is_none());
        assert!(check_screen_cycle(Some(60)).is_none());
    }

    // ── /ui/oled ──────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn put_oled_applies_fields_persists_and_echoes_full_blob() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        let ui = dir.path().join("ground-station-ui.json");
        std::fs::write(&cfg, "agent:\n  name: gs-1\n").unwrap();

        let update = OledUpdate {
            brightness: Some(120),
            auto_dim_enabled: Some(false),
            screen_cycle_seconds: None,
        };
        let resp = put_ui_oled_at(&cfg, &ui, &update);
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        // The full UI blob, with the oled section mutated. brightness + auto_dim
        // take the request; screen_cycle keeps the default (5).
        assert_eq!(body["oled"]["brightness"], json!(120));
        assert_eq!(body["oled"]["auto_dim_enabled"], json!(false));
        assert_eq!(body["oled"]["screen_cycle_seconds"], json!(5));
        // The untouched sections are the full defaults.
        assert_eq!(
            body["buttons"]["mapping"]["B1_short"],
            json!("cycle_screen")
        );
        assert_eq!(
            body["screens"]["order"],
            json!(["home", "link", "drone", "network", "system", "qr"])
        );

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
        assert_eq!(
            oled.get("auto_dim_enabled")
                .and_then(serde_norway::Value::as_bool),
            Some(false)
        );
        // screen_cycle_seconds = 5 (the default that came through _load_ui_config).
        assert_eq!(
            oled.get("screen_cycle_seconds")
                .and_then(serde_norway::Value::as_i64),
            Some(5)
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
    async fn put_oled_overlays_an_existing_side_file_section() {
        // A stored oled brightness in the side-file is the base; the request
        // overrides only the supplied field; the rest of the stored value stands.
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        let ui = dir.path().join("ground-station-ui.json");
        std::fs::write(
            &ui,
            r#"{"oled":{"brightness":40,"screen_cycle_seconds":9}}"#,
        )
        .unwrap();

        let update = OledUpdate {
            brightness: None,
            auto_dim_enabled: Some(true),
            screen_cycle_seconds: None,
        };
        let resp = put_ui_oled_at(&cfg, &ui, &update);
        let body = body_json(resp).await;
        // brightness keeps the side-file value (40); screen_cycle keeps 9;
        // auto_dim takes the request.
        assert_eq!(body["oled"]["brightness"], json!(40));
        assert_eq!(body["oled"]["screen_cycle_seconds"], json!(9));
        assert_eq!(body["oled"]["auto_dim_enabled"], json!(true));
    }

    #[tokio::test]
    async fn put_oled_persist_fault_is_a_500_e_ui_save_failed() {
        // Point the config at a path whose parent cannot be created (a file stands
        // where a directory would need to be) so the YAML persist fails → 500.
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"x").unwrap();
        let cfg = blocker.join("config.yaml"); // parent "blocker" is a file
        let ui = dir.path().join("ground-station-ui.json");

        let resp = put_ui_oled_at(
            &cfg,
            &ui,
            &OledUpdate {
                brightness: Some(50),
                ..Default::default()
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
        let ui = dir.path().join("ground-station-ui.json");

        let mut mapping = std::collections::BTreeMap::new();
        mapping.insert("B1_short".to_string(), "show_qr".to_string());
        mapping.insert("B2_short".to_string(), "confirm".to_string());
        let update = ButtonsUpdate {
            mapping: Some(mapping),
        };
        let resp = put_ui_buttons_at(&cfg, &ui, &update);
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
        let ui = dir.path().join("ground-station-ui.json");

        let resp = put_ui_buttons_at(&cfg, &ui, &ButtonsUpdate::default());
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(
            body["buttons"]["mapping"]["B1_short"],
            json!("cycle_screen")
        );
    }

    // ── /ui/screens ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn put_screens_applies_order_and_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        let ui = dir.path().join("ground-station-ui.json");

        let update = ScreensUpdate {
            order: Some(vec!["home".to_string(), "link".to_string()]),
            enabled: Some(vec!["home".to_string()]),
        };
        let resp = put_ui_screens_at(&cfg, &ui, &update);
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
        let ui = dir.path().join("ground-station-ui.json");
        let update = ScreensUpdate {
            order: Some(vec!["home".to_string()]),
            enabled: None,
        };
        let resp = put_ui_screens_at(&cfg, &ui, &update);
        let body = body_json(resp).await;
        assert_eq!(body["screens"]["order"], json!(["home"]));
        // enabled keeps the full default list.
        assert_eq!(
            body["screens"]["enabled"],
            json!(["home", "link", "drone", "network", "system", "qr"])
        );
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
        assert_eq!(body["kiosk_enabled"], json!(false));
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
        assert_eq!(body["kiosk_enabled"], json!(false));

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
}
