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
//!
//! The loop body is grouped into sibling modules by concern: [`config`] holds
//! the tuning knobs and the error type; [`batch`] holds the per-batch
//! transaction, the WAL checkpoint, and the shutdown close; [`control`] holds
//! the mark-synced control plane; [`maintenance`] holds the retention pass;
//! [`session`] holds the flight-session boundary rule; [`encode`] holds the row
//! insertion and the shared clock. Every previously-public item is re-exported
//! here so the writer's external surface is unchanged.

mod batch;
mod config;
mod control;
mod encode;
mod maintenance;
mod session;

pub use config::{
    WriterConfig, WriterError, BROADCAST_CAPACITY, CONTROL_QUEUE_CAPACITY, DEFAULT_BATCH_MAX,
    DEFAULT_BATCH_MAX_ROWS, DEFAULT_CHECKPOINT_INTERVAL_FRAMES,
};
pub use control::{ControlMsg, MarkResult};
pub use encode::now_us;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rusqlite::Connection;
use tokio::sync::{broadcast, mpsc};

use ados_protocol::logd::IngestFrame;

use crate::db;

use self::config::{DRAIN_POLL, IDLE_WAKE};
use self::session::open_session;

/// How long the writer may go without stamping progress before it is judged
/// stalled.
///
/// The run loop stamps once per turn and turns at least every [`IDLE_WAKE`]
/// (250 ms) when nothing is flowing, so this is three orders of magnitude of
/// slack over the idle cadence. The size is set by the one legitimate step
/// that cannot be interrupted or subdivided: the periodic full `VACUUM`, which
/// rewrites the whole file in a single SQLite call and on a gigabyte store on
/// tired flash takes minutes. Withholding the watchdog under that would have
/// systemd SIGKILL the daemon mid-rewrite, which is precisely how the store
/// gets torn — a worse failure than the one this guards.
///
/// The common case does not wait for it: a writer whose loop RETURNS is
/// reported dead immediately by the terminal `ended` flag, with no timing
/// involved. This budget only covers a writer that is still inside a call and
/// never coming back.
pub const WRITER_STALL_BUDGET: Duration = Duration::from_secs(600);

/// The writer's liveness stamp, shared with the daemon and the read surface.
///
/// Without it, `/v1/healthz` answered `writer_alive: true` unconditionally and
/// the daemon fed the systemd watchdog from a timer that knew nothing about the
/// writer — so a writer that died mid-flight left the Black Box silently not
/// recording behind a 200 `{ok:true}` and an `active (running)` unit. The
/// supervisor's `MonitorProgress` couples its watchdog to real pass progress
/// the same way; this is that pattern for the store.
///
/// Cheap enough to stamp every turn of the run loop: two relaxed atomics, no
/// lock, so the stamp can never be the thing that blocks the writer.
#[derive(Clone, Debug)]
pub struct WriterHealth {
    inner: Arc<WriterHealthInner>,
}

#[derive(Debug)]
struct WriterHealthInner {
    /// Process-start epoch the millisecond stamps are measured from.
    epoch: Instant,
    /// Milliseconds since `epoch` at the last stamp.
    last_ms: AtomicU64,
    /// Set once the writer's run loop has returned, for any reason. Terminal:
    /// the writer thread is not restarted in place.
    ended: AtomicBool,
}

impl WriterHealth {
    /// Start marked fresh and running, so the read surface and the watchdog
    /// report healthy from the moment the writer is spawned.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(WriterHealthInner {
                epoch: Instant::now(),
                last_ms: AtomicU64::new(0),
                ended: AtomicBool::new(false),
            }),
        }
    }

    /// Stamp progress. Called once per turn of the writer's run loop.
    pub fn mark(&self) {
        let ms = self.inner.epoch.elapsed().as_millis() as u64;
        self.inner.last_ms.store(ms, Ordering::Relaxed);
    }

    /// Record that the run loop has returned. After this the writer is dead
    /// whatever the stamp says.
    pub fn mark_ended(&self) {
        self.inner.ended.store(true, Ordering::SeqCst);
    }

    /// How long since the last stamp.
    pub fn since_mark(&self) -> Duration {
        let now_ms = self.inner.epoch.elapsed().as_millis() as u64;
        Duration::from_millis(now_ms.saturating_sub(self.inner.last_ms.load(Ordering::Relaxed)))
    }

    /// Whether the writer is actually still persisting: its loop has not
    /// returned and it stamped progress within `budget`.
    pub fn is_alive(&self, budget: Duration) -> bool {
        !self.inner.ended.load(Ordering::SeqCst) && self.since_mark() <= budget
    }
}

