//! Unpacking a zip payload (`agent.payloads[].unpack: zip`) into the install
//! tree.
//!
//! The caller has already checked the file against the size and sha256 the
//! signed manifest pins, so nothing here runs on unverified bytes. The members
//! land under one directory with the archive's member rules: a safe relative
//! path ([`safe_member_path`]), no symlink or special file (only regular files
//! and directories, judged by the mode bits the entry carries; the zip format
//! has no hard links), no member twice, no member inflating past the
//! compression-ratio bound. Caps of their own bound the member count, each
//! member and the total, since a runtime is far larger than a plugin archive.
//! Members are streamed to disk, never held whole in memory. A file's mode
//! follows [`Executables::runs`] on its path inside the plugin tree and its
//! first bytes (`0755` or `0644`, never the entry's own bits); every directory
//! is `0755`. The host runs as root, so the tree is root's and no plugin user
//! can write to it.
//!
//! The members are written into a hidden sibling of the destination
//! (`.<name>.unpack`) and renamed onto it only once every member is in, so an
//! interrupted or refused unpack never leaves a partial tree at the
//! destination; the sibling is removed on failure.

use std::fs::{File, OpenOptions};
use std::io::Write;

use super::*;

/// Bounds on an unpacked payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PayloadCaps {
    /// Most members (files and directories).
    pub entries: usize,
    /// Most bytes one member may inflate to.
    pub entry_bytes: u64,
    /// Most bytes all members together may inflate to.
    pub total_bytes: u64,
}

impl PayloadCaps {
    /// The caps an install applies: room for an interpreter with its
    /// libraries, bounded well under a node's disk.
    pub const DEFAULT: PayloadCaps = PayloadCaps {
        entries: 50_000,
        entry_bytes: 512 * 1024 * 1024,
        total_bytes: 2 * 1024 * 1024 * 1024,
    };
}

const S_IFREG: u32 = 0o100000;
const S_IFDIR: u32 = 0o040000;

/// One unpacked file: its path inside the plugin tree and the lowercase hex
/// sha256 of its bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnpackedFile {
    pub path: String,
    pub sha256: String,
}

/// Unpack the verified zip at `zip_path` into `dest` (which must not exist)
/// under [`PayloadCaps::DEFAULT`], `rel` being `dest`'s path inside the plugin
/// tree. Returns every file written, sorted by path.
pub fn unpack_payload_zip(
    zip_path: &Path,
    dest: &Path,
    rel: &str,
    executables: &Executables,
) -> Result<Vec<UnpackedFile>, ArchiveError> {
    unpack_payload_zip_capped(zip_path, dest, rel, executables, PayloadCaps::DEFAULT)
}

/// [`unpack_payload_zip`] under the given caps.
pub fn unpack_payload_zip_capped(
    zip_path: &Path,
    dest: &Path,
    rel: &str,
    executables: &Executables,
    caps: PayloadCaps,
) -> Result<Vec<UnpackedFile>, ArchiveError> {
    let name = dest.file_name().and_then(|n| n.to_str()).ok_or_else(|| {
        ArchiveError(format!(
            "payload destination {} has no name",
            dest.display()
        ))
    })?;
    let parent = dest.parent().ok_or_else(|| {
        ArchiveError(format!(
            "payload destination {} has no parent",
            dest.display()
        ))
    })?;
    if dest.symlink_metadata().is_ok() {
        return Err(ArchiveError(format!(
            "payload destination {rel} already exists"
        )));
    }
    std::fs::create_dir_all(parent)
        .map_err(|e| ArchiveError(format!("cannot create {}: {e}", parent.display())))?;
    let tmp = parent.join(format!(".{name}.unpack"));
    if tmp.symlink_metadata().is_ok() {
        std::fs::remove_dir_all(&tmp)
            .map_err(|e| ArchiveError(format!("cannot clear {}: {e}", tmp.display())))?;
    }
    let result = unpack_into(zip_path, &tmp, rel, executables, caps).and_then(|files| {
        std::fs::rename(&tmp, dest)
            .map_err(|e| ArchiveError(format!("cannot move {rel} into place: {e}")))?;
        Ok(files)
    });
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&tmp);
    }
    result
}

