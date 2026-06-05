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

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rusqlite::{params, Connection};
use tokio::sync::{broadcast, mpsc, oneshot};

use ados_protocol::logd::{EventFrame, IngestFrame, Level, SyncRequest, SyncTable};

use crate::db::{self, DbError};
use crate::retention::{self, MaintenanceReport, RetentionConfig};

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

/// Capacity of the control channel from the read surface to the writer. Small:
/// mark-synced requests are explicit and infrequent, and each caller awaits its
/// reply, so a full channel backpressures the caller, never the writer.
pub const CONTROL_QUEUE_CAPACITY: usize = 32;

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

/// The longest the run loop waits for a frame before it wakes to check the
/// maintenance deadline. This keeps the writer reactive to its own retention
/// timer even when no frames are flowing, while staying idle-cheap: an idle
/// writer wakes a handful of times a second to glance at the clock and goes back
/// to waiting. It is well under any maintenance interval, so the maintenance
/// cadence is honoured closely.
const IDLE_WAKE: Duration = Duration::from_millis(250);

/// Knobs for the batch boundaries, the checkpoint cadence, and the retention
/// maintenance the writer folds into its loop. Defaults are tuned for
/// SD-card-backed boards; the config layer overrides them at start.
#[derive(Debug, Clone)]
pub struct WriterConfig {
    /// Commit once this many rows have accumulated.
    pub batch_max_rows: usize,
    /// Commit a partial batch after this long.
    pub batch_max: Duration,
    /// Truncate the WAL after this many persisted frames.
    pub checkpoint_interval_frames: u64,
    /// Retention windows, size cap, and maintenance/vacuum cadences. The writer
    /// runs retention on its own thread against its own connection — there is
    /// never a second read-write connection.
    pub retention: RetentionConfig,
}