impl Default for WriterHealth {
    fn default() -> Self {
        Self::new()
    }
}

/// The dedicated-thread writer. Owns the only read-write connection, the ingest
/// receiver, the live-tail broadcaster, and the current session bookkeeping.
pub struct Writer {
    pub(super) conn: Connection,
    rx: mpsc::Receiver<IngestFrame>,
    /// The control channel the read surface enqueues mark-synced requests on.
    /// Drained between ingest batches so a mark never starves ingest.
    pub(super) control_rx: mpsc::Receiver<ControlMsg>,
    /// A clone of the control sender, so the daemon can wire the read surface
    /// to the writer via [`Writer::control_handle`], symmetric with the
    /// broadcast handle.
    control_tx: mpsc::Sender<ControlMsg>,
    pub(super) broadcast_tx: broadcast::Sender<IngestFrame>,
    pub(super) config: WriterConfig,
    pub(super) db_path: PathBuf,
    /// The boot session opened at start; rows that are not inside a flight
    /// session are attributed to it.
    pub(super) boot_session: i64,
    /// The currently-open flight session, if any. Rows are attributed here while
    /// armed; it closes on disarm.
    pub(super) flight_session: Option<i64>,
    /// Frames persisted since the last WAL truncate.
    pub(super) frames_since_checkpoint: u64,
    /// When the next retention maintenance pass is due. Advanced by the
    /// maintenance interval after each pass.
    pub(super) next_maintenance: Instant,
    /// When the next periodic `VACUUM` is due. Advanced by the vacuum interval
    /// after each vacuum (a maintenance pass that vacuums for any reason resets
    /// this).
    pub(super) next_vacuum: Instant,
    /// The daemon's shutdown-pending flag. While set, the writer starts no new
    /// maintenance pass and skips the `VACUUM` inside one already mid-flight, so a
    /// long rewrite can never overrun the shutdown bound and be torn mid-write.
    pub(super) stop: Arc<AtomicBool>,
    /// The liveness stamp the read surface and the systemd watchdog read. The
    /// run loop stamps it once per turn and marks it ended when it returns.
    health: WriterHealth,
    /// Wall clock read once when this writer opened, and the monotonic instant
    /// it was read at. Comparing the two later detects a clock STEP, which is
    /// what makes an absolute-time retention cutoff unsafe.
    pub(super) session_start_us: i64,
    pub(super) session_start_at: Instant,
    /// Set once a clock step has been observed in this session. Sticky: the
    /// rows already written carry stamps from the pre-step clock, so absolute
    /// age is a lie about them for as long as this writer lives.
    pub(super) clock_stepped: bool,
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
        let session_start_us = now_us();
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
            health: WriterHealth::new(),
            session_start_us,
            session_start_at: now,
            clock_stepped: false,
        })
    }

    /// A handle the daemon clones into the read surface and the watchdog ticker
    /// so both read the writer's real liveness instead of assuming it.
    pub fn health_handle(&self) -> WriterHealth {
        self.health.clone()
    }

    /// Stamp liveness from a sibling module mid-turn, so a long-running step
    /// inside one loop turn is not mistaken for a stall.
    pub(super) fn mark_progress(&self) {
        self.health.mark();
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
    ///
    /// Every turn stamps [`WriterHealth`], and the return marks it ended. That
    /// stamp is the only evidence anything else has that the store is still
    /// recording: `/v1/healthz` reads it, and the daemon feeds the systemd
    /// watchdog only while it advances, so a writer that wedges or exits stops
    /// being reported healthy instead of leaving a dead Black Box behind a
    /// green probe.
    pub fn run(mut self) -> Result<(), WriterError> {
        let health = self.health.clone();
        let out = self.run_inner();
        health.mark_ended();
        out
    }

    fn run_inner(&mut self) -> Result<(), WriterError> {
        let mut batch: Vec<IngestFrame> = Vec::with_capacity(self.config.batch_max_rows);
        loop {
            self.health.mark();
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

#[cfg(test)]
mod tests;
