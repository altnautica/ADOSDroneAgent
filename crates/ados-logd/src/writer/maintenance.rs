//! Retention maintenance folded into the writer loop.
//!
//! The writer runs retention on its own thread against its own connection —
//! there is never a second read-write connection. A pass rolls up closed metric
//! windows, TTL-deletes aged rows, evicts oldest-first over the size cap, and
//! vacuums on cadence. When it evicts, a `retention.evicted` event is written to
//! the store and fanned out to any live tail so the pruning is observable.

use std::sync::atomic::Ordering;
use std::time::Instant;

use ados_protocol::logd::{EventFrame, IngestFrame, Level};

use crate::retention::{self, MaintenanceReport};

use super::config::WriterError;
use super::encode::{insert_frame, now_us};
use super::Writer;

impl Writer {
    /// Run a retention maintenance pass if its deadline has arrived, then advance
    /// the next-maintenance deadline. A pass rolls up closed metric windows,
    /// TTL-deletes aged raw and rollup rows, evicts oldest-first if the store is
    /// over its size cap, and vacuums on the vacuum cadence (or after an
    /// eviction). When the pass evicts rows it records a `retention.evicted`
    /// event so the operator and the read API see that history was pruned.
    pub(super) fn maybe_run_maintenance(&mut self) -> Result<(), WriterError> {
        // Do not start a pass once shutdown is in flight: the daemon is draining
        // the channel toward a bounded join, and the pass's `VACUUM` could overrun
        // it. The next pass after a clean start reclaims any deferred space.
        if self.stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        let now = Instant::now();
        if now < self.next_maintenance {
            return Ok(());
        }
        self.next_maintenance = now + self.config.retention.maintenance_interval;

        let do_vacuum = now >= self.next_vacuum;
        let report = retention::run_maintenance(
            &self.conn,
            &self.config.retention,
            now_us(),
            &self.db_path,
            do_vacuum,
            &self.stop,
        )?;
        // A pass that vacuumed for any reason (the cadence, or post-eviction)
        // resets the vacuum cadence so the next periodic vacuum is a full
        // interval away.
        if report.vacuumed {
            self.next_vacuum = now + self.config.retention.vacuum_interval;
        }
        self.record_maintenance(&report)?;
        Ok(())
    }

    /// Record the outcome of a maintenance pass: log a one-line summary when it
    /// did anything, and on an eviction write a `retention.evicted` event into
    /// the store and fan it out to any live-tail subscriber, so the pruning is
    /// visible both in the durable store and on the live stream.
    fn record_maintenance(&mut self, report: &MaintenanceReport) -> Result<(), WriterError> {
        if report.rolled_up_rows > 0
            || report.ttl_deleted_rows > 0
            || report.rollup_ttl_deleted_rows > 0
            || report.had_eviction()
        {
            tracing::info!(
                rolled_up = report.rolled_up_rows,
                ttl_deleted = report.ttl_deleted_rows,
                rollup_ttl_deleted = report.rollup_ttl_deleted_rows,
                evicted = report.evicted_rows,
                vacuumed = report.vacuumed,
                "retention maintenance pass"
            );
        }
        if report.had_eviction() {
            let event = self.eviction_event(report);
            // The writer owns the connection, so the event row goes in directly
            // under the current session, with no second connection. A bad insert
            // is logged and skipped rather than aborting the writer.
            let session = self.flight_session.unwrap_or(self.boot_session);
            if let Err(e) = insert_frame(&self.conn, &event, session, false) {
                tracing::warn!(error = %e, "failed to record the retention eviction event");
            }
            // Fan the same event out to any live-tail subscriber.
            let _ = self.broadcast_tx.send(event);
        }
        Ok(())
    }

    /// Build the `retention.evicted` event describing what the size cap freed:
    /// the row count and the timestamp span of the evicted rows.
    fn eviction_event(&self, report: &MaintenanceReport) -> IngestFrame {
        let mut ev = EventFrame::new(now_us(), "retention.evicted", "ados-logd", Level::Warn);
        ev.detail
            .insert("rows".to_string(), rmpv::Value::from(report.evicted_rows));
        if let Some(from) = report.evicted_from_us {
            ev.detail
                .insert("from_us".to_string(), rmpv::Value::from(from));
        }
        if let Some(to) = report.evicted_to_us {
            ev.detail.insert("to_us".to_string(), rmpv::Value::from(to));
        }
        IngestFrame::Event(ev)
    }
}
