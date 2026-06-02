//! The single-writer store loop.
//!
//! SQLite is single-writer at the file level and `rusqlite` is synchronous and
//! blocking, so the write connection lives on its own dedicated OS thread and is
//! never touched from an async task. The async ingest side hands frames over a
//! bounded channel; this thread drains them, redacts every secret-bearing field
//! before the row reaches disk, batches inserts into one transaction per size or
//! time boundary (one fsync per batch on the SD card), checkpoints the WAL
//! periodically, and broadcasts each persisted frame to any future live-tail
//! subscriber.
//!
//! The loop blocks on the first frame of an empty batch (this is a dedicated
//! thread, so blocking is correct and cheap), then drains additional frames
//! without blocking until either the batch fills or the time boundary passes.
//! On a clean shutdown the channel closes, the final partial batch is committed,
//! the open session is closed, and the WAL is truncated.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rusqlite::{params, Connection};
use tokio::sync::{broadcast, mpsc};

use ados_protocol::logd::IngestFrame;

use crate::db::{self, DbError};

/// How many rows accumulate before a batch is committed.
pub const DEFAULT_BATCH_MAX_ROWS: usize = 500;

/// How long a partial batch is held before it is committed anyway, so low-rate
/// data is not stranded in memory.
pub const DEFAULT_BATCH_MAX: Duration = Duration::from_millis(100);

/// How many frames are persisted between WAL truncating checkpoints. Keeps the
/// `-wal` file from growing without bound on a long-running daemon, on top of
/// the connection's `wal_autocheckpoint`.
pub const DEFAULT_CHECKPOINT_INTERVAL_FRAMES: u64 = 10_000;

/// How often the writer re-polls a non-empty, not-yet-due batch for more frames
/// while waiting out the time boundary. Small enough that the boundary is
/// honoured closely, large enough that an idle wait does not spin a core.
const DRAIN_POLL: Duration = Duration::from_millis(2);

/// Capacity of the live-tail broadcast channel. A subscriber that falls behind
/// loses the oldest buffered frames (lagged), never the writer.
pub const BROADCAST_CAPACITY: usize = 1024;

/// Knobs for the batch boundaries and the checkpoint cadence. Defaults are tuned
/// for SD-card-backed boards; the config layer overrides them at start.
#[derive(Debug, Clone)]
pub struct WriterConfig {
    /// Commit once this many rows have accumulated.
    pub batch_max_rows: usize,
    /// Commit a partial batch after this long.
    pub batch_max: Duration,
    /// Truncate the WAL after this many persisted frames.
    pub checkpoint_interval_frames: u64,
}

impl Default for WriterConfig {
    fn default() -> Self {
        Self {
            batch_max_rows: DEFAULT_BATCH_MAX_ROWS,
            batch_max: DEFAULT_BATCH_MAX,
            checkpoint_interval_frames: DEFAULT_CHECKPOINT_INTERVAL_FRAMES,
        }
    }
}

