//! `.adosplug` archive reader and the canonical payload hash.
//!
//! Archive layout (zip):
//!
//! ```text
//! manifest.yaml                   required
//! SIGNATURE                       optional, format below
//! agent/                          optional, agent half
//! gcs/                            optional, GCS half
//! assets/                         optional, additional files
//! ```
//!
//! SIGNATURE format:
//!
//! ```text
//! line 1: signer-id
//! line 2: base64 ed25519 signature over the canonical payload hash
//! ```
//!
//! The canonical payload hash is `sha256` over the sorted list of
//! `"<path>\n<hex sha256 of bytes>\n"` across every entry except `SIGNATURE`
//! itself. Sorting by path makes the signing payload deterministic regardless
//! of zip ordering. **This is the value that gets signed and must be
//! reproduced byte-for-byte** — see [`canonical_payload_hash`].
//!
//! The archive size limit is 50 MiB; the per-entry size limit is 25 MiB. Both
//! fail at parse with [`ArchiveError`]. Path-traversal entries (`..` segments,
//! absolute paths) and symlink entries are rejected.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::errors::{
    ArchiveError, LifecycleError, ManifestError, SignatureError, SignatureErrorKind,
};
use crate::manifest::PluginManifest;

pub const ARCHIVE_MAX_BYTES: u64 = 50 * 1024 * 1024;
pub const ENTRY_MAX_BYTES: u64 = 25 * 1024 * 1024;
/// Cap on the sum of every entry's decompressed bytes. A zip can declare a
/// small per-entry uncompressed size yet inflate to far more, and many small
/// entries can each stay under the per-entry cap while their sum blows past
/// memory, so the running total is capped independently of the per-entry bound.
pub const TOTAL_DECOMPRESSED_MAX: u64 = 100 * 1024 * 1024;
/// Most an entry may expand over its compressed size once it inflates past
/// [`ENTRY_RATIO_FLOOR_BYTES`]: a higher ratio is a decompression bomb, not a
/// plugin file. Matches the Python reader.
pub const ENTRY_MAX_COMPRESSION_RATIO: u64 = 200;
/// Entries at or under this inflated size skip the ratio check: a small
/// all-zero file legitimately compresses to almost nothing.
pub const ENTRY_RATIO_FLOOR_BYTES: u64 = 1024 * 1024;
pub const SIGNATURE_FILENAME: &str = "SIGNATURE";
pub const MANIFEST_FILENAME: &str = "manifest.yaml";

/// Unix mode bit-mask for symlink entries, carried in the upper 16 bits of the
/// zip `external_attr`.
const S_IFMT: u32 = 0o170000;
const S_IFLNK: u32 = 0o120000;

/// Parsed archive contents prior to signature verification.
#[derive(Debug, Clone)]
pub struct ArchiveContents {
    pub manifest: PluginManifest,
    /// The 32-byte canonical payload hash (the value that is signed).
    pub payload_hash: [u8; 32],
    pub signer_id: Option<String>,
    pub signature_b64: Option<String>,
    /// The `SIGNATURE` entry verbatim, when the archive carries one.
    pub signature_text: Option<String>,
    /// Lowercase hex sha256 of every entry except `SIGNATURE`, by path: the
    /// list the canonical payload hash is computed over.
    pub file_digests: BTreeMap<String, String>,
    /// The raw archive bytes, retained so the caller can unpack after verify.
    pub raw_archive_bytes: Vec<u8>,
}

/// Reject path-traversal, absolute-path and non-canonical entries. A leading
/// `/`, any backslash, any `..` path segment (or a segment that starts with
/// `..`), a `.` segment, or an empty segment (`a//b`) is refused. The last two
/// matter because `./manifest.yaml` and `manifest.yaml` are different zip names
/// that unpack to the same file: accepting both lets the manifest the install
/// gates validated differ from the one written to disk. A directory entry's
/// single trailing `/` is not a segment. The top-level attestation file name is
/// reserved: install writes it after unpack, so an archive entry under that
/// name would be attested and then overwritten.
fn safe_member_path(name: &str) -> Result<&str, ArchiveError> {
    if name.is_empty() || name.starts_with('/') || name.contains('\\') {
        return Err(ArchiveError(format!("unsafe archive entry path: {name:?}")));
    }
    let body = name.strip_suffix('/').unwrap_or(name);
    for part in body.split('/') {
        if part.is_empty() || part == "." || part.starts_with("..") {
            return Err(ArchiveError(format!("unsafe archive entry path: {name:?}")));
        }
    }
    if body == crate::attestation::ATTESTATION_FILENAME {
        return Err(ArchiveError(format!(
            "archive entry {name:?} uses a name reserved for the install attestation"
        )));
    }
    Ok(name)
}

