//! Typed readers over a msgpack-map `args` value, shared by the control socket,
//! the in-process handlers and the real host so every surface coerces a field
//! the same way.

use rmpv::Value;

use crate::host::HostError;

/// The value under `key` in a msgpack map, or `None` when `args` is not a map or
/// has no such key.
pub(crate) fn map_get<'a>(args: &'a Value, key: &str) -> Option<&'a Value> {
    match args {
        Value::Map(entries) => entries
            .iter()
            .find(|(k, _)| k.as_str() == Some(key))
            .map(|(_, v)| v),
        _ => None,
    }
}

/// Whether the map carries `key` at all (a present nil counts).
pub(crate) fn map_has(args: &Value, key: &str) -> bool {
    map_get(args, key).is_some()
}

/// A string field, or `None` when absent or not a string.
pub(crate) fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    map_get(args, key).and_then(Value::as_str)
}

/// `env.args.get(key)` coerced to a clone, or `Value::Nil` when absent.
pub(crate) fn arg_owned(args: &Value, key: &str) -> Value {
    map_get(args, key).cloned().unwrap_or(Value::Nil)
}

/// A map field, with a missing or non-map value read as an empty map
/// (`env.args.get("payload") or {}`).
pub(crate) fn arg_map(args: &Value, key: &str) -> Value {
    map_get(args, key)
        .filter(|v| matches!(v, Value::Map(_)))
        .cloned()
        .unwrap_or_else(|| Value::Map(vec![]))
}

/// An integer field, accepting a signed or unsigned msgpack integer. `None` for
/// an absent or non-integer value.
pub(crate) fn arg_i64(args: &Value, key: &str) -> Option<i64> {
    let v = map_get(args, key)?;
    v.as_i64()
        .or_else(|| v.as_u64().and_then(|n| i64::try_from(n).ok()))
}

/// An integer field narrowed to `T`. An absent key (or nil) is `Ok(None)`; a
/// present non-integer, or a value outside `T`'s range, is a `"{key} out of
/// range"` error rather than a silent wrap.
pub(crate) fn arg_int<T: TryFrom<i64>>(args: &Value, key: &str) -> Result<Option<T>, HostError> {
    match map_get(args, key) {
        None | Some(Value::Nil) => Ok(None),
        Some(_) => arg_i64(args, key)
            .and_then(|n| T::try_from(n).ok())
            .map(Some)
            .ok_or_else(|| HostError::Rpc(format!("{key} out of range"))),
    }
}

/// A required integer field narrowed to `T`: absent is `"{key} is required"`,
/// out of range as in [`arg_int`].
pub(crate) fn arg_int_required<T: TryFrom<i64>>(args: &Value, key: &str) -> Result<T, HostError> {
    arg_int(args, key)?.ok_or_else(|| HostError::Rpc(format!("{key} is required")))
}

/// A numeric field as an f64, accepting any msgpack number (a float OR an
/// integer, so `0` and `0.0` both read). A present non-numeric value is an error;
/// `Ok(None)` is an absent key. The value may be non-finite — the setpoint
/// validator rejects that on an active axis downstream, so the finiteness check
/// lives in one place.
pub(crate) fn arg_f64_opt(args: &Value, key: &str) -> Result<Option<f64>, HostError> {
    match map_get(args, key) {
        None | Some(Value::Nil) => Ok(None),
        Some(v) => match v.as_f64() {
            Some(n) => Ok(Some(n)),
            None => Err(HostError::Rpc(format!("{key} must be a number"))),
        },
    }
}

/// A numeric field as an f64, defaulting an absent key to 0.0 (an axis the type
/// mask ignores is conventionally left at 0). A present non-number errors.
pub(crate) fn arg_f64(args: &Value, key: &str) -> Result<f64, HostError> {
    Ok(arg_f64_opt(args, key)?.unwrap_or(0.0))
}

/// A numeric field as an f32, defaulting an absent key to 0.0. The narrowing
/// matches the wire width of the velocity / accel / yaw setpoint fields.
pub(crate) fn arg_f32(args: &Value, key: &str) -> Result<f32, HostError> {
    Ok(arg_f64(args, key)? as f32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(entries: &[(&str, Value)]) -> Value {
        Value::Map(
            entries
                .iter()
                .map(|(k, v)| (Value::from(*k), v.clone()))
                .collect(),
        )
    }

    #[test]
    fn narrowed_integers_refuse_to_wrap() {
        let args = map(&[
            ("ok", Value::from(255)),
            ("big", Value::from(256)),
            ("neg", Value::from(-1)),
            ("text", Value::from("7")),
            ("nil", Value::Nil),
        ]);
        assert_eq!(arg_int::<u8>(&args, "ok"), Ok(Some(255)));
        for key in ["big", "neg", "text"] {
            assert_eq!(
                arg_int::<u8>(&args, key),
                Err(HostError::Rpc(format!("{key} out of range")))
            );
        }
        assert_eq!(arg_int::<u8>(&args, "nil"), Ok(None));
        assert_eq!(arg_int::<u8>(&args, "absent"), Ok(None));
        assert_eq!(arg_int_required::<u16>(&args, "big"), Ok(256));
        assert_eq!(
            arg_int_required::<u16>(&args, "absent"),
            Err(HostError::Rpc("absent is required".to_string()))
        );
    }

    #[test]
    fn a_non_map_payload_reads_as_an_empty_map() {
        let args = map(&[
            ("payload", Value::from("x")),
            ("inner", map(&[("a", Value::from(1))])),
        ]);
        assert_eq!(arg_map(&args, "payload"), Value::Map(vec![]));
        assert_eq!(arg_map(&args, "absent"), Value::Map(vec![]));
        assert_eq!(arg_map(&args, "inner"), map(&[("a", Value::from(1))]));
    }
}
