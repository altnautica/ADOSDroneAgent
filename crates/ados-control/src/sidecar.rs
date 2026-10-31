//! Readers for the JSON sidecar files other daemons write under the runtime
//! and config directories.
//!
//! Every reader degrades to `None` on any gap (absent file, read error, parse
//! error, wrong root type) so a route renders "not known" rather than failing.

use std::path::Path;

use serde_json::{Map, Value};

/// Read + parse a sidecar file into an object, or `None` on any gap (absent /
/// read error / non-object body).
pub(crate) fn read_object(path: &Path) -> Option<Map<String, Value>> {
    let text = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(map)) => Some(map),
        _ => None,
    }
}

/// [`read_object`] plus the best-effort schema-drift warning against the
/// shared contract registry. Never rejects on a version mismatch: a stale
/// sidecar from an older agent still reads.
pub(crate) fn read_versioned(path: &Path, contract_id: &str) -> Option<Map<String, Value>> {
    let obj = read_object(path)?;
    warn_on_version_drift(&obj, contract_id);
    Some(obj)
}

/// The best-effort schema-drift warning for a parsed sidecar object.
pub(crate) fn warn_on_version_drift(obj: &Map<String, Value>, contract_id: &str) {
    let got = obj.get("version").and_then(Value::as_u64).unwrap_or(0) as u16;
    if let Some(ours) = ados_protocol::contracts::sidecar_version(contract_id) {
        ados_protocol::sidecar::check_sidecar_version(contract_id, got, ours);
    }
}

/// Read + deserialize a sidecar file into `T`, or `None` on any gap.
pub(crate) fn read_typed<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_object_handles_absent_list_and_object_bodies() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_object(&dir.path().join("absent.json")), None);
        let list = dir.path().join("list.json");
        std::fs::write(&list, "[1,2,3]").unwrap();
        assert_eq!(read_object(&list), None);
        let garbage = dir.path().join("garbage.json");
        std::fs::write(&garbage, "{not json").unwrap();
        assert_eq!(read_object(&garbage), None);
        let obj = dir.path().join("obj.json");
        std::fs::write(&obj, r#"{"neighbors":[],"version":1}"#).unwrap();
        let got = read_versioned(&obj, "mesh-state").expect("an object body reads");
        assert!(got.contains_key("neighbors"));
    }

    #[test]
    fn read_typed_rejects_a_wrong_shape() {
        #[derive(serde::Deserialize)]
        struct Doc {
            n: u32,
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("doc.json");
        std::fs::write(&path, r#"{"n":7}"#).unwrap();
        assert_eq!(read_typed::<Doc>(&path).map(|d| d.n), Some(7));
        std::fs::write(&path, r#"{"n":"seven"}"#).unwrap();
        assert!(read_typed::<Doc>(&path).is_none());
    }
}