/// Detect a symlink entry via the upper 16 bits of `external_attr`. Unix file
/// modes ride in `external_attr >> 16`; symlinks have the `0o120000` mode bits.
/// A symlink, once unpacked, can target arbitrary paths outside the install
/// dir even when the entry name itself is innocent, so it is rejected.
fn is_symlink_external_attr(external_attr: u32) -> bool {
    let mode = (external_attr >> 16) & 0xFFFF;
    (mode & S_IFMT) == S_IFLNK
}

/// Inflate one zip entry into memory under a hard byte cap, never trusting the
/// archive-declared uncompressed size.
///
/// The declared `size()` is attacker-controlled, so a guard on it alone (and a
/// `Vec::with_capacity(size)` from it) does not bound the real DEFLATE stream:
/// a tiny declared size can inflate to gigabytes (a decompression bomb) and OOM
/// the host. Reading through `take(ENTRY_MAX_BYTES + 1)` bounds the actual
/// inflated bytes; the `+1` lets an overrun be detected (a full cap+1 read means
/// the stream was larger than the cap). `name` only labels the error.
fn read_entry_bounded<R: Read>(reader: &mut R, name: &str) -> Result<Vec<u8>, ArchiveError> {
    let limit = ENTRY_MAX_BYTES + 1;
    let mut buf = Vec::new();
    reader
        .take(limit)
        .read_to_end(&mut buf)
        .map_err(|e| ArchiveError(format!("read of {name} failed: {e}")))?;
    if buf.len() as u64 > ENTRY_MAX_BYTES {
        return Err(ArchiveError(format!(
            "archive entry {name} decompresses past the per-entry cap {ENTRY_MAX_BYTES}"
        )));
    }
    Ok(buf)
}

/// Refuse an entry that inflated past [`ENTRY_RATIO_FLOOR_BYTES`] at more than
/// [`ENTRY_MAX_COMPRESSION_RATIO`] times its compressed size.
fn check_compression_ratio(name: &str, inflated: u64, compressed: u64) -> Result<(), ArchiveError> {
    let compressed = compressed.max(1);
    if inflated > ENTRY_RATIO_FLOOR_BYTES
        && inflated > compressed.saturating_mul(ENTRY_MAX_COMPRESSION_RATIO)
    {
        return Err(ArchiveError(format!(
            "archive entry {name} expands {}x from {compressed} compressed bytes; per-entry \
             ratio cap is {ENTRY_MAX_COMPRESSION_RATIO}x",
            inflated / compressed
        )));
    }
    Ok(())
}

/// Compute the deterministic payload hash over manifest + assets.
///
/// Sort by path. Concatenate `"<path>\n<hex sha256>\n"` for each entry. Hash
/// the concatenation. Excludes [`SIGNATURE_FILENAME`].
///
/// **Security boundary** — this is the value the Ed25519 signature covers. It
/// must stay byte-identical to the Python `_canonical_payload_hash`. A
/// `BTreeMap` keeps the entries sorted by path; the per-entry digest is the
/// lowercase hex of the entry bytes' sha256, exactly as Python's
/// `hashlib.sha256(...).hexdigest()`.
pub fn canonical_payload_hash(entries: &BTreeMap<String, Vec<u8>>) -> [u8; 32] {
    canonical_hash_of_digests(&entry_digests(entries))
}

/// The lowercase hex sha256 of every entry except [`SIGNATURE_FILENAME`].
pub fn entry_digests(entries: &BTreeMap<String, Vec<u8>>) -> BTreeMap<String, String> {
    entries
        .iter()
        .filter(|(path, _)| path.as_str() != SIGNATURE_FILENAME)
        .map(|(path, bytes)| (path.clone(), hex::encode(Sha256::digest(bytes))))
        .collect()
}

