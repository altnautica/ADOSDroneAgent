//! Reading the seam M file: [`VoxSource`] copies new publications out of it,
//! and [`VoxFeed`] runs one on its own thread so the control loop only
//! swaps buffers.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use ados_protocol::voxmap::{ReadStatus, VoxSnapshot, VoxmapReader, VOXMAP_PATH};

/// The pace of reopening a missing or silent map file, ms: a Navigation
/// restart that recreated the file is picked up, and a vehicle without a
/// map pays one failed open a second.
pub const REOPEN_MS: u64 = 1_000;

/// How often the reader thread looks for a new publication, ms: a 10 Hz map
/// is handed on within a control tick of its publication.
pub const POLL_MS: u64 = 10;

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
    /// last poll. Both planes are copied (the distance plane serves
    /// [`super::VoxWorld::clearance`]). A missing, refused or broken file is
    /// retried every [`REOPEN_MS`].
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

    /// As [`VoxSource::read`], handing a new publication out by swapping it
    /// with `spare` (`true`); `spare`'s buffers are read into next time.
    pub fn take_fresh(&mut self, now_ms: u64, spare: &mut VoxSnapshot) -> bool {
        if self.read(now_ms).is_none() {
            return false;
        }
        std::mem::swap(&mut self.buf, spare);
        // The reader tells an unchanged file by the guard the buffer last
        // read: keep it naming the publication just handed out, so the same
        // one is not copied again.
        self.buf.valid = true;
        self.buf.seq = spare.seq;
        true
    }
}

/// The newest publication not yet taken, and a used buffer to read into.
#[derive(Default)]
struct Slot {
    fresh: Option<VoxSnapshot>,
    spare: Option<VoxSnapshot>,
}

/// A [`VoxSource`] polled on its own thread every [`POLL_MS`]. The control
/// loop takes the newest publication ([`VoxFeed::take`]) and gives the
/// buffers it no longer needs back ([`VoxFeed::recycle`]); both only move
/// pointers under the lock. The thread ends when the feed is dropped.
pub struct VoxFeed {
    slot: Arc<Mutex<Slot>>,
}

impl VoxFeed {
    /// Start polling `source` on a thread named `pilot-voxmap`.
    pub fn spawn(mut source: VoxSource) -> std::io::Result<Self> {
        let slot = Arc::new(Mutex::new(Slot::default()));
        let shared = Arc::clone(&slot);
        std::thread::Builder::new()
            .name("pilot-voxmap".into())
            .spawn(move || {
                // Reopen pacing only: counted in polls, not read off a clock.
                let mut polls = 0u64;
                while Arc::strong_count(&shared) > 1 {
                    std::thread::sleep(Duration::from_millis(POLL_MS));
                    polls += 1;
                    let mut spare = lock(&shared).spare.take().unwrap_or_default();
                    if source.take_fresh(polls * POLL_MS, &mut spare) {
                        let mut s = lock(&shared);
                        // An untaken older map becomes the next spare.
                        if let Some(old) = s.fresh.replace(spare) {
                            s.spare = Some(old);
                        }
                    } else {
                        lock(&shared).spare.get_or_insert(spare);
                    }
                }
            })?;
        Ok(Self { slot })
    }

    /// The newest publication since the last take, if any.
    pub fn take(&self) -> Option<VoxSnapshot> {
        lock(&self.slot).fresh.take()
    }

    /// Hand back a buffer for the reader to fill.
    pub fn recycle(&self, buf: VoxSnapshot) {
        lock(&self.slot).spare = Some(buf);
    }
}

fn lock(slot: &Mutex<Slot>) -> std::sync::MutexGuard<'_, Slot> {
    slot.lock().unwrap_or_else(PoisonError::into_inner)
}
