//! `nav.voxmap.v1` (seam M): the 3D voxel map Navigation publishes to the
//! pilot through one shared-memory file.
//!
//! The map is a robot-centred window of voxels in Navigation's map frame
//! (north, east, down, metres). The file at [`VOXMAP_SHM_PATH`] holds a fixed
//! [`HEADER_LEN`]-byte header followed by two byte planes of `nx * ny * nz`
//! entries each, x fastest (`index = x + nx * (y + ny * z)`, window-relative):
//!
//! ```text
//! off  size  field
//!   0     4  magic "ADVX"
//!   4     2  version (u16, 1)
//!   6     2  reserved (0)
//!   8     8  seq (u64; odd while the writer is mid-publish)
//!  16     8  stamp_ns (u64, ADOS time of the newest data in the map)
//!  24     4  res_m (f32, voxel edge)
//!  28    12  origin_cell (i32[3], absolute index of window voxel (0, 0, 0))
//!  40     6  dims (u16[3], nx, ny, nz)
//!  46     2  reserved (0)
//!  48    28  map_from_body (f32[7]: t north, east, down; q w, x, y, z)
//!  76     1  tier (u8, see [`VoxTier`])
//!  77     3  reserved (0)
//!  80     N  occupancy (u8: 0 unknown, 1 free, 2 occupied)
//! 80+N    N  esdf_dm (u8: distance to the nearest obstacle voxel, decimetres,
//!            capped at [`ESDF_CAP_DM`]; 0 on an obstacle voxel. An obstacle
//!            voxel is occupied, or was and still holds positive evidence,
//!            so it can read unknown in the occupancy plane)
//! ```
//!
//! All integers and floats are little-endian. Absolute voxel `(i, j, k)` spans
//! `[i * res_m, (i + 1) * res_m)` on each axis.
//!
//! # Seqlock over plain file I/O
//!
//! One writer, any number of readers, no locks. The writer stores an odd
//! `seq`, then the header and both planes, then the next even `seq`. A reader
//! loads `seq`, copies the header and planes, loads `seq` again, and keeps the
//! copy only when both loads are the same even value; otherwise it retries a
//! bounded number of times ([`READ_ATTEMPTS`]).
//!
//! The copies are positioned reads and writes (`pread`/`pwrite`) on the tmpfs
//! file rather than a memory map, so this crate needs no mapping dependency;
//! each side copies the planes once either way. A `SeqCst` fence between the
//! system calls orders the kernel's copies of one process against the guard
//! loads and stores, which is what the seqlock needs on a weakly-ordered CPU.
//!
//! # Who may write
//!
//! The writer creates the file `0640` and hands it to [`VOXMAP_GROUP`]; a
//! plugin joins that group only while it holds the `shm.voxmap` capability.
//! [`VoxmapReader::open_trusted`] accepts the file only when its group is
//! [`VOXMAP_GROUP`] and neither group nor other may write it: a process
//! outside the group cannot give a file that group, so a file squatted at the
//! path by any other plugin is refused rather than read as the map.

mod reader;
mod writer;

#[cfg(test)]
mod tests;

pub use reader::{ReadStatus, VoxmapReader, READ_ATTEMPTS};
pub use writer::VoxmapWriter;

/// Registry version of the `nav.voxmap.v1` contract.
pub const VOXMAP_V1_VERSION: u16 = match crate::contracts::contract_version("nav.voxmap.v1") {
    Some(v) => v,
    None => panic!("nav.voxmap.v1 is registered in contracts.toml"),
};

/// Where Navigation publishes the map.
pub const VOXMAP_SHM_PATH: &str = "/dev/shm/ados-voxmap-v1";

/// The group a writer hands the file to; a plugin joins it only while it holds
/// the `shm.voxmap` capability.
pub const VOXMAP_GROUP: &str = "ados-voxmap";

/// The first four bytes of a published map.
pub const VOXMAP_MAGIC: [u8; 4] = *b"ADVX";

/// Header length; the occupancy plane starts here.
pub const HEADER_LEN: usize = 80;