impl Default for WriterConfig {
    fn default() -> Self {
        Self {
            batch_max_rows: DEFAULT_BATCH_MAX_ROWS,
            batch_max: DEFAULT_BATCH_MAX,
            checkpoint_interval_frames: DEFAULT_CHECKPOINT_INTERVAL_FRAMES,
            retention: RetentionConfig::default(),
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
    /// The control channel the read surface enqueues mark-synced requests on.
    /// Drained between ingest batches so a mark never starves ingest.
    control_rx: mpsc::Receiver<ControlMsg>,
    /// A clone of the control sender, so the daemon can wire the read surface
    /// to the writer via [`Writer::control_handle`], symmetric with the
    /// broadcast handle.
    control_tx: mpsc::Sender<ControlMsg>,
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
    /// When the next retention maintenance pass is due. Advanced by the
    /// maintenance interval after each pass.
    next_maintenance: Instant,
    /// When the next periodic `VACUUM` is due. Advanced by the vacuum interval
    /// after each vacuum (a maintenance pass that vacuums for any reason resets
    /// this).
    next_vacuum: Instant,
    /// The daemon's shutdown-pending flag. While set, the writer starts no new
    /// maintenance pass and skips the `VACUUM` inside one already mid-flight, so a
    /// long rewrite can never overrun the shutdown bound and be torn mid-write.
    stop: Arc<AtomicBool>,
}

impl Writer {
    /// Open the store read-write, run migrations, run the integrity check (the
    /// caller has already quarantined and recreated on a prior failure), open a
    /// boot session, and return a ready writer. The returned [`broadcast::Sender`]
    /// is cloned by the daemon so a future live tail can subscribe.
    pub fn new(
        db_path: impl AsRef<Path>,
        rx: mpsc::Receiver<IngestFrame>,
        mut config: WriterConfig,
        stop: Arc<AtomicBool>,
    ) -> Result<Self, WriterError> {
        let db_path = db_path.as_ref().to_path_buf();
        let conn = db::open(&db_path)?;
        let (broadcast_tx, _) = broadcast::channel(BROADCAST_CAPACITY);
        // The control channel is owned by the writer; the read surface gets a
        // sender clone via `control_handle()`. Bounding it caps the number of
        // queued mark requests; the read handler awaits its reply, so a full
        // channel backpressures the caller rather than the writer.
        let (control_tx, control_rx) = mpsc::channel(CONTROL_QUEUE_CAPACITY);
        let boot_session = open_session(&conn, now_us(), "boot", Some("start"))?;
        tracing::info!(session = boot_session, "boot session opened");
        // Clamp the retention knobs once at start so the maintenance step can
        // trust the cap floor and the bounded low-water ratio.
        config.retention = config.retention.clamped();
        // Stagger the first maintenance/vacuum from start: the first pass runs one
        // interval in, not at t=0, so a fresh boot is not spent vacuuming.
        let now = Instant::now();
        let next_maintenance = now + config.retention.maintenance_interval;
        let next_vacuum = now + config.retention.vacuum_interval;
        Ok(Self {
            conn,
            rx,
            control_rx,
            control_tx,
            broadcast_tx,
            config,
            db_path,
            boot_session,
            flight_session: None,
            frames_since_checkpoint: 0,
            next_maintenance,
            next_vacuum,
            stop,
        })
    }

    /// A handle the daemon clones to wire the future live-tail consumer. Holding
    /// it does not keep the writer alive; the writer ends when the ingest channel
    /// closes.
    pub fn broadcast_handle(&self) -> broadcast::Sender<IngestFrame> {
        self.broadcast_tx.clone()
    }

    /// A handle the daemon clones into the read surface so a query handler can
    /// enqueue a mark-synced request on the writer's control channel. Holding it
    /// keeps the control channel open for the read surface's lifetime; the writer
    /// also holds its own clone, so its `try_recv` never sees a closed channel
    /// while the daemon is up.
    pub fn control_handle(&self) -> mpsc::Sender<ControlMsg> {
        self.control_tx.clone()
    }

    /// The boot session id opened at start.
    pub fn boot_session(&self) -> i64 {
        self.boot_session
    }

    /// The store path this writer owns (for the shutdown checkpoint log line).
    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    /// The blocking run loop. Drains the ingest channel in batches and folds the
    /// retention maintenance pass in on its own timer, until the channel closes
    /// (every sender dropped) and is drained, then commits the final partial
    /// batch, closes the boot session, and truncates the WAL. Intended to be the
    /// body of a dedicated `std::thread`; it must not run inside an async task
    /// because every `rusqlite` call here blocks.
    ///
    /// The loop waits for the next frame with a bounded wake (`IDLE_WAKE`) rather
    /// than an unbounded block, so the maintenance deadline is checked even when
    /// no frames are flowing. An idle writer wakes a few times a second, glances
    /// at the clock, and goes back to waiting — cheap, and reactive to its own
    /// retention timer. Maintenance runs on the same connection the inserts use;
    /// there is never a second read-write connection.
    pub fn run(mut self) -> Result<(), WriterError> {
        let mut batch: Vec<IngestFrame> = Vec::with_capacity(self.config.batch_max_rows);
        loop {
            // Wait for the first frame of an otherwise-empty batch, but no longer
            // than the next maintenance deadline (capped by IDLE_WAKE so a long
            // interval still wakes regularly). `recv_with_deadline` returns the
            // frame, or signals a timeout (run maintenance, loop) or a closed
            // channel (drain and exit).
            let wait = self.maintenance_wait();
            match self.recv_with_deadline(wait) {
                RecvOutcome::Frame(frame) => {
                    batch.push(frame);
                    // Fill the batch until the size or time boundary, without
                    // blocking. A channel close mid-fill commits what we have and
                    // is detected again on the next outer wait.
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
                            Err(mpsc::error::TryRecvError::Disconnected) => break,
                        }
                    }
                    self.commit_batch(&mut batch)?;
                    // Service any pending mark-synced requests after the batch is
                    // durable, so a busy writer still answers the control channel.
                    self.drain_control();
                }
                RecvOutcome::Idle => {
                    // The wait elapsed with no frame: run maintenance if due, then
                    // service the control channel, then loop back to waiting.
                    self.maybe_run_maintenance()?;
                    self.drain_control();
                }
                RecvOutcome::Closed => break,
            }

            // Maintenance is also checked after committing a batch, so a busy
            // writer (which never hits the idle path) still runs retention.
            self.maybe_run_maintenance()?;
            // Drain the control channel once more at the bottom of the loop so a
            // mark-synced request is serviced whether or not frames are flowing,
            // without ever blocking ingest (the drain is `try_recv`, never awaited).
            self.drain_control();
        }

        // Clean shutdown: close the session and truncate the WAL. Any final
        // partial batch was already committed inside the loop.
        self.shutdown()?;
        Ok(())
    }

