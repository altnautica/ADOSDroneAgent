//! Joining a fleet slot to a device identity.
//!
//! The bus knows only slots — that is all a 20-byte beacon can afford to carry.
//! Identities live in the ground station's fleet registry at
//! [`FLEET_REGISTRY_PATH`], written at pair time. This reads that file so the
//! operator's fleet view can name a drone instead of showing a bare number.
//!
//! The canonical document is what `FleetRegistry::persist` writes: the registry is
//! `#[serde(transparent)]` over `BTreeMap<u8, FleetSlot>`, so the file is a bare
//! top-level object keyed by the slot as a decimal string, values
//! `{slot, device_id, paired_at}`. An empty fleet is `{}`, and the file does not
//! exist at all until the first pair completes.
//!
//! The read is nonetheless **tolerant and best-effort**. It is a cross-service file,
//! the join is decorative (`device_id` is contractually nullable), and a registry
//! that is absent or newer than this build must degrade to "no names" rather than
//! take the neighbour table with it. Also accepted: an array of slot rows, and
//! either shape nested under a `"slots"` key, in case the registry grows a sibling
//! field. (Partial documents should not be observable — the writer renames a temp
//! file into place — but a truncated read is handled rather than trusted.)
//!
//! There is deliberately no cargo dependency on the crate that writes this file:
//! that would pull a mesh-discovery stack and a supervisor into the swarm bus
//! binary for a two-field lookup.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::Value;

/// Where the ground station persists its slot registry.
pub const FLEET_REGISTRY_PATH: &str = "/var/lib/ados/fleet.json";

/// Read the slot-to-device-id map, or an empty map when the registry is absent or
/// unreadable — the steady state on a drone, which has no registry at all.
pub fn load_device_ids(path: &Path) -> BTreeMap<u8, String> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .map(|v| device_ids_from_value(&v))
        .unwrap_or_default()
}

/// Extract the map from a parsed registry document.
pub fn device_ids_from_value(root: &Value) -> BTreeMap<u8, String> {
    // Unwrap one level of `{"slots": ...}` if present, then accept an array of slot
    // objects or a map keyed by slot.
    let body = root.get("slots").unwrap_or(root);
    let mut out = BTreeMap::new();
    match body {
        Value::Array(rows) => {
            for row in rows {
                if let (Some(slot), Some(id)) = (slot_of(row), device_id_of(row)) {
                    out.insert(slot, id);
                }
            }
        }
        Value::Object(map) => {
            for (key, row) in map {
                // The slot is the key; a `slot` field inside the row wins when
                // present, so a map keyed by device id also reads correctly.
                let slot = slot_of(row).or_else(|| key.parse::<u8>().ok());
                if let (Some(slot), Some(id)) = (slot, device_id_of(row)) {
                    out.insert(slot, id);
                }
            }
        }
        _ => {}
    }
    out
}

