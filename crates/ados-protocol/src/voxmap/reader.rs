//! A reader of a seam M file.

use std::fs::{File, OpenOptions};
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::sync::atomic::{fence, Ordering};

use super::{offset, VoxSnapshot, VoxmapError, VoxmapMeta, HEADER_LEN};

/// Copies attempted per [`VoxmapReader::read_into`] before it reports
/// [`ReadStatus::Torn`]. A publication takes about a millisecond at 10 Hz, so
/// two overlaps in a row are already rare.
pub const READ_ATTEMPTS: usize = 4;

/// What one [`VoxmapReader::read_into`] did to the snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadStatus {
    /// A new publication was copied whole; the snapshot is valid.
    Fresh,
    /// Every attempt overlapped a publication; the snapshot is invalidated.
    Torn,
    /// Nothing new (or nothing published yet); the snapshot is untouched.
    Stale,
}

/// Reads publications out of a seam M file.
pub struct VoxmapReader {
    file: File,
    last_seq: u64,
    head: [u8; HEADER_LEN],
}

impl VoxmapReader {
    /// Open the file at `path` read-only without checking who wrote it (tests,
    /// simulation, a process that created the file itself). Symlinks are not
    /// followed.
    pub fn open(path: &Path) -> Result<Self, VoxmapError> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(nix::fcntl::OFlag::O_NOFOLLOW.bits())
            .open(path)?;
        if !file.metadata()?.file_type().is_file() {
            return Err(VoxmapError::Untrusted(format!(
                "{} is not a regular file",
                path.display()
            )));
        }
        Ok(Self {
            file,
            last_seq: 0,
            head: [0u8; HEADER_LEN],
        })
    }

    /// Open the file at `path` and accept it only when its group is `group`
    /// and neither group nor other may write it. Only a member of `group` (or
    /// root) can give a file that group, so this refuses a file another
    /// process squatted at the path. The check runs on the opened descriptor.
    pub fn open_trusted(path: &Path, group: &str) -> Result<Self, VoxmapError> {
        let gid = group_gid(group)?;
        let reader = Self::open(path)?;
        let md = reader.file.metadata()?;
        if md.gid() != gid {
            return Err(VoxmapError::Untrusted(format!(
                "{} has gid {}, not group {group}",
                path.display(),
                md.gid()
            )));
        }
        if md.mode() & 0o022 != 0 {
            return Err(VoxmapError::Untrusted(format!(
                "{} is writable beyond its owner (mode {:o})",
                path.display(),
                md.mode() & 0o777
            )));
        }
        Ok(reader)
    }

    /// Copy the newest publication into `snap` when there is one.
    ///
    /// Retries a copy that overlapped a publication up to [`READ_ATTEMPTS`]
    /// times. A header that is present but malformed, or a file shorter than
    /// its header declares, is an error; an all-zero header (created, never
    /// published) is [`ReadStatus::Stale`].
    pub fn read_into(&mut self, snap: &mut VoxSnapshot) -> Result<ReadStatus, VoxmapError> {
        for _ in 0..READ_ATTEMPTS {
            let seq1 = self.load_seq()?;
            if seq1 & 1 == 1 {
                std::thread::yield_now();
                continue;
            }
            if seq1 == self.last_seq && ((snap.valid && snap.seq == seq1) || seq1 == 0) {
                return Ok(ReadStatus::Stale);
            }
            fence(Ordering::SeqCst);
            self.file.read_exact_at(&mut self.head, 0)?;
            if self.head[offset::MAGIC..offset::MAGIC + 4] == [0; 4] {
                return Ok(ReadStatus::Stale);
            }
            let meta = match VoxmapMeta::decode(&self.head) {
                Ok((s, meta)) if s == seq1 => meta,
                // The header moved under the guard load: a publication began.
                Ok(_) => continue,
                Err(e) => {
                    if self.load_seq()? != seq1 {
                        continue;
                    }
                    return Err(e);
                }
            };
            let n = meta.voxel_count();
            let need = (HEADER_LEN + 2 * n) as u64;
            let got = self.file.metadata()?.len();
            if got < need {
                return Err(VoxmapError::Truncated { got, need });
            }
            snap.valid = false;
            snap.occupancy.resize(n, 0);
            snap.esdf_dm.resize(n, 0);
            self.file
                .read_exact_at(&mut snap.occupancy, HEADER_LEN as u64)?;
            self.file
                .read_exact_at(&mut snap.esdf_dm, (HEADER_LEN + n) as u64)?;
            fence(Ordering::SeqCst);
            if self.load_seq()? != seq1 {
                continue;
            }
            snap.valid = true;
            snap.seq = seq1;
            snap.meta = meta;
            self.last_seq = seq1;
            return Ok(ReadStatus::Fresh);
        }
        snap.valid = false;
        Ok(ReadStatus::Torn)
    }

    fn load_seq(&self) -> Result<u64, VoxmapError> {
        let mut b = [0u8; 8];
        match self.file.read_exact_at(&mut b, offset::SEQ as u64) {
            Ok(()) => Ok(u64::from_le_bytes(b)),
            // Created but not yet sized: nothing published.
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(0),
            Err(e) => Err(e.into()),
        }
    }
}

/// The gid of group `name`.
pub(super) fn group_gid(name: &str) -> Result<u32, VoxmapError> {
    match nix::unistd::Group::from_name(name) {
        Ok(Some(g)) => Ok(g.gid.as_raw()),
        _ => Err(VoxmapError::NoSuchGroup(name.to_string())),
    }
}
