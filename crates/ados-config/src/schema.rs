//! The agent-config JSON Schema and the effective config built from it.
//!
//! `schemas/agent-config.schema.json` is generated from the config model and
//! carries every field's type, enum, bounds and default. That makes it enough to
//! answer "what does this node's config say" without the model itself: the
//! effective config is the schema defaults overlaid with `config.yaml` along the
//! schema's model structure (unknown keys are ignored, as the model ignores
//! them).
//!
//! A native reader takes its typed slice through [`load_slice`], which reads
//! the effective config (so an absent key is the schema default, stated once)
//! and fails loudly on a malformed file or slice. [`assert_slice_conforms`] is
//! the test-side guard that a slice's own `Default` agrees with the schema.

use std::path::Path;
use std::sync::LazyLock;

use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{json, Map, Value};

/// The committed schema asset, embedded at build time.
pub const SCHEMA: &str = include_str!("../../../schemas/agent-config.schema.json");

static PARSED: LazyLock<Value> = LazyLock::new(|| {
    serde_json::from_str(SCHEMA).expect("the committed config schema is valid JSON")
});

/// The parsed schema.
pub fn parsed() -> &'static Value {
    &PARSED
}

/// Follow `$ref` and single-entry `allOf` wrappers to the node they name.
pub fn resolve(mut node: &Value) -> &Value {
    loop {
        if let Some(name) = node
            .get("$ref")
            .and_then(Value::as_str)
            .and_then(|r| r.rsplit('/').next())
        {
            match PARSED.get("$defs").and_then(|d| d.get(name)) {
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
pub fn properties(node: &Value) -> Option<&Map<String, Value>> {
    let node = resolve(node);
    if let Some(props) = node.get("properties").and_then(Value::as_object) {
        return Some(props);
    }
    node.get("anyOf")?
        .as_array()?
        .iter()
        .find_map(|branch| resolve(branch).get("properties").and_then(Value::as_object))
}

/// The schema node of a dotted key's parts, walking model fields only.
pub fn field<'a>(parts: impl IntoIterator<Item = &'a str>) -> Option<&'static Value> {
    parts.into_iter().try_fold(parsed(), |node, part| {
        properties(node).and_then(|props| props.get(part))
    })
}

/// A field's default: its declared `default`, else (a sub-model built by a
/// factory) the object of its own fields' defaults.
pub fn default_of(node: &Value) -> Option<Value> {
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

/// The whole default config.
pub fn defaults() -> Value {
    default_of(parsed()).unwrap_or_else(|| json!({}))
}

/// Overlay an on-disk value onto a default along the schema: model fields merge
/// key by key (unknown keys ignored), everything else is replaced. A null where a
/// sub-model belongs keeps the default.
pub fn merge(node: &Value, base: Value, overlay: &Value) -> Value {
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

/// The raw `config.yaml` document as JSON: `Ok(None)` when the file is absent
/// or empty; an unreadable or unparseable file is an error.
pub fn read_document(config_path: &Path) -> Result<Option<Value>, String> {
    let text = match std::fs::read_to_string(config_path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("config unreadable: {e}")),
    };
    if text.trim().is_empty() {
        return Ok(None);
    }
    serde_norway::from_str(&text)
        .map(Some)
        .map_err(|e| format!("config unparseable: {e}"))
}

/// The effective config: schema defaults overlaid with `config.yaml`. An absent
/// file is the defaults; an unreadable or unparseable one is an error, never a
/// silent fall back to defaults the node is not running.
pub fn effective(config_path: &Path) -> Result<Value, String> {
    let defaults = defaults();
    Ok(match read_document(config_path)? {
        Some(on_disk) => merge(parsed(), defaults, &on_disk),
        None => defaults,
    })
}

/// Read the typed slice at the JSON `pointer` (`"/video/wfb"`, `""` for the
/// whole config) of the effective config. Loud-fail: a malformed file, a pointer
/// outside the schema or a slice that does not deserialize is logged (once per
/// distinct error) and returned as the error, never silently defaulted.
pub fn load_slice<T: DeserializeOwned>(config_path: &Path, pointer: &str) -> Result<T, String> {
    let parsed = effective(config_path).and_then(|config| {
        let slice = config
            .pointer(pointer)
            .ok_or_else(|| format!("config slice {pointer:?} is not in the schema"))?;
        T::deserialize(slice).map_err(|e| match offending_key::<T>(slice, pointer) {
            Some(key) => format!("config slice {pointer:?} field {key:?}: {e}"),
            None => format!("config slice {pointer:?}: {e}"),
        })
    });
    match parsed {
        Ok(v) => {
            crate::clear_reported(pointer);
            Ok(v)
        }
        Err(e) => {
            crate::report_error(pointer, &e);
            Err(e)
        }
    }
}

/// The top-level key of `slice` whose value breaks deserialization: the first
/// key that differs from the schema default and, once put back to that default,
/// lets `T` deserialize. A `serde_json::Value` deserializer reports the type
/// mismatch but not where it is, and an operator needs the field to fix the file.
fn offending_key<T: DeserializeOwned>(slice: &Value, pointer: &str) -> Option<String> {
    let defaults = defaults();
    let defaults = defaults.pointer(pointer)?.as_object()?;
    let obj = slice.as_object()?;
    obj.iter().find_map(|(key, value)| {
        let default = defaults.get(key)?;
        if value == default {
            return None;
        }
        let mut probe = obj.clone();
        probe.insert(key.clone(), default.clone());
        T::deserialize(Value::Object(probe))
            .is_ok()
            .then(|| key.clone())
    })
}

/// Test guard: every key of `T::default()` (serialized) is a schema field at
/// `pointer`, and every default equals the schema's. Panics with the first
/// mismatch.
pub fn assert_slice_conforms<T: Default + Serialize>(pointer: &str) {
    let ours = serde_json::to_value(T::default()).expect("the slice default serializes");
    let node = pointer
        .split('/')
        .skip(1)
        .try_fold(parsed(), |node, part| {
            properties(node).and_then(|p| p.get(part))
        })
        .unwrap_or_else(|| panic!("config slice {pointer:?} is not in the schema"));
    conform(node, &ours, pointer);
}

fn conform(node: &Value, ours: &Value, at: &str) {
    if let (Some(props), Value::Object(fields)) = (properties(node), ours) {
        for (key, value) in fields {
            let child = props
                .get(key)
                .unwrap_or_else(|| panic!("{at}/{key} is read natively but not in the schema"));
            conform(child, value, &format!("{at}/{key}"));
        }
        return;
    }
    let schema_default = default_of(node).unwrap_or(Value::Null);
    let equal = match (schema_default.as_f64(), ours.as_f64()) {
        (Some(a), Some(b)) => (a - b).abs() <= 1e-6 * a.abs().max(1.0),
        _ => schema_default == *ours,
    };
    assert!(
        equal,
        "{at}: native default {ours} differs from the schema default {schema_default}"
    );
}

/// Coerce a string to the type the field currently holds (text callers can only
/// send strings); a native JSON scalar is taken as sent.
pub fn coerce(raw: &Value, current: &Value) -> Result<Value, String> {
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
pub fn validate(node: &Value, value: &Value) -> Result<Value, Vec<String>> {
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
    use serde::Deserialize;

    #[derive(Debug, Default, Deserialize, Serialize)]
    struct Separation {
        radius_m: i64,
        hard_m: i64,
    }

    #[derive(Debug, Deserialize, Serialize)]
    struct Conforming {
        radius_m: i64,
        hard_m: i64,
    }

    impl Default for Conforming {
        fn default() -> Self {
            Self {
                radius_m: 8,
                hard_m: 4,
            }
        }
    }

    #[derive(Debug, Default, Serialize)]
    struct Stray {
        not_a_field: bool,
    }

    #[test]
    fn slice_loader_reads_defaults_and_file_values() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        let s: Separation = load_slice(&cfg, "/swarm/separation").unwrap();
        assert_eq!((s.radius_m, s.hard_m), (8, 4));
        std::fs::write(&cfg, "swarm:\n  separation:\n    hard_m: 2\n").unwrap();
        let s: Separation = load_slice(&cfg, "/swarm/separation").unwrap();
        assert_eq!((s.radius_m, s.hard_m), (8, 2));
    }

    #[test]
    fn slice_loader_loud_fails() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        std::fs::write(&cfg, "swarm: [unclosed\n").unwrap();
        assert!(load_slice::<Separation>(&cfg, "/swarm/separation").is_err());
        std::fs::write(&cfg, "swarm:\n  separation:\n    hard_m: not-a-number\n").unwrap();
        let err = load_slice::<Separation>(&cfg, "/swarm/separation").unwrap_err();
        assert!(err.contains("hard_m"), "{err}");
        assert!(load_slice::<Separation>(&cfg, "/swarm/nope").is_err());
    }

    #[test]
    fn a_conforming_slice_passes() {
        assert_slice_conforms::<Conforming>("/swarm/separation");
    }

    #[test]
    #[should_panic(expected = "differs from the schema default")]
    fn a_divergent_default_panics() {
        assert_slice_conforms::<Separation>("/swarm/separation");
    }

    #[test]
    #[should_panic(expected = "not in the schema")]
    fn a_key_outside_the_schema_panics() {
        assert_slice_conforms::<Stray>("/swarm/separation");
    }
}
