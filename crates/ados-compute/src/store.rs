//! The job store: a single-writer SQLite database holding datasets, jobs, and
//! outputs. Mirrors the durable single-writer pattern the logging daemon uses.
//! Open `:memory:` for tests and a file path on a node.

use rusqlite::{params, Connection, Row};
use serde::{Deserialize, Serialize};

use crate::{ComputeError, ComputeJobKind, ComputeJobState};

/// An input dataset a job consumes (a keyframe bag, or a live-session handle).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Dataset {
    pub id: String,
    /// Free-form kind label (e.g. `bag`, `live_session`).
    pub kind: String,
    pub created_ms: i64,
    pub meta: serde_json::Value,
}

/// One row of the job queue.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobRecord {
    pub id: String,
    pub kind: ComputeJobKind,
    pub dataset_id: Option<String>,
    pub state: ComputeJobState,
    pub progress: f32,
    pub params: serde_json::Value,
    pub result_ref: Option<String>,
    pub error: Option<String>,
    pub created_ms: i64,
    pub updated_ms: i64,
}

/// A finished artifact produced by a job.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Output {
    pub id: String,
    pub job_id: String,
    /// Artifact kind (e.g. `splat`, `pointcloud`, `detection`).
    pub kind: String,
    /// Where the artifact can be fetched (a stream-lane url or a local path).
    pub uri: String,
    pub created_ms: i64,
}

fn enum_to_db<T: Serialize>(v: &T) -> Result<String, ComputeError> {
    match serde_json::to_value(v)? {
        serde_json::Value::String(s) => Ok(s),
        other => Err(ComputeError::Backend {
            backend: "store".into(),
            message: format!("expected a string-serialized enum, got {other}"),
        }),
    }
}

fn enum_from_db<T: for<'de> Deserialize<'de>>(s: &str) -> Result<T, ComputeError> {
    Ok(serde_json::from_value(serde_json::Value::String(
        s.to_string(),
    ))?)
}

/// The job store over one SQLite connection.
pub struct JobStore {
    conn: Connection,
}

impl JobStore {
    /// Open (and migrate) a store at `path`. Pass `:memory:` for an ephemeral
    /// in-memory store.
    pub fn open(path: &str) -> Result<Self, ComputeError> {
        let conn = Connection::open(path)?;
        Self::from_conn(conn)
    }

    /// Open an in-memory store (tests).
    pub fn open_in_memory() -> Result<Self, ComputeError> {
        Self::from_conn(Connection::open_in_memory()?)
    }

