//! The single writer of a seam M file.

use std::fs::{File, OpenOptions, Permissions};
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::atomic::{fence, Ordering};

use nix::fcntl::OFlag;

use super::reader::open_nonblocking;
use super::{file_len, offset, voxel_count, VoxmapError, VoxmapMeta, HEADER_LEN};

/// Publishes maps of one window size into a seam M file.
///
/// The staging buffer (header plus both planes) is allocated once at
/// [`create`](Self::create); [`publish`](Self::publish) fills it through the
/// caller's closure before the guard goes odd, so the window in which readers
/// see a publication in progress is only the write itself.
pub struct VoxmapWriter {
    file: File,
    dims: [u16; 3],
    seq: u64,
    staging: Vec<u8>,
}

impl VoxmapWriter {
    /// Create (or reopen) the file at `path` for windows of `dims`, mode
    /// `0640`, and when `group` is given hand it to that group (the writer
    /// must be a member).
    ///
    /// The file's directory must be this writer's own and writable by it
    /// alone (the plugin host hands [`super::VOXMAP_DIR`] to the one granted
    /// writer), so no other uid can have planted anything at the path. A
    /// file there owned by another user, with more than one link, or that is
    /// not a regular file (a FIFO is opened without blocking and refused) is
    /// refused: it was not created by this writer and cannot be trusted to
    /// carry only its data. Symlinks are not followed.
    pub fn create(path: &Path, dims: [u16; 3], group: Option<&str>) -> Result<Self, VoxmapError> {
        if dims.contains(&0) || voxel_count(dims) > super::MAX_VOXELS {
            return Err(VoxmapError::BadDims(dims));
        }
        let me = nix::unistd::geteuid().as_raw();
        let dir_path = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let dir = open_nonblocking(dir_path, OFlag::O_DIRECTORY)?.metadata()?;
        if dir.uid() != me || dir.mode() & 0o022 != 0 {
            return Err(VoxmapError::Untrusted(format!(
                "{} is not this writer's own directory (uid {}, mode {:o}): another user \
                 could plant a file there",
                dir_path.display(),
                dir.uid(),
                dir.mode() & 0o7777
            )));
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o640)
            .custom_flags((OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC).bits())
            .open(path)?;
        let md = file.metadata()?;
        if !md.file_type().is_file() {
            return Err(VoxmapError::Untrusted(format!(
                "{} is not a regular file",
                path.display()
            )));
        }
        if md.uid() != me {
            return Err(VoxmapError::Untrusted(format!(
                "{} is owned by uid {}, not this writer",
                path.display(),
                md.uid()
            )));
        }
        if md.nlink() != 1 {
            return Err(VoxmapError::Untrusted(format!(
                "{} has {} links, not one",
                path.display(),
                md.nlink()
            )));
        }
        if let Some(name) = group {
            let gid = super::reader::group_gid(name)?;
            std::os::unix::fs::fchown(&file, None, Some(gid))?;
        }
        file.set_permissions(Permissions::from_mode(0o640))?;

        // Continue the guard sequence of a previous writer of this file, so a
        // reader's "nothing new" check never mistakes a restart for old data.
        let mut head = [0u8; HEADER_LEN];
        let seq = match file.read_exact_at(&mut head, 0) {
            Ok(()) => VoxmapMeta::decode(&head).map_or(0, |(s, _)| (s + 1) & !1),
            Err(_) => 0,
        };
        let len = file_len(dims);
        file.set_len(len as u64)?;
        Ok(Self {
            file,
            dims,
            seq,
            staging: vec![0u8; len],
        })
    }

    /// The window size this writer publishes.
    pub fn dims(&self) -> [u16; 3] {
        self.dims
    }

    /// The guard value of the last publication (0 before the first).
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// Publish one map. `fill` writes the occupancy and ESDF planes (each
    /// `nx * ny * nz` bytes, x fastest); the buffers hold the previous
    /// publication's bytes on entry. Returns the new guard value.
    pub fn publish(
        &mut self,
        meta: &VoxmapMeta,
        fill: impl FnOnce(&mut [u8], &mut [u8]),
    ) -> Result<u64, VoxmapError> {
        if meta.dims != self.dims {
            return Err(VoxmapError::DimsMismatch {
                got: meta.dims,
                want: self.dims,
            });
        }
        let n = voxel_count(self.dims);
        let (head, planes) = self.staging.split_at_mut(HEADER_LEN);
        let (occ, esdf) = planes.split_at_mut(n);
        fill(occ, esdf);

        let odd = self.seq + 1;
        let even = self.seq + 2;
        let head: &mut [u8; HEADER_LEN] = head.try_into().expect("header slice");
        meta.encode(odd, head);

        // Guard odd, then the header and planes, then guard even. The fences
        // keep the kernel's copies of one call from being observed on the
        // wrong side of a guard store by a reader on another core.
        self.file
            .write_all_at(&odd.to_le_bytes(), offset::SEQ as u64)?;
        fence(Ordering::SeqCst);
        self.file.write_all_at(&self.staging, 0)?;
        fence(Ordering::SeqCst);
        self.file
            .write_all_at(&even.to_le_bytes(), offset::SEQ as u64)?;
        self.seq = even;
        Ok(even)
    }
}