/// Byte offsets of the header fields.
pub mod offset {
    pub const MAGIC: usize = 0;
    pub const VERSION: usize = 4;
    pub const SEQ: usize = 8;
    pub const STAMP_NS: usize = 16;
    pub const RES_M: usize = 24;
    pub const ORIGIN_CELL: usize = 28;
    pub const DIMS: usize = 40;
    pub const MAP_FROM_BODY: usize = 48;
    pub const TIER: usize = 76;
}

/// Occupancy plane value: never observed, or evidence inconclusive.
pub const VOX_UNKNOWN: u8 = 0;
/// Occupancy plane value: observed free.
pub const VOX_FREE: u8 = 1;
/// Occupancy plane value: observed occupied.
pub const VOX_OCCUPIED: u8 = 2;

/// The largest `esdf_dm` value: 4.0 m, and every voxel farther than that from
/// the nearest occupied voxel.
pub const ESDF_CAP_DM: u8 = 40;

/// The most voxels a header may declare (both planes together stay under
/// 64 MiB); a larger declaration is refused as corrupt.
pub const MAX_VOXELS: usize = 1 << 25;

/// The sensing tier whose data built the map.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum VoxTier {
    #[default]
    Unknown = 0,
    /// 3D lidar.
    Lidar3d = 1,
    /// Stereo or time-of-flight depth camera.
    Depth = 2,
    /// Learned depth from a camera.
    Camera = 3,
    /// Planar lidar placed in 3D from attitude.
    Planar = 4,
}

impl VoxTier {
    /// The tier for a header byte; an unassigned value reads as `Unknown`.
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => VoxTier::Lidar3d,
            2 => VoxTier::Depth,
            3 => VoxTier::Camera,
            4 => VoxTier::Planar,
            _ => VoxTier::Unknown,
        }
    }
}

/// Every header field but the seqlock guard.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct VoxmapMeta {
    /// ADOS time of the newest data in the map, nanoseconds.
    pub stamp_ns: u64,
    /// Voxel edge, metres.
    pub res_m: f32,
    /// Absolute index of window voxel (0, 0, 0).
    pub origin_cell: [i32; 3],
    /// Window size in voxels (nx, ny, nz).
    pub dims: [u16; 3],
    /// Body pose in the map frame: translation north, east, down, then the
    /// unit quaternion w, x, y, z.
    pub map_from_body: [f32; 7],
    pub tier: VoxTier,
}

impl VoxmapMeta {
    /// Voxels in one plane.
    pub fn voxel_count(&self) -> usize {
        voxel_count(self.dims)
    }

    /// Encode the header with guard value `seq`.
    pub fn encode(&self, seq: u64, out: &mut [u8; HEADER_LEN]) {
        out.fill(0);
        out[offset::MAGIC..offset::MAGIC + 4].copy_from_slice(&VOXMAP_MAGIC);
        put(out, offset::VERSION, &VOXMAP_V1_VERSION.to_le_bytes());
        put(out, offset::SEQ, &seq.to_le_bytes());
        put(out, offset::STAMP_NS, &self.stamp_ns.to_le_bytes());
        put(out, offset::RES_M, &self.res_m.to_le_bytes());
        for (i, c) in self.origin_cell.iter().enumerate() {
            put(out, offset::ORIGIN_CELL + 4 * i, &c.to_le_bytes());
        }
        for (i, d) in self.dims.iter().enumerate() {
            put(out, offset::DIMS + 2 * i, &d.to_le_bytes());
        }
        for (i, v) in self.map_from_body.iter().enumerate() {
            put(out, offset::MAP_FROM_BODY + 4 * i, &v.to_le_bytes());
        }
        out[offset::TIER] = self.tier as u8;
    }

