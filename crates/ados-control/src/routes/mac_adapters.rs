//! Stable-MAC adapter read: the per-adapter pin verdicts (GET).
//!
//! An onboard adapter with no efuse MAC randomizes its address each driver load.
//! The agent tracks each adapter's stable-MAC verdict (stable / pinned / a learner
//! candidate / deferred) in an on-disk state file the installer step + the
//! supervisor reconciler write. This route exposes that state to the GCS Network
//! panel, the same shape the cloud heartbeat's `macStability` block carries.
//!
//! - **`GET /api/v1/network/mac/adapters`** — read the per-adapter verdicts. Returns
//!   `{"version": N, "adapters": [...]}`; an empty list on a board with no tracked
//!   adapters. Each adapter is the camelCase projection of one state-file entry.
//!
//! ## Why this ports cleanly to the native front
//!
//! It is a pure read with no side effect: the FastAPI handler reads the on-disk
//! state file (the same `mac-pins.state` the sibling MAC-pin write route reads for
//! its learner-candidate fallback) and maps each entry to camelCase — it never
//! enumerates live adapters, runs a command, or mutates anything. The native front
//! reuses the identical state-file seam (`ados-macpin`'s `STATE_PATH`, overridable
//! via `ADOS_MAC_PINS_STATE` for tests, exactly as `mac_pin.rs` resolves it) and
//! reproduces the camelCase projection byte-for-byte, with no daemon round-trip.
//!
//! ## The camelCase projection (matched to the FastAPI mapper)
//!
//! Each state-file adapter dict is reduced to:
//! - Always present: `name`, `vidpid`, `usbPath`, `state` (each carried through
//!   verbatim, so a missing source key renders JSON `null`), and `appliedLive` (a
//!   bool, the source `applied_live` coerced to a boolean with a `false` default).
//! - Present only when the source value is non-null: `source`, `pinnedMac`
//!   (`pinned_mac`), `lastSeenMac` (`last_seen_mac`), `linkFile` (`link_file`),
//!   `deferredReason` (`deferred_reason`). A `null` source value omits the key,
//!   matching the Python `a.get(src) is not None` guard (a present-but-falsy value
//!   like an empty string is still emitted).
//!
//! A non-dict entry in the source list is skipped (the Python `if isinstance(a,
//! dict)` filter). An absent / malformed state file reads as no document, so the
//! body is `{"version": 1, "adapters": []}` — the `version` default the Python
//! `raw.get("version", 1)` applies.

use std::path::{Path, PathBuf};

use axum::Json;
use serde_json::{json, Map, Value};

use ados_macpin::engine::STATE_PATH;

/// The on-disk state file with the per-adapter verdicts (`/etc/ados/mac-pins.state`),
/// the same file the installer step + supervisor reconciler write and the heartbeat
/// reads. Overridable via `ADOS_MAC_PINS_STATE` for tests, resolved identically to
/// the sibling MAC-pin write route so both read one source of truth.
fn state_file_path() -> PathBuf {
    PathBuf::from(std::env::var("ADOS_MAC_PINS_STATE").unwrap_or_else(|_| STATE_PATH.to_string()))
}

