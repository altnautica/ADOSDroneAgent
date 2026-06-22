//! A small bounded pool of read-only connections to the store.
//!
//! Each `/v1` read handler used to open a fresh read-only connection per
//! request. Every connection carries its own page cache, and under the constant
//! poll load from the agent and Mission Control that churned a steady stream of
//! short-lived opens through the allocator, which kept the freed pages resident.
//! The pool reuses a small set of warm read-only connections instead: a checkout
//! hands back a parked connection when one is idle and opens a fresh one
//! otherwise, and a returned connection parks itself again only while the idle
//! set is below its cap (any surplus is dropped).
//!
//! The cap bounds *retained* connections, not concurrency: a checkout never
//! blocks waiting for a slot, so a burst still opens as many connections as it
//! needs and a long-held reader (the streaming export opens its own connection
//! outside this pool) can never starve a quick query. Each connection carries
//! the small read-only page cache from [`crate::db::open_readonly`], so a
//! handful of parked connections is a bounded, modest cost.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rusqlite::Connection;

use crate::db::{self, DbError};

/// How many idle read-only connections to retain between requests. A handful
/// keeps the steady-state reuse warm without holding more than a few page caches
/// resident; surplus connections from a burst are dropped on return.
pub const DEFAULT_MAX_IDLE: usize = 4;

/// A bounded pool of parked read-only connections over one store path.
pub struct ConnPool {
    db_path: PathBuf,
    idle: Mutex<Vec<Connection>>,
    max_idle: usize,
}

impl ConnPool {
    /// Build a pool over the store at `db_path`, retaining up to `max_idle`
    /// parked connections.
    pub fn new(db_path: PathBuf, max_idle: usize) -> Arc<Self> {
        Arc::new(Self {
            db_path,
            idle: Mutex::new(Vec::new()),
            max_idle,
        })
    }

    /// Check out a read-only connection: reuse a parked one if available, else
    /// open a fresh one. The returned guard parks the connection back into the
    /// pool when it drops.
    pub fn checkout(self: &Arc<Self>) -> Result<PooledConn, DbError> {
        // Pop under the lock, then release it before any DB work.
        let parked = self.idle.lock().unwrap().pop();
        let conn = match parked {
            Some(conn) => conn,
            None => db::open_readonly(&self.db_path)?,
        };
        Ok(PooledConn {
            conn: Some(conn),
            pool: Arc::clone(self),
        })
    }

    /// Park a connection back into the idle set, or drop it if the set is full.
    fn park(&self, conn: Connection) {
        let mut idle = self.idle.lock().unwrap();
        if idle.len() < self.max_idle {
            idle.push(conn);
        }
        // else: drop `conn`, bounding the retained memory.
    }
}

/// A checked-out read-only connection that parks itself back into its pool on
/// drop. Derefs to the underlying [`Connection`] so call sites pass `&conn`
/// to the row readers unchanged.
pub struct PooledConn {
    // `Some` for the whole guard lifetime; only `drop` takes the connection out.
    conn: Option<Connection>,
    pool: Arc<ConnPool>,
}

impl std::ops::Deref for PooledConn {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        self.conn
            .as_ref()
            .expect("pooled connection is present until drop")
    }
}

impl Drop for PooledConn {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            self.pool.park(conn);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Seed a minimal store so a read-only open succeeds against it.
    fn seed(path: &std::path::Path) {
        let conn = db::open(path).unwrap();
        conn.execute(
            "INSERT INTO sessions (started_us, kind) VALUES (1, 'boot')",
            [],
        )
        .unwrap();
    }

    #[test]
    fn checkout_reuses_a_parked_connection() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs.db");
        seed(&path);
        let pool = ConnPool::new(path, DEFAULT_MAX_IDLE);

        // First checkout opens fresh; dropping it parks one connection.
        {
            let conn = pool.checkout().unwrap();
            let n: i64 = conn
                .query_row("SELECT count(*) FROM sessions", [], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 1);
        }
        assert_eq!(pool.idle.lock().unwrap().len(), 1, "one parked after drop");

        // Second checkout reuses the parked one, leaving the idle set empty
        // while it is held.
        let conn = pool.checkout().unwrap();
        assert_eq!(pool.idle.lock().unwrap().len(), 0, "reused, not reopened");
        drop(conn);
        assert_eq!(pool.idle.lock().unwrap().len(), 1, "re-parked on drop");
    }

    #[test]
    fn idle_set_is_capped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs.db");
        seed(&path);
        let pool = ConnPool::new(path, 2);

        // Hold three concurrently (the pool never blocks a checkout), then drop
        // them all: only `max_idle` (2) park, the surplus is dropped.
        let a = pool.checkout().unwrap();
        let b = pool.checkout().unwrap();
        let c = pool.checkout().unwrap();
        drop(a);
        drop(b);
        drop(c);
        assert_eq!(pool.idle.lock().unwrap().len(), 2, "capped at max_idle");
    }
}
