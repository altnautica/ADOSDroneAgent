//! A reader of a seam M file.

use std::fs::{File, Metadata, OpenOptions};
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::sync::atomic::{fence, Ordering};

use nix::fcntl::OFlag;

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

/// Open `path` read-only without following a symlink at its last component
/// and without blocking on a FIFO or device there.
pub(super) fn open_nonblocking(path: &Path, extra: OFlag) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags((OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC | extra).bits())
        .open(path)
}

/// The fields of an inode the trust check reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Inode {
    pub uid: u32,
    pub mode: u32,
    pub nlink: u64,
    pub is_file: bool,
    pub is_dir: bool,
}

impl From<&Metadata> for Inode {
    fn from(md: &Metadata) -> Self {
        Self {
            uid: md.uid(),
            mode: md.mode(),
            nlink: md.nlink(),
            is_file: md.file_type().is_file(),
            is_dir: md.file_type().is_dir(),
        }
    }
}

/// Group or other may write.
const SHARED_WRITE: u32 = 0o022;

/// Whether a map file may be read as the map, given its directory's parent
/// (`base`), its directory (`dir`) and itself (`file`), for a reader running
/// as `me`. The directory's owner is the writer root recorded; see the module
/// documentation.
pub(super) fn trust(base: Inode, dir: Inode, file: Inode, me: u32) -> Result<(), String> {
    if !base.is_dir || (base.uid != 0 && base.uid != me) || base.mode & SHARED_WRITE != 0 {
        return Err(format!(
            "its directory's parent is not root's alone (uid {}, mode {:o})",
            base.uid,
            base.mode & 0o7777
        ));
    }
    if !dir.is_dir || dir.mode & SHARED_WRITE != 0 {
        return Err(format!(
            "its directory is writable beyond its owner (mode {:o})",
            dir.mode & 0o7777
        ));
    }
    if !file.is_file {
        return Err("it is not a regular file".to_string());
    }
    if file.uid != dir.uid {
        return Err(format!(
            "it is owned by uid {}, not the directory's writer uid {}",
            file.uid, dir.uid
        ));
    }
    if file.nlink != 1 {
        return Err(format!("it has {} links, not one", file.nlink));
    }
    if file.mode & SHARED_WRITE != 0 {
        return Err(format!(
            "it is writable beyond its owner (mode {:o})",
            file.mode & 0o777
        ));
    }
    Ok(())
}

fn untrusted(path: &Path, why: impl std::fmt::Display) -> VoxmapError {
    VoxmapError::Untrusted(format!("{}: {why}", path.display()))
}

impl VoxmapReader {
    /// Open the file at `path` read-only without checking who wrote it (tests,
    /// simulation, a process that created the file itself). A symlink, FIFO or
    /// anything else but a regular file is refused without blocking.
    pub fn open(path: &Path) -> Result<Self, VoxmapError> {
        let file = open_nonblocking(path, OFlag::empty())?;
        if !file.metadata()?.file_type().is_file() {
            return Err(untrusted(path, "it is not a regular file"));
        }
        Ok(Self {
            file,
            last_seq: 0,
            head: [0u8; HEADER_LEN],
        })
    }

    /// Open the file at `path` and accept it only from the writer its
    /// directory records: the directory's parent is root's (or this
    /// process's) and writable by its owner only, the directory is writable
    /// by its owner only, and the file is a regular file owned by the
    /// directory's owner, with one link, writable by its owner only. The
    /// directories are opened without following a symlink and the file's
    /// checks run on its opened descriptor, before any read.
    pub fn open_trusted(path: &Path) -> Result<Self, VoxmapError> {
        let dir_path = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .ok_or_else(|| untrusted(path, "it has no directory"))?;
        let base_path = dir_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .ok_or_else(|| untrusted(path, "its directory has no parent"))?;
        let base = open_nonblocking(base_path, OFlag::O_DIRECTORY)?.metadata()?;
        let dir = open_nonblocking(dir_path, OFlag::O_DIRECTORY)?.metadata()?;
        let reader = Self::open(path)?;
        let md = reader.file.metadata()?;
        let me = nix::unistd::geteuid().as_raw();
        trust((&base).into(), (&dir).into(), (&md).into(), me)
            .map_err(|why| untrusted(path, why))?;
        Ok(reader)
    }

    /// Copy the newest publication, both planes, into `snap` when there is
    /// one.
    ///
    /// Retries a copy that overlapped a publication up to [`READ_ATTEMPTS`]
    /// times. A header that is present but malformed, or a file shorter than
    /// its header declares, is an error; an all-zero header (created, never
    /// published) is [`ReadStatus::Stale`].
    pub fn read_into(&mut self, snap: &mut VoxSnapshot) -> Result<ReadStatus, VoxmapError> {
        self.read(snap, true)
    }

    /// [`read_into`](Self::read_into) for a reader that needs only the
    /// occupancy plane: the distance plane is not copied and `snap.esdf_dm`
    /// is left empty.
    pub fn read_occupancy_into(
        &mut self,
        snap: &mut VoxSnapshot,
    ) -> Result<ReadStatus, VoxmapError> {
        self.read(snap, false)
    }

    fn read(&mut self, snap: &mut VoxSnapshot, esdf: bool) -> Result<ReadStatus, VoxmapError> {
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
            self.file
                .read_exact_at(&mut snap.occupancy, HEADER_LEN as u64)?;
            if esdf {
                snap.esdf_dm.resize(n, 0);
                self.file
                    .read_exact_at(&mut snap.esdf_dm, (HEADER_LEN + n) as u64)?;
            } else {
                snap.esdf_dm.clear();
            }
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