/// Read + parse the state file into a JSON document, or `None` when the file is
/// absent or malformed. Mirrors the Python `read_mac_pins_state`, which returns
/// `None` on `OSError` / `ValueError`.
fn read_state(state_path: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(state_path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Project one state-file adapter object to the camelCase shape the GCS expects,
/// byte-identically to the Python `_mac_adapter_to_camel`.
fn adapter_to_camel(a: &Map<String, Value>) -> Value {
    let mut out = Map::new();
    // Always present (carried through verbatim — an absent source key renders
    // JSON null, matching the Python `a.get(...)`).
    out.insert(
        "name".to_string(),
        a.get("name").cloned().unwrap_or(Value::Null),
    );
    out.insert(
        "vidpid".to_string(),
        a.get("vidpid").cloned().unwrap_or(Value::Null),
    );
    out.insert(
        "usbPath".to_string(),
        a.get("usb_path").cloned().unwrap_or(Value::Null),
    );
    out.insert(
        "state".to_string(),
        a.get("state").cloned().unwrap_or(Value::Null),
    );
    // `applied_live` coerced to a bool with a false default (the Python
    // `bool(a.get("applied_live", False))`).
    let applied_live = a.get("applied_live").map(value_is_truthy).unwrap_or(false);
    out.insert("appliedLive".to_string(), Value::Bool(applied_live));

    // Present only when the source value is non-null (the Python
    // `if a.get(src) is not None`).
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

/// Python truthiness for the `bool(a.get("applied_live", False))` coercion: a JSON
/// bool is itself; `null` is false; a number is false iff zero; a string/array/
/// object is false iff empty. Matches what `bool(...)` would yield for the value
/// FastAPI deserialized from the JSON state file.
fn value_is_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// `GET /api/v1/network/mac/adapters` → the per-adapter stable-MAC verdicts.
///
/// Reads the on-disk state file and maps each adapter entry to the camelCase
/// shape the GCS reads, returning `{"version": N, "adapters": [...]}`. An absent /
/// malformed state file yields `{"version": 1, "adapters": []}`. A pure read — no
/// live enumeration, no command, no mutation.
pub async fn get_mac_adapters() -> Json<Value> {
    get_mac_adapters_at(&state_file_path())
}

/// The adapter-read logic against an explicit state-file path. The public handler
/// resolves the path from the env / default; this takes it directly so a test can
/// point it at a temp file without mutating process-global env.
fn get_mac_adapters_at(state_path: &Path) -> Json<Value> {
    let raw = read_state(state_path);
    // `version` defaults to 1 (the Python `raw.get("version", 1)`); an absent /
    // malformed document is treated as an empty `{}` for both the version default
    // and the empty adapter list.
    let version = raw
        .as_ref()
        .and_then(|d| d.get("version"))
        .cloned()
        .unwrap_or(Value::from(1));
    let adapters: Vec<Value> = raw
        .as_ref()
        .and_then(|d| d.get("adapters"))
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_object)
                .map(adapter_to_camel)
                .collect()
        })
        .unwrap_or_default();
    Json(json!({ "version": version, "adapters": adapters }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Drive a `Json<Value>` body into a plain `Value` for assertions.
    fn body(j: Json<Value>) -> Value {
        j.0
    }

    #[test]
    fn an_absent_state_file_is_version_one_and_an_empty_list() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("absent.state");
        let out = body(get_mac_adapters_at(&state));
        assert_eq!(out, json!({ "version": 1, "adapters": [] }));
    }

    #[test]
    fn a_malformed_state_file_is_version_one_and_an_empty_list() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("bad.state");
        std::fs::write(&state, "{not json").unwrap();
        let out = body(get_mac_adapters_at(&state));
        assert_eq!(out, json!({ "version": 1, "adapters": [] }));
    }

    #[test]
    fn an_empty_adapters_list_passes_through_the_version() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("empty.state");
        std::fs::write(
            &state,
            serde_json::to_string(&json!({"version": 3, "adapters": []})).unwrap(),
        )
        .unwrap();
        let out = body(get_mac_adapters_at(&state));
        assert_eq!(out, json!({ "version": 3, "adapters": [] }));
    }

    #[test]
    fn a_full_adapter_carries_every_optional_key_in_camel_case() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("full.state");
        std::fs::write(
            &state,
            serde_json::to_string(&json!({
                "version": 2,
                "adapters": [{
                    "name": "wlan0",
                    "vidpid": "0bda:a81a",
                    "usb_path": "1-1.2",
                    "state": "pinned",
                    "applied_live": true,
                    "source": "learner",
                    "pinned_mac": "02:c6:75:83:1a:3e",
                    "last_seen_mac": "aa:bb:cc:dd:ee:ff",
                    "link_file": "/etc/systemd/network/10-ados-wlan0.link",
                    "deferred_reason": "management interface",
                }],
            }))
            .unwrap(),
        )
        .unwrap();
        let out = body(get_mac_adapters_at(&state));
        assert_eq!(
            out,
            json!({
                "version": 2,
                "adapters": [{
                    "name": "wlan0",
                    "vidpid": "0bda:a81a",
                    "usbPath": "1-1.2",
                    "state": "pinned",
                    "appliedLive": true,
                    "source": "learner",
                    "pinnedMac": "02:c6:75:83:1a:3e",
                    "lastSeenMac": "aa:bb:cc:dd:ee:ff",
                    "linkFile": "/etc/systemd/network/10-ados-wlan0.link",
                    "deferredReason": "management interface",
                }],
            })
        );
    }

    #[test]
    fn a_minimal_adapter_omits_absent_optionals_and_keeps_the_required_keys() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("min.state");
        // Only the required-source keys; the five optionals are absent, and
        // applied_live is absent so it defaults to false.
        std::fs::write(
            &state,
            serde_json::to_string(&json!({
                "version": 1,
                "adapters": [{
                    "name": "eth0",
                    "vidpid": null,
                    "usb_path": null,
                    "state": "stable",
                }],
            }))
            .unwrap(),
        )
        .unwrap();
        let out = body(get_mac_adapters_at(&state));
        let adapters = out["adapters"].as_array().unwrap();
        assert_eq!(adapters.len(), 1);
        let a = adapters[0].as_object().unwrap();
        // The required keys are present (vidpid/usbPath render JSON null when the
        // source key is null).
        assert_eq!(a.get("name"), Some(&json!("eth0")));
        assert_eq!(a.get("vidpid"), Some(&Value::Null));
        assert_eq!(a.get("usbPath"), Some(&Value::Null));
        assert_eq!(a.get("state"), Some(&json!("stable")));
        // applied_live absent → false.
        assert_eq!(a.get("appliedLive"), Some(&Value::Bool(false)));
        // The five optionals are omitted entirely (not null).
        for k in [
            "source",
            "pinnedMac",
            "lastSeenMac",
            "linkFile",
            "deferredReason",
        ] {
            assert!(!a.contains_key(k), "{k} should be omitted when absent");
        }
        // Exactly the five required keys remain.
        assert_eq!(a.len(), 5);
    }

    #[test]
    fn a_null_optional_value_is_omitted_not_emitted_as_null() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("nullopt.state");
        std::fs::write(
            &state,
            serde_json::to_string(&json!({
                "version": 1,
                "adapters": [{
                    "name": "wlan1",
                    "vidpid": "0bda:8812",
                    "usb_path": "1-1.3",
                    "state": "learner",
                    "applied_live": false,
                    // present-but-null → omitted (the `is not None` guard).
                    "pinned_mac": null,
                    // present-but-non-null falsy → still emitted.
                    "source": "",
                }],
            }))
            .unwrap(),
        )
        .unwrap();
        let out = body(get_mac_adapters_at(&state));
        let a = out["adapters"][0].as_object().unwrap();
        assert!(!a.contains_key("pinnedMac"), "a null optional is omitted");
        // An empty-string source is non-null, so it is emitted verbatim.
        assert_eq!(a.get("source"), Some(&json!("")));
    }

    #[test]
    fn a_non_dict_entry_in_the_adapters_list_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("mixed.state");
        std::fs::write(
            &state,
            serde_json::to_string(&json!({
                "version": 1,
                "adapters": [
                    "not-a-dict",
                    42,
                    {"name": "wlan0", "vidpid": "x", "usb_path": "y", "state": "stable"},
                ],
            }))
            .unwrap(),
        )
        .unwrap();
        let out = body(get_mac_adapters_at(&state));
        let adapters = out["adapters"].as_array().unwrap();
        // Only the one dict survives the isinstance(a, dict) filter.
        assert_eq!(adapters.len(), 1);
        assert_eq!(adapters[0]["name"], json!("wlan0"));
    }
}
