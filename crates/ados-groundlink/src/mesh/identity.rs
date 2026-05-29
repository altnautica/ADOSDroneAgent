//! Mesh identity: the deployment `mesh-id` + shared PSK sentinels.
//!
//! Ports `_ensure_mesh_identity` from `mesh_manager.py`. A receiver node
//! generates both on first boot and writes them under `/etc/ados/mesh/`
//! (`mesh-id` as 0644 text, `psk.key` as 0600 raw bytes). A relay picks the
//! values up from a pairing invite bundle. If the files are missing on a relay,
//! [`MeshIdentityError::Missing`] signals the caller to downgrade to `direct`
//! rather than crash-loop.

use std::path::Path;

use sha2::{Digest, Sha256};

use crate::paths::MESH_ID_PATH;

/// Identity-resolution outcome.
#[derive(Debug)]
pub enum MeshIdentityError {
    /// A relay role was requested but no invite has delivered the identity yet.
    /// The caller downgrades to `direct` instead of crash-looping.
    Missing(String),
    /// The PSK on disk is shorter than the 16-byte minimum.
    PskTooShort(usize),
    /// An I/O error writing/reading a sentinel.
    Io(std::io::Error),
}

impl std::fmt::Display for MeshIdentityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MeshIdentityError::Missing(m) => write!(f, "mesh identity missing: {m}"),
            MeshIdentityError::PskTooShort(n) => {
                write!(f, "mesh PSK is {n} bytes, shorter than 16")
            }
            MeshIdentityError::Io(e) => write!(f, "mesh identity io error: {e}"),
        }
    }
}

impl std::error::Error for MeshIdentityError {}

impl From<std::io::Error> for MeshIdentityError {
    fn from(e: std::io::Error) -> Self {
        MeshIdentityError::Io(e)
    }
}

/// Resolved mesh identity: the deployment id + the shared PSK bytes.
#[derive(Debug)]
pub struct MeshIdentity {
    pub mesh_id: String,
    pub psk: Vec<u8>,
}

/// Load or create the deployment `mesh_id` + shared PSK for `role`, using the
/// canonical Contract-E `mesh-id` path. Convenience wrapper over
/// [`ensure_mesh_identity_at`].
pub fn ensure_mesh_identity(
    role: &str,
    configured_id: &str,
    psk_path: &Path,
    device_id: &str,
) -> Result<MeshIdentity, MeshIdentityError> {
    ensure_mesh_identity_at(
        role,
        configured_id,
        Path::new(MESH_ID_PATH),
        psk_path,
        device_id,
    )
}

/// Load or create the deployment `mesh_id` + shared PSK for `role`, with the
/// `mesh-id` path given explicitly (test seam).
///
/// `configured_id` is the operator's `ground_station.mesh.mesh_id` (empty when
/// unset). `psk_path` is `ground_station.mesh.shared_key_path`. `device_id`
/// seeds the receiver's first-boot id derivation. Mirrors
/// `_ensure_mesh_identity`. The mesh-id precedence is: configured id wins, else
/// an existing `mesh-id` file, else a receiver derives
/// `ados-<sha256(device_id)[:10]>` and writes it 0644, else (a relay with
/// nothing on disk) returns `Missing`. The PSK is an existing `psk.key`
/// (at least 16 bytes), else a receiver generates 32 random bytes 0600, else
/// `Missing`. The directory is only created on a write path so a non-root caller
/// resolving a configured id never trips a permission error.
pub fn ensure_mesh_identity_at(
    role: &str,
    configured_id: &str,
    id_path: &Path,
    psk_path: &Path,
    device_id: &str,
) -> Result<MeshIdentity, MeshIdentityError> {
    let mesh_id = if !configured_id.is_empty() {
        configured_id.to_string()
    } else if id_path.is_file() {
        std::fs::read_to_string(id_path)?.trim().to_string()
    } else if role == "receiver" {
        // Stable short id from the device_id. SHA-256 truncation keeps it
        // deterministic per device (HKDF is overkill for a 16-char SSID).
        let seed = if device_id.is_empty() {
            random_hex(8)
        } else {
            device_id.to_string()
        };
        let mut h = Sha256::new();
        h.update(seed.as_bytes());
        let digest = hex::encode(h.finalize());
        let mesh_id = format!("ados-{}", &digest[..10]);
        if let Some(parent) = id_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        write_text_0644(id_path, &format!("{mesh_id}\n"))?;
        mesh_id
    } else {
        return Err(MeshIdentityError::Missing(
            "mesh_id missing. A relay must be paired with a receiver before \
             mesh_manager can start."
                .to_string(),
        ));
    };

    let psk = if psk_path.is_file() {
        let raw = std::fs::read(psk_path)?;
        // Python `.strip()`s the bytes; mirror by trimming ASCII whitespace.
        let trimmed = trim_ascii_whitespace(&raw);
        if trimmed.len() < 16 {
            return Err(MeshIdentityError::PskTooShort(trimmed.len()));
        }
        trimmed.to_vec()
    } else if role == "receiver" {
        let mut psk = vec![0u8; 32];
        getrandom::getrandom(&mut psk).expect("OS RNG for mesh PSK");
        if let Some(parent) = psk_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        write_bytes_0600(psk_path, &psk)?;
        psk
    } else {
        return Err(MeshIdentityError::Missing(format!(
            "mesh PSK missing at {}. A relay must be paired before mesh_manager \
             can start.",
            psk_path.display()
        )));
    };

    Ok(MeshIdentity { mesh_id, psk })
}

fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    getrandom::getrandom(&mut buf).expect("OS RNG");
    hex::encode(buf)
}

fn trim_ascii_whitespace(b: &[u8]) -> &[u8] {
    let start = b
        .iter()
        .position(|c| !c.is_ascii_whitespace())
        .unwrap_or(b.len());
    let end = b
        .iter()
        .rposition(|c| !c.is_ascii_whitespace())
        .map(|i| i + 1)
        .unwrap_or(start);
    &b[start..end]
}

fn write_text_0644(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o644)
        .open(path)?;
    f.write_all(text.as_bytes())?;
    f.sync_all()?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644))?;
    Ok(())
}

fn write_bytes_0600(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn configured_id_wins() {
        let dir = tempfile::tempdir().unwrap();
        let id_path = dir.path().join("mesh/id");
        let psk = dir.path().join("psk.key");
        std::fs::write(&psk, vec![7u8; 32]).unwrap();
        let id = ensure_mesh_identity_at("relay", "ados-operator-pin", &id_path, &psk, "dev123")
            .unwrap();
        assert_eq!(id.mesh_id, "ados-operator-pin");
        assert_eq!(id.psk.len(), 32);
    }

    #[test]
    fn relay_without_identity_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        // No psk file, no configured id, role relay, no mesh-id file.
        let id_path = dir.path().join("mesh/id");
        let psk = dir.path().join("absent-psk.key");
        let err = ensure_mesh_identity_at("relay", "", &id_path, &psk, "dev123");
        // mesh-id is missing for a relay → Missing (the relay must be paired).
        match err {
            Err(MeshIdentityError::Missing(_)) => {}
            other => panic!("expected Missing, got {other:?}"),
        }
    }

    #[test]
    fn receiver_generates_id_and_psk_0600() {
        let dir = tempfile::tempdir().unwrap();
        let id_path = dir.path().join("mesh/id");
        let psk = dir.path().join("mesh/psk.key");
        // No configured id: a receiver derives a stable id from the device-id
        // and writes both sentinels.
        let id = ensure_mesh_identity_at("receiver", "", &id_path, &psk, "device-abc").unwrap();
        // Derived id is ados-<sha256(device-abc)[:10]>, deterministic.
        assert!(id.mesh_id.starts_with("ados-"));
        assert_eq!(id.mesh_id.len(), 5 + 10);
        assert_eq!(id.psk.len(), 32);
        // mesh-id written 0644, PSK 0600.
        let id_mode = std::fs::metadata(&id_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(id_mode, 0o644);
        let psk_mode = std::fs::metadata(&psk).unwrap().permissions().mode() & 0o777;
        assert_eq!(psk_mode, 0o600);
        // A second call reads the persisted id back (stable per device).
        let again = ensure_mesh_identity_at("receiver", "", &id_path, &psk, "device-abc").unwrap();
        assert_eq!(again.mesh_id, id.mesh_id);
    }

    #[test]
    fn short_psk_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let id_path = dir.path().join("mesh/id");
        let psk = dir.path().join("psk.key");
        std::fs::write(&psk, b"tooshort").unwrap(); // 8 bytes < 16
        let err = ensure_mesh_identity_at("receiver", "ados-id", &id_path, &psk, "dev");
        assert!(matches!(err, Err(MeshIdentityError::PskTooShort(8))));
    }

    #[test]
    fn trim_ascii_whitespace_strips_edges() {
        assert_eq!(trim_ascii_whitespace(b"  abc\n"), b"abc");
        assert_eq!(trim_ascii_whitespace(b"abc"), b"abc");
        assert_eq!(trim_ascii_whitespace(b"   "), b"");
    }
}
