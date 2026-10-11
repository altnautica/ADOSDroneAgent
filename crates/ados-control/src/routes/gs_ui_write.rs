//! Ground-station persisted-UI write routes.
//!
//! The OLED / button / screen UI knobs and the HDMI kiosk display config are
//! written from the GS setup webapp + the GCS Hardware tab. The read views live
//! in [`crate::routes::gs_ui_read`]; this module serves the four writes the front
//! can reproduce faithfully:
//!
//! - **`PUT /api/v1/ground-station/ui/oled`** — apply the supplied OLED
//!   `brightness` (the SSD1306 contrast, 0-255), persist the merged section into
//!   `ground_station.ui.oled` of the agent config, signal the OLED display
//!   service to reload, and echo the full UI config blob. Any other key in the
//!   body (including the retired `auto_dim_enabled` / `screen_cycle_seconds`,
//!   which no renderer ever applied) is a `422`.
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
//! ## One source of truth
//!
//! The three `/ui/*` writes seed from, and persist into, `ground_station.ui.<section>`
//! of the YAML agent config (the path the live services read, and the one the
//! `/ui` read serves), with each section merged over the built-in defaults, and
//! echo the full `{oled, buttons, screens}` blob. The legacy side-file is migrated
//! into that section when the config loads and nothing writes it any more, so
//! seeding from it would reset every earlier write on the next one.
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
//! After persisting, the front SIGHUPs the live display service so it reloads its
//! config without a restart (`ados-oled.service` for the display config,
//! `ados-pic.service` for the buttons) via `systemctl kill -s HUP <unit>`.
//!
//! Best-effort as to the RESPONSE -- the write is already durable, so a reload
//! that does not land costs a restart, not the setting -- but not silent: a
//! signal that does not land is logged with the unit, because otherwise the
//! write returns 200 while the running config is unchanged and nothing explains
//! why. It is also BOUNDED: the SIGHUP goes through [`crate::probe`], so a
//! `systemctl` blocked on a busy bus can no longer stall the write's response.
//! See [`signal_reload`].
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

use crate::config_store::{section_path, update_config};
use crate::routes::gs_ui_read::read_gs_section;
use crate::state::AppState;

mod blob;
mod persist;
mod ui_sections;

use blob::*;
use persist::*;
pub use ui_sections::put_ui_buttons;
pub use ui_sections::put_ui_oled;
pub use ui_sections::put_ui_screens;
#[cfg(test)]
use ui_sections::*;

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

/// Build the FastAPI 422 validation-error body for one numeric bound violation on a
/// request-body field.
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

/// OLED keys earlier releases accepted and persisted although no renderer applied
/// them. They are rejected on write and stripped from a persisted section so a
/// stale config stops advertising them.
const RETIRED_OLED_KEYS: [&str; 2] = ["auto_dim_enabled", "screen_cycle_seconds"];

// ---------------------------------------------------------------------------
// On-disk seam: the agent config.
// ---------------------------------------------------------------------------

/// The agent config path (`/etc/ados/config.yaml` on a real box), the YAML store
/// the `/ui/*` sections persist into under `ground_station.ui.<section>`.
fn config_yaml_path(state: &AppState) -> PathBuf {
    state.pairing_paths.config.clone()
}

// ---------------------------------------------------------------------------
// SIGHUP the live UI services so a config write takes effect without a restart.
//
// The signal is raised by the async handlers, not by the sync `put_*_at` writers
// that used to raise it. Bounding the SIGHUP makes `signal_reload` async, and
// hoisting the effect one frame up was the smaller and better change than
// turning the write logic — and the seven unit tests that call it directly —
// async for a fire-and-forget call whose result is discarded either way. Each
// handler fires it on exactly the previous condition: the persist having
// succeeded, which for these writers is the same thing as the response being a
// success (the only other outcome is the `E_UI_SAVE_FAILED` 500).
// ---------------------------------------------------------------------------

/// SIGHUP the named systemd unit so it reloads its UI config without a restart.
///
/// Best-effort by design: the config write has already succeeded and is durable,
/// so a reload that does not land costs the operator a restart, not the setting.
/// The failure is LOGGED rather than swallowed, because the cases where it fails
/// are exactly the ones worth knowing about -- the unit inactive on a box whose
/// binaries never landed, or systemd absent -- and without a line here the write
/// returns 200 while the running config is unchanged, with nothing to explain
/// why. `systemctl kill -s HUP <unit>` targets the unit rather than resolving a
/// MainPID, so it needs no knowledge of the process tree.
///
/// Bounded through [`crate::probe::status_only`] rather than a blocking
/// `Command::status()`. That call WAITED for `systemctl` to exit, on the reactor,
/// inside a config-write handler: a `systemctl` blocking on a busy D-Bus stalled
/// the write's response for as long as the bus took, which is unbounded. The
/// probe kills and reaps the child at [`crate::probe::PROBE_TIMEOUT`], so the
/// worst case is a late warning instead of a wedged request.
///
/// The two failures the blocking version told apart (a refused signal vs. one
/// that could not be sent) collapse into one warning, because `status_only`
/// folds a non-zero exit, a spawn error and a timeout together. The operator's
/// action is the same in each case and the response never depended on which it
/// was, so nothing downstream loses information.
async fn signal_reload(unit: &str) {
    // A non-Linux dev host has no systemd, so there is no unit to signal and
    // nothing to report. Keeping the guaranteed no-op (rather than letting the
    // cross-platform probe spawn a `systemctl` that cannot exist) avoids both the
    // pointless spawn and a warning on every UI write that would be true and
    // useless.
    if cfg!(not(target_os = "linux")) {
        return;
    }
    if !crate::probe::status_only(
        "systemctl",
        &["kill", "-s", "HUP", unit],
        crate::probe::PROBE_TIMEOUT,
    )
    .await
    {
        tracing::warn!(
            unit,
            "UI config saved but the reload signal did not land; \
             the running config is unchanged until the unit restarts"
        );
    }
}

/// SIGHUP the OLED display service so it reloads its config.
async fn signal_oled_reload() {
    signal_reload("ados-oled.service").await;
}

/// SIGHUP the daemon that owns the button mapping so a write takes effect.
///
/// This is `ados-pic`, not a separate button unit. The native arbiter reads the
/// front-panel GPIO in-process and rebuilds its mapping on SIGHUP; the packaged
/// button service it replaced is gone. Signalling that old unit meant the write
/// reported success while the mapping stayed as it was until the daemon next
/// restarted.
async fn signal_buttons_reload() {
    signal_reload("ados-pic.service").await;
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
///
/// A kiosk that is disabled exits and stays down, so turning it back on
/// restarts `ados-kiosk`. Disabling it and changing the resolution need no
/// restart: the running kiosk watches the config and applies both itself.
pub async fn put_display(
    State(state): State<AppState>,
    Json(update): Json<DisplayUpdate>,
) -> Response {
    if !is_ground_station(&state) {
        return profile_mismatch();
    }
    let path = config_yaml_path(&state);
    let was_enabled = display_wire_from_kiosk(&read_gs_kiosk_section(&path))
        .get("kiosk_enabled")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let response = put_display_at(&path, &update);
    if response.status().is_success() && update.kiosk_enabled == Some(true) && !was_enabled {
        let outcome = crate::routes::service_control::restart_unit("ados-kiosk").await;
        if outcome.get("status").and_then(Value::as_str) == Some("error") {
            tracing::warn!(outcome = %outcome, "kiosk enabled but its restart failed");
        }
    }
    response
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
mod tests;
