//! The out-of-band control plane: mark-synced requests from the read surface.
//!
//! The single-writer invariant lives here: the writer thread owns the only
//! read-write connection, so the one place a row flips from unsynced to synced
//! is on this thread, on this connection, reached through this channel. The read
//! surface opens the store read-only and never mutates it; it enqueues a
//! [`ControlMsg`] and awaits the [`oneshot`] reply rather than writing the store
//! itself.

use std::collections::BTreeMap;

use rusqlite::Connection;
use tokio::sync::{mpsc, oneshot};

use ados_protocol::logd::{EventFrame, IngestFrame, Level, SyncRequest, SyncTable};

use super::encode::{insert_frame, now_us};
use super::Writer;

/// An out-of-band control message the writer services between ingest batches.
///
/// The single-writer invariant lives here: this thread owns the only read-write
/// connection, so the one place a row flips from unsynced to synced is on this
/// thread, on this connection, reached through this channel. The read surface
/// opens the store read-only and never mutates it; it enqueues a [`ControlMsg`]
/// and awaits the [`oneshot`] reply rather than writing the store itself.
pub enum ControlMsg {
    /// Flip the rows in the request's window from unsynced to synced, reply with
    /// the per-table flipped count and the remaining unsynced count.
    MarkSynced {
        /// The window selector to mark.
        req: SyncRequest,
        /// The reply channel; dropping it without a send signals the caller to
        /// surface a service-unavailable error.
        ack: oneshot::Sender<MarkResult>,
    },
}

/// The outcome of a [`ControlMsg::MarkSynced`]: rows flipped per requested table
/// and rows still unsynced per table (all four) after the flip.
pub struct MarkResult {
    /// Rows flipped to synced, keyed by table name.
    pub marked: BTreeMap<String, i64>,
    /// Rows still unsynced after the flip, keyed by table name (all four).
    pub unsynced_after: BTreeMap<String, i64>,
}

impl Writer {
    /// Drain every pending control message without blocking, servicing each on
    /// the writer's own connection between ingest batches. Uses `try_recv` so it
    /// never starves ingest: an empty or disconnected channel returns at once.
    ///
    /// On a mark-synced request it flips the window in one transaction, records a
    /// durable event row describing the marked window (itself unsynced, so it is
    /// a candidate for the next push), fans that event out to any live tail, and
    /// acknowledges with the per-table counts. A failed flip is logged and the
    /// ack is dropped, which the read handler maps to a service-unavailable error.
    pub(super) fn drain_control(&mut self) {
        loop {
            match self.control_rx.try_recv() {
                Ok(ControlMsg::MarkSynced { req, ack }) => {
                    match apply_mark_synced(&self.conn, &req) {
                        Ok(res) => {
                            let total: i64 = res.marked.values().sum();
                            let event = self.pushed_window_event(&req, total);
                            let session = self.flight_session.unwrap_or(self.boot_session);
                            // The event is written unsynced so it is a candidate for
                            // the next push; a bad insert is logged, never fatal.
                            if let Err(e) = insert_frame(&self.conn, &event, session, false) {
                                tracing::warn!(error = %e, "failed to record the pushed-window event");
                            }
                            let _ = self.broadcast_tx.send(event);
                            // A dropped ack means the caller already gave up; the
                            // mark itself is durable regardless.
                            let _ = ack.send(res);
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "mark-synced failed");
                            // Dropping `ack` without a send signals the read handler
                            // to surface a service-unavailable error.
                        }
                    }
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => break,
            }
        }
    }

    /// Build the durable event describing a marked-synced window: the row count
    /// flipped, the session/window selector, and the tables touched. Recorded so
    /// an operator (and a later push) can see what was exported and when.
    fn pushed_window_event(&self, req: &SyncRequest, rows: i64) -> IngestFrame {
        let mut ev = EventFrame::new(now_us(), "blackbox.pushed_window", "ados-logd", Level::Info);
        ev.detail
            .insert("rows".to_string(), rmpv::Value::from(rows));
        if let Some(s) = req.session {
            ev.detail
                .insert("session".to_string(), rmpv::Value::from(s));
        }
        if let Some(f) = req.from_us {
            ev.detail
                .insert("from_us".to_string(), rmpv::Value::from(f));
        }
        if let Some(t) = req.to_us {
            ev.detail.insert("to_us".to_string(), rmpv::Value::from(t));
        }
        let tables: Vec<rmpv::Value> = req
            .tables_or_all()
            .iter()
            .map(|t| rmpv::Value::from(t.as_str()))
            .collect();
        ev.detail
            .insert("tables".to_string(), rmpv::Value::Array(tables));
        IngestFrame::Event(ev)
    }
}

/// Flip the rows in the request's window from unsynced to synced on the one
/// read-write connection, in one transaction, returning the per-table flipped
/// count and the per-table remaining-unsynced count (all four tables).
///
/// Kept private to the writer: this is the single place a `synced` flag changes,
/// and it runs only on the writer thread, on the writer's exclusively-owned
/// connection, so `unchecked_transaction()` is safe — the writer is never inside
/// another transaction at the drain point. The `WHERE` is built with boxed
/// `ToSql` params (mirroring the read path's builder) so the slow-port cross
/// target stays free of borrow/needless-ref lints.
pub(crate) fn apply_mark_synced(
    conn: &Connection,
    req: &SyncRequest,
) -> rusqlite::Result<MarkResult> {
    let tx = conn.unchecked_transaction()?;
    let mut marked = BTreeMap::new();
    for table in req.tables_or_all() {
        let name = table.as_str();
        let mut clauses = vec!["synced = 0".to_string()];
        let mut bound: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        if let Some(session) = req.session {
            clauses.push("session = ?".to_string());
            bound.push(Box::new(session));
        }
        if let Some(lo) = req.from_us {
            clauses.push("ts_us >= ?".to_string());
            bound.push(Box::new(lo));
        }
        if let Some(hi) = req.to_us {
            clauses.push("ts_us < ?".to_string());
            bound.push(Box::new(hi));
        }
        let sql = format!(
            "UPDATE {name} SET synced = 1 WHERE {}",
            clauses.join(" AND ")
        );
        let params: Vec<&dyn rusqlite::types::ToSql> = bound.iter().map(|b| b.as_ref()).collect();
        let n = tx.execute(&sql, params.as_slice())?;
        marked.insert(name.to_string(), n as i64);
    }
    let mut unsynced_after = BTreeMap::new();
    for table in SyncTable::ALL {
        let name = table.as_str();
        let n: i64 = tx.query_row(
            &format!("SELECT count(*) FROM {name} WHERE synced = 0"),
            [],
            |r| r.get(0),
        )?;
        unsynced_after.insert(name.to_string(), n);
    }
    tx.commit()?;
    Ok(MarkResult {
        marked,
        unsynced_after,
    })
}
