//! Entrypoint checks and unpacking an archive onto disk.

use super::*;

/// The archive-relative files a manifest's declared halves require.
///
/// Returns `(label, relative_path)` pairs for every entrypoint that must exist
/// as a real file: the GCS bundle at `gcs.entrypoint` when the `gcs` block
/// names one, and the agent binary at `agent.entrypoint` when
/// `agent.runtime: rust`. A Python agent's `module:Class` entrypoint is
/// resolved by the runner, not a packed file, so any value containing a `:` is
/// excluded. A `bin:<name>` entrypoint or service command requires this host's
/// `<arch>-<os>` entry of `agent.binaries`, unless that path is delivered as a
/// payload (fetched and hash-checked separately).
pub(super) fn required_entrypoints(
    manifest: &PluginManifest,
    arch_os: &str,
) -> Vec<(String, String)> {
    let mut required: Vec<(String, String)> = Vec::new();
    if let Some(entrypoint) = manifest.gcs.as_ref().and_then(|g| g.entrypoint.as_ref()) {
        if !entrypoint.contains(':') {
            required.push(("gcs.entrypoint".to_string(), entrypoint.clone()));
        }
    }
    let Some(agent) = &manifest.agent else {
        return required;
    };
    let payload_paths: BTreeSet<&str> = agent.payloads.iter().map(|p| p.path.as_str()).collect();
    let mut bins: Vec<(String, String)> = Vec::new();
    match crate::manifest::bin_reference(&agent.entrypoint) {
        Some(name) => bins.push(("agent.entrypoint".to_string(), name.to_string())),
        None => {
            if agent.runtime == crate::manifest::AgentRuntime::Rust
                && !agent.entrypoint.contains(':')
            {
                required.push(("agent.entrypoint".to_string(), agent.entrypoint.clone()));
            }
        }
    }
    for service in crate::services::declared_services(manifest).unwrap_or_default() {
        if let Some(first) = service.command.split_whitespace().next() {
            if let Some(name) = crate::manifest::bin_reference(first) {
                bins.push((format!("service {}", service.name), name.to_string()));
            }
        }
    }
    for (label, name) in bins {
        if let Some(path) = agent.binary_path(&name, arch_os) {
            if !payload_paths.contains(path) {
                required.push((format!("{label} binary {name}"), path.to_string()));
            }
        }
    }
    required
}

/// Assert every must-exist entrypoint is present.
///
/// `present_paths` is the set of archive-relative posix paths actually in the
/// archive (packed or unpacked). Without this check a cloud-relayed install of
/// an archive whose GCS bundle or agent binary was never built reported
/// success, then surfaced as an empty iframe or a unit dying with 203/EXEC —
/// the failure landing two layers away from its cause. `arch_os` selects the
/// `binaries` entry a `bin:` reference needs on this host.
pub fn verify_entrypoints_present(
    manifest: &PluginManifest,
    present_paths: &BTreeSet<String>,
    arch_os: &str,
) -> Result<(), ArchiveError> {
    for (label, rel) in required_entrypoints(manifest, arch_os) {
        if !present_paths.contains(&rel) {
            return Err(ArchiveError(format!(
                "plugin {}: manifest declares {label} {rel:?} but that file is \
                 not present in the archive",
                manifest.id
            )));
        }
    }
    Ok(())
}

/// The set of archive-relative posix paths under an unpacked install dir, for
/// [`verify_entrypoints_present`].
pub fn unpacked_paths(root: &Path) -> BTreeSet<String> {
    let mut out: BTreeSet<String> = BTreeSet::new();
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(ft) if ft.is_dir() => stack.push(path),
                Ok(ft) if ft.is_file() => {
                    if let Ok(rel) = path.strip_prefix(root) {
                        out.insert(
                            rel.components()
                                .map(|c| c.as_os_str().to_string_lossy())
                                .collect::<Vec<_>>()
                                .join("/"),
                        );
                    }
                }
                _ => {}
            }
        }
    }
    out
}

/// The files install marks executable (`0755`; everything else is `0644`):
/// the files a manifest runs (a rust agent's path entrypoint, every `binaries`
/// path, the first word of a service command that is a path, any file named
/// by a `subprocess_spawn` basename), plus any file whose content starts with
/// an executable header ([`has_exec_header`]: ELF magic or `#!`), which covers
/// helper programs a plugin launches itself. One rule ([`Self::runs`]) for
/// archive members and fetched payloads alike. Both inputs are inside the
/// signed content (the manifest, the file bytes); a zip entry's mode bits are
/// not, so re-zipping a signed archive must not change what can run.
#[derive(Debug, Default)]
pub struct Executables {
    paths: BTreeSet<String>,
    basenames: BTreeSet<String>,
}

