//! Unpacking a zip payload (`agent.payloads[].unpack: zip`) into the install
//! tree.
//!
//! The caller has already checked the file against the size and sha256 the
//! signed manifest pins, so nothing here runs on unverified bytes. Before the
//! zip reader parses anything, the end-of-central-directory record (Zip64
//! when present) is read by hand and the declared member count and central
//! directory size are held to caps, so a hostile directory never reaches the
//! reader's allocations; the reader's own count must then match the declared
//! one. The members land under one directory with the archive's member rules:
//! a safe relative path ([`safe_member_path`]) of at most
//! [`MAX_NAME_BYTES`] bytes, [`MAX_SEGMENTS`] segments and
//! [`MAX_SEGMENT_BYTES`] bytes a segment, no symlink or special file (only
//! regular files and directories, judged by the mode bits the entry carries;
//! the zip format has no hard links), no member twice, no member inflating
//! past the compression-ratio bound (checked while it streams). Caps of their
//! own bound the files and directories created, each member and the total,
//! and the filesystem must have room for the total the members declare
//! before anything is written. Members are streamed to disk, never held whole
//! in memory. A file's mode follows [`Executables::runs`] on its path inside
//! the plugin tree and its first bytes (`0755` or `0644`, never the entry's
//! own bits); every directory is `0755`. The host runs as root, so the tree is
//! root's and no plugin user can write to it.
//!
//! The members are written into a hidden sibling of the destination
//! (`.<name>.unpack`) and renamed onto it only once every member is in, so an
//! interrupted or refused unpack never leaves a partial tree at the
//! destination; the sibling is removed on failure. A sibling already there
//! (left by a crash) is refused, never deleted: the staging tree it lives in
//! is the caller's to clear.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};

use super::*;

/// Bounds on an unpacked payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PayloadCaps {
    /// Most members, and most files plus directories created.
    pub entries: usize,
    /// Most bytes one member may inflate to.
    pub entry_bytes: u64,
    /// Most bytes all members together may inflate to.
    pub total_bytes: u64,
    /// Largest central directory the reader is handed.
    pub directory_bytes: u64,
}

impl PayloadCaps {
    /// The caps an install applies: room for an interpreter with its
    /// libraries, bounded well under a node's disk.
    pub const DEFAULT: PayloadCaps = PayloadCaps {
        entries: 50_000,
        entry_bytes: 512 * 1024 * 1024,
        total_bytes: 2 * 1024 * 1024 * 1024,
        directory_bytes: 16 * 1024 * 1024,
    };
}

/// Longest member name, in bytes.
pub const MAX_NAME_BYTES: usize = 1024;
/// Most path segments in a member name.
pub const MAX_SEGMENTS: usize = 32;
/// Longest path segment, in bytes.
pub const MAX_SEGMENT_BYTES: usize = 255;

const S_IFREG: u32 = 0o100000;
const S_IFDIR: u32 = 0o040000;
const EOCD_SIG: u32 = 0x0605_4b50;
const ZIP64_LOCATOR_SIG: u32 = 0x0706_4b50;
const ZIP64_EOCD_SIG: u32 = 0x0606_4b50;

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
        return Err(ArchiveError(format!(
            "payload {rel}: a previous unpack left {} behind",
            tmp.display()
        )));
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