    /// Decode a header into its guard value and fields.
    pub fn decode(buf: &[u8; HEADER_LEN]) -> Result<(u64, VoxmapMeta), VoxmapError> {
        if buf[offset::MAGIC..offset::MAGIC + 4] != VOXMAP_MAGIC {
            return Err(VoxmapError::BadMagic);
        }
        let version = u16::from_le_bytes(take(buf, offset::VERSION));
        if version != VOXMAP_V1_VERSION {
            return Err(VoxmapError::BadVersion(version));
        }
        let seq = u64::from_le_bytes(take(buf, offset::SEQ));
        let dims = [0, 1, 2].map(|i| u16::from_le_bytes(take(buf, offset::DIMS + 2 * i)));
        if dims.contains(&0) || voxel_count(dims) > MAX_VOXELS {
            return Err(VoxmapError::BadDims(dims));
        }
        let meta = VoxmapMeta {
            stamp_ns: u64::from_le_bytes(take(buf, offset::STAMP_NS)),
            res_m: f32::from_le_bytes(take(buf, offset::RES_M)),
            origin_cell: [0, 1, 2]
                .map(|i| i32::from_le_bytes(take(buf, offset::ORIGIN_CELL + 4 * i))),
            dims,
            map_from_body: [0, 1, 2, 3, 4, 5, 6]
                .map(|i| f32::from_le_bytes(take(buf, offset::MAP_FROM_BODY + 4 * i))),
            tier: VoxTier::from_u8(buf[offset::TIER]),
        };
        Ok((seq, meta))
    }
}

/// Voxels in one plane of a window of `dims`.
pub fn voxel_count(dims: [u16; 3]) -> usize {
    dims.iter().map(|&d| d as usize).product()
}

/// File length for a window of `dims`: header plus both planes.
pub fn file_len(dims: [u16; 3]) -> usize {
    HEADER_LEN + 2 * voxel_count(dims)
}

fn put(out: &mut [u8; HEADER_LEN], off: usize, bytes: &[u8]) {
    out[off..off + bytes.len()].copy_from_slice(bytes);
}

fn take<const N: usize>(buf: &[u8; HEADER_LEN], off: usize) -> [u8; N] {
    let mut b = [0u8; N];
    b.copy_from_slice(&buf[off..off + N]);
    b
}

/// A reader's copy of one published map. Its buffers are reused across reads
/// and only reallocated when the window size changes.
#[derive(Debug, Clone, Default)]
pub struct VoxSnapshot {
    /// True only after a [`ReadStatus::Fresh`] read; a torn read clears it,
    /// because the planes were overwritten mid-copy.
    pub valid: bool,
    /// The guard value of the copied publication.
    pub seq: u64,
    pub meta: VoxmapMeta,
    pub occupancy: Vec<u8>,
    pub esdf_dm: Vec<u8>,
}

impl VoxSnapshot {
    /// Plane index of window-relative voxel `w`, or `None` outside the window.
    pub fn index(&self, w: [i64; 3]) -> Option<usize> {
        let d = self.meta.dims.map(i64::from);
        if (0..3).any(|a| w[a] < 0 || w[a] >= d[a]) {
            return None;
        }
        Some((w[0] + d[0] * (w[1] + d[1] * w[2])) as usize)
    }

    /// Plane index of the voxel holding map point `p` (north, east, down).
    pub fn index_of_point(&self, p: [f32; 3]) -> Option<usize> {
        let res = self.meta.res_m;
        if !self.valid || res <= 0.0 {
            return None;
        }
        let w =
            [0, 1, 2].map(|a| (p[a] / res).floor() as i64 - i64::from(self.meta.origin_cell[a]));
        self.index(w)
    }

    /// Age of the map at ADOS time `now_ns`; zero when the stamp is ahead.
    pub fn age_ns(&self, now_ns: u64) -> u64 {
        now_ns.saturating_sub(self.meta.stamp_ns)
    }
}

/// A seam M failure.
#[derive(Debug, thiserror::Error)]
pub enum VoxmapError {
    #[error("voxmap I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("voxmap header magic is not ADVX")]
    BadMagic,
    #[error("voxmap header version {0} is not supported")]
    BadVersion(u16),
    #[error("voxmap dims {0:?} are empty or too large")]
    BadDims([u16; 3]),
    #[error("voxmap file is {got} bytes, the header declares {need}")]
    Truncated { got: u64, need: u64 },
    #[error("voxmap publish dims {got:?} differ from the writer's {want:?}")]
    DimsMismatch { got: [u16; 3], want: [u16; 3] },
    #[error("voxmap file is not trusted: {0}")]
    Untrusted(String),
    #[error("group {0} does not exist")]
    NoSuchGroup(String),
}
