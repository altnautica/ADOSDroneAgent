//! The runtime's reader of the seam M file.

use std::path::PathBuf;

use ados_protocol::voxmap::{ReadStatus, VoxSnapshot, VoxmapReader, VOXMAP_PATH};

/// The pace of reopening a missing or silent map file, ms: a Navigation
/// restart that recreated the file is picked up, and a vehicle without a
/// map pays one failed open a second.
pub const REOPEN_MS: u64 = 1_000;

/// Reads publications out of the seam M file into one reused buffer.
pub struct VoxSource {
    path: PathBuf,
    /// Open the file only when its writer is trusted
    /// ([`VoxmapReader::open_trusted`]); unchecked (tests and simulation
    /// only) otherwise.
    trusted: bool,
    reader: Option<VoxmapReader>,
    buf: VoxSnapshot,
    /// When the file was last opened (or tried), and when its `seq` last
    /// advanced, ms.
    opened_ms: Option<u64>,
    advanced_ms: Option<u64>,
}

impl Default for VoxSource {
    /// The map Navigation publishes, opened only from a trusted writer.
    fn default() -> Self {
        Self::new(PathBuf::from(VOXMAP_PATH), true)
    }
}

impl VoxSource {
    pub fn new(path: PathBuf, trusted: bool) -> Self {
        Self {
            path,
            trusted,
            reader: None,
            buf: VoxSnapshot::default(),
            opened_ms: None,
            advanced_ms: None,
        }
    }

    /// Poll at `now_ms`: the new publication, when one arrived since the
    /// last poll. A missing, refused or broken file is retried every
    /// [`REOPEN_MS`].
    pub fn read(&mut self, now_ms: u64) -> Option<&VoxSnapshot> {
        let due = |t: Option<u64>| t.is_none_or(|t| now_ms.saturating_sub(t) >= REOPEN_MS);
        if due(self.advanced_ms.or(self.opened_ms)) && due(self.opened_ms) {
            self.opened_ms = Some(now_ms);
            let opened = if self.trusted {
                VoxmapReader::open_trusted(&self.path)
            } else {
                VoxmapReader::open(&self.path)
            };
            self.reader = match opened {
                Ok(r) => Some(r),
                Err(e) => {
                    tracing::debug!(target: "ados_pilot", error = %e, "pilot_voxmap_open_failed");
                    None
                }
            };
        }
        match self.reader.as_mut()?.read_into(&mut self.buf) {
            Ok(ReadStatus::Fresh) => {
                self.advanced_ms = Some(now_ms);
                Some(&self.buf)
            }
            Ok(ReadStatus::Stale | ReadStatus::Torn) => None,
            Err(e) => {
                tracing::debug!(target: "ados_pilot", error = %e, "pilot_voxmap_read_failed");
                self.reader = None;
                None
            }
        }
    }
}
