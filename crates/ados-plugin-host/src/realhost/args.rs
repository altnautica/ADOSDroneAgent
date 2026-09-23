//! msgpack argument readers, reply converters and the Python-compat
//! formatting helpers the wire error strings use.

use super::*;

// ---------------------------------------------------------------------
// rmpv arg helpers
// ---------------------------------------------------------------------

pub(super) fn map_get<'a>(args: &'a Value, key: &str) -> Option<&'a Value> {
    match args {
        Value::Map(entries) => entries
            .iter()
            .find(|(k, _)| k.as_str() == Some(key))
            .map(|(_, v)| v),
        _ => None,
    }
}

pub(super) fn map_has(args: &Value, key: &str) -> bool {
    matches!(args, Value::Map(entries) if entries.iter().any(|(k, _)| k.as_str() == Some(key)))
}

pub(super) fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    map_get(args, key).and_then(Value::as_str)
}

/// `env.args.get(key)` coerced to a clone, or `Value::Nil` when absent.
pub(super) fn arg_owned(args: &Value, key: &str) -> Value {
    map_get(args, key).cloned().unwrap_or(Value::Nil)
}

/// Read an integer field from a msgpack-map `args`, accepting a signed or
/// unsigned msgpack integer. Returns `None` for an absent or non-integer value.
pub(super) fn arg_i64(args: &Value, key: &str) -> Option<i64> {
    let v = map_get(args, key)?;
    v.as_i64()
        .or_else(|| v.as_u64().and_then(|n| i64::try_from(n).ok()))
}

/// Read a numeric field from a msgpack-map `args` as an f64, accepting any
/// msgpack number (a float OR an integer, so `0` and `0.0` both read). A present
/// non-numeric value is an error (the caller distinguishes it from absent);
/// `Ok(None)` is an absent key, which the caller defaults to 0.0. The returned
/// f64 may be non-finite (a NaN/inf encoded by the client) — the setpoint
/// validator rejects a non-finite value on an active axis downstream, so the
/// finiteness check is one place, not scattered through the reads.
pub(super) fn arg_f64_opt(args: &Value, key: &str) -> Result<Option<f64>, HostError> {
    match map_get(args, key) {
        None | Some(Value::Nil) => Ok(None),
        Some(v) => match v.as_f64() {
            Some(n) => Ok(Some(n)),
            None => Err(HostError::Rpc(format!("{key} must be a number"))),
        },
    }
}

/// Read a numeric field as an f64, defaulting an absent key to 0.0 (an axis the
/// type mask ignores is conventionally left at 0). A present non-number errors.
pub(super) fn arg_f64(args: &Value, key: &str) -> Result<f64, HostError> {
    Ok(arg_f64_opt(args, key)?.unwrap_or(0.0))
}

/// Read a numeric field as an f32, defaulting an absent key to 0.0. A present
/// non-number errors. The f64→f32 narrowing matches the wire field width of the
/// velocity / accel / yaw setpoint fields.
pub(super) fn arg_f32(args: &Value, key: &str) -> Result<f32, HostError> {
    Ok(arg_f64(args, key)? as f32)
}

/// Convert a `serde_json::Value` (a command-socket reply) to the msgpack
/// `rmpv::Value` the plugin sees as the response `args`. Integers stay integers,
/// floats stay floats, null becomes nil, so the reply shape round-trips into the
/// plugin's response envelope unchanged.
pub(super) fn json_to_mpv(value: &serde_json::Value) -> Value {
    match value {
        serde_json::Value::Null => Value::Nil,
        serde_json::Value::Bool(b) => Value::Boolean(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Integer(i.into())
            } else if let Some(u) = n.as_u64() {
                Value::Integer(u.into())
            } else {
                Value::F64(n.as_f64().unwrap_or(0.0))
            }
        }
        serde_json::Value::String(s) => Value::from(s.as_str()),
        serde_json::Value::Array(items) => Value::Array(items.iter().map(json_to_mpv).collect()),
        serde_json::Value::Object(map) => Value::Map(
            map.iter()
                .map(|(k, v)| (Value::from(k.as_str()), json_to_mpv(v)))
                .collect(),
        ),
    }
}

/// Render a compute-node reply struct (any `Serialize`) as the msgpack response
/// map the `ctx.compute` facade parses, mapping the typed reply 1:1 through
/// JSON (the struct's serde field names are the keys the facade reads).
pub(super) fn compute_reply<T: serde::Serialize>(value: &T) -> Result<HostResult, HostError> {
    let json = serde_json::to_value(value).map_err(|e| HostError::Rpc(e.to_string()))?;
    Ok(json_to_mpv(&json))
}