    fn from_conn(conn: Connection) -> Result<Self, ComputeError> {
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS datasets (
                 id TEXT PRIMARY KEY,
                 kind TEXT NOT NULL,
                 created_ms INTEGER NOT NULL,
                 meta TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS jobs (
                 id TEXT PRIMARY KEY,
                 kind TEXT NOT NULL,
                 dataset_id TEXT,
                 state TEXT NOT NULL,
                 progress REAL NOT NULL,
                 params TEXT NOT NULL,
                 result_ref TEXT,
                 error TEXT,
                 created_ms INTEGER NOT NULL,
                 updated_ms INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS jobs_state_created ON jobs(state, created_ms);
             CREATE TABLE IF NOT EXISTS outputs (
                 id TEXT PRIMARY KEY,
                 job_id TEXT NOT NULL,
                 kind TEXT NOT NULL,
                 uri TEXT NOT NULL,
                 created_ms INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS outputs_job ON outputs(job_id);",
        )?;
        Ok(Self { conn })
    }

    // --- datasets ---------------------------------------------------------

    pub fn insert_dataset(&self, d: &Dataset) -> Result<(), ComputeError> {
        self.conn.execute(
            "INSERT INTO datasets (id, kind, created_ms, meta) VALUES (?1, ?2, ?3, ?4)",
            params![d.id, d.kind, d.created_ms, serde_json::to_string(&d.meta)?],
        )?;
        Ok(())
    }

    pub fn get_dataset(&self, id: &str) -> Result<Option<Dataset>, ComputeError> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, kind, created_ms, meta FROM datasets WHERE id = ?1")?;
        let mut rows = stmt.query(params![id])?;
        match rows.next()? {
            Some(row) => Ok(Some(Dataset {
                id: row.get(0)?,
                kind: row.get(1)?,
                created_ms: row.get(2)?,
                meta: serde_json::from_str(&row.get::<_, String>(3)?)?,
            })),
            None => Ok(None),
        }
    }

    // --- jobs -------------------------------------------------------------

    /// Insert a new job (typically in [`ComputeJobState::Queued`]).
    pub fn submit_job(&self, j: &JobRecord) -> Result<(), ComputeError> {
        self.conn.execute(
            "INSERT INTO jobs
                (id, kind, dataset_id, state, progress, params, result_ref, error, created_ms, updated_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                j.id,
                enum_to_db(&j.kind)?,
                j.dataset_id,
                enum_to_db(&j.state)?,
                j.progress,
                serde_json::to_string(&j.params)?,
                j.result_ref,
                j.error,
                j.created_ms,
                j.updated_ms,
            ],
        )?;
        Ok(())
    }

    fn row_to_job(row: &Row) -> Result<JobRecord, ComputeError> {
        Ok(JobRecord {
            id: row.get(0)?,
            kind: enum_from_db(&row.get::<_, String>(1)?)?,
            dataset_id: row.get(2)?,
            state: enum_from_db(&row.get::<_, String>(3)?)?,
            progress: row.get(4)?,
            params: serde_json::from_str(&row.get::<_, String>(5)?)?,
            result_ref: row.get(6)?,
            error: row.get(7)?,
            created_ms: row.get(8)?,
            updated_ms: row.get(9)?,
        })
    }

    const JOB_COLS: &'static str =
        "id, kind, dataset_id, state, progress, params, result_ref, error, created_ms, updated_ms";

    /// The oldest queued job, or `None` if the queue is empty. This is the
    /// scheduler's pick; a real multi-worker node would claim it atomically.
    pub fn next_queued_job(&self) -> Result<Option<JobRecord>, ComputeError> {
        let sql = format!(
            "SELECT {} FROM jobs WHERE state = 'queued' ORDER BY created_ms ASC LIMIT 1",
            Self::JOB_COLS
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let mut rows = stmt.query([])?;
        match rows.next()? {
            Some(row) => Ok(Some(Self::row_to_job(row)?)),
            None => Ok(None),
        }
    }

    pub fn get_job(&self, id: &str) -> Result<Option<JobRecord>, ComputeError> {
        let sql = format!("SELECT {} FROM jobs WHERE id = ?1", Self::JOB_COLS);
        let mut stmt = self.conn.prepare(&sql)?;
        let mut rows = stmt.query(params![id])?;
        match rows.next()? {
            Some(row) => Ok(Some(Self::row_to_job(row)?)),
            None => Ok(None),
        }
    }

    pub fn list_jobs(&self) -> Result<Vec<JobRecord>, ComputeError> {
        let sql = format!(
            "SELECT {} FROM jobs ORDER BY created_ms ASC",
            Self::JOB_COLS
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([], |row| {
            Ok(Self::row_to_job(row).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, e.into())
            }))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r??);
        }
        Ok(out)
    }

    /// Update a job's running state and result. Returns `NotFound` if no row
    /// matched.
    pub fn set_job_state(
        &self,
        id: &str,
        state: ComputeJobState,
        progress: f32,
        result_ref: Option<&str>,
        error: Option<&str>,
        updated_ms: i64,
    ) -> Result<(), ComputeError> {
        let n = self.conn.execute(
            "UPDATE jobs SET state = ?2, progress = ?3, result_ref = ?4, error = ?5, updated_ms = ?6
             WHERE id = ?1",
            params![id, enum_to_db(&state)?, progress, result_ref, error, updated_ms],
        )?;
        if n == 0 {
            return Err(ComputeError::NotFound(format!("job {id}")));
        }
        Ok(())
    }

    /// Cancel a job if it has not reached a terminal state. Returns whether a
    /// row was cancelled.
    pub fn cancel_job(&self, id: &str, updated_ms: i64) -> Result<bool, ComputeError> {
        let n = self.conn.execute(
            "UPDATE jobs SET state = 'cancelled', updated_ms = ?2
             WHERE id = ?1 AND state IN ('queued', 'running')",
            params![id, updated_ms],
        )?;
        Ok(n > 0)
    }

    // --- outputs ----------------------------------------------------------

    pub fn insert_output(&self, o: &Output) -> Result<(), ComputeError> {
        self.conn.execute(
            "INSERT INTO outputs (id, job_id, kind, uri, created_ms) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![o.id, o.job_id, o.kind, o.uri, o.created_ms],
        )?;
        Ok(())
    }

