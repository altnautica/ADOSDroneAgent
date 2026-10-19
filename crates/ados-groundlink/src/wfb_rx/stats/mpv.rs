//! JSON to msgpack value mapping for the store event detail map.

/// Recursively map a `serde_json::Value` onto the msgpack value the detail map
/// carries. Integers stay integers (signed when negative), floats stay floats,
/// and null becomes nil.
pub(super) fn json_to_mpv(value: &serde_json::Value) -> ados_protocol::logd::Value {
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