impl Executables {
    pub fn of(manifest: &PluginManifest) -> Self {
        let mut out = Executables::default();
        let Some(agent) = &manifest.agent else {
            return out;
        };
        let is_path =
            |value: &str| !value.is_empty() && !value.contains(':') && !value.starts_with('/');
        if agent.runtime == crate::manifest::AgentRuntime::Rust && is_path(&agent.entrypoint) {
            out.paths.insert(agent.entrypoint.clone());
        }
        for by_arch in agent.binaries.values() {
            out.paths.extend(by_arch.values().cloned());
        }
        for service in crate::services::declared_services(manifest).unwrap_or_default() {
            if let Some(first) = service.command.split_whitespace().next() {
                if is_path(first) {
                    out.paths.insert(first.to_string());
                }
            }
        }
        out.basenames.extend(agent.subprocess_spawn.iter().cloned());
        out
    }

    /// Whether the archive-relative `path` is one the manifest runs.
    pub fn contains(&self, path: &str) -> bool {
        self.paths.contains(path)
            || path
                .rsplit('/')
                .next()
                .is_some_and(|base| self.basenames.contains(base))
    }

    /// Whether the file at `path` whose content begins with `head` installs
    /// executable: the manifest runs it, or it carries an executable header.
    pub fn runs(&self, path: &str, head: &[u8]) -> bool {
        self.contains(path) || has_exec_header(head)
    }
}

/// Whether `head` (a file's first bytes) starts an ELF image or a `#!` script.
pub fn has_exec_header(head: &[u8]) -> bool {
    head.starts_with(b"\x7fELF") || head.starts_with(b"#!")
}

/// Unpack validated archive bytes to `dest`. The caller is responsible for
/// having verified the signature first. The same traversal/symlink rejects run
/// again so a caller that hands raw bytes straight to unpack is still safe.
/// Only the paths in `executables` unpack runnable.
pub fn unpack_to(
    archive_bytes: &[u8],
    dest: &Path,
    executables: &Executables,
) -> Result<(), ArchiveError> {
    std::fs::create_dir_all(dest)
        .map_err(|e| ArchiveError(format!("cannot create {}: {e}", dest.display())))?;
    let mut zf = zip::ZipArchive::new(Cursor::new(archive_bytes))
        .map_err(|e| ArchiveError(format!("not a valid zip archive: {e}")))?;
    let mut total_decompressed: u64 = 0;
    let mut written: BTreeSet<String> = BTreeSet::new();
    for i in 0..zf.len() {
        let (name, unix_mode, compressed_size, is_dir) = {
            let file = zf
                .by_index(i)
                .map_err(|e| ArchiveError(format!("corrupt zip entry {i}: {e}")))?;
            (
                file.name().to_string(),
                file.unix_mode(),
                file.compressed_size(),
                file.is_dir(),
            )
        };
        let external_attr = unix_mode.map(|m| m << 16).unwrap_or(0);
        let safe = safe_member_path(&name)?.to_string();
        if is_dir || safe.ends_with('/') {
            continue;
        }
        if is_symlink_external_attr(external_attr) {
            return Err(ArchiveError(format!(
                "archive entry {safe} is a symlink; symlinks not allowed"
            )));
        }
        if !written.insert(safe.clone()) {
            return Err(ArchiveError(format!("archive entry {safe} appears twice")));
        }
        let target = dest.join(&safe);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| ArchiveError(format!("cannot create {}: {e}", parent.display())))?;
        }
        // Bound the inflated bytes the same way the in-memory parse does, so a
        // caller that hands raw bytes straight to unpack is still protected from
        // a decompression bomb.
        let buf = {
            let mut file = zf
                .by_index(i)
                .map_err(|e| ArchiveError(format!("corrupt zip entry {i}: {e}")))?;
            read_entry_bounded(&mut file, &safe)?
        };
        check_compression_ratio(&safe, buf.len() as u64, compressed_size)?;
        total_decompressed = total_decompressed.saturating_add(buf.len() as u64);
        if total_decompressed > TOTAL_DECOMPRESSED_MAX {
            return Err(ArchiveError(format!(
                "archive decompresses past the total cap {TOTAL_DECOMPRESSED_MAX}"
            )));
        }
        std::fs::write(&target, &buf)
            .map_err(|e| ArchiveError(format!("write of {} failed: {e}", target.display())))?;
        set_entry_mode(&target, executables.runs(&safe, &buf))?;
    }
    Ok(())
}

/// Set an unpacked file's mode: `0755` for a file the manifest runs, `0644`
/// otherwise. Never the entry's own bits, which are outside the signed payload
/// hash. Unix-only; a no-op elsewhere so the crate builds on a non-Unix dev
/// host.
#[cfg(unix)]
pub(super) fn set_entry_mode(target: &Path, exec: bool) -> Result<(), ArchiveError> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(if exec { 0o755 } else { 0o644 });
    std::fs::set_permissions(target, perms)
        .map_err(|e| ArchiveError(format!("chmod of {} failed: {e}", target.display())))
}

#[cfg(not(unix))]
pub(super) fn set_entry_mode(_target: &Path, _exec: bool) -> Result<(), ArchiveError> {
    Ok(())
}