    pub fn outputs_for_job(&self, job_id: &str) -> Result<Vec<Output>, ComputeError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, job_id, kind, uri, created_ms FROM outputs WHERE job_id = ?1
             ORDER BY created_ms ASC",
        )?;
        let rows = stmt.query_map(params![job_id], |row| {
            Ok(Output {
                id: row.get(0)?,
                job_id: row.get(1)?,
                kind: row.get(2)?,
                uri: row.get(3)?,
                created_ms: row.get(4)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Retention: drop terminal jobs (and their outputs) updated before
    /// `cutoff_ms`. Returns the number of jobs removed.
    pub fn purge_terminal_before(&self, cutoff_ms: i64) -> Result<usize, ComputeError> {
        self.conn.execute(
            "DELETE FROM outputs WHERE job_id IN
                (SELECT id FROM jobs
                 WHERE state IN ('completed','failed','cancelled') AND updated_ms < ?1)",
            params![cutoff_ms],
        )?;
        let n = self.conn.execute(
            "DELETE FROM jobs
             WHERE state IN ('completed','failed','cancelled') AND updated_ms < ?1",
            params![cutoff_ms],
        )?;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(id: &str, kind: ComputeJobKind, now: i64) -> JobRecord {
        JobRecord {
            id: id.into(),
            kind,
            dataset_id: Some("ds-1".into()),
            state: ComputeJobState::Queued,
            progress: 0.0,
            params: serde_json::json!({}),
            result_ref: None,
            error: None,
            created_ms: now,
            updated_ms: now,
        }
    }

    #[test]
    fn dataset_round_trips() {
        let s = JobStore::open_in_memory().unwrap();
        let d = Dataset {
            id: "ds-1".into(),
            kind: "bag".into(),
            created_ms: 100,
            meta: serde_json::json!({ "cameras": 1 }),
        };
        s.insert_dataset(&d).unwrap();
        assert_eq!(s.get_dataset("ds-1").unwrap(), Some(d));
        assert_eq!(s.get_dataset("nope").unwrap(), None);
    }

    #[test]
    fn queue_is_fifo_by_created_ms() {
        let s = JobStore::open_in_memory().unwrap();
        s.submit_job(&job("job-b", ComputeJobKind::Reconstruct, 200))
            .unwrap();
        s.submit_job(&job("job-a", ComputeJobKind::Reconstruct, 100))
            .unwrap();
        // The older job comes first regardless of insertion order.
        assert_eq!(s.next_queued_job().unwrap().unwrap().id, "job-a");
    }

    #[test]
    fn state_transition_and_outputs() {
        let s = JobStore::open_in_memory().unwrap();
        s.submit_job(&job("job-1", ComputeJobKind::Reconstruct, 100))
            .unwrap();
        s.set_job_state("job-1", ComputeJobState::Running, 0.5, None, None, 110)
            .unwrap();
        s.set_job_state(
            "job-1",
            ComputeJobState::Completed,
            1.0,
            Some("mock://splat/ds-1"),
            None,
            120,
        )
        .unwrap();
        let j = s.get_job("job-1").unwrap().unwrap();
        assert_eq!(j.state, ComputeJobState::Completed);
        assert_eq!(j.result_ref.as_deref(), Some("mock://splat/ds-1"));

        s.insert_output(&Output {
            id: "out-1".into(),
            job_id: "job-1".into(),
            kind: "splat".into(),
            uri: "mock://splat/ds-1".into(),
            created_ms: 120,
        })
        .unwrap();
        assert_eq!(s.outputs_for_job("job-1").unwrap().len(), 1);
    }

    #[test]
    fn cancel_only_non_terminal() {
        let s = JobStore::open_in_memory().unwrap();
        s.submit_job(&job("job-1", ComputeJobKind::Reconstruct, 100))
            .unwrap();
        assert!(s.cancel_job("job-1", 110).unwrap());
        // A second cancel is a no-op (already terminal).
        assert!(!s.cancel_job("job-1", 120).unwrap());
        assert_eq!(
            s.get_job("job-1").unwrap().unwrap().state,
            ComputeJobState::Cancelled
        );
    }

    #[test]
    fn retention_purges_terminal() {
        let s = JobStore::open_in_memory().unwrap();
        s.submit_job(&job("old", ComputeJobKind::Reconstruct, 100))
            .unwrap();
        s.set_job_state("old", ComputeJobState::Completed, 1.0, None, None, 100)
            .unwrap();
        s.submit_job(&job("fresh", ComputeJobKind::Reconstruct, 500))
            .unwrap();
        let removed = s.purge_terminal_before(200).unwrap();
        assert_eq!(removed, 1);
        assert!(s.get_job("old").unwrap().is_none());
        assert!(s.get_job("fresh").unwrap().is_some());
    }

    #[test]
    fn set_state_unknown_job_is_not_found() {
        let s = JobStore::open_in_memory().unwrap();
        let err = s
            .set_job_state("ghost", ComputeJobState::Running, 0.0, None, None, 1)
            .unwrap_err();
        assert!(matches!(err, ComputeError::NotFound(_)));
    }
}
