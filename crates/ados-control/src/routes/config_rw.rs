//! `GET /api/config` and `PUT /api/config`: the agent config, read and written
//! natively.
//!
//! The config model is described by the committed JSON Schema the front already
//! serves at `/api/config/schema` (generated from the config model, drift-guarded
//! against it). That schema carries every field's default, which makes it enough to
//! answer both routes without the model itself:
//!
//! * **Read.** The defaults, overlaid with the on-disk `config.yaml` along the
//!   schema's model structure (unknown keys are ignored, as the model ignores
//!   them), plus `agent.board_override` from the board-override file beside the
//!   config. Every `x-secret` field that holds a value is replaced with `***`.
//! * **Write.** One dotted key at a time. The key must name a field in the
//!   schema; a string value is coerced to the field's current type (text callers
//!   can only send strings); the result is validated against the field's schema
//!   (type, enum, bounds) and merged into `config.yaml` through the shared config
//!   store, which preserves every other key and the file's 0600 mode.
//!
//! Response shapes match the handlers they replace: the write answers
//! `{status, key, value, persisted[, persist_error]}`, a key the model does not
//! have is a 200 `{"error": "Key not found: <key>"}`, a string that does not
//! coerce is a 200 `{"error": "Invalid value: …"}`, a value the field rejects is a
//! 422 `E_VALIDATION`, and writing the `***` sentinel to a secret is a 400
//! `E_REDACTED_SENTINEL`.

use std::path::Path;
use std::sync::LazyLock;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::config_store::{section_path, update_config};
use crate::routes::detail;
use crate::state::AppState;

/// What a secret reads as when it holds a value.
const REDACTED_SENTINEL: &str = "***";

/// The file the HAL detector reads a forced board slug from, beside the config.
const BOARD_OVERRIDE_FILE: &str = "board_override";

/// The parsed config schema, shared with `/api/config/schema`.
static SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    serde_json::from_str(crate::routes::config_schema::AGENT_CONFIG_SCHEMA)
        .expect("the committed config schema is valid JSON")
});

/// Every `x-secret` field, as its dotted path split into keys.
static SECRET_PATHS: LazyLock<Vec<Vec<String>>> = LazyLock::new(|| {
    let mut out = Vec::new();
    collect_secrets(&SCHEMA, &mut Vec::new(), &mut out);
    out
});

// ---------------------------------------------------------------------------
// Schema walking.
// ---------------------------------------------------------------------------

/// Follow `$ref` and single-entry `allOf` wrappers to the node they name.
fn resolve(mut node: &Value) -> &Value {
    loop {
        if let Some(name) = node
            .get("$ref")
            .and_then(Value::as_str)
            .and_then(|r| r.rsplit('/').next())
        {
            match SCHEMA.get("$defs").and_then(|d| d.get(name)) {
                Some(target) => {
                    node = target;
                    continue;
                }
                None => return node,
            }
        }
        if let Some([only]) = node
            .get("allOf")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
        {
            node = only;
            continue;
        }
        return node;
    }
}

/// The model fields of a node (through `$ref`, `allOf`, or an optional
/// `anyOf` branch), or `None` for a leaf, list or free-form map.
fn properties(node: &Value) -> Option<&Map<String, Value>> {
    let node = resolve(node);
    if let Some(props) = node.get("properties").and_then(Value::as_object) {
        return Some(props);
    }
    node.get("anyOf")?
        .as_array()?
        .iter()
        .find_map(|branch| resolve(branch).get("properties").and_then(Value::as_object))
}

/// A field's default: its declared `default`, else (a sub-model built by a
/// factory) the object of its own fields' defaults.
fn default_of(node: &Value) -> Option<Value> {
    if let Some(d) = node.get("default") {
        return Some(d.clone());
    }
    let props = properties(node)?;
    Some(Value::Object(
        props
            .iter()
            .filter_map(|(k, p)| default_of(p).map(|d| (k.clone(), d)))
            .collect(),
    ))
}

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