/// The canonical payload hash over already-computed per-entry digests.
pub fn canonical_hash_of_digests(digests: &BTreeMap<String, String>) -> [u8; 32] {
    let mut h = Sha256::new();
    for (path, digest) in digests {
        h.update(format!("{path}\n{digest}\n").as_bytes());
    }
    h.finalize().into()
}

/// Open and parse a `.adosplug` archive from a file without verifying the
/// signature. Validates structural sanity; signature verification is a
/// separate step (see [`crate::signing`]). The size cap is checked on the file
/// metadata before any byte is read, and the read itself stops one byte past
/// the cap, so an oversized file is never held in memory.
pub fn open_archive(path: &Path) -> Result<ArchiveContents, LifecycleError> {
    let meta = std::fs::metadata(path)
        .map_err(|e| ArchiveError(format!("cannot read archive {}: {e}", path.display())))?;
    if meta.len() > ARCHIVE_MAX_BYTES {
        return Err(ArchiveError(format!(
            "archive {} is {} bytes; cap is {ARCHIVE_MAX_BYTES}",
            path.display(),
            meta.len()
        ))
        .into());
    }
    let mut raw = Vec::new();
    std::fs::File::open(path)
        .and_then(|f| f.take(ARCHIVE_MAX_BYTES + 1).read_to_end(&mut raw))
        .map_err(|e| ArchiveError(format!("cannot read archive {}: {e}", path.display())))?;
    parse_archive_bytes(raw)
}

/// Parse archive bytes already in memory. A structural problem (or a malformed
/// manifest) is a [`LifecycleError::Archive`]; a malformed `SIGNATURE` blob is
/// a [`LifecycleError::Signature`] of kind `invalid`, so the caller can map it
/// to the signature-invalid outcome without matching strings.
pub fn parse_archive_bytes(raw: Vec<u8>) -> Result<ArchiveContents, LifecycleError> {
    if raw.len() as u64 > ARCHIVE_MAX_BYTES {
        return Err(ArchiveError(format!(
            "archive is {} bytes; cap is {ARCHIVE_MAX_BYTES}",
            raw.len()
        ))
        .into());
    }

    let entries = read_entries(&raw)?;

    let manifest_bytes = entries
        .get(MANIFEST_FILENAME)
        .ok_or_else(|| ArchiveError(format!("archive missing {MANIFEST_FILENAME}")))?;

    let manifest_text = std::str::from_utf8(manifest_bytes)
        .map_err(|e| ArchiveError(format!("manifest is not valid UTF-8: {e}")))?;
    let manifest = PluginManifest::from_yaml_text(manifest_text)
        .map_err(|e: ManifestError| ArchiveError(e.0))?;

    let file_digests = entry_digests(&entries);
    let payload_hash = canonical_hash_of_digests(&file_digests);
    let (signer_id, signature_b64) = read_signature(entries.get(SIGNATURE_FILENAME))?;
    let signature_text = entries
        .get(SIGNATURE_FILENAME)
        .map(|b| String::from_utf8_lossy(b).into_owned());

    Ok(ArchiveContents {
        manifest,
        payload_hash,
        signer_id,
        signature_b64,
        signature_text,
        file_digests,
        raw_archive_bytes: raw,
    })
}

