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
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rusqlite::Connection;
use tokio::sync::{broadcast, mpsc};

use ados_protocol::logd::IngestFrame;

use crate::db;

use self::config::{DRAIN_POLL, IDLE_WAKE};
use self::session::open_session;

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
