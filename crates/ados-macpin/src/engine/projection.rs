//! A camelCase JSON projection of the per-adapter verdicts, for a status
//! API or a telemetry payload. The keys are already camelCase, so a consumer
//! must not put the result through a snake-to-camel remap.

use std::path::Path;

/// Read + parse the state file into a JSON document, or `None` when the file is
/// absent or malformed.
pub fn read_state(state_path: &Path) -> Option<serde_json::Value> {
    let text = std::fs::read_to_string(state_path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Project one state-file adapter object to camelCase keys.
pub fn adapter_to_camel(a: &serde_json::Map<String, serde_json::Value>) -> serde_json::Value {
    use serde_json::Value;
    let mut out = serde_json::Map::new();
    // Always present (carried through verbatim; an absent source key renders
    // JSON null).
    for key in ["name", "vidpid"] {
        out.insert(key.to_string(), a.get(key).cloned().unwrap_or(Value::Null));
    }
    out.insert(
        "usbPath".to_string(),
        a.get("usb_path").cloned().unwrap_or(Value::Null),
    );
    out.insert(
        "state".to_string(),
        a.get("state").cloned().unwrap_or(Value::Null),
    );
    // `applied_live` coerced to a bool with a false default.
    let applied_live = a.get("applied_live").map(value_is_truthy).unwrap_or(false);
    out.insert("appliedLive".to_string(), Value::Bool(applied_live));

    // Present only when the source value is non-null.
    for (src, dst) in [
        ("source", "source"),
        ("pinned_mac", "pinnedMac"),
        ("last_seen_mac", "lastSeenMac"),
        ("link_file", "linkFile"),
        ("deferred_reason", "deferredReason"),
    ] {
        if let Some(v) = a.get(src) {
            if !v.is_null() {
                out.insert(dst.to_string(), v.clone());
            }
        }
    }
    Value::Object(out)
}

/// Truthiness for the `applied_live` coercion: a JSON bool is itself; `null` is
/// false; a number is false iff zero; a string/array/object is false iff empty.
fn value_is_truthy(v: &serde_json::Value) -> bool {
    use serde_json::Value;
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// The projected adapter list from a parsed state document, or an empty list when
/// the document is absent / malformed / carries no `adapters` array.
pub fn state_json_to_camel(raw: Option<&serde_json::Value>) -> Vec<serde_json::Value> {
    raw.and_then(|d| d.get("adapters"))
        .and_then(serde_json::Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(serde_json::Value::as_object)
                .map(adapter_to_camel)
                .collect()
        })
        .unwrap_or_default()
}

/// The `{adapters: [...]}` camelCase block for a status body or telemetry
/// payload, or `None` when the state file is absent, malformed, or lists no
/// adapter.
///
/// `None` rather than an empty list on purpose: a node that has never pinned a
/// MAC has no verdict to report, and an empty list would claim it looked and
/// found nothing. There is no staleness gate: the state file is persistent, so
/// an old reading is still the current verdict.
pub fn adapters_camel_from(state_path: &Path) -> Option<serde_json::Value> {
    let raw = read_state(state_path);
    let adapters = state_json_to_camel(raw.as_ref());
    if adapters.is_empty() {
        return None;
    }
    Some(serde_json::json!({ "adapters": adapters }))
}
