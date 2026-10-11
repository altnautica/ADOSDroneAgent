//! JSON body -> logging-store detail map conversion for the sidecar events.

/// Convert a JSON object into the logging store's open detail map (`Fields`), so
/// the full wfb-status body can ride a single `link.wfb_status` event. Recurses
/// through nested arrays / objects; numbers preserve their integer-vs-float kind,
/// and JSON null round-trips to msgpack nil. A non-object input yields an empty
/// map (the body is always an object, so this only guards against a future
/// change). The map decodes back to the identical JSON in the query path, so the
/// store row is a faithful copy of the sidecar.
pub(crate) fn json_object_to_fields(value: &serde_json::Value) -> ados_protocol::logd::Fields {
    match value {
        serde_json::Value::Object(map) => map
            .iter()
            .map(|(k, v)| (k.clone(), json_to_mpv(v)))
            .collect(),
        _ => ados_protocol::logd::Fields::new(),
    }
}

/// Recursively map a `serde_json::Value` onto the msgpack value the detail map
/// carries. Integers stay integers (signed when negative), floats stay floats,
/// and null becomes nil, so the round-trip through the store preserves the exact
/// JSON shape the REST base merges over.
fn json_to_mpv(value: &serde_json::Value) -> ados_protocol::logd::Value {
    use ados_protocol::logd::Value as MpVal;
    match value {
        serde_json::Value::Null => MpVal::Nil,
        serde_json::Value::Bool(b) => MpVal::from(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                MpVal::from(i)
            } else if let Some(u) = n.as_u64() {
                MpVal::from(u)
            } else {
                MpVal::from(n.as_f64().unwrap_or(0.0))
            }
        }
        serde_json::Value::String(s) => MpVal::from(s.as_str()),
        serde_json::Value::Array(items) => MpVal::Array(items.iter().map(json_to_mpv).collect()),
        serde_json::Value::Object(map) => MpVal::Map(
            map.iter()
                .map(|(k, v)| (MpVal::from(k.as_str()), json_to_mpv(v)))
                .collect(),
        ),
    }
}