/// The member count and central directory size the end-of-central-directory
/// record declares (the Zip64 record's when the classic one defers to it).
fn declared_directory(file: &mut File, rel: &str) -> Result<(u64, u64, u64), ArchiveError> {
    let bad = |why: &str| ArchiveError(format!("payload {rel} is not a valid zip archive: {why}"));
    let len = file
        .seek(SeekFrom::End(0))
        .map_err(|e| bad(&e.to_string()))?;
    // The record is 22 bytes plus a comment of at most 65535.
    let tail_len = len.min(22 + 65_535);
    let mut tail = vec![0u8; tail_len as usize];
    file.seek(SeekFrom::Start(len - tail_len))
        .and_then(|_| file.read_exact(&mut tail))
        .map_err(|e| bad(&e.to_string()))?;
    let u16_at = |b: &[u8], i: usize| u64::from(u16::from_le_bytes([b[i], b[i + 1]]));
    let u32_at =
        |b: &[u8], i: usize| u64::from(u32::from_le_bytes(b[i..i + 4].try_into().unwrap()));
    let u64_at = |b: &[u8], i: usize| u64::from_le_bytes(b[i..i + 8].try_into().unwrap());
    let at = (0..tail.len().saturating_sub(21))
        .rev()
        .find(|&i| u32_at(&tail, i) == u64::from(EOCD_SIG))
        .ok_or_else(|| bad("no end-of-central-directory record"))?;
    let (entries, size) = (u16_at(&tail, at + 10), u32_at(&tail, at + 12));
    let offset = u32_at(&tail, at + 16);
    if entries != 0xFFFF && size != 0xFFFF_FFFF && offset != 0xFFFF_FFFF {
        return Ok((entries, size, offset));
    }
    // Zip64: the locator sits just before the classic record.
    let eocd_pos = len - tail_len + at as u64;
    let mut loc = [0u8; 20];
    file.seek(SeekFrom::Start(
        eocd_pos
            .checked_sub(20)
            .ok_or_else(|| bad("no Zip64 locator"))?,
    ))
    .and_then(|_| file.read_exact(&mut loc))
    .map_err(|e| bad(&e.to_string()))?;
    if u32_at(&loc, 0) != u64::from(ZIP64_LOCATOR_SIG) {
        return Err(bad("no Zip64 locator"));
    }
    let mut rec = [0u8; 56];
    file.seek(SeekFrom::Start(u64_at(&loc, 8)))
        .and_then(|_| file.read_exact(&mut rec))
        .map_err(|e| bad(&e.to_string()))?;
    if u32_at(&rec, 0) != u64::from(ZIP64_EOCD_SIG) {
        return Err(bad("no Zip64 end-of-central-directory record"));
    }
    Ok((u64_at(&rec, 32), u64_at(&rec, 40), u64_at(&rec, 48)))
}

/// The file headers actually in the central directory at `offset`, `size`
/// bytes long (already capped).
fn directory_headers(
    file: &mut File,
    offset: u64,
    size: u64,
    rel: &str,
) -> Result<u64, ArchiveError> {
    let mut dir = vec![0u8; size as usize];
    file.seek(SeekFrom::Start(offset))
        .and_then(|_| file.read_exact(&mut dir))
        .map_err(|e| {
            ArchiveError(format!(
                "payload {rel}: cannot read its central directory: {e}"
            ))
        })?;
    let u16_at = |i: usize| usize::from(u16::from_le_bytes([dir[i], dir[i + 1]]));
    let (mut at, mut count) = (0usize, 0u64);
    while at + 46 <= dir.len() && dir[at..at + 4] == [0x50, 0x4b, 0x01, 0x02] {
        at += 46 + u16_at(at + 28) + u16_at(at + 30) + u16_at(at + 32);
        count += 1;
    }
    Ok(count)
}

/// Room on the filesystem holding `dir` for `bytes` more.
fn check_room(dir: &Path, bytes: u64, rel: &str) -> Result<(), ArchiveError> {
    let st = nix::sys::statvfs::statvfs(dir)
        .map_err(|e| ArchiveError(format!("payload {rel}: cannot read free space: {e}")))?;
    let free = st.blocks_available().saturating_mul(st.fragment_size());
    if bytes > free {
        return Err(ArchiveError(format!(
            "payload {rel} declares {bytes} bytes; {free} are free"
        )));
    }
    Ok(())
}

fn check_name(rel: &str, name: &str) -> Result<(), ArchiveError> {
    let body = name.trim_end_matches('/');
    if name.len() > MAX_NAME_BYTES
        || body.split('/').count() > MAX_SEGMENTS
        || body.split('/').any(|s| s.len() > MAX_SEGMENT_BYTES)
    {
        return Err(ArchiveError(format!(
            "payload {rel}: member name {:?} is too long or too deep",
            name.chars().take(64).collect::<String>()
        )));
    }
    Ok(())
}

