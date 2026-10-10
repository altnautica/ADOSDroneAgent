//! Batch persistence, the periodic WAL checkpoint, and the clean-shutdown close.
//!
//! One batch commits inside a single transaction (one fsync), with the session
//! transitions applied inline in frame order so a log emitted between an arm and
//! a disarm in the same batch is attributed to the flight session. The WAL is
//! truncated periodically to bound the `-wal` file, and a clean stop closes the
//! open sessions and truncates the WAL so the store replays clean.
//!
//! A batch that fails with `SQLITE_FULL` is not fatal on the first try: the
//! writer evicts the oldest batch of history (see [`super::reclaim`]) and retries
//! the whole batch once. Only a second failure reaches the run loop.

use ados_protocol::logd::IngestFrame;

use crate::retention;

use super::config::WriterError;
use super::encode::{insert_frame, now_us};
use super::session::{close_session, open_session, session_transition, SessionTransition};
use super::Writer;

impl Writer {
    /// Persist one batch inside a single transaction, redacting every frame
    /// before insert, then broadcast each persisted frame and run a periodic WAL
    /// truncate. A single bad row is logged and skipped; it never aborts the
    /// whole batch. A full disk is different: the batch is rolled back, one
    /// emergency eviction frees space, and the batch is retried exactly once.
    ///
    /// Session transitions are applied inline, in frame order, inside the same
    /// transaction: an arm event opens the flight session before the next frame
    /// is inserted, so a log emitted between arm and disarm in the same batch is
    /// correctly attributed to the flight session. Because the session row and
    /// the rows that reference it commit together, the foreign key holds.
    pub(super) fn commit_batch(&mut self, batch: &mut Vec<IngestFrame>) -> Result<(), WriterError> {
        if batch.is_empty() {
            return Ok(());
        }
        // No row is ever written to disk unredacted. Redaction runs exactly once,
        // before any attempt, so a retry stores the same flags: a second pass over
        // an already-redacted frame would report that nothing changed.
        let redacted: Vec<bool> = batch.iter_mut().map(|frame| frame.redact()).collect();
        let persisted = match self.persist_batch(batch, &redacted) {
            Err(e) if retention::is_disk_full(&e) => {
                self.emergency_reclaim(&e)?;
                self.persist_batch(batch, &redacted)?
            }
            other => other?,
        };

        // Fan the persisted frames out to any live-tail subscriber. A send with
        // no subscribers is a no-op; a lagging subscriber drops, never the writer.
        for frame in batch.drain(..) {
            let _ = self.broadcast_tx.send(frame);
        }

        self.frames_since_checkpoint += persisted;
        self.maybe_checkpoint()?;
        Ok(())
    }

    /// One attempt at the batch transaction. Returns how many frames were
    /// persisted. `redacted[i]` is the redaction flag for `batch[i]`.
    ///
    /// Session state is tracked locally and promoted to the writer only after
    /// the commit, so a failed attempt leaves nothing half-applied and a retry
    /// starts from the same state.
    fn persist_batch(
        &mut self,
        batch: &[IngestFrame],
        redacted: &[bool],
    ) -> Result<u64, rusqlite::Error> {
        let tx = self.conn.transaction()?;
        let mut persisted = 0u64;
        let mut flight = self.flight_session;
        let mut new_flight_log: Vec<(i64, &'static str)> = Vec::new();
        for (frame, &was_redacted) in batch.iter().zip(redacted) {
            // Apply an arm transition before inserting so this frame and the
            // ones after it in the batch are attributed to the flight session.
            match session_transition(frame) {
                Some(SessionTransition::Arm) if flight.is_none() => {
                    let id = open_session(&tx, now_us(), "flight", Some("arm"))?;
                    flight = Some(id);
                    new_flight_log.push((id, "opened"));
                }
                _ => {}
            }
            let session = flight.unwrap_or(self.boot_session);
            match insert_frame(&tx, frame, session, was_redacted) {
                Ok(()) => persisted += 1,
                // A full disk is not a bad row: abandon the transaction (the
                // drop rolls it back) so the caller can reclaim and retry.
                Err(e) if retention::is_disk_full(&e) => return Err(e),
                Err(e) => {
                    tracing::warn!(error = %e, "skipping a frame that failed to insert");
                }
            }
            // Apply a disarm transition after inserting so the disarm event
            // itself is still inside the flight session window.
            if let Some(SessionTransition::Disarm) = session_transition(frame) {
                if let Some(id) = flight.take() {
                    close_session(&tx, id, now_us(), "disarm")?;
                    new_flight_log.push((id, "closed"));
                }
            }
        }
        tx.commit()?;
        // The transaction is durable: promote the local session state and log.
        self.flight_session = flight;
        for (id, action) in new_flight_log {
            tracing::info!(session = id, "flight session {action}");
        }
        Ok(persisted)
    }

    /// Truncate the WAL once enough frames have been persisted since the last
    /// truncate, bounding the `-wal` file independently of `wal_autocheckpoint`.
    fn maybe_checkpoint(&mut self) -> Result<(), WriterError> {
        if self.frames_since_checkpoint >= self.config.checkpoint_interval_frames {
            self.frames_since_checkpoint = 0;
            // TRUNCATE checkpoints back into the main file and shrinks the WAL.
            self.conn
                .pragma_update(None, "wal_checkpoint", "TRUNCATE")?;
        }
        Ok(())
    }

    /// Close the open flight (if any) and the boot session, then truncate the
    /// WAL so a clean stop leaves a small, replayed-clean store.
    pub(super) fn shutdown(&mut self) -> Result<(), WriterError> {
        let ts = now_us();
        if let Some(id) = self.flight_session.take() {
            close_session(&self.conn, id, ts, "shutdown")?;
        }
        close_session(&self.conn, self.boot_session, ts, "shutdown")?;
        self.conn
            .pragma_update(None, "wal_checkpoint", "TRUNCATE")?;
        tracing::info!(path = %self.db_path.display(), "store checkpointed and closed");
        Ok(())
    }
}