    /// How long to wait for the next frame: the time until the next maintenance
    /// deadline, capped at [`IDLE_WAKE`] so an idle writer still wakes regularly
    /// even with a long maintenance interval, and floored at zero so an overdue
    /// maintenance pass is not delayed.
    fn maintenance_wait(&self) -> Duration {
        let until = self
            .next_maintenance
            .saturating_duration_since(Instant::now());
        until.min(IDLE_WAKE)
    }

    /// Wait up to `wait` for one frame on the ingest channel. Polls with the
    /// short [`DRAIN_POLL`] sleep so the thread stays idle-cheap and the
    /// maintenance deadline is honoured closely.
    fn recv_with_deadline(&mut self, wait: Duration) -> RecvOutcome {
        let deadline = Instant::now() + wait;
        loop {
            match self.rx.try_recv() {
                Ok(frame) => return RecvOutcome::Frame(frame),
                Err(mpsc::error::TryRecvError::Disconnected) => return RecvOutcome::Closed,
                Err(mpsc::error::TryRecvError::Empty) => {
                    if Instant::now() >= deadline {
                        return RecvOutcome::Idle;
                    }
                    std::thread::sleep(DRAIN_POLL);
                }
            }
        }
    }

    /// Run a retention maintenance pass if its deadline has arrived, then advance
    /// the next-maintenance deadline. A pass rolls up closed metric windows,
    /// TTL-deletes aged raw and rollup rows, evicts oldest-first if the store is
    /// over its size cap, and vacuums on the vacuum cadence (or after an
    /// eviction). When the pass evicts rows it records a `retention.evicted`
    /// event so the operator and the read API see that history was pruned.
    fn maybe_run_maintenance(&mut self) -> Result<(), WriterError> {
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

    /// Drain every pending control message without blocking, servicing each on
    /// the writer's own connection between ingest batches. Uses `try_recv` so it
    /// never starves ingest: an empty or disconnected channel returns at once.
    ///
    /// On a mark-synced request it flips the window in one transaction, records a
    /// durable event row describing the marked window (itself unsynced, so it is
    /// a candidate for the next push), fans that event out to any live tail, and
    /// acknowledges with the per-table counts. A failed flip is logged and the
    /// ack is dropped, which the read handler maps to a service-unavailable error.
    fn drain_control(&mut self) {
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

/// The outcome of one bounded wait for the next ingest frame.
enum RecvOutcome {
    /// A frame arrived.
    Frame(IngestFrame),
    /// The wait elapsed with no frame; the writer should check its maintenance
    /// deadline and loop.
    Idle,
    /// Every sender dropped; the writer should drain (nothing left) and exit.
    Closed,
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
fn apply_mark_synced(conn: &Connection, req: &SyncRequest) -> rusqlite::Result<MarkResult> {
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
        let writer = Writer::new(path, rx, config, Arc::new(AtomicBool::new(false))).unwrap();
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
            ..WriterConfig::default()
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
            ..WriterConfig::default()
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
            ..WriterConfig::default()
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

    /// Drive the writer through its real run loop with a near-immediate
    /// maintenance interval and a forced size cap, holding the channel open long
    /// enough for at least one maintenance pass to fire while the writer is idle.
    /// The pass must run on the writer's own connection (no second connection
    /// exists), evict the oldest rows down toward the cap, and write a
    /// `retention.evicted` event row into the store. Proves the retention path is
    /// wired through the writer loop, not only callable as a free function.
    #[test]
    fn maintenance_runs_on_the_writer_thread_and_records_an_eviction_event() {
        let (_dir, path) = temp_db();
        // Maintenance fires almost immediately and then every 50 ms; the size cap
        // is floored, and the low-water target is half the cap so eviction frees
        // a real chunk. The vacuum interval is long so the only vacuum is the
        // post-eviction one.
        let retention = RetentionConfig {
            maintenance_interval: Duration::from_millis(20),
            vacuum_interval: Duration::from_secs(3600),
            max_bytes: crate::retention::MIN_MAX_BYTES,
            low_water_ratio: 0.5,
            ..RetentionConfig::default()
        };
        let config = WriterConfig {
            batch_max_rows: 4096,
            batch_max: Duration::from_millis(20),
            retention,
            ..WriterConfig::default()
        };

        // A bounded channel small enough that the writer must be draining for the
        // sends to make progress (proving the loop runs), large enough not to
        // deadlock the feed.
        let (tx, rx) = mpsc::channel::<IngestFrame>(4096);
        let writer = Writer::new(&path, rx, config, Arc::new(AtomicBool::new(false))).unwrap();
        let handle = std::thread::spawn(move || writer.run().unwrap());

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let total: i64 = 70_000;
        rt.block_on(async {
            // Push enough fat log rows that the store crosses the floored cap, so
            // the idle maintenance pass has something to evict.
            let base = now_us();
            for i in 0..total {
                let mut log = LogFrame::new(base + i * 1_000, "bulk", Level::Info, "x".repeat(512));
                log.target = Some("t".to_string());
                tx.send(IngestFrame::Log(log)).await.unwrap();
            }
            // Hold the channel open while the idle maintenance timer fires, then
            // close it so the writer drains and exits.
            tokio::time::sleep(Duration::from_millis(500)).await;
            drop(tx);
        });
        handle.join().unwrap();

        let ro = db::open_readonly(&path).unwrap();
        // The seed is sized to exceed the floored cap, so the writer's own
        // maintenance pass must have evicted and recorded the event.
        let evictions: i64 = ro
            .query_row(
                "SELECT count(*) FROM events WHERE kind='retention.evicted'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            evictions >= 1,
            "the writer recorded a retention.evicted event on its own thread"
        );
        // The event carries a positive row count and the freed span in its blob.
        let detail_blob: Vec<u8> = ro
            .query_row(
                "SELECT detail FROM events WHERE kind='retention.evicted' \
                 ORDER BY id LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let detail: ados_protocol::logd::Fields = rmp_serde::from_slice(&detail_blob).unwrap();
        let rows = detail.get("rows").and_then(|v| v.as_u64()).unwrap();
        assert!(rows > 0, "the eviction event reports the rows it freed");
        assert!(
            detail.contains_key("from_us") && detail.contains_key("to_us"),
            "the eviction event reports the freed span"
        );
        // Bulk rows were removed.
        let remaining: i64 = ro
            .query_row("SELECT count(*) FROM logs WHERE source='bulk'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(remaining < total, "the size cap removed rows");
        db::integrity_check(&ro).unwrap();
    }

    /// Seed a store directly with a mix of synced/unsynced rows across all four
    /// tables, then call `apply_mark_synced` against it and return the result so
    /// the flip can be asserted without spinning up the whole writer loop.
    fn seed_unsynced(path: &Path) {
        let conn = db::open(path).unwrap();
        conn.execute(
            "INSERT INTO sessions (started_us, kind) VALUES (1000, 'boot')",
            [],
        )
        .unwrap();
        let session = conn.last_insert_rowid();
        // logs at ts 2000..2005, all unsynced.
        for i in 0..6i64 {
            conn.execute(
                "INSERT INTO logs (ts_us, session, source, level, msg, synced) \
                 VALUES (?1, ?2, 'api', 2, ?3, 0)",
                rusqlite::params![2000 + i, session, format!("m{i}")],
            )
            .unwrap();
        }
        // a couple of metrics, events, and one hw row, all unsynced.
        for i in 0..3i64 {
            conn.execute(
                "INSERT INTO metrics (ts_us, session, metric, value, synced) \
                 VALUES (?1, ?2, 'cpu.load', 0.5, 0)",
                rusqlite::params![2100 + i, session],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO events (ts_us, session, kind, source, severity, synced) \
                 VALUES (?1, ?2, 'radio.lock', 'ados-radio', 2, 0)",
                rusqlite::params![2200 + i, session],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO hw (ts_us, session, signals, synced) VALUES (2300, ?1, ?2, 0)",
            rusqlite::params![
                session,
                rmp_serde::to_vec_named(&ados_protocol::logd::Fields::new()).unwrap()
            ],
        )
        .unwrap();
    }

    fn unsynced_count(conn: &Connection, table: &str) -> i64 {
        conn.query_row(
            &format!("SELECT count(*) FROM {table} WHERE synced = 0"),
            [],
            |r| r.get(0),
        )
        .unwrap()
    }

    #[test]
    fn mark_synced_flips_exactly_the_window() {
        let (_dir, path) = temp_db();
        seed_unsynced(&path);
        let conn = db::open(&path).unwrap();
        // Mark only the logs in [2000, 2003): rows at 2000, 2001, 2002 (three).
        let req = SyncRequest {
            session: None,
            from_us: Some(2000),
            to_us: Some(2003),
            tables: vec![SyncTable::Logs],
        };
        let res = apply_mark_synced(&conn, &req).unwrap();
        assert_eq!(res.marked.get("logs"), Some(&3));
        // The remaining logs (2003, 2004, 2005) are still unsynced; the other
        // tables are untouched.
        assert_eq!(res.unsynced_after.get("logs"), Some(&3));
        assert_eq!(unsynced_count(&conn, "logs"), 3);
        assert_eq!(unsynced_count(&conn, "metrics"), 3);
        assert_eq!(unsynced_count(&conn, "events"), 3);
        assert_eq!(unsynced_count(&conn, "hw"), 1);
        db::integrity_check(&conn).unwrap();
    }

    #[test]
    fn mark_synced_empty_tables_marks_all_four() {
        let (_dir, path) = temp_db();
        seed_unsynced(&path);
        let conn = db::open(&path).unwrap();
        // An empty selector marks every unsynced row in all four tables.
        let res = apply_mark_synced(&conn, &SyncRequest::default()).unwrap();
        assert_eq!(res.marked.get("logs"), Some(&6));
        assert_eq!(res.marked.get("metrics"), Some(&3));
        assert_eq!(res.marked.get("events"), Some(&3));
        assert_eq!(res.marked.get("hw"), Some(&1));
        for t in ["logs", "metrics", "events", "hw"] {
            assert_eq!(res.unsynced_after.get(t), Some(&0));
            assert_eq!(unsynced_count(&conn, t), 0);
        }
    }

    #[test]
    fn mark_synced_writes_a_pushed_window_event() {
        // Drive the real run loop: open the channels, mark a window over the
        // control channel, await the ack, and confirm a durable
        // `blackbox.pushed_window` event landed (proving the write went through
        // the writer's own connection) while the store stays consistent.
        let (_dir, path) = temp_db();
        let (tx, rx) = mpsc::channel::<IngestFrame>(64);
        let writer = Writer::new(
            &path,
            rx,
            WriterConfig::default(),
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        let control = writer.control_handle();
        let handle = std::thread::spawn(move || writer.run().unwrap());

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            // Ingest a few rows first so there is something to mark.
            for i in 0..4i64 {
                tx.send(IngestFrame::Telemetry(TelemetryFrame::new(
                    1000 + i,
                    "cpu.load",
                    i as f64,
                )))
                .await
                .unwrap();
            }
            // Give the writer a moment to commit them, then mark all metrics.
            tokio::time::sleep(Duration::from_millis(50)).await;
            let (ack_tx, ack_rx) = oneshot::channel::<MarkResult>();
            control
                .send(ControlMsg::MarkSynced {
                    req: SyncRequest {
                        tables: vec![SyncTable::Metrics],
                        ..SyncRequest::default()
                    },
                    ack: ack_tx,
                })
                .await
                .unwrap();
            let res = tokio::time::timeout(Duration::from_secs(5), ack_rx)
                .await
                .expect("the writer acknowledged in time")
                .expect("the writer did not drop the ack");
            assert!(res.marked.get("metrics").copied().unwrap_or(0) >= 1);
            drop(tx);
        });
        handle.join().unwrap();

        let ro = db::open_readonly(&path).unwrap();
        let count: i64 = ro
            .query_row(
                "SELECT count(*) FROM events WHERE kind='blackbox.pushed_window'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "exactly one pushed-window event was recorded");
        // The event detail records a positive marked-row count.
        let detail_blob: Vec<u8> = ro
            .query_row(
                "SELECT detail FROM events WHERE kind='blackbox.pushed_window'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let detail: ados_protocol::logd::Fields = rmp_serde::from_slice(&detail_blob).unwrap();
        assert!(
            detail.get("rows").and_then(|v| v.as_i64()).unwrap_or(0) >= 1,
            "the pushed-window event reports the rows it marked"
        );
        assert!(
            detail.contains_key("tables"),
            "it records the tables marked"
        );
        db::integrity_check(&ro).unwrap();
    }

    #[test]
    fn mark_synced_ack_returns_while_ingest_flows() {
        // Drive the run loop with a steady ingest stream and a concurrent
        // mark-synced. The ack must come back (the control channel is not starved
        // by ingest) and the ingested frames must still land (the mark does not
        // stall ingest).
        let (_dir, path) = temp_db();
        let (tx, rx) = mpsc::channel::<IngestFrame>(64);
        let writer = Writer::new(
            &path,
            rx,
            WriterConfig::default(),
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        let control = writer.control_handle();
        let handle = std::thread::spawn(move || writer.run().unwrap());

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let total: i64 = 200;
        rt.block_on(async {
            let base = now_us();
            for i in 0..total {
                tx.send(IngestFrame::Telemetry(TelemetryFrame::new(
                    base + i,
                    "cpu.load",
                    i as f64,
                )))
                .await
                .unwrap();
                // Halfway through the stream, fire a mark and await the ack while
                // ingest is still flowing.
                if i == total / 2 {
                    let (ack_tx, ack_rx) = oneshot::channel::<MarkResult>();
                    control
                        .send(ControlMsg::MarkSynced {
                            req: SyncRequest::default(),
                            ack: ack_tx,
                        })
                        .await
                        .unwrap();
                    let res = tokio::time::timeout(Duration::from_secs(5), ack_rx)
                        .await
                        .expect("the ack returned while ingest was flowing")
                        .expect("the writer did not drop the ack");
                    // The mark covered whatever had been committed by then.
                    assert!(res.unsynced_after.contains_key("metrics"));
                }
            }
            drop(tx);
        });
        handle.join().unwrap();

        let ro = db::open_readonly(&path).unwrap();
        let n: i64 = ro
            .query_row("SELECT count(*) FROM metrics", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, total, "every ingested frame still landed");
        db::integrity_check(&ro).unwrap();
    }
}
