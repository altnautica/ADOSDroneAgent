//! Placing the payloads fetched at install into the staged tree.

use super::*;

/// Move each fetched payload from `payload_dir` into the staged tree at its
/// manifest path, or for an `unpack: zip` payload unpack it into the directory
/// at that path ([`crate::archive::unpack_payload_zip`]). A payload may not
/// overwrite anything the archive ships. A file's mode follows the archive
/// members' rule ([`Executables::runs`]): `0755` when the manifest runs it or
/// it starts with an ELF or `#!` header, else `0644`. Returns the attestation
/// entry of every file placed.
pub(super) fn place_payloads(
    manifest: &PluginManifest,
    payload_dir: &Path,
    staging: &Path,
    fetched: &[(String, String)],
) -> Result<Vec<AttestedFile>, LifecycleError> {
    let executables = Executables::of(manifest);
    let mut placed = Vec::with_capacity(fetched.len());
    // One counter for the install: what the payloads downloaded, then every
    // byte each unpack writes, against the per-install cap.
    let specs = manifest
        .agent
        .as_ref()
        .map(|a| a.payloads.as_slice())
        .unwrap_or_default();
    let spec_of = |path: &str| specs.iter().find(|p| p.path == path);
    let downloaded: u64 = fetched
        .iter()
        .filter_map(|(p, _)| spec_of(p).map(|s| s.size_bytes))
        .sum();
    let mut left = PAYLOADS_INSTALL_MAX_BYTES.saturating_sub(downloaded);
    for (path, sha256) in fetched {
        let dest = staging.join(path);
        if dest.symlink_metadata().is_ok() {
            return Err(SupervisorError(format!(
                "plugin {}: payload {path} collides with a file in the archive",
                manifest.id
            ))
            .into());
        }
        let unpack = spec_of(path).and_then(|p| p.unpack);
        if let Some(PayloadUnpack::Zip) = unpack {
            let caps = crate::archive::PayloadCaps {
                total_bytes: left.min(crate::archive::PayloadCaps::DEFAULT.total_bytes),
                ..crate::archive::PayloadCaps::DEFAULT
            };
            let files = crate::archive::unpack_payload_zip_capped(
                &payload_dir.join(path),
                &dest,
                path,
                &executables,
                caps,
            )
            .map_err(|e| SupervisorError(format!("plugin {}: {e}", manifest.id)))?;
            left = left.saturating_sub(files.iter().map(|f| f.bytes).sum());
            placed.extend(files.into_iter().map(|f| {
                AttestedFile::payload(f.path, &f.sha256, Some((path.clone(), sha256.clone())))
            }));
            continue;
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::rename(payload_dir.join(path), &dest)?;
        let mut head = [0u8; 4];
        let n = {
            use std::io::Read;
            std::fs::File::open(&dest)?.read(&mut head)?
        };
        use std::os::unix::fs::PermissionsExt;
        let mode = if executables.runs(path, &head[..n]) {
            0o755
        } else {
            0o644
        };
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(mode))?;
        placed.push(AttestedFile::payload(path.clone(), sha256, None));
    }
    Ok(placed)
}
