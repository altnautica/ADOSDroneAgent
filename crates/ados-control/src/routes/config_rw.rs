//! `GET /api/config` and `PUT /api/config`: the agent config, read and written
//! natively.
//!
//! The config model is described by the committed JSON Schema
//! ([`ados_config::schema`], also served at `/api/config/schema`). That schema
//! carries every field's default, which makes it enough to answer both routes
//! without the model itself:
//!
//! * **Read.** The effective config (defaults overlaid with the on-disk
//!   `config.yaml`, unknown keys ignored, as the model ignores them), plus
//!   `agent.board_override` from the board-override file beside the config.
//!   Every `x-secret` field that holds a value is replaced with `***`.
//! * **Write.** One dotted key at a time. The key must name a field in the
//!   schema; a string value is coerced to the field's current type (text callers
//!   can only send strings); the result is validated against the field's schema
//!   (type, enum, bounds), then the whole resulting config against the model's
//!   cross-field rules ([`CROSS_FIELD`]), and merged into `config.yaml` through
//!   the shared config store, which preserves every other key and the file's
//!   0600 mode. A write that landed then runs the config side effects (enable
//!   markers, unit kicks; [`ados_config::side_effects`]).
//!
//! Response shapes: the write answers `{status: "ok", key, value, persisted:
//! true}`, or a 500 `{status: "error", key, value, persisted: false,
//! persist_error}` when the file write failed, a key the model does not have is
//! a 200 `{"error": "Key not found: <key>"}`, a string that does not coerce is a
//! 200 `{"error": "Invalid value: …"}`, a value the field rejects is a 422
//! `E_VALIDATION`, a value that breaks a cross-field rule is a 422 `{"error":
//! "E_INVALID", "field", "message"}`, and writing the `***` sentinel to a
//! secret is a 400 `E_REDACTED_SENTINEL`.

use std::path::Path;
use std::sync::LazyLock;

use ados_config::schema::{self, coerce, properties, validate};
use ados_config::side_effects;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::config_store::{section_path, update_config};
use crate::routes::detail;
use crate::state::AppState;

/// What a secret reads as when it holds a value.
const REDACTED_SENTINEL: &str = "***";

/// The file the HAL detector reads a forced board slug from, beside the config.
const BOARD_OVERRIDE_FILE: &str = "board_override";

/// Every `x-secret` field, as its dotted path split into keys.
static SECRET_PATHS: LazyLock<Vec<Vec<String>>> = LazyLock::new(|| {
    let mut out = Vec::new();
    collect_secrets(schema::parsed(), &mut Vec::new(), &mut out);
    out
});

fn collect_secrets(node: &Value, path: &mut Vec<String>, out: &mut Vec<Vec<String>>) {
    let Some(props) = properties(node) else {
        return;
    };
    for (key, prop) in props {
        path.push(key.clone());
        if prop.get("x-secret") == Some(&Value::Bool(true)) {
            out.push(path.clone());
        } else {
            collect_secrets(prop, path, out);
        }
        path.pop();
    }
}

// ---------------------------------------------------------------------------
// Cross-field rules.
// ---------------------------------------------------------------------------

/// A rule over the whole effective config that no single field's schema can
/// state.
pub(crate) struct CrossRule {
    /// The dotted field a refusal names.
    pub field: &'static str,
    pub message: &'static str,
    /// True when `config` breaks the rule.
    pub violated: fn(&Value) -> bool,
}

/// The config model's cross-field validators, one row per rejecting
/// `model_validator`. A config that breaks one fails to load in every Python
/// unit, so the native write refuses it the same way.
pub(crate) const CROSS_FIELD: &[CrossRule] = &[
    CrossRule {
        field: "battery.critical_cell_mv",
        message: "battery.critical_cell_mv must be less than battery.low_cell_mv",
        violated: battery_critical_not_below_low,
    },
    CrossRule {
        field: "swarm.separation.hard_m",
        message: "swarm.separation.hard_m must be less than swarm.separation.radius_m",
        violated: separation_hard_not_below_radius,
    },
];

/// A write-time rule, not a model rule: a drone has no pilot-in-command arbiter
/// (it runs on a ground station), so arbitration there would refuse every
/// plugin command. Only a write that introduces it is refused, so an existing
/// file still loads and other keys stay writable.
pub(crate) static INJECTOR_ARBITRATION_ON_DRONE: CrossRule = CrossRule {
    field: "mavlink.injector_arbitration",
    message:
        "injector arbitration needs the pilot-in-command arbiter, which runs on a ground station",
    violated: arbitration_on_drone,
};

fn int_at(config: &Value, pointer: &str) -> Option<i64> {
    config.pointer(pointer).and_then(Value::as_i64)
}

