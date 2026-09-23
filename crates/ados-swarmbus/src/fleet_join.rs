//! Joining a fleet slot to a device identity.
//!
//! The bus knows only slots — that is all a 20-byte beacon can afford to carry.
//! Identities live in the ground station's fleet registry at
//! [`FLEET_REGISTRY_PATH`], written at pair time. This reads that file so the
//! operator's fleet view can name a drone instead of showing a bare number.
//!
//! The document is what `FleetRegistry::persist` writes: the registry is
//! `#[serde(transparent)]` over `BTreeMap<u8, FleetSlot>`, so the file is a bare
//! top-level object keyed by the slot as a decimal string, values
//! `{slot, device_id, paired_at_ms}`. An empty fleet is `{}`, and the file does not
//! exist at all until the first pair completes.
//!
//! The join is decorative (`device_id` is contractually nullable), so a registry
//! that is absent or does not parse degrades to "no names" rather than taking the
//! neighbour table with it.
//!
//! There is deliberately no cargo dependency on the crate that writes this file:
//! that would pull a mesh-discovery stack and a supervisor into the swarm bus
//! binary for a two-field lookup.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

/// Where the ground station persists its slot registry.
pub const FLEET_REGISTRY_PATH: &str = "/var/lib/ados/fleet.json";

/// The one field of a registry row this join needs.
#[derive(Deserialize)]
struct RegistryRow {
    device_id: String,
}

/// Read the slot-to-device-id map, or an empty map when the registry is absent or
/// unreadable — the steady state on a drone, which has no registry at all.
pub fn load_device_ids(path: &Path) -> BTreeMap<u8, String> {
    std::fs::read_to_string(path)
        .map(|text| device_ids_from_str(&text))
        .unwrap_or_default()
}

/// Extract the map from a registry document. A document that is not the
/// slot-keyed registry yields no names. A blank identity is dropped, because it
/// would render as a nameless-but-known drone.
pub fn device_ids_from_str(text: &str) -> BTreeMap<u8, String> {
    serde_json::from_str::<BTreeMap<u8, RegistryRow>>(text)
        .map(|rows| {
            rows.into_iter()
                .filter_map(|(slot, row)| {
                    let id = row.device_id.trim();
                    (!id.is_empty()).then(|| (slot, id.to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The literal document `FleetRegistry::persist` writes. Written verbatim
    /// rather than constructed, so this test fails if the producer's
    /// serialization changes rather than only if the reader does.
    #[test]
    fn the_registrys_own_persisted_document_joins_every_slot() {
        let persisted = r#"{
  "1": {
    "slot": 1,
    "device_id": "ados-abc123",
    "paired_at_ms": 1700000000000
  },
  "24": {
    "slot": 24,
    "device_id": "ados-def456",
    "paired_at_ms": 1700000001000
  }
}"#;
        let got = device_ids_from_str(persisted);
        assert_eq!(got.len(), 2);
        assert_eq!(got.get(&1).unwrap(), "ados-abc123");
        assert_eq!(got.get(&24).unwrap(), "ados-def456");
        // Slot 0 is the ground station and never appears in the registry.
        assert!(!got.contains_key(&0));
        // An empty fleet is `{}`, which is not an error.
        assert!(device_ids_from_str("{}").is_empty());
    }

    /// Garbage must produce no names, never a panic and never a fabricated one.
    #[test]
    fn malformed_registries_degrade_to_no_names() {
        for doc in [
            "[]",
            "null",
            "\"not a registry\"",
            "42",
            // A slot key outside a u8.
            r#"{"999": {"slot": 999, "device_id": "ados-y"}}"#,
            // A non-numeric key.
            r#"{"six": {"slot": 6, "device_id": "ados-six"}}"#,
            // A row without an identity, and a non-string one.
            r#"{"1": {"slot": 1}}"#,
            r#"{"1": {"slot": 1, "device_id": 42}}"#,
            // Blank identities.
            r#"{"1": {"slot": 1, "device_id": ""}}"#,
            r#"{"1": {"slot": 1, "device_id": "   "}}"#,
        ] {
            assert!(
                device_ids_from_str(doc).is_empty(),
                "must yield no names: {doc}"
            );
        }
    }

    #[test]
    fn an_absent_or_truncated_registry_file_yields_no_names() {
        assert!(load_device_ids(Path::new("/nonexistent/ados/fleet.json")).is_empty());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fleet.json");
        // A truncated file mid-rename.
        std::fs::write(&path, "{\"1\": {\"dev").unwrap();
        assert!(load_device_ids(&path).is_empty());
        // And a well-formed one reads.
        std::fs::write(
            &path,
            r#"{"1":{"slot":1,"device_id":"ados-abc123","paired_at_ms":1700000000000}}"#,
        )
        .unwrap();
        assert_eq!(load_device_ids(&path).get(&1).unwrap(), "ados-abc123");
    }
}