/// Walk the zip central directory, applying the traversal/symlink/size rejects,
/// and read every file entry into memory keyed by its (validated) name. The
/// returned `BTreeMap` is path-sorted, which feeds the canonical hash directly.
fn read_entries(raw: &[u8]) -> Result<BTreeMap<String, Vec<u8>>, ArchiveError> {
    let mut zf = zip::ZipArchive::new(Cursor::new(raw))
        .map_err(|e| ArchiveError(format!("not a valid zip archive: {e}")))?;

    let mut entries: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut total_decompressed: u64 = 0;
    for i in 0..zf.len() {
        // Read the metadata first so the size + symlink checks run before any
        // payload bytes are read into memory. The declared `size()` is only used
        // as an early reject; the real inflation is bounded separately because
        // the declared size is attacker-controlled.
        let (name, file_size, compressed_size, external_attr, is_dir) = {
            let file = zf
                .by_index(i)
                .map_err(|e| ArchiveError(format!("corrupt zip entry {i}: {e}")))?;
            (
                file.name().to_string(),
                file.size(),
                file.compressed_size(),
                file.unix_mode().map(|m| m << 16).unwrap_or(0),
                file.is_dir(),
            )
        };

        let safe = safe_member_path(&name)?.to_string();
        if is_dir || safe.ends_with('/') {
            continue;
        }
        if is_symlink_external_attr(external_attr) {
            return Err(ArchiveError(format!(
                "archive entry {safe} is a symlink; symlinks not allowed"
            )));
        }
        // Early reject on the declared size (cheap), then the real, bounded read.
        if file_size > ENTRY_MAX_BYTES {
            return Err(ArchiveError(format!(
                "archive entry {safe} is {file_size} bytes; per-entry cap is {ENTRY_MAX_BYTES}"
            )));
        }

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
        if entries.insert(safe.clone(), buf).is_some() {
            return Err(ArchiveError(format!("archive entry {safe} appears twice")));
        }
    }
    Ok(entries)
}

/// Parse the two-line `SIGNATURE` blob. Mirrors the Python `_read_signature`:
/// strip blank lines, require exactly two non-blank lines, return
/// `(signer_id, signature_b64)`. A non-UTF-8 or mis-shaped blob is a
/// [`SignatureError`] of kind `invalid`.
fn read_signature(
    blob: Option<&Vec<u8>>,
) -> Result<(Option<String>, Option<String>), SignatureError> {
    let Some(blob) = blob else {
        return Ok((None, None));
    };
    let text = std::str::from_utf8(blob).map_err(|e| {
        SignatureError::new(
            SignatureErrorKind::Invalid,
            format!("SIGNATURE is not valid UTF-8: {e}"),
        )
    })?;
    let lines: Vec<&str> = text
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect();
    if lines.len() != 2 {
        return Err(SignatureError::new(
            SignatureErrorKind::Invalid,
            format!(
                "SIGNATURE must be 2 non-blank lines (signer-id + sig), got {}",
                lines.len()
            ),
        ));
    }
    Ok((Some(lines[0].to_string()), Some(lines[1].to_string())))
}

/// The archive-relative files a manifest's declared halves require.
///
/// Returns `(label, relative_path)` pairs for every entrypoint that must exist
/// as a real file: the GCS bundle at `gcs.entrypoint` when a `gcs` block
/// exists, and the agent binary at `agent.entrypoint` when
/// `agent.runtime: rust`. A Python agent's `module:Class` entrypoint is
/// resolved by the runner, not a packed file, so any value containing a `:` is
/// excluded. A `bin:<name>` entrypoint or service command requires this host's
/// `<arch>-<os>` entry of `agent.binaries`, unless that path is delivered as a
/// payload (fetched and hash-checked separately).
fn required_entrypoints(manifest: &PluginManifest, arch_os: &str) -> Vec<(String, String)> {
    let mut required: Vec<(String, String)> = Vec::new();
    if let Some(gcs) = &manifest.gcs {
        if !gcs.entrypoint.contains(':') {
            required.push(("gcs.entrypoint".to_string(), gcs.entrypoint.clone()));
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

/// The files a manifest runs, and so the only ones install marks executable:
/// a rust agent's path entrypoint, every `binaries` path, the first word of a
/// service command that is a path, and any file named by a `subprocess_spawn`
/// basename. Derived from the signed manifest because a zip entry's mode bits
/// sit outside the signed payload hash, so re-zipping a signed archive must not
/// change what can run.
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
        set_entry_mode(&target, executables.contains(&safe))?;
    }
    Ok(())
}

/// Set an unpacked file's mode: `0755` for a file the manifest runs, `0644`
/// otherwise. Never the entry's own bits, which are outside the signed payload
/// hash. Unix-only; a no-op elsewhere so the crate builds on a non-Unix dev
/// host.
#[cfg(unix)]
fn set_entry_mode(target: &Path, exec: bool) -> Result<(), ArchiveError> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(if exec { 0o755 } else { 0o644 });
    std::fs::set_permissions(target, perms)
        .map_err(|e| ArchiveError(format!("chmod of {} failed: {e}", target.display())))
}

