//! The writer's emergency response to a full disk.
//!
//! A batch commit that fails with `SQLITE_FULL` used to end the writer, and with
//! it the Black Box, at exactly the moment the box was in trouble. Instead the
//! writer evicts the single oldest batch of raw history, hands the freed pages
//! back to the filesystem, and lets [`Writer::commit_batch`] retry the batch
//! once. The periodic retention pass does the thorough work on its own cadence;
//! this is only enough to keep recording.

use crate::retention;

use super::config::WriterError;
use super::Writer;

impl Writer {
    /// Evict one oldest-first batch and reclaim its pages after `cause` (an
    /// `SQLITE_FULL`) failed a batch. A failure here propagates: there is
    /// nothing further to free inline.
    pub(super) fn emergency_reclaim(&mut self, cause: &rusqlite::Error) -> Result<(), WriterError> {
        self.mark_progress();
        let freed = retention::emergency_evict(&self.conn)?;
        self.emergency_evictions += 1;
        tracing::warn!(
            error = %cause,
            evicted_rows = freed.rows,
            evicted_from_us = ?freed.from_us,
            evicted_to_us = ?freed.to_us,
            reclaimed_pages = freed.reclaimed_pages,
            emergency_evictions = self.emergency_evictions,
            "store write hit a full disk; evicted the oldest history, retrying once"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests;
