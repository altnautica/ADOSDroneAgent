//! Writer tuning knobs, channel capacities, and the writer error type.
//!
//! The batch boundaries, the checkpoint cadence, and the retention maintenance
//! the writer folds into its loop are grouped here so the run loop stays focused
//! on orchestration. Defaults are tuned for SD-card-backed boards; the config
//! layer overrides them at start.

use std::time::Duration;

use crate::db::DbError;
use crate::retention::RetentionConfig;

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
pub(crate) const DRAIN_POLL: Duration = Duration::from_millis(2);

/// Capacity of the live-tail broadcast channel. A subscriber that falls behind
/// loses the oldest buffered frames (lagged), never the writer.
pub const BROADCAST_CAPACITY: usize = 1024;

/// Capacity of the control channel from the read surface to the writer. Small:
/// mark-synced requests are explicit and infrequent, and each caller awaits its
/// reply, so a full channel backpressures the caller, never the writer.
pub const CONTROL_QUEUE_CAPACITY: usize = 32;

/// The longest the run loop waits for a frame before it wakes to check the
/// maintenance deadline. This keeps the writer reactive to its own retention
/// timer even when no frames are flowing, while staying idle-cheap: an idle
/// writer wakes a handful of times a second to glance at the clock and goes back
/// to waiting. It is well under any maintenance interval, so the maintenance
/// cadence is honoured closely.
pub(crate) const IDLE_WAKE: Duration = Duration::from_millis(250);

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