/// A row's `slot`, accepting the number or a numeric string.
fn slot_of(row: &Value) -> Option<u8> {
    let v = row.get("slot")?;
    match v {
        Value::Number(n) => n.as_u64().and_then(|n| u8::try_from(n).ok()),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

/// A row's non-empty `device_id`. An empty string is not an identity, and emitting
/// one would render as a nameless-but-known drone.
fn device_id_of(row: &Value) -> Option<String> {
    let id = row.get("device_id")?.as_str()?.trim();
    (!id.is_empty()).then(|| id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The **primary case**: the literal document `FleetRegistry::persist` writes.
    /// The registry is `#[serde(transparent)]` over `BTreeMap<u8, FleetSlot>`, so
    /// it is a bare top-level object keyed by the slot as a decimal string, with no
    /// wrapper. Written verbatim rather than constructed with `json!`, so this test
    /// fails if the producer's serialization changes rather than only if my reader
    /// does.
    #[test]
    fn the_registrys_own_persisted_document_joins_every_slot() {
        let persisted = r#"{
  "1": {
    "slot": 1,
    "device_id": "ados-abc123",
    "paired_at": 1700000000.0
  },
  "24": {
    "slot": 24,
    "device_id": "ados-def456",
    "paired_at": 1700000001.0
  }
}"#;
        let doc: Value = serde_json::from_str(persisted).expect("the registry parses");
        let got = device_ids_from_value(&doc);
        assert_eq!(got.len(), 2);
        assert_eq!(got.get(&1).unwrap(), "ados-abc123");
        assert_eq!(got.get(&24).unwrap(), "ados-def456");
        // Slot 0 is the ground station and never appears in the registry.
        assert!(!got.contains_key(&0));
        // An empty fleet is `{}`, which is not an error.
        assert!(device_ids_from_value(&json!({})).is_empty());
    }

    /// A `"slots"`-wrapped map is tolerated too, in case the registry ever grows a
    /// sibling field beside the slot table.
    #[test]
    fn a_slots_wrapped_registry_map_is_also_accepted() {
        let doc = json!({
            "slots": {
                "1": {"slot": 1, "device_id": "ados-abc123", "paired_at": 1.7e9},
                "3": {"slot": 3, "device_id": "ados-def456", "paired_at": 1.7e9},
            }
        });
        let got = device_ids_from_value(&doc);
        assert_eq!(got.len(), 2);
        assert_eq!(got.get(&1).unwrap(), "ados-abc123");
        assert_eq!(got.get(&3).unwrap(), "ados-def456");
    }

    /// An array of slot objects, with or without the `slots` wrapper.
    #[test]
    fn an_array_of_slot_rows_joins_at_either_nesting() {
        let rows = json!([
            {"slot": 2, "device_id": "ados-two", "paired_at": 0.0},
            {"slot": 7, "device_id": "ados-seven", "paired_at": 0.0},
        ]);
        for doc in [rows.clone(), json!({"slots": rows})] {
            let got = device_ids_from_value(&doc);
            assert_eq!(got.len(), 2, "{doc}");
            assert_eq!(got.get(&2).unwrap(), "ados-two");
            assert_eq!(got.get(&7).unwrap(), "ados-seven");
        }
    }

    /// A top-level slot-keyed map with no `slots` wrapper, and rows that omit the
    /// redundant inner `slot` field.
    #[test]
    fn a_bare_map_keyed_by_slot_reads_the_key_as_the_slot() {
        let doc = json!({
            "4": {"device_id": "ados-four"},
            "24": {"device_id": "ados-twentyfour"},
        });
        let got = device_ids_from_value(&doc);
        assert_eq!(got.get(&4).unwrap(), "ados-four");
        assert_eq!(got.get(&24).unwrap(), "ados-twentyfour");
    }

    /// A registry keyed by device id instead of slot still joins, because the row's
    /// own `slot` field takes precedence over the key.
    #[test]
    fn an_inner_slot_field_wins_over_the_map_key() {
        let doc = json!({
            "ados-abc123": {"slot": 5, "device_id": "ados-abc123"},
            // A key that parses AND an inner slot that disagrees: the row wins.
            "9": {"slot": 11, "device_id": "ados-eleven"},
        });
        let got = device_ids_from_value(&doc);
        assert_eq!(got.get(&5).unwrap(), "ados-abc123");
        assert_eq!(got.get(&11).unwrap(), "ados-eleven");
        assert!(!got.contains_key(&9));
    }

    /// Garbage must produce no names, never a panic and never a fabricated one. A
    /// half-written registry is a realistic case: the writer renames a temp file
    /// into place, and a reader can catch the moment before.
    #[test]
    fn malformed_and_partial_registries_degrade_to_no_names() {
        for doc in [
            json!({}),
            json!([]),
            json!(null),
            json!("not a registry"),
            json!(42),
            json!({"slots": null}),
            json!({"slots": "truncated"}),
            // Rows missing the fields that matter.
            json!([{"slot": 1}, {"device_id": "ados-x"}, {}]),
            // A slot outside a u8, and an empty identity.
            json!([{"slot": 999, "device_id": "ados-y"}]),
            json!([{"slot": 1, "device_id": ""}]),
            json!([{"slot": 1, "device_id": "   "}]),
            json!([{"slot": 1, "device_id": 42}]),
        ] {
            assert!(
                device_ids_from_value(&doc).is_empty(),
                "must yield no names: {doc}"
            );
        }
    }

    /// Slot numbers written as strings are accepted; the registry's own JSON may
    /// quote them depending on how it serializes a `u8` key inside a row.
    #[test]
    fn a_numeric_string_slot_is_accepted() {
        let doc = json!([{"slot": "6", "device_id": "ados-six"}]);
        assert_eq!(device_ids_from_value(&doc).get(&6).unwrap(), "ados-six");
        // But a non-numeric one is not silently coerced.
        let bad = json!([{"slot": "six", "device_id": "ados-six"}]);
        assert!(device_ids_from_value(&bad).is_empty());
    }

    #[test]
    fn an_absent_registry_file_yields_no_names() {
        assert!(load_device_ids(Path::new("/nonexistent/ados/fleet.json")).is_empty());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fleet.json");
        // A truncated file mid-rename.
        std::fs::write(&path, "{\"slots\": {\"1\": {\"dev").unwrap();
        assert!(load_device_ids(&path).is_empty());
        // And a well-formed one reads.
        std::fs::write(
            &path,
            r#"{"slots":{"1":{"slot":1,"device_id":"ados-abc123","paired_at":1700000000.0}}}"#,
        )
        .unwrap();
        assert_eq!(load_device_ids(&path).get(&1).unwrap(), "ados-abc123");
    }
}