fn unpack_into(
    zip_path: &Path,
    root: &Path,
    rel: &str,
    executables: &Executables,
    caps: PayloadCaps,
) -> Result<Vec<UnpackedFile>, ArchiveError> {
    let mut file = File::open(zip_path)
        .map_err(|e| ArchiveError(format!("cannot open payload {rel}: {e}")))?;
    let (declared, directory, offset) = declared_directory(&mut file, rel)?;
    if declared > caps.entries as u64 {
        return Err(ArchiveError(format!(
            "payload {rel} holds {declared} members; cap is {}",
            caps.entries
        )));
    }
    if directory > caps.directory_bytes {
        return Err(ArchiveError(format!(
            "payload {rel} has a {directory}-byte central directory; cap is {}",
            caps.directory_bytes
        )));
    }
    let listed = directory_headers(&mut file, offset, directory, rel)?;
    if listed != declared {
        return Err(ArchiveError(format!(
            "payload {rel} declares {declared} members but its directory lists {listed}"
        )));
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|e| ArchiveError(format!("cannot read payload {rel}: {e}")))?;
    let mut zf = zip::ZipArchive::new(file)
        .map_err(|e| ArchiveError(format!("payload {rel} is not a valid zip archive: {e}")))?;
    if zf.len() as u64 != declared {
        return Err(ArchiveError(format!(
            "payload {rel} declares {declared} members but its directory lists {}",
            zf.len()
        )));
    }
    let mut promised: u64 = 0;
    for i in 0..zf.len() {
        let entry = zf
            .by_index_raw(i)
            .map_err(|e| ArchiveError(format!("payload {rel}: corrupt member {i}: {e}")))?;
        promised = promised.saturating_add(entry.size());
    }
    if promised > caps.total_bytes {
        return Err(ArchiveError(format!(
            "payload {rel} inflates past the total cap {}",
            caps.total_bytes
        )));
    }
    let parent = root.parent().unwrap_or(root);
    check_room(parent, promised, rel)?;
    make_dir(root)?;
    let mut created: usize = 0;
    let mut total: u64 = 0;
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut files = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    for i in 0..zf.len() {
        let mut entry = zf
            .by_index(i)
            .map_err(|e| ArchiveError(format!("payload {rel}: corrupt member {i}: {e}")))?;
        check_name(rel, entry.name())?;
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
            created += make_dirs(root, &body)?;
        } else {
            if let Some((dir, _)) = body.rsplit_once('/') {
                created += make_dirs(root, dir)?;
            }
            created += 1;
        }
        if created > caps.entries {
            return Err(ArchiveError(format!(
                "payload {rel} creates more than {} files and directories",
                caps.entries
            )));
        }
        if is_dir {
            continue;
        }
        // `create_new` never follows or replaces what is already there.
        let mut out = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
            .map_err(|e| ArchiveError(format!("payload {rel}: cannot create {body}: {e}")))?;
        let compressed = entry.compressed_size();
        let mut hasher = Sha256::new();
        let mut head = [0u8; 4];
        let mut head_len = 0usize;
        let mut written: u64 = 0;
        loop {
            let n = entry
                .read(&mut buf)
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
            // The ratio bound holds while the member streams, not only after.
            check_compression_ratio(&body, written, compressed)?;
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
/// Returns how many directories it created.
fn make_dirs(root: &Path, dirs: &str) -> Result<usize, ArchiveError> {
    let mut at = root.to_path_buf();
    let mut made = 0;
    for part in dirs.split('/') {
        at.push(part);
        match at.symlink_metadata() {
            Ok(m) if m.file_type().is_dir() => {}
            Ok(_) => {
                return Err(ArchiveError(format!(
                    "payload member path {dirs} runs through a file"
                )))
            }
            Err(_) => {
                make_dir(&at)?;
                made += 1;
            }
        }
    }
    Ok(made)
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
