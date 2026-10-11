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
    // A control character (a newline above all) in a name would let one entry
    // impersonate several in the `path\nhex\n` canonical stream the signature
    // covers, so a re-zipped archive could verify over a different file set.
    if name.is_empty()
        || name.starts_with('/')
        || name.contains('\\')
        || name.chars().any(char::is_control)
    {
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

mod unpack;

pub use unpack::*;

#[cfg(test)]
mod tests;