/// Overlay an on-disk value onto a default along the schema: model fields merge
/// key by key (unknown keys ignored), everything else is replaced. A null where a
/// sub-model belongs keeps the default.
fn merge(node: &Value, base: Value, overlay: &Value) -> Value {
    let Some(props) = properties(node) else {
        return overlay.clone();
    };
    let Value::Object(over) = overlay else {
        return base;
    };
    let mut out = match base {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    for (key, value) in over {
        let Some(prop) = props.get(key) else {
            continue;
        };
        let base = out
            .remove(key)
            .or_else(|| default_of(prop))
            .unwrap_or(Value::Null);
        out.insert(key.clone(), merge(prop, base, value));
    }
    Value::Object(out)
}

// ---------------------------------------------------------------------------
// The effective config.
// ---------------------------------------------------------------------------

/// The effective config: schema defaults overlaid with `config.yaml`. An absent
/// file is the defaults; an unreadable or unparseable one is an error, never a
/// silent fall back to defaults the node is not running.
fn effective_config(config_path: &Path) -> Result<Value, String> {
    let defaults = default_of(&SCHEMA).unwrap_or_else(|| json!({}));
    let text = match std::fs::read_to_string(config_path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(defaults),
        Err(e) => return Err(format!("config unreadable: {e}")),
    };
    if text.trim().is_empty() {
        return Ok(defaults);
    }
    let on_disk: Value =
        serde_norway::from_str(&text).map_err(|e| format!("config unparseable: {e}"))?;
    Ok(merge(&SCHEMA, defaults, &on_disk))
}

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

// ---------------------------------------------------------------------------
// GET /api/config
// ---------------------------------------------------------------------------

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
    let mut config = effective_config(config_path)?;
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
    match tokio::task::spawn_blocking(move || put_config_at(&path, &update)).await {
        Ok(resp) => resp,
        Err(e) => detail(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

fn nested_error(status: StatusCode, error: Value) -> Response {
    (status, Json(json!({ "detail": { "error": error } }))).into_response()
}

fn put_config_at(config_path: &Path, update: &ConfigUpdate) -> Response {
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

    // The field's schema node, walking model fields only.
    let Some(leaf) = parts.iter().try_fold(&*SCHEMA, |node, part| {
        properties(node).and_then(|props| props.get(*part))
    }) else {
        return Json(json!({ "error": format!("Key not found: {key}") })).into_response();
    };

    let config = match effective_config(config_path) {
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

    let yaml_value = match serde_norway::to_value(&value) {
        Ok(v) => v,
        Err(e) => return detail(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let (parents, last) = parts.split_at(parts.len() - 1);
    let persisted = update_config(config_path, |root| {
        section_path(root, parents)
            .insert(serde_norway::Value::String(last[0].to_string()), yaml_value);
        Ok(())
    });
    let mut body =
        json!({ "status": "ok", "key": key, "value": value, "persisted": persisted.is_ok() });
    if let Err(e) = persisted {
        tracing::warn!(error = %e, key, "config write failed");
        body["persist_error"] = json!(e.to_string());
    }
    Json(body).into_response()
}

/// Coerce a string to the type the field currently holds (text callers can only
/// send strings); a native JSON scalar is taken as sent.
fn coerce(raw: &Value, current: &Value) -> Result<Value, String> {
    let Value::String(text) = raw else {
        return Ok(raw.clone());
    };
    Ok(match current {
        Value::Bool(_) => json!(matches!(
            text.trim().to_ascii_lowercase().as_str(),
            "true" | "1" | "yes" | "on"
        )),
        Value::Number(n) if n.is_f64() => json!(text
            .trim()
            .parse::<f64>()
            .map_err(|e| format!("could not convert string to float: '{text}' ({e})"))?),
        Value::Number(_) => json!(text
            .trim()
            .parse::<i64>()
            .map_err(|e| format!("invalid literal for int(): '{text}' ({e})"))?),
        _ => raw.clone(),
    })
}

/// Validate a scalar against a field's schema, returning the stored form (an
/// integer written to a number field is stored as a float). Errors are the
/// reasons the field refused it.
fn validate(node: &Value, value: &Value) -> Result<Value, Vec<String>> {
    let node = resolve(node);
    if let Some(branches) = node.get("anyOf").and_then(Value::as_array) {
        let mut reasons = Vec::new();
        for branch in branches {
            match validate(branch, value) {
                Ok(v) => return Ok(v),
                Err(mut r) => reasons.append(&mut r),
            }
        }
        return Err(reasons);
    }
    let typed = match node.get("type").and_then(Value::as_str) {
        Some("boolean") => value.is_boolean().then(|| value.clone()),
        Some("integer") => value
            .as_i64()
            .or_else(|| {
                value
                    .as_f64()
                    .filter(|f| f.fract() == 0.0)
                    .map(|f| f as i64)
            })
            .map(|i| json!(i)),
        Some("number") => value.as_f64().map(|f| json!(f)),
        Some("string") => value.is_string().then(|| value.clone()),
        Some("null") => value.is_null().then_some(Value::Null),
        Some(other) => return Err(vec![format!("a {other} field cannot be set to a scalar")]),
        None => Some(value.clone()),
    };
    let Some(typed) = typed else {
        let want = node.get("type").and_then(Value::as_str).unwrap_or("value");
        return Err(vec![format!("expected a {want}, got {value}")]);
    };
    if let Some(allowed) = node.get("enum").and_then(Value::as_array) {
        if !allowed.contains(&typed) {
            return Err(vec![format!(
                "must be one of {}",
                Value::Array(allowed.clone())
            )]);
        }
    }
    if let Some(n) = typed.as_f64() {
        if let Some(min) = node.get("minimum").and_then(Value::as_f64) {
            if n < min {
                return Err(vec![format!("must be at least {min}")]);
            }
        }
        if let Some(max) = node.get("maximum").and_then(Value::as_f64) {
            if n > max {
                return Err(vec![format!("must be at most {max}")]);
            }
        }
    }
    Ok(typed)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn body_of(resp: Response) -> (StatusCode, Value) {
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    fn update(key: &str, value: Value) -> ConfigUpdate {
        ConfigUpdate {
            key: key.to_string(),
            value,
        }
    }

    #[test]
    fn an_absent_config_reads_as_the_full_default_model() {
        let dir = tempfile::tempdir().unwrap();
        let body = read_config_at(&dir.path().join("config.yaml")).unwrap();
        // Top-level blocks and a factory-built sub-model are all present.
        for block in [
            "agent", "mavlink", "video", "network", "server", "security", "ui",
        ] {
            assert!(body.get(block).is_some(), "missing {block}");
        }
        assert_eq!(body["ui"]["theme"], json!("dark"));
        assert_eq!(body["agent"]["board_override"], json!(""));
        assert!(body["mavlink"]["endpoints"].is_array());
    }

    #[test]
    fn the_file_overlays_the_defaults_and_unknown_keys_are_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        std::fs::write(
            &cfg,
            "agent:\n  name: gs-7\n  bogus: 1\nvideo:\n  wfb:\n    channel: 161\nnot_a_block: true\n",
        )
        .unwrap();
        std::fs::write(dir.path().join(BOARD_OVERRIDE_FILE), "rock-5c\n").unwrap();
        let body = read_config_at(&cfg).unwrap();
        assert_eq!(body["agent"]["name"], json!("gs-7"));
        assert_eq!(
            body["agent"]["profile"],
            json!("auto"),
            "unset sibling keeps its default"
        );
        assert!(body["agent"].get("bogus").is_none());
        assert!(body.get("not_a_block").is_none());
        assert_eq!(body["video"]["wfb"]["channel"], json!(161));
        assert_eq!(body["agent"]["board_override"], json!("rock-5c"));
    }

    #[test]
    fn every_set_secret_is_redacted_and_an_unset_one_is_left_empty() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        std::fs::write(
            &cfg,
            "security:\n  api:\n    api_key: real-key\n  hmac_secret: s3cret\nserver:\n  mqtt_password: pw\nnetwork:\n  hotspot:\n    password: hotspotpw\n",
        )
        .unwrap();
        let body = read_config_at(&cfg).unwrap();
        assert_eq!(SECRET_PATHS.len(), 6);
        assert_eq!(body["security"]["api"]["api_key"], json!("***"));
        assert_eq!(body["security"]["hmac_secret"], json!("***"));
        assert_eq!(body["server"]["mqtt_password"], json!("***"));
        assert_eq!(body["network"]["hotspot"]["password"], json!("***"));
        assert!(!serde_json::to_string(&body).unwrap().contains("s3cret"));
        assert_ne!(body["network"]["wifi_client"]["password"], json!("***"));
    }

    #[test]
    fn an_unparseable_config_is_an_error_not_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        std::fs::write(&cfg, "agent: [unclosed\n").unwrap();
        assert!(read_config_at(&cfg).is_err());
    }

    #[tokio::test]
    async fn a_write_coerces_a_string_to_the_field_type_and_persists_it() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        std::fs::write(&cfg, "agent:\n  name: gs-7\n").unwrap();
        let (status, body) = body_of(put_config_at(
            &cfg,
            &update("video.wfb.channel", json!("149")),
        ))
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({"status": "ok", "key": "video.wfb.channel", "value": 149, "persisted": true})
        );
        let read = read_config_at(&cfg).unwrap();
        assert_eq!(read["video"]["wfb"]["channel"], json!(149));
        assert_eq!(read["agent"]["name"], json!("gs-7"), "other keys survive");
    }

    #[tokio::test]
    async fn an_unknown_key_is_a_key_not_found_error() {
        let dir = tempfile::tempdir().unwrap();
        let (status, body) = body_of(put_config_at(
            &dir.path().join("c.yaml"),
            &update("agent.nope", json!(1)),
        ))
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({"error": "Key not found: agent.nope"}));
    }

    #[tokio::test]
    async fn a_value_the_field_refuses_is_a_422_and_nothing_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        std::fs::write(&cfg, "agent:\n  name: x\n").unwrap();
        for (key, value) in [
            ("ui.theme", json!("purple")),
            ("agent.name", json!(5)),
            ("mavlink.endpoints", json!("x")),
        ] {
            let (status, body) = body_of(put_config_at(&cfg, &update(key, value))).await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{key}");
            assert_eq!(body["detail"]["error"]["code"], json!("E_VALIDATION"));
        }
        assert_eq!(
            std::fs::read_to_string(&cfg).unwrap(),
            "agent:\n  name: x\n"
        );
    }

    #[tokio::test]
    async fn the_redaction_sentinel_is_never_written_to_a_secret() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        std::fs::write(&cfg, "security:\n  hmac_secret: real\n").unwrap();
        let (status, body) = body_of(put_config_at(
            &cfg,
            &update("security.hmac_secret", json!("***")),
        ))
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            body["detail"]["error"]["code"],
            json!("E_REDACTED_SENTINEL")
        );
        assert!(std::fs::read_to_string(&cfg).unwrap().contains("real"));
    }

    #[tokio::test]
    async fn a_string_that_does_not_coerce_is_an_invalid_value_error() {
        let dir = tempfile::tempdir().unwrap();
        let (status, body) = body_of(put_config_at(
            &dir.path().join("c.yaml"),
            &update("video.wfb.channel", json!("abc")),
        ))
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["error"]
            .as_str()
            .unwrap()
            .starts_with("Invalid value:"));
    }
}