fn battery_critical_not_below_low(c: &Value) -> bool {
    let critical = int_at(c, "/battery/critical_cell_mv");
    let low = int_at(c, "/battery/low_cell_mv");
    matches!((critical, low), (Some(crit), Some(low)) if crit >= low)
}

fn separation_hard_not_below_radius(c: &Value) -> bool {
    let hard = int_at(c, "/swarm/separation/hard_m");
    let radius = int_at(c, "/swarm/separation/radius_m");
    matches!((hard, radius), (Some(hard), Some(radius)) if hard >= radius)
}

fn arbitration_on_drone(c: &Value) -> bool {
    c.pointer("/agent/profile").and_then(Value::as_str) == Some("drone")
        && c.pointer("/mavlink/injector_arbitration") == Some(&Value::Bool(true))
}

/// The rule a write from `current` to `candidate` breaks, if any.
fn refused_by(current: &Value, candidate: &Value) -> Option<&'static CrossRule> {
    if let Some(rule) = CROSS_FIELD.iter().find(|r| (r.violated)(candidate)) {
        return Some(rule);
    }
    let rule = &INJECTOR_ARBITRATION_ON_DRONE;
    ((rule.violated)(candidate) && !(rule.violated)(current)).then_some(rule)
}

// ---------------------------------------------------------------------------
// GET /api/config
// ---------------------------------------------------------------------------