/// A sub-value of the args map as `serde_json`, for forwarding `meta` / `params`
/// to the compute node verbatim. Absent / null / non-convertible becomes an
/// empty object (the facade always sends a dict; the node expects an object).
pub(super) fn compute_json_arg(args: &Value, key: &str) -> serde_json::Value {
    map_get(args, key)
        .and_then(|v| serde_json::to_value(v).ok())
        .filter(|v| !v.is_null())
        .unwrap_or_else(|| serde_json::Value::Object(Default::default()))
}

/// Coerce a msgpack value to raw bytes, mirroring the Python `msg_bytes`
/// handling: a binary value is taken verbatim; a list of ints is coerced to
/// bytes (msgpack may decode bytes-of-ints as a list on some configs). Any other
/// type (including a string) is rejected, matching the Python
/// `isinstance(msg_bytes, (bytes, bytearray))` check. Returns the coerced bytes,
/// or an `Err` string when the type is wrong / the list coercion fails. The
/// error text is the fixed wire string the Python handler emits.
pub(super) fn coerce_msg_bytes(value: &Value) -> Result<Vec<u8>, String> {
    match value {
        Value::Binary(b) => Ok(b.clone()),
        Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                match item.as_u64() {
                    Some(n) if n <= 255 => out.push(n as u8),
                    _ => return Err("msg_bytes coercion failed".to_string()),
                }
            }
            Ok(out)
        }
        _ => Err("msg_bytes must be bytes".to_string()),
    }
}

/// Coerce a msgpack value to an i64 component id, mirroring the Python
/// `int(component_id)`: an integer is taken directly, and a numeric string is
/// trimmed and parsed (`int("197")`). Any other value yields the fixed
/// `"component_id not integer"` wire string the Python handler emits.
pub(super) fn coerce_component_id(value: &Value) -> Result<i64, String> {
    if let Some(n) = value.as_i64() {
        return Ok(n);
    }
    if let Some(n) = value.as_u64() {
        if let Ok(n) = i64::try_from(n) {
            return Ok(n);
        }
    }
    // Python `int("197")` parses a numeric string (after stripping surrounding
    // whitespace); a non-numeric string raises ValueError -> the fixed message.
    if let Some(s) = value.as_str() {
        if let Ok(n) = s.trim().parse::<i64>() {
            return Ok(n);
        }
    }
    Err("component_id not integer".to_string())
}

// ---------------------------------------------------------------------
// Python-compat formatting helpers
// ---------------------------------------------------------------------

/// `bool(value)` truthiness, matching Python's coercion of the config `scope`
/// arg. Empty containers / zero / nil / false are falsy.
pub(super) fn python_bool(value: &Value) -> bool {
    match value {
        Value::Nil => false,
        Value::Boolean(b) => *b,
        Value::Integer(i) => i.as_i64().map(|n| n != 0).unwrap_or(true),
        Value::F32(f) => *f != 0.0,
        Value::F64(f) => *f != 0.0,
        Value::String(s) => !s.as_str().map(str::is_empty).unwrap_or(true),
        Value::Binary(b) => !b.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Map(m) => !m.is_empty(),
        Value::Ext(_, b) => !b.is_empty(),
    }
}

/// Python `repr()` of a string: single-quoted. Used in the few error strings
/// that interpolate a value with `{x!r}` so the wire body matches byte-for-byte.
pub(super) fn py_repr(s: &str) -> String {
    format!("'{s}'")
}

/// Python `repr()` of an rmpv value where the handler used `{scope!r}` on a
/// non-string scope arg. Strings are single-quoted; containers recurse so inner
/// strings are single-quoted too (`repr(['x'])` == `['x']`, `repr({'a': 1})` ==
/// `{'a': 1}`). Only reached for a truthy non-string scope, which is an exotic
/// error path; the common case is a plain string.
pub(super) fn py_repr_value(value: &Value) -> String {
    match value {
        Value::String(s) => py_repr(s.as_str().unwrap_or("")),
        Value::Nil => "None".to_string(),
        Value::Boolean(b) => if *b { "True" } else { "False" }.to_string(),
        Value::Integer(i) => i.to_string(),
        Value::F32(f) => f.to_string(),
        Value::F64(f) => f.to_string(),
        Value::Array(items) => {
            let inner: Vec<String> = items.iter().map(py_repr_value).collect();
            format!("[{}]", inner.join(", "))
        }
        Value::Map(entries) => {
            let inner: Vec<String> = entries
                .iter()
                .map(|(k, v)| format!("{}: {}", py_repr_value(k), py_repr_value(v)))
                .collect();
            format!("{{{}}}", inner.join(", "))
        }
        other => format!("{other}"),
    }
}