#[cfg(not(unix))]
fn set_entry_mode(_target: &Path, _exec: bool) -> Result<(), ArchiveError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    fn manifest_yaml() -> &'static str {
        "id: com.example.thermal\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0\"\nagent:\n  entrypoint: agent/py/thermal.py\n"
    }

    /// Build an in-memory stored zip from (name, bytes) pairs. A matching
    /// `symlink` name is written as a real symlink entry (the writer sets the
    /// `S_IFLNK` mode bits in the central-directory external attr) so the
    /// reader's symlink reject is exercised against a genuine symlink, not a
    /// faked permission mask.
    fn build_zip(entries: &[(&str, &[u8])], symlink: Option<&str>) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut w = zip::ZipWriter::new(Cursor::new(&mut buf));
            for (name, bytes) in entries {
                let opts =
                    SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
                if symlink == Some(name) {
                    let target = std::str::from_utf8(bytes).unwrap();
                    w.add_symlink(*name, target, opts).unwrap();
                } else {
                    w.start_file(*name, opts).unwrap();
                    w.write_all(bytes).unwrap();
                }
            }
            w.finish().unwrap();
        }
        buf
    }

    #[test]
    fn parses_a_well_formed_archive() {
        let zip = build_zip(
            &[
                ("manifest.yaml", manifest_yaml().as_bytes()),
                ("agent/py/thermal.py", b"print('hi')"),
            ],
            None,
        );
        let c = parse_archive_bytes(zip).unwrap();
        assert_eq!(c.manifest.id, "com.example.thermal");
        assert!(c.signer_id.is_none());
    }

    #[test]
    fn missing_manifest_is_rejected() {
        let zip = build_zip(&[("agent/py/x.py", b"x")], None);
        let err = parse_archive_bytes(zip).unwrap_err().to_string();
        assert!(err.contains("missing manifest.yaml"), "{}", err);
    }

    #[test]
    fn traversal_entry_is_rejected() {
        let zip = build_zip(
            &[
                ("manifest.yaml", manifest_yaml().as_bytes()),
                ("../escape.py", b"x"),
            ],
            None,
        );
        let err = parse_archive_bytes(zip).unwrap_err().to_string();
        assert!(err.contains("unsafe archive entry path"), "{}", err);
    }

    #[test]
    fn absolute_path_entry_is_rejected() {
        let zip = build_zip(
            &[
                ("manifest.yaml", manifest_yaml().as_bytes()),
                ("/etc/passwd", b"x"),
            ],
            None,
        );
        let err = parse_archive_bytes(zip).unwrap_err().to_string();
        assert!(err.contains("unsafe archive entry path"), "{}", err);
    }

    #[test]
    fn symlink_entry_is_rejected() {
        let zip = build_zip(
            &[
                ("manifest.yaml", manifest_yaml().as_bytes()),
                ("link", b"/etc/shadow"),
            ],
            Some("link"),
        );
        let err = parse_archive_bytes(zip).unwrap_err().to_string();
        assert!(err.contains("symlink"), "{}", err);
    }

    #[test]
    fn signature_blob_two_lines_parses() {
        let zip = build_zip(
            &[
                ("manifest.yaml", manifest_yaml().as_bytes()),
                ("SIGNATURE", b"altnautica-2026-A\nQUJD\n"),
            ],
            None,
        );
        let c = parse_archive_bytes(zip).unwrap();
        assert_eq!(c.signer_id.as_deref(), Some("altnautica-2026-A"));
        assert_eq!(c.signature_b64.as_deref(), Some("QUJD"));
    }

    #[test]
    fn signature_blob_wrong_line_count_is_rejected() {
        let zip = build_zip(
            &[
                ("manifest.yaml", manifest_yaml().as_bytes()),
                ("SIGNATURE", b"only-one-line\n"),
            ],
            None,
        );
        let err = parse_archive_bytes(zip).unwrap_err().to_string();
        assert!(err.contains("2 non-blank lines"), "{}", err);
    }

    #[test]
    fn canonical_hash_excludes_signature_and_is_path_sorted() {
        // Two orderings of the same entries must hash equal; adding SIGNATURE
        // must not change the hash.
        let mut a: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        a.insert("manifest.yaml".into(), b"m".to_vec());
        a.insert("agent/x.py".into(), b"x".to_vec());
        let h1 = canonical_payload_hash(&a);

        let mut b = a.clone();
        b.insert("SIGNATURE".into(), b"sig-noise".to_vec());
        let h2 = canonical_payload_hash(&b);
        assert_eq!(h1, h2, "SIGNATURE must be excluded from the hash");
    }

    #[test]
    fn unpack_round_trips_files() {
        let dir = tempfile::tempdir().unwrap();
        let zip = build_zip(
            &[
                ("manifest.yaml", manifest_yaml().as_bytes()),
                ("agent/py/thermal.py", b"print('hi')"),
            ],
            None,
        );
        unpack_to(&zip, dir.path(), &Executables::default()).unwrap();
        let got = std::fs::read(dir.path().join("agent/py/thermal.py")).unwrap();
        assert_eq!(got, b"print('hi')");
    }

    #[test]
    fn oversized_decompressed_entry_is_rejected() {
        // A deflated entry of highly-compressible zeros that inflates past the
        // per-entry cap. The compressed bytes stay tiny (well under the archive
        // cap), so the only thing standing between this and an OOM is the
        // bounded-inflation guard, not the on-disk size checks.
        let big = (ENTRY_MAX_BYTES + 4096) as usize;
        let mut buf = Vec::new();
        {
            let mut w = zip::ZipWriter::new(Cursor::new(&mut buf));
            let stored =
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
            w.start_file("manifest.yaml", stored).unwrap();
            w.write_all(manifest_yaml().as_bytes()).unwrap();
            let deflated =
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
            w.start_file("assets/bomb.bin", deflated).unwrap();
            // Write in chunks so the test does not hold the full payload twice.
            let chunk = vec![0u8; 1024 * 1024];
            let mut written = 0usize;
            while written < big {
                let n = chunk.len().min(big - written);
                w.write_all(&chunk[..n]).unwrap();
                written += n;
            }
            w.finish().unwrap();
        }
        // The compressed archive is far smaller than the archive cap, yet the
        // entry inflates past the per-entry cap, so the parse must refuse it.
        assert!(
            (buf.len() as u64) < ARCHIVE_MAX_BYTES,
            "compressed archive should be small ({})",
            buf.len()
        );
        let err = parse_archive_bytes(buf.clone()).unwrap_err().to_string();
        assert!(
            err.contains("per-entry cap"),
            "expected a per-entry decompression cap error, got: {}",
            err
        );
        // unpack_to must enforce the same bound (it had no cap at all before).
        let dir = tempfile::tempdir().unwrap();
        let err2 = unpack_to(&buf, dir.path(), &Executables::default()).unwrap_err();
        assert!(
            err2.0.contains("per-entry cap"),
            "unpack must reject the bomb too, got: {}",
            err2.0
        );
    }

    #[cfg(unix)]
    #[test]
    fn only_files_the_manifest_runs_unpack_executable() {
        use std::os::unix::fs::PermissionsExt;
        let manifest = PluginManifest::from_yaml_text(
            "id: com.example.geo\nversion: 1.0.0\nrisk: critical\ncompatibility:\n  ados_version: \">=0.1.0\"\nagent:\n  entrypoint: bin:geofence\n  runtime: rust\n  binaries:\n    geofence:\n      aarch64-linux: agent/bin/geofence\n  permissions:\n    - process.spawn\n  subprocess_spawn:\n    - helper\n",
        )
        .unwrap();
        // The zip's own mode bits say the opposite of the manifest: the
        // binary is plain and an asset carries every exec bit.
        let mut buf = Vec::new();
        {
            let mut w = zip::ZipWriter::new(Cursor::new(&mut buf));
            let stored =
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
            w.start_file("manifest.yaml", stored.unix_permissions(0o777))
                .unwrap();
            w.write_all(b"id: x\n").unwrap();
            w.start_file("agent/bin/geofence", stored.unix_permissions(0o644))
                .unwrap();
            w.write_all(b"#!/bin/sh\n").unwrap();
            w.start_file("vendor/helper", stored.unix_permissions(0o600))
                .unwrap();
            w.write_all(b"#!/bin/sh\n").unwrap();
            w.finish().unwrap();
        }
        let dir = tempfile::tempdir().unwrap();
        unpack_to(&buf, dir.path(), &Executables::of(&manifest)).unwrap();
        let mode = |rel: &str| {
            std::fs::metadata(dir.path().join(rel))
                .unwrap()
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode("agent/bin/geofence"), 0o755);
        assert_eq!(mode("vendor/helper"), 0o755);
        assert_eq!(mode("manifest.yaml"), 0o644);
    }

    #[test]
    fn an_entry_under_the_attestation_name_is_refused() {
        let zip = build_zip(
            &[
                ("manifest.yaml", manifest_yaml().as_bytes()),
                (".attestation.json", b"{}"),
            ],
            None,
        );
        let err = parse_archive_bytes(zip).unwrap_err().to_string();
        assert!(err.contains("reserved"), "{err}");
        // The same name below the top level is an ordinary file.
        let nested = build_zip(
            &[
                ("manifest.yaml", manifest_yaml().as_bytes()),
                ("assets/.attestation.json", b"{}"),
            ],
            None,
        );
        assert!(parse_archive_bytes(nested).is_ok());
    }

    #[test]
    fn an_entry_that_expands_past_the_ratio_cap_is_refused() {
        // Two MiB of zeros deflate to a few KiB: past the floor and far past
        // the ratio, while still under the per-entry byte cap.
        let mut buf = Vec::new();
        {
            let mut w = zip::ZipWriter::new(Cursor::new(&mut buf));
            let stored =
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
            w.start_file("manifest.yaml", stored).unwrap();
            w.write_all(manifest_yaml().as_bytes()).unwrap();
            let deflated =
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
            w.start_file("assets/zeros.bin", deflated).unwrap();
            w.write_all(&vec![0u8; 2 * 1024 * 1024]).unwrap();
            w.finish().unwrap();
        }
        let err = parse_archive_bytes(buf.clone()).unwrap_err().to_string();
        assert!(err.contains("ratio cap"), "{err}");
        let dir = tempfile::tempdir().unwrap();
        let err = unpack_to(&buf, dir.path(), &Executables::default()).unwrap_err();
        assert!(err.0.contains("ratio cap"), "{}", err.0);
    }

    #[test]
    fn dot_and_empty_segments_cannot_alias_the_manifest() {
        // `./manifest.yaml` unpacks over `manifest.yaml`, so the validated
        // manifest and the one on disk could differ.
        for alias in ["./manifest.yaml", "agent//x.py", "agent/./x.py"] {
            let zip = build_zip(
                &[
                    ("manifest.yaml", manifest_yaml().as_bytes()),
                    (alias, b"id: com.example.other\n"),
                ],
                None,
            );
            let err = parse_archive_bytes(zip.clone()).unwrap_err().to_string();
            assert!(err.contains("unsafe archive entry path"), "{alias}: {err}");
            let dir = tempfile::tempdir().unwrap();
            assert!(
                unpack_to(&zip, dir.path(), &Executables::default()).is_err(),
                "{alias}"
            );
        }
    }

    #[test]
    fn malformed_signature_keeps_the_invalid_kind() {
        let zip = build_zip(
            &[
                ("manifest.yaml", manifest_yaml().as_bytes()),
                ("SIGNATURE", b"only-one-line\n"),
            ],
            None,
        );
        match parse_archive_bytes(zip) {
            Err(LifecycleError::Signature(e)) => {
                assert_eq!(e.kind, SignatureErrorKind::Invalid)
            }
            other => panic!("expected a signature error, got {other:?}"),
        }
    }

    #[test]
    fn oversized_archive_file_is_refused_before_it_is_read() {
        // A sparse file past the cap: refused on its metadata, never buffered.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.adosplug");
        std::fs::File::create(&path)
            .unwrap()
            .set_len(ARCHIVE_MAX_BYTES + 1)
            .unwrap();
        let err = open_archive(&path).unwrap_err().to_string();
        assert!(err.contains("cap is"), "{err}");
    }
}