fn unpack_into(
    zip_path: &Path,
    root: &Path,
    rel: &str,
    executables: &Executables,
    caps: PayloadCaps,
) -> Result<Vec<UnpackedFile>, ArchiveError> {
    let file = File::open(zip_path)
        .map_err(|e| ArchiveError(format!("cannot open payload {rel}: {e}")))?;
    let mut zf = zip::ZipArchive::new(file)
        .map_err(|e| ArchiveError(format!("payload {rel} is not a valid zip archive: {e}")))?;
    if zf.len() > caps.entries {
        return Err(ArchiveError(format!(
            "payload {rel} holds {} members; cap is {}",
            zf.len(),
            caps.entries
        )));
    }
    make_dir(root)?;
    let mut total: u64 = 0;
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut files = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    for i in 0..zf.len() {
        let mut entry = zf
            .by_index(i)
            .map_err(|e| ArchiveError(format!("payload {rel}: corrupt member {i}: {e}")))?;
        let member = safe_member_path(entry.name())?.to_string();
        let kind = entry.unix_mode().map_or(0, |m| m & S_IFMT);
        let is_dir = entry.is_dir() || member.ends_with('/');
        let allowed = match kind {
            0 => true,
            S_IFDIR => is_dir,
            S_IFREG => !is_dir,
            _ => false,
        };
        if !allowed {
            return Err(ArchiveError(format!(
                "payload {rel}: member {member} is not a regular file or directory (mode {kind:o})"
            )));
        }
        let body = member.trim_end_matches('/').to_string();
        if !seen.insert(body.clone()) {
            return Err(ArchiveError(format!(
                "payload {rel}: member {body} appears twice"
            )));
        }
        let target = root.join(&body);
        if is_dir {
            make_dirs(root, &body)?;
            continue;
        }
        if let Some((dir, _)) = body.rsplit_once('/') {
            make_dirs(root, dir)?;
        }
        // `create_new` never follows or replaces what is already there.
        let mut out = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
            .map_err(|e| ArchiveError(format!("payload {rel}: cannot create {body}: {e}")))?;
        let mut hasher = Sha256::new();
        let mut head = [0u8; 4];
        let mut head_len = 0usize;
        let mut written: u64 = 0;
        loop {
            let n = std::io::Read::read(&mut entry, &mut buf)
                .map_err(|e| ArchiveError(format!("payload {rel}: read of {body} failed: {e}")))?;
            if n == 0 {
                break;
            }
            written += n as u64;
            if written > caps.entry_bytes {
                return Err(ArchiveError(format!(
                    "payload {rel}: member {body} inflates past the per-member cap {}",
                    caps.entry_bytes
                )));
            }
            total += n as u64;
            if total > caps.total_bytes {
                return Err(ArchiveError(format!(
                    "payload {rel} inflates past the total cap {}",
                    caps.total_bytes
                )));
            }
            let take = (head.len() - head_len).min(n);
            head[head_len..head_len + take].copy_from_slice(&buf[..take]);
            head_len += take;
            hasher.update(&buf[..n]);
            out.write_all(&buf[..n])
                .map_err(|e| ArchiveError(format!("payload {rel}: write of {body} failed: {e}")))?;
        }
        check_compression_ratio(&body, written, entry.compressed_size())?;
        let path = format!("{rel}/{body}");
        set_entry_mode(&target, executables.runs(&path, &head[..head_len]))?;
        files.push(UnpackedFile {
            path,
            sha256: hex::encode(hasher.finalize()),
        });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

/// Create `root/<dirs>` one component at a time, each `0755`; a component
/// that exists must be a directory this unpack made (never a file or link).
fn make_dirs(root: &Path, dirs: &str) -> Result<(), ArchiveError> {
    let mut at = root.to_path_buf();
    for part in dirs.split('/') {
        at.push(part);
        match at.symlink_metadata() {
            Ok(m) if m.file_type().is_dir() => {}
            Ok(_) => {
                return Err(ArchiveError(format!(
                    "payload member path {dirs} runs through a file"
                )))
            }
            Err(_) => make_dir(&at)?,
        }
    }
    Ok(())
}

fn make_dir(path: &Path) -> Result<(), ArchiveError> {
    std::fs::create_dir(path)
        .map_err(|e| ArchiveError(format!("cannot create {}: {e}", path.display())))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| ArchiveError(format!("chmod of {} failed: {e}", path.display())))?;
    }
    Ok(())
}