/// Replace every secret that holds a value with the sentinel. An unset secret
/// stays as it is, so the read still says whether one is set.
fn redact(config: &mut Value) {
    for path in SECRET_PATHS.iter() {
        let Some((leaf, parents)) = path.split_last() else {
            continue;
        };
        let parent = parents.iter().try_fold(&mut *config, |n, k| n.get_mut(k));
        if let Some(value) = parent.and_then(|p| p.get_mut(leaf)) {
            if truthy(value) {
                *value = json!(REDACTED_SENTINEL);
            }
        }
    }
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// The forced board slug, or `""` for auto-detect.
fn board_override(config_path: &Path) -> String {
    config_path
        .parent()
        .map(|dir| dir.join(BOARD_OVERRIDE_FILE))
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// `GET /api/config` → the effective config, secrets redacted.
pub async fn get_config(State(state): State<AppState>) -> Response {
    let path = state.pairing_paths.config.clone();
    match tokio::task::spawn_blocking(move || read_config_at(&path)).await {
        Ok(Ok(body)) => Json(body).into_response(),
        Ok(Err(e)) => detail(StatusCode::SERVICE_UNAVAILABLE, e),
        Err(e) => detail(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

fn read_config_at(config_path: &Path) -> Result<Value, String> {
    let mut config = schema::effective(config_path)?;
    if let Some(agent) = config.get_mut("agent").and_then(Value::as_object_mut) {
        agent.insert(
            "board_override".to_string(),
            json!(board_override(config_path)),
        );
    }
    redact(&mut config);
    Ok(config)
}

// ---------------------------------------------------------------------------
// PUT /api/config
// ---------------------------------------------------------------------------

/// The `PUT /api/config` body: a dotted key and a scalar value.
#[derive(Debug, Deserialize)]
pub struct ConfigUpdate {
    pub key: String,
    pub value: Value,
}

/// `PUT /api/config` → `{status, key, value, persisted[, persist_error]}`.
pub async fn put_config(
    State(state): State<AppState>,
    Json(update): Json<ConfigUpdate>,
) -> Response {
    let path = state.pairing_paths.config.clone();
    // The blocking pass writes the file and the markers and records the unit
    // commands; the commands then run here as bounded async probes, so a wedged
    // `systemctl` never holds a reactor or blocking-pool thread.
    let issued: std::sync::Arc<std::sync::Mutex<Vec<Vec<String>>>> = Default::default();
    let record = std::sync::Arc::clone(&issued);
    let write = move || {
        let run = move |program: &str, args: &[&str]| {
            let mut cmd = vec![program.to_string()];
            cmd.extend(args.iter().map(|a| a.to_string()));
            record.lock().unwrap_or_else(|p| p.into_inner()).push(cmd);
            true
        };
        put_config_at(&path, &update, &run)
    };
    let resp = match tokio::task::spawn_blocking(write).await {
        Ok(resp) => resp,
        Err(e) => return detail(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let commands = std::mem::take(&mut *issued.lock().unwrap_or_else(|p| p.into_inner()));
    for cmd in commands {
        let Some((program, args)) = cmd.split_first() else {
            continue;
        };
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        if !crate::probe::status_only(program, &args, crate::probe::PROBE_TIMEOUT).await {
            tracing::warn!(command = %cmd.join(" "), "config side-effect command failed");
        }
    }
    resp
}

fn nested_error(status: StatusCode, error: Value) -> Response {
    (status, Json(json!({ "detail": { "error": error } }))).into_response()
}

/// The write. `run` issues the side-effect commands; the enable markers live
/// beside the config.
fn put_config_at(
    config_path: &Path,
    update: &ConfigUpdate,
    run: &dyn Fn(&str, &[&str]) -> bool,
) -> Response {
    let key = update.key.as_str();
    if !matches!(
        update.value,
        Value::Bool(_) | Value::Number(_) | Value::String(_)
    ) {
        return detail(
            StatusCode::UNPROCESSABLE_ENTITY,
            "value must be a boolean, a number or a string",
        );
    }
    let parts: Vec<&str> = key.split('.').collect();
    let is_secret = SECRET_PATHS.iter().any(|p| p.iter().eq(parts.iter()));
    if is_secret && update.value == json!(REDACTED_SENTINEL) {
        return nested_error(
            StatusCode::BAD_REQUEST,
            json!({
                "code": "E_REDACTED_SENTINEL",
                "message": format!(
                    "Refusing to write the redaction sentinel '{REDACTED_SENTINEL}' to secret path '{key}'. Submit the real value or omit this field from the PUT."
                ),
            }),
        );
    }

    let Some(leaf) = schema::field(parts.iter().copied()) else {
        return Json(json!({ "error": format!("Key not found: {key}") })).into_response();
    };

    let config = match schema::effective(config_path) {
        Ok(config) => config,
        Err(e) => return detail(StatusCode::SERVICE_UNAVAILABLE, e),
    };
    let current = parts
        .iter()
        .try_fold(&config, |n, k| n.get(*k))
        .unwrap_or(&Value::Null);

    let candidate = match coerce(&update.value, current) {
        Ok(v) => v,
        Err(e) => return Json(json!({ "error": format!("Invalid value: {e}") })).into_response(),
    };
    let value = match validate(leaf, &candidate) {
        Ok(v) => v,
        Err(messages) => {
            return nested_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                json!({ "code": "E_VALIDATION", "key": key, "messages": messages }),
            )
        }
    };

    let mut next = config.clone();
    if let Some(slot) = parts.iter().try_fold(&mut next, |n, k| n.get_mut(*k)) {
        *slot = value.clone();
    }
    if let Some(rule) = refused_by(&config, &next) {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "error": "E_INVALID", "field": rule.field, "message": rule.message })),
        )
            .into_response();
    }

    let yaml_value = match serde_norway::to_value(&value) {
        Ok(v) => v,
        Err(e) => return detail(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let (parents, last) = parts.split_at(parts.len() - 1);
    let persisted = update_config(config_path, |root| {
        let before = root.clone();
        section_path(root, parents)
            .insert(serde_norway::Value::String(last[0].to_string()), yaml_value);
        Ok((before, root.clone()))
    });
    // The route has no live-apply leg, so a value that did not reach the file
    // took no effect at all: that is a server error, not a success.
    match persisted {
        Ok(write) => {
            if write.wrote {
                let (before, after) = write.value;
                run_side_effects(config_path, before, after, run);
            }
            Json(json!({ "status": "ok", "key": key, "value": value, "persisted": true }))
                .into_response()
        }
        Err(e) => {
            tracing::warn!(error = %e, key, "config write failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "status": "error",
                    "key": key,
                    "value": value,
                    "persisted": false,
                    "persist_error": e.to_string(),
                })),
            )
                .into_response()
        }
    }
}

/// Mirror a landed write onto the markers and units. A document that does not
/// convert to JSON (a non-string YAML key) is skipped rather than read as
/// empty, which would remove every marker.
fn run_side_effects(
    config_path: &Path,
    before: serde_norway::Mapping,
    after: serde_norway::Mapping,
    run: &dyn Fn(&str, &[&str]) -> bool,
) {
    let as_json = |m| serde_json::to_value(serde_norway::Value::Mapping(m));
    let (Ok(old), Ok(new)) = (as_json(before), as_json(after)) else {
        tracing::warn!("config side effects skipped: the document has a non-string key");
        return;
    };
    let etc_dir = config_path.parent().unwrap_or(Path::new("."));
    let report = side_effects::apply(&old, &new, etc_dir, run);
    if !report.failures.is_empty() {
        tracing::warn!(failures = ?report.failures, "config side effects partly failed");
    } else if !report.commands.is_empty() || !report.markers_changed.is_empty() {
        tracing::info!(
            markers = ?report.markers_changed,
            commands = ?report.commands,
            "config side effects applied"
        );
    }
}

#[cfg(test)]
mod tests;
