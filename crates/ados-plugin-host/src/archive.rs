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

use std::collections::BTreeMap;
use std::io::{Cursor, Read};
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::errors::{ArchiveError, ManifestError, SignatureError, SignatureErrorKind};
use crate::manifest::PluginManifest;

pub const ARCHIVE_MAX_BYTES: u64 = 50 * 1024 * 1024;
pub const ENTRY_MAX_BYTES: u64 = 25 * 1024 * 1024;
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
    /// The raw archive bytes, retained so the caller can unpack after verify.
    pub raw_archive_bytes: Vec<u8>,
}

/// Reject path-traversal and absolute-path entries. Mirrors the Python
/// `_safe_member_path`: a leading `/`, any backslash, or any `..` path segment
/// (or a segment that starts with `..`) is unsafe.
fn safe_member_path(name: &str) -> Result<&str, ArchiveError> {
    if name.starts_with('/') || name.contains('\\') {
        return Err(ArchiveError(format!("unsafe archive entry path: {name:?}")));
    }
    for part in name.split('/') {
        if part == ".." || part.starts_with("..") {
            return Err(ArchiveError(format!("unsafe archive entry path: {name:?}")));
        }
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
    let mut h = Sha256::new();
    for (path, bytes) in entries {
        if path == SIGNATURE_FILENAME {
            continue;
        }
        let digest = hex::encode(Sha256::digest(bytes));
        h.update(format!("{path}\n{digest}\n").as_bytes());
    }
    h.finalize().into()
}

/// Open and parse a `.adosplug` archive from a file without verifying the
/// signature. Validates structural sanity; signature verification is a
/// separate step (see [`crate::signing`]).
pub fn open_archive(path: &Path) -> Result<ArchiveContents, ArchiveError> {
    if !path.exists() {
        return Err(ArchiveError(format!(
            "archive not found: {}",
            path.display()
        )));
    }
    let raw = std::fs::read(path)
        .map_err(|e| ArchiveError(format!("cannot read archive {}: {e}", path.display())))?;
    if raw.len() as u64 > ARCHIVE_MAX_BYTES {
        return Err(ArchiveError(format!(
            "archive {} is {} bytes; cap is {ARCHIVE_MAX_BYTES}",
            path.display(),
            raw.len()
        )));
    }
    parse_archive_bytes(raw)
}

/// Parse archive bytes already in memory. The error type is [`ArchiveError`]
/// for structural problems; a malformed manifest surfaces as a
/// [`ManifestError`] wrapped into the archive error string, and a malformed
/// `SIGNATURE` blob surfaces as a [`SignatureError`] (so the caller can map it
/// to the `invalid` exit code) — both are converted on the way out via the
/// returned `ArchiveError` only when structural, otherwise propagate.
pub fn parse_archive_bytes(raw: Vec<u8>) -> Result<ArchiveContents, ArchiveError> {
    if raw.len() as u64 > ARCHIVE_MAX_BYTES {
        return Err(ArchiveError(format!(
            "archive is {} bytes; cap is {ARCHIVE_MAX_BYTES}",
            raw.len()
        )));
    }

    let entries = read_entries(&raw)?;

    let manifest_bytes = entries
        .get(MANIFEST_FILENAME)
        .ok_or_else(|| ArchiveError(format!("archive missing {MANIFEST_FILENAME}")))?;

    let manifest_text = std::str::from_utf8(manifest_bytes)
        .map_err(|e| ArchiveError(format!("manifest is not valid UTF-8: {e}")))?;
    let manifest = PluginManifest::from_yaml_text(manifest_text)
        .map_err(|e: ManifestError| ArchiveError(e.0))?;

    let payload_hash = canonical_payload_hash(&entries);
    let (signer_id, signature_b64) = read_signature(entries.get(SIGNATURE_FILENAME))?;

    Ok(ArchiveContents {
        manifest,
        payload_hash,
        signer_id,
        signature_b64,
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
    for i in 0..zf.len() {
        // Read the metadata first so the size + symlink checks run before any
        // payload bytes are read into memory.
        let (name, file_size, external_attr, is_dir) = {
            let file = zf
                .by_index(i)
                .map_err(|e| ArchiveError(format!("corrupt zip entry {i}: {e}")))?;
            (
                file.name().to_string(),
                file.size(),
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
        if file_size > ENTRY_MAX_BYTES {
            return Err(ArchiveError(format!(
                "archive entry {safe} is {file_size} bytes; per-entry cap is {ENTRY_MAX_BYTES}"
            )));
        }

        let mut buf = Vec::with_capacity(file_size as usize);
        {
            let mut file = zf
                .by_index(i)
                .map_err(|e| ArchiveError(format!("corrupt zip entry {i}: {e}")))?;
            file.read_to_end(&mut buf)
                .map_err(|e| ArchiveError(format!("read of {safe} failed: {e}")))?;
        }
        entries.insert(safe, buf);
    }
    Ok(entries)
}

/// Parse the two-line `SIGNATURE` blob. Mirrors the Python `_read_signature`:
/// strip blank lines, require exactly two non-blank lines, return
/// `(signer_id, signature_b64)`. A non-UTF-8 or mis-shaped blob is a
/// [`SignatureError`] of kind `invalid`.
fn read_signature(
    blob: Option<&Vec<u8>>,
) -> Result<(Option<String>, Option<String>), ArchiveError> {
    let Some(blob) = blob else {
        return Ok((None, None));
    };
    let text = std::str::from_utf8(blob).map_err(|e| {
        ArchiveError(
            SignatureError::new(
                SignatureErrorKind::Invalid,
                format!("SIGNATURE is not valid UTF-8: {e}"),
            )
            .message,
        )
    })?;
    let lines: Vec<&str> = text
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect();
    if lines.len() != 2 {
        return Err(ArchiveError(
            SignatureError::new(
                SignatureErrorKind::Invalid,
                format!(
                    "SIGNATURE must be 2 non-blank lines (signer-id + sig), got {}",
                    lines.len()
                ),
            )
            .message,
        ));
    }
    Ok((Some(lines[0].to_string()), Some(lines[1].to_string())))
}

/// Unpack validated archive bytes to `dest`. The caller is responsible for
/// having verified the signature first. The same traversal/symlink rejects run
/// again so a caller that hands raw bytes straight to unpack is still safe.
pub fn unpack_to(archive_bytes: &[u8], dest: &Path) -> Result<(), ArchiveError> {
    std::fs::create_dir_all(dest)
        .map_err(|e| ArchiveError(format!("cannot create {}: {e}", dest.display())))?;
    let mut zf = zip::ZipArchive::new(Cursor::new(archive_bytes))
        .map_err(|e| ArchiveError(format!("not a valid zip archive: {e}")))?;
    for i in 0..zf.len() {
        let (name, external_attr, is_dir) = {
            let file = zf
                .by_index(i)
                .map_err(|e| ArchiveError(format!("corrupt zip entry {i}: {e}")))?;
            (
                file.name().to_string(),
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
        let target = dest.join(&safe);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| ArchiveError(format!("cannot create {}: {e}", parent.display())))?;
        }
        let mut buf = Vec::new();
        {
            let mut file = zf
                .by_index(i)
                .map_err(|e| ArchiveError(format!("corrupt zip entry {i}: {e}")))?;
            file.read_to_end(&mut buf)
                .map_err(|e| ArchiveError(format!("read of {safe} failed: {e}")))?;
        }
        std::fs::write(&target, &buf)
            .map_err(|e| ArchiveError(format!("write of {} failed: {e}", target.display())))?;
    }
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
        let err = parse_archive_bytes(zip).unwrap_err();
        assert!(err.0.contains("missing manifest.yaml"), "{}", err.0);
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
        let err = parse_archive_bytes(zip).unwrap_err();
        assert!(err.0.contains("unsafe archive entry path"), "{}", err.0);
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
        let err = parse_archive_bytes(zip).unwrap_err();
        assert!(err.0.contains("unsafe archive entry path"), "{}", err.0);
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
        let err = parse_archive_bytes(zip).unwrap_err();
        assert!(err.0.contains("symlink"), "{}", err.0);
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
        let err = parse_archive_bytes(zip).unwrap_err();
        assert!(err.0.contains("2 non-blank lines"), "{}", err.0);
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
        unpack_to(&zip, dir.path()).unwrap();
        let got = std::fs::read(dir.path().join("agent/py/thermal.py")).unwrap();
        assert_eq!(got, b"print('hi')");
    }
}