/// Errors raised opening or running the writer.
#[derive(Debug, thiserror::Error)]
pub enum WriterError {
    #[error(transparent)]
    Db(#[from] DbError),
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

/// The dedicated-thread writer. Owns the only read-write connection, the ingest
/// receiver, the live-tail broadcaster, and the current session bookkeeping.
pub struct Writer {
    conn: Connection,
    rx: mpsc::Receiver<IngestFrame>,
    broadcast_tx: broadcast::Sender<IngestFrame>,
    config: WriterConfig,
    db_path: PathBuf,
    /// The boot session opened at start; rows that are not inside a flight
    /// session are attributed to it.
    boot_session: i64,
    /// The currently-open flight session, if any. Rows are attributed here while
    /// armed; it closes on disarm.
    flight_session: Option<i64>,
    /// Frames persisted since the last WAL truncate.
    frames_since_checkpoint: u64,
}

impl Writer {
    /// Open the store read-write, run migrations, run the integrity check (the
    /// caller has already quarantined and recreated on a prior failure), open a
    /// boot session, and return a ready writer. The returned [`broadcast::Sender`]
    /// is cloned by the daemon so a future live tail can subscribe.
    pub fn new(
        db_path: impl AsRef<Path>,
        rx: mpsc::Receiver<IngestFrame>,
        config: WriterConfig,
    ) -> Result<Self, WriterError> {
        let db_path = db_path.as_ref().to_path_buf();
        let conn = db::open(&db_path)?;
        let (broadcast_tx, _) = broadcast::channel(BROADCAST_CAPACITY);
        let boot_session = open_session(&conn, now_us(), "boot", Some("start"))?;
        tracing::info!(session = boot_session, "boot session opened");
        Ok(Self {
            conn,
            rx,
            broadcast_tx,
            config,
            db_path,
            boot_session,
            flight_session: None,
            frames_since_checkpoint: 0,
        })
    }

    /// A handle the daemon clones to wire the future live-tail consumer. Holding
    /// it does not keep the writer alive; the writer ends when the ingest channel
    /// closes.
    pub fn broadcast_handle(&self) -> broadcast::Sender<IngestFrame> {
        self.broadcast_tx.clone()
    }

    /// The boot session id opened at start.
    pub fn boot_session(&self) -> i64 {
        self.boot_session
    }

    /// The store path this writer owns (for the shutdown checkpoint log line).
    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    /// The blocking run loop. Drains the ingest channel in batches until the
    /// channel closes (every sender dropped), commits the final partial batch,
    /// closes the boot session, and truncates the WAL. Intended to be the body
    /// of a dedicated `std::thread`; it must not run inside an async task because
    /// every `rusqlite` call here blocks.
    pub fn run(mut self) -> Result<(), WriterError> {
        let mut batch: Vec<IngestFrame> = Vec::with_capacity(self.config.batch_max_rows);
        // Block for the first frame of each otherwise-empty batch. A dedicated
        // thread blocking here is correct and idle-cheap. When every sender has
        // dropped, `blocking_recv` returns `None` and the loop ends; any frames
        // pulled into `batch` before that are committed below.
        while let Some(frame) = self.rx.blocking_recv() {
            batch.push(frame);

            // Fill the batch until the size or time boundary, without blocking.
            let deadline = Instant::now() + self.config.batch_max;
            while batch.len() < self.config.batch_max_rows {
                match self.rx.try_recv() {
                    Ok(frame) => batch.push(frame),
                    Err(mpsc::error::TryRecvError::Empty) => {
                        if Instant::now() >= deadline {
                            break;
                        }
                        std::thread::sleep(DRAIN_POLL);
                    }
                    // Channel closed mid-fill: commit what we have, then the
                    // outer `blocking_recv` returns `None` and the loop ends.
                    Err(mpsc::error::TryRecvError::Disconnected) => break,
                }
            }

            self.commit_batch(&mut batch)?;
        }

        // Clean shutdown: close the session and truncate the WAL. Any final
        // partial batch was already committed inside the loop.
        self.shutdown()?;
        Ok(())
    }

    /// Persist one batch inside a single transaction, redacting every frame
    /// before insert, then broadcast each persisted frame and run a periodic WAL
    /// truncate. A single bad row is logged and skipped; it never aborts the
    /// whole batch.
    ///
    /// Session transitions are applied inline, in frame order, inside the same
    /// transaction: an arm event opens the flight session before the next frame
    /// is inserted, so a log emitted between arm and disarm in the same batch is
    /// correctly attributed to the flight session. Because the session row and
    /// the rows that reference it commit together, the foreign key holds.
    fn commit_batch(&mut self, batch: &mut Vec<IngestFrame>) -> Result<(), WriterError> {
        if batch.is_empty() {
            return Ok(());
        }
        let tx = self.conn.transaction()?;
        let mut persisted = 0u64;
        // Track the session locally across the transaction; commit it back to the
        // writer's state only after the transaction succeeds.
        let mut flight = self.flight_session;
        let mut new_flight_log: Vec<(i64, &'static str)> = Vec::new();
        for frame in batch.iter_mut() {
            // No row is ever written to disk unredacted. The flag records whether
            // redaction actually changed a value on this frame.
            let redacted = frame.redact();
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
            match insert_frame(&tx, frame, session, redacted) {
                Ok(()) => persisted += 1,
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

        // Fan the persisted frames out to any live-tail subscriber. A send with
        // no subscribers is a no-op; a lagging subscriber drops, never the writer.
        for frame in batch.drain(..) {
            let _ = self.broadcast_tx.send(frame);
        }

        self.frames_since_checkpoint += persisted;
        self.maybe_checkpoint()?;
        Ok(())
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
    fn shutdown(&mut self) -> Result<(), WriterError> {
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

/// A session-boundary transition derived from a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionTransition {
    /// A flight session should open.
    Arm,
    /// The open flight session should close.
    Disarm,
}

/// Derive a session transition from a frame. Arm/disarm is signalled by an event
/// whose `reason` detail is `arm`/`disarm`, or by a `pairing`/`service` event
/// carrying the same reason field. The state-socket tap in a later chunk feeds
/// these events; this keeps the rule in one place.
fn session_transition(frame: &IngestFrame) -> Option<SessionTransition> {
    let IngestFrame::Event(e) = frame else {
        return None;
    };
    let reason = e.detail.get("reason").and_then(|v| v.as_str())?;
    match reason {
        "arm" => Some(SessionTransition::Arm),
        "disarm" => Some(SessionTransition::Disarm),
        _ => None,
    }
}

/// Insert one frame into its table under `session`. The frame is already
/// redacted by the caller; `redacted` records whether that pass actually changed
/// a value, so the stored flag reflects a real redaction rather than the mere
/// presence of structured fields.
fn insert_frame(
    conn: &Connection,
    frame: &IngestFrame,
    session: i64,
    redacted: bool,
) -> Result<(), rusqlite::Error> {
    match frame {
        IngestFrame::Log(l) => {
            let fields = encode_map(&l.fields);
            let redacted = i64::from(redacted);
            conn.execute(
                "INSERT INTO logs (ts_us, session, source, level, target, msg, fields, redacted) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    l.ts_us,
                    session,
                    l.source,
                    i64::from(l.level.as_u8()),
                    l.target,
                    l.msg,
                    fields,
                    redacted,
                ],
            )?;
        }
        IngestFrame::Telemetry(t) => {
            let tags = encode_map(&t.tags);
            conn.execute(
                "INSERT INTO metrics (ts_us, session, metric, value, tags) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![t.ts_us, session, t.metric, t.value, tags],
            )?;
        }
        IngestFrame::Event(e) => {
            let detail = encode_map(&e.detail);
            conn.execute(
                "INSERT INTO events (ts_us, session, kind, source, severity, detail) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    e.ts_us,
                    session,
                    e.kind,
                    e.source,
                    i64::from(e.severity.as_u8()),
                    detail,
                ],
            )?;
        }
        IngestFrame::Hw(h) => {
            // The whole snapshot rides in the signals blob; an empty snapshot
            // still encodes a valid (empty) msgpack map so the NOT NULL holds.
            let signals = rmp_serde::to_vec_named(&h.signals).unwrap_or_default();
            conn.execute(
                "INSERT INTO hw (ts_us, session, signals) VALUES (?1, ?2, ?3)",
                params![h.ts_us, session, signals],
            )?;
        }
    }
    Ok(())
}

/// Encode an open fields/tags/detail map to a msgpack blob, or `NULL` when empty
/// so an absent map does not waste a row's blob column.
fn encode_map(map: &ados_protocol::logd::Fields) -> Option<Vec<u8>> {
    if map.is_empty() {
        None
    } else {
        rmp_serde::to_vec_named(map).ok()
    }
}

/// Insert a session row, returning its id.
fn open_session(
    conn: &Connection,
    started_us: i64,
    kind: &str,
    reason: Option<&str>,
) -> Result<i64, rusqlite::Error> {
    conn.execute(
        "INSERT INTO sessions (started_us, kind, reason) VALUES (?1, ?2, ?3)",
        params![started_us, kind, reason],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Close a session row: stamp `ended_us` and the closing `reason`.
fn close_session(
    conn: &Connection,
    id: i64,
    ended_us: i64,
    reason: &str,
) -> Result<(), rusqlite::Error> {
    conn.execute(
        "UPDATE sessions SET ended_us = ?1, reason = ?2 WHERE id = ?3",
        params![ended_us, reason, id],
    )?;
    Ok(())
}

/// The current wall-clock time in microseconds since the Unix epoch. A clock set
/// before the epoch yields zero rather than a negative timestamp.
pub fn now_us() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_protocol::logd::{EventFrame, HwSnapshot, Level, LogFrame, TelemetryFrame};
    use rmpv::Value as MpVal;

    fn temp_db() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs.db");
        (dir, path)
    }

    /// Spawn the writer on a real dedicated thread (the production threading
    /// model), feed it frames over the bounded channel, and return after the
    /// thread has committed and exited so the DB is safe to read.
    fn run_writer_to_completion(path: &Path, config: WriterConfig, frames: Vec<IngestFrame>) {
        let (tx, rx) = mpsc::channel::<IngestFrame>(64);
        let writer = Writer::new(path, rx, config).unwrap();
        let handle = std::thread::spawn(move || writer.run().unwrap());
        // A small blocking runtime feeds the async-side sender from this test
        // thread; the writer itself is the blocking thread above.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            for f in frames {
                tx.send(f).await.unwrap();
            }
            // Dropping the sender closes the channel, which ends the writer.
            drop(tx);
        });
        handle.join().unwrap();
    }

    #[test]
    fn writer_inserts_each_frame_into_the_right_table() {
        let (_dir, path) = temp_db();
        let mut log = LogFrame::new(1_000, "test-src", Level::Warn, "a message");
        log.target = Some("mod::path".to_string());
        log.fields.insert("attempt".to_string(), MpVal::from(3u64));
        let frames = vec![
            IngestFrame::Log(log),
            IngestFrame::Telemetry(TelemetryFrame::new(1_001, "cpu.load", 0.5)),
            IngestFrame::Event(EventFrame::new(
                1_002,
                "radio.lock",
                "test-src",
                Level::Info,
            )),
            IngestFrame::Hw({
                let mut h = HwSnapshot::new(1_003);
                h.signals
                    .insert("thermal.soc_c".to_string(), MpVal::from(42.0));
                h
            }),
        ];
        run_writer_to_completion(&path, WriterConfig::default(), frames);

        let ro = db::open_readonly(&path).unwrap();
        for (table, want) in [("logs", 1), ("metrics", 1), ("events", 1), ("hw", 1)] {
            let n: i64 = ro
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(n, want, "{table} row count");
        }
        // Every data row is attributed to the boot session.
        let boot_open: i64 = ro
            .query_row(
                "SELECT count(*) FROM logs WHERE session = (SELECT id FROM sessions WHERE kind='boot')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(boot_open, 1);
        // A log that carries only a non-secret field is not flagged redacted:
        // the flag tracks an actual redaction, not the mere presence of fields.
        let redacted: i64 = ro
            .query_row("SELECT redacted FROM logs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            redacted, 0,
            "a non-secret field must not flag the row redacted"
        );
        db::integrity_check(&ro).unwrap();
    }

    #[test]
    fn secret_fields_are_redacted_before_insert() {
        let (_dir, path) = temp_db();
        let mut log = LogFrame::new(2_000, "test-src", Level::Info, "with a secret");
        log.fields
            .insert("api_key".to_string(), MpVal::from("ABCDEFGHIJ1234567890"));
        run_writer_to_completion(&path, WriterConfig::default(), vec![IngestFrame::Log(log)]);

        let ro = db::open_readonly(&path).unwrap();
        let (fields_blob, redacted): (Vec<u8>, i64) = ro
            .query_row(
                "SELECT fields, redacted FROM logs WHERE source='test-src'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(redacted, 1);
        let fields: ados_protocol::logd::Fields = rmp_serde::from_slice(&fields_blob).unwrap();
        let api_key = fields.get("api_key").and_then(|v| v.as_str()).unwrap();
        assert!(
            api_key.starts_with("redacted:"),
            "api_key must be redacted on disk: {api_key}"
        );
    }

    #[test]
    fn size_boundary_commits_at_max_rows() {
        // A tiny batch cap and a long time bound: the writer must commit on the
        // size boundary, not the timer. Feed exactly two full batches.
        let (_dir, path) = temp_db();
        let config = WriterConfig {
            batch_max_rows: 5,
            batch_max: Duration::from_secs(3600),
            checkpoint_interval_frames: 1_000_000,
        };
        let frames: Vec<IngestFrame> = (0..10)
            .map(|i| IngestFrame::Telemetry(TelemetryFrame::new(i, "cpu.load", i as f64)))
            .collect();
        run_writer_to_completion(&path, config, frames);

        let ro = db::open_readonly(&path).unwrap();
        let n: i64 = ro
            .query_row("SELECT count(*) FROM metrics", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 10);
    }

    #[test]
    fn time_boundary_commits_a_partial_batch() {
        // A large size cap and a short time bound: a single frame must still be
        // committed once the time boundary passes (it never waits for the cap).
        let (_dir, path) = temp_db();
        let config = WriterConfig {
            batch_max_rows: 10_000,
            batch_max: Duration::from_millis(20),
            checkpoint_interval_frames: 1_000_000,
        };
        run_writer_to_completion(
            &path,
            config,
            vec![IngestFrame::Telemetry(TelemetryFrame::new(
                1, "cpu.load", 1.0,
            ))],
        );
        let ro = db::open_readonly(&path).unwrap();
        let n: i64 = ro
            .query_row("SELECT count(*) FROM metrics", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn arm_event_opens_a_flight_session_and_disarm_closes_it() {
        let (_dir, path) = temp_db();
        let mut arm = EventFrame::new(3_000, "state", "test-src", Level::Info);
        arm.detail.insert("reason".to_string(), MpVal::from("arm"));
        // A log emitted while armed must carry the flight session.
        let mid = IngestFrame::Log(LogFrame::new(3_001, "test-src", Level::Info, "in flight"));
        let mut disarm = EventFrame::new(3_002, "state", "test-src", Level::Info);
        disarm
            .detail
            .insert("reason".to_string(), MpVal::from("disarm"));
        run_writer_to_completion(
            &path,
            WriterConfig::default(),
            vec![IngestFrame::Event(arm), mid, IngestFrame::Event(disarm)],
        );

        let ro = db::open_readonly(&path).unwrap();
        let (flight_id, ended): (i64, Option<i64>) = ro
            .query_row(
                "SELECT id, ended_us FROM sessions WHERE kind='flight'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(ended.is_some(), "flight session must be closed on disarm");
        // The in-flight log row is attributed to the flight session.
        let in_flight: i64 = ro
            .query_row(
                "SELECT count(*) FROM logs WHERE session = ?1 AND msg = 'in flight'",
                [flight_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(in_flight, 1);
    }

    #[test]
    fn shutdown_closes_the_boot_session_with_reason_shutdown() {
        let (_dir, path) = temp_db();
        run_writer_to_completion(
            &path,
            WriterConfig::default(),
            vec![IngestFrame::Telemetry(TelemetryFrame::new(1, "m", 1.0))],
        );
        let ro = db::open_readonly(&path).unwrap();
        let (ended, reason): (Option<i64>, Option<String>) = ro
            .query_row(
                "SELECT ended_us, reason FROM sessions WHERE kind='boot'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(ended.is_some());
        assert_eq!(reason.as_deref(), Some("shutdown"));
    }

    #[test]
    fn periodic_checkpoint_keeps_the_store_intact() {
        // A checkpoint every frame exercises the WAL-truncate path many times;
        // the store stays consistent and the integrity check passes.
        let (_dir, path) = temp_db();
        let config = WriterConfig {
            batch_max_rows: 1,
            batch_max: Duration::from_secs(3600),
            checkpoint_interval_frames: 1,
        };
        let frames: Vec<IngestFrame> = (0..20)
            .map(|i| IngestFrame::Telemetry(TelemetryFrame::new(i, "cpu.load", i as f64)))
            .collect();
        run_writer_to_completion(&path, config, frames);
        let ro = db::open_readonly(&path).unwrap();
        let n: i64 = ro
            .query_row("SELECT count(*) FROM metrics", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 20);
        db::integrity_check(&ro).unwrap();
    }
}
