//! Mesh-pairing revocation list.
//!
//! Ports `pairing_manager`'s revocation primitives: a sorted JSON array of
//! revoked device-ids persisted at `/etc/ados/mesh/revocations.json` with
//! **0600** perms (owner-only). A revoked relay's join request is dropped.
//! `save` writes atomically (tmp + rename) with the list sorted, matching
//! `json.dumps(sorted(revoked))`.

use std::collections::BTreeSet;
use std::path::Path;

use crate::paths::MESH_REVOCATIONS_JSON;

/// Read the current revocation set from disk. A missing/unreadable/malformed
/// file is an empty set (matching the Python `_read_revocations_from_disk`
/// tolerance). Reads from the canonical Contract-E path.
pub fn load() -> BTreeSet<String> {
    load_from(Path::new(MESH_REVOCATIONS_JSON))
}

/// Read the revocation set from an explicit path (test seam).
pub fn load_from(path: &Path) -> BTreeSet<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return BTreeSet::new();
    };
    match serde_json::from_str::<Vec<String>>(&text) {
        Ok(list) => list.into_iter().collect(),
        Err(_) => BTreeSet::new(),
    }
}

/// Persist the revocation set atomically with 0600 perms, sorted. A `BTreeSet`
/// is already ordered, so serialization is deterministic (matching
/// `json.dumps(sorted(revoked))`).
pub fn save(revoked: &BTreeSet<String>) -> std::io::Result<()> {
    save_to(Path::new(MESH_REVOCATIONS_JSON), revoked)
}

/// Persist to an explicit path (test seam).
pub fn save_to(path: &Path, revoked: &BTreeSet<String>) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    // BTreeSet iterates in sorted order → `["a","b",...]` matching sorted().
    let sorted: Vec<&String> = revoked.iter().collect();
    let body = serde_json::to_vec(&sorted).map_err(std::io::Error::other)?;
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(&body)?;
        f.sync_all()?;
    }
    // Re-assert 0600: the create mode is masked by umask, and a stale tmp from a
    // prior crash may carry looser perms.
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Add a device-id to the revocation list (idempotent).
pub fn revoke(path: &Path, device_id: &str) -> std::io::Result<()> {
    let mut rs = load_from(path);
    rs.insert(device_id.to_string());
    save_to(path, &rs)
}

/// Remove a device-id from the revocation list (idempotent).
pub fn unrevoke(path: &Path, device_id: &str) -> std::io::Result<()> {
    let mut rs = load_from(path);
    rs.remove(device_id);
    save_to(path, &rs)
}

/// True when `device_id` is currently revoked.
pub fn is_revoked(path: &Path, device_id: &str) -> bool {
    load_from(path).contains(device_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn empty_when_file_absent() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("revocations.json");
        assert!(load_from(&p).is_empty());
        assert!(!is_revoked(&p, "anything"));
    }

    #[test]
    fn save_writes_sorted_list_with_0600() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("mesh/revocations.json");
        let mut set = BTreeSet::new();
        set.insert("zeta".to_string());
        set.insert("alpha".to_string());
        set.insert("mike".to_string());
        save_to(&p, &set).unwrap();

        // Sorted on disk (BTreeSet order == sorted()).
        let body = std::fs::read_to_string(&p).unwrap();
        assert_eq!(body, r#"["alpha","mike","zeta"]"#);

        // 0600 owner-only.
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        // No leftover tmp.
        assert!(!dir.path().join("mesh/revocations.tmp").exists());
    }

    #[test]
    fn revoke_unrevoke_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("revocations.json");
        revoke(&p, "drone-bad").unwrap();
        assert!(is_revoked(&p, "drone-bad"));
        // Idempotent revoke.
        revoke(&p, "drone-bad").unwrap();
        assert_eq!(load_from(&p).len(), 1);
        unrevoke(&p, "drone-bad").unwrap();
        assert!(!is_revoked(&p, "drone-bad"));
    }

    #[test]
    fn malformed_file_is_empty_set() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("revocations.json");
        std::fs::write(&p, "{not a list}").unwrap();
        assert!(load_from(&p).is_empty());
    }
}
