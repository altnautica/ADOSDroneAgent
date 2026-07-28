//! The flight-controller parameter cache, read off disk.
//!
//! The MAVLink router used to republish its whole parameter map inside the 10 Hz
//! vehicle-state snapshot, and every parameter surface here read it from there.
//! That map is ~24 KB of JSON against ArduPilot's ~700 parameters, with no delta
//! and no cap, so a relayed `/api/telemetry` needed ~21 aux-lane fragments and
//! delivered 85% of the time. The snapshot now carries only a `param_generation`
//! counter, and the map's one reachable source is the file the router already
//! persists atomically (temp file + rename) at [`DEFAULT_PARAMS_PATH`].
//!
//! A consumer holds its last-seen `param_generation` and refetches `GET
//! /api/params` once when it changes. That is the whole protocol: no delta
//! encoding, no subscription, and a router restart (which resets the counter to
//! 0) reads as a mismatch and costs exactly one refetch.
//!
//! On-disk shape, written by `ados-mavlink-router`'s `ParamCache`:
//!
//! ```json
//! { "WPNAV_SPEED": { "value": 500.0, "param_type": 9, "last_updated": 1.7e9 } }
//! ```
//!
//! Every reader here wants the flattened `{name: value}` form the snapshot blob
//! used to carry, so [`read_param_blob`] projects to that shape and every route
//! keeps the body it always returned.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

/// Where `ados-mavlink-router` persists its parameter cache. Mirrors that
/// crate's `param_cache::DEFAULT_PARAMS_PATH`; the two are separate binaries with
/// no shared crate between them, so the constant is duplicated rather than
/// pulling a router dependency into the HTTP front for one string.
pub const DEFAULT_PARAMS_PATH: &str = "/var/lib/ados/params.json";

/// Resolve the cache path, honouring `ADOS_PARAMS_JSON` the way the sibling
/// path overrides work, so a test (or a rig with a relocated state dir) can point
/// it elsewhere without a code change.
pub fn default_params_path() -> PathBuf {
    std::env::var("ADOS_PARAMS_JSON")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_PARAMS_PATH))
}

/// The cached parameters as the flattened `{name: value}` object every parameter
/// route projects.
///
/// Degrades to an empty map on every failure — an absent file (no router has run
/// yet, or the FC has never answered a `PARAM_REQUEST_LIST`), an unreadable file,
/// a truncated or malformed document, or a document whose root is not an object.
/// That is the same empty shape these routes returned for an absent state
/// snapshot, so a missing cache stays a 200 with `params: {}` and never a 500.
///
/// Entries whose `value` is absent or non-numeric are skipped individually,
/// matching the router's own loader: one corrupt entry must not discard the other
/// 699.
pub fn read_param_blob(path: &Path) -> Map<String, Value> {
    let Ok(body) = std::fs::read(path) else {
        return Map::new();
    };
    let Ok(Value::Object(parsed)) = serde_json::from_slice::<Value>(&body) else {
        return Map::new();
    };
    parsed
        .into_iter()
        .filter_map(|(name, entry)| {
            let value = entry.get("value")?;
            value.as_f64().map(|_| (name, value.clone()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Write a router-shaped cache document.
    fn write_cache(path: &Path, body: &Value) {
        std::fs::write(path, serde_json::to_vec(body).unwrap()).unwrap();
    }

    #[test]
    fn flattens_the_router_shape_to_name_value() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("params.json");
        write_cache(
            &path,
            &json!({
                "WPNAV_SPEED": { "value": 500.0, "param_type": 9, "last_updated": 1.0 },
                "ATC_RAT_RLL_P": { "value": 0.135, "param_type": 9, "last_updated": 2.0 },
            }),
        );
        let blob = read_param_blob(&path);
        assert_eq!(blob.len(), 2);
        assert_eq!(blob.get("WPNAV_SPEED"), Some(&json!(500.0)));
        assert_eq!(blob.get("ATC_RAT_RLL_P"), Some(&json!(0.135)));
    }

    #[test]
    fn an_integer_value_keeps_its_json_number_form() {
        // The single-param read returns the value verbatim, so an integer must not
        // be widened to a float on the way through.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("params.json");
        write_cache(&path, &json!({ "SYSID_THISMAV": { "value": 1 } }));
        let value = read_param_blob(&path)
            .get("SYSID_THISMAV")
            .cloned()
            .unwrap();
        assert_eq!(value, json!(1));
        assert!(value.is_i64() || value.is_u64(), "integer form preserved");
    }

    #[test]
    fn a_missing_file_is_an_empty_map_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_param_blob(&dir.path().join("nope.json")).is_empty());
    }

    #[test]
    fn an_unparseable_or_non_object_document_is_an_empty_map() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("params.json");
        std::fs::write(&path, b"{ this is not json").unwrap();
        assert!(read_param_blob(&path).is_empty());
        // A valid document whose root is the wrong type is equally unusable.
        for body in [json!([1, 2, 3]), json!("nope"), json!(null)] {
            write_cache(&path, &body);
            assert!(
                read_param_blob(&path).is_empty(),
                "root {body} is not a map"
            );
        }
    }

    #[test]
    fn a_corrupt_entry_is_skipped_without_discarding_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("params.json");
        write_cache(
            &path,
            &json!({
                "GOOD": { "value": 1.5, "param_type": 9 },
                "NO_VALUE": { "param_type": 9 },
                "NOT_NUMERIC": { "value": "nope" },
                "NOT_AN_ENTRY": 7,
            }),
        );
        let blob = read_param_blob(&path);
        assert_eq!(blob.len(), 1, "only the well-formed entry survives");
        assert_eq!(blob.get("GOOD"), Some(&json!(1.5)));
    }

    #[test]
    fn the_path_override_wins_over_the_default() {
        // Documents the resolution order; the env var is read live so the daemon
        // picks up a relocated state dir with no code change.
        assert_eq!(
            PathBuf::from(DEFAULT_PARAMS_PATH),
            PathBuf::from("/var/lib/ados/params.json")
        );
        assert!(default_params_path().is_absolute());
    }
}
