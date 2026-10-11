//! Retention, rollup, and eviction — the maintenance the single writer runs to
//! keep the store bounded on a small card.
//!
//! All of this runs on the writer thread, against the writer's one read-write
//! connection. There is never a second read-write connection: SQLite is
//! single-writer at the file level, and a second writer is the one corruption
//! trap the design refuses to allow. The writer folds a periodic maintenance
//! call into its drain loop (see [`crate::writer`]); this module is the body of
//! that call, pure over a `&Connection` so a test drives it against a temp store
//! with seeded rows.
//!
//! One maintenance pass runs four steps in order:
//!
//! 1. **Rollup** — fold closed raw-metric windows into the coarse rollup tables
//!    *before* the raw rows can age out, so a long-horizon trend survives the
//!    short raw window. A bucket is rolled up only once it is closed (its window
//!    is entirely in the past), and only raw rows newer than what is already
//!    rolled are folded, so the step is incremental and idempotent.
//! 2. **TTL delete (raw)** — delete raw rows older than the raw window from
//!    `logs`, `metrics`, `events`, and `hw`, in bounded batches so a single
//!    delete never holds a long write transaction.
//! 3. **TTL delete (rollup)** — delete rollup rows older than the (much longer)
//!    rollup window, so even the downsampled history is eventually reclaimed.
//! 4. **Size-cap eviction** — the safety net. When the store file plus its WAL
//!    exceeds the high-water cap, evict the oldest raw rows first, across all
//!    raw tables, down to the low-water mark, and report the freed span so the
//!    writer records a `retention.evicted` event. This is what stops a runaway
//!    producer from wedging the box. The same oldest-first loop also runs when
//!    the *filesystem* the store lives on drops below its free-space reserve
//!    (see [`FsUsage`]), so a card filled by something else cannot turn every
//!    insert into a disk-full error while the store sits under its own cap.
//!
//! Reclaim is **incremental**, never a full rewrite on the eviction path. A full
//! `VACUUM` rewrites the entire file; with the store parked at its size cap and
//! eviction triggering every 45 to 100 minutes, that meant rewriting ~900 MB on
//! that cadence, forever. On an SD card that single behaviour dominated the
//! write wear of the whole box, and a torn rewrite is exactly how the store gets
//! corrupted. So: eviction frees pages onto SQLite's free list, subsequent
//! inserts reuse them, and a bounded `PRAGMA incremental_vacuum` keeps the file
//! from drifting upward. The full `VACUUM` survives only as an escape valve: it
//! runs on its long cadence *and* only when actually warranted — a store still
//! needing the one-time conversion to incremental mode, or a file whose free
//! list has grown past a quarter of it. A healthy incremental store never
//! rewrites itself, which matters beyond write volume: a `VACUUM` is one
//! transaction, so the whole rewrite lands in the WAL, and a ~1 GB store made a
//! 955 MB WAL that the next start then had to recover inside its start timeout.
//!
//! That choice forces a second one. The size cap must then be measured against
//! the **logical** used size plus the WAL, not the raw file size — because a
//! file whose freed pages are being reused legitimately stays at its high-water
//! mark. Triggering on raw file size while no longer shrinking the file would
//! re-trigger eviction on every pass and drain the store to empty.
//!
//! Every window, cap, batch size, and cadence is a constant in this one place.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use rusqlite::{params, Connection};

mod headroom;
mod rollup;
mod size_cap;

pub use headroom::{
    emergency_evict, is_disk_full, read_fs_usage, EmergencyEviction, FsUsage, MIN_FREE_BYTES,
    MIN_FREE_PERCENT,
};
use rollup::*;
pub use size_cap::store_size_bytes;
use size_cap::*;

/// Default raw-row retention window. Raw `logs`/`metrics`/`events`/`hw` rows
/// older than this are deleted (after the metric rows are rolled up).
pub const DEFAULT_RAW_RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Default rollup-row retention window. The downsampled `metrics_1m`/`metrics_1h`
/// rows outlive the raw rows by a wide margin, so year-scale charts survive raw
/// eviction.
pub const DEFAULT_ROLLUP_RETENTION: Duration = Duration::from_secs(365 * 24 * 60 * 60);

/// Default high-water size cap for the store (file plus WAL). Above this the
/// oldest raw rows are evicted down to the low-water mark. Sized for SBC SD/eMMC
/// flash: a 1 GB store evicts to ~850 MB at the default low-water ratio, keeping
/// the post-eviction `VACUUM` fast enough to finish inside the shutdown bound.
pub const DEFAULT_MAX_BYTES: u64 = 1_000_000_000;

/// Default low-water ratio: eviction runs until the store is at or below this
/// fraction of the cap, so it does not run on every single pass once near the
/// cap (it frees a real chunk each time it triggers).
pub const DEFAULT_LOW_WATER_RATIO: f64 = 0.85;

/// Default interval between maintenance passes. Retention is not time-critical;
/// a minute-scale cadence keeps the store bounded without churning the card.
pub const DEFAULT_MAINTENANCE_INTERVAL: Duration = Duration::from_secs(300);

/// Default interval between periodic `VACUUM`s. Weekly: reclaiming freed pages
/// is housekeeping, not urgent, and a `VACUUM` rewrites the file.
pub const DEFAULT_VACUUM_INTERVAL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// How many rows a single TTL/eviction delete removes before committing and
/// looping. Bounds the write-transaction length so a delete never blocks ingest
/// for long, and bounds how much eviction frees per inner step.
pub const DELETE_BATCH_ROWS: usize = 2_000;

/// Upper bound on how many pages one maintenance pass reclaims with
/// `PRAGMA incremental_vacuum`. Bounds how long the reclaim step can hold the
/// writer, the same way [`DELETE_BATCH_ROWS`] bounds a delete. At the usual
/// 4 KiB page size this is ~64 MiB per pass, comfortably more than one
/// eviction's worth of freed pages, so a normal pass reclaims everything it
/// freed while a pathological backlog still cannot turn one pass into an
/// unbounded rewrite.
pub const MAX_INCREMENTAL_VACUUM_PAGES: i64 = 16_384;

/// A safety bound on the number of delete batches one maintenance pass runs, so
/// a pathologically large backlog cannot turn one pass into an unbounded loop.
/// The remainder is handled on the next pass.
pub const MAX_DELETE_BATCHES_PER_PASS: usize = 256;

/// The one-minute rollup bucket width in microseconds.
pub const BUCKET_1M_US: i64 = 60_000_000;

/// The one-hour rollup bucket width in microseconds.
pub const BUCKET_1H_US: i64 = 3_600_000_000;

/// How far the wall clock may drift from the monotonic clock before it counts
/// as a STEP rather than slew. NTP disciplines in milliseconds and slews small
/// offsets; the failure this catches is a jump of months, when an RTC-less
/// board that booted at the kernel build date first reaches a time source.
pub const CLOCK_STEP_TOLERANCE: Duration = Duration::from_secs(60);

/// The systemd-timesyncd marker: written once a reference is acquired. Its
/// presence is the cheapest on-box evidence that the wall clock means
/// something, and it is a single tmpfs `stat`, which matters because the
/// writer thread reads it.
const TIMESYNC_MARKER: &str = "/run/systemd/timesync/synchronized";

/// Whether the retention pass may delete by absolute time.
///
/// Deleting by a `now - window` cutoff asserts that `now` and the row stamps
/// were taken from the same, correct clock. On an SBC with no RTC that is a
/// claim, not a fact, and getting it wrong deletes the flight's own boot window
/// — the thing the Black Box exists to keep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockTrust {
    /// Confirmed against a time source and unstepped since the rows in this
    /// session were stamped: the TTL cutoff means what it says.
    Established,
    /// Never confirmed, or stepped after rows were already written. Age-based
    /// deletes are refused; the size cap, which orders by stored timestamp
    /// rather than trusting it as an age, still bounds the store.
    Unestablished,
}

impl ClockTrust {
    fn allows_ttl(self) -> bool {
        matches!(self, ClockTrust::Established)
    }
}

/// Whether this box's wall clock has been established against a time source.
///
/// Linux: the systemd-timesyncd marker. A synchronous `stat` on tmpfs, so it is
/// safe on the writer thread and needs no subprocess — deliberately narrower
/// than the control front's `ntp_synced` probe, which shells out to `chronyc`
/// and `timedatectl` and cannot run here.
#[cfg(target_os = "linux")]
pub fn wall_clock_established() -> bool {
    std::path::Path::new(TIMESYNC_MARKER).is_file()
}

/// Off Linux there is no RTC-less-SBC failure to guard: a dev host's clock is
/// set by the OS before anything runs. Reporting established keeps retention
/// behaving identically under test on any host, and the deterministic proof of
/// the gate itself is driven by passing [`ClockTrust`] explicitly.
#[cfg(not(target_os = "linux"))]
pub fn wall_clock_established() -> bool {
    let _ = TIMESYNC_MARKER;
    true
}

/// Tunable retention knobs, all in one place. Defaults are baked in; the config
/// layer overrides them at start. `max_bytes` is floored so the store always
/// keeps a usable window even if a config sets it absurdly low.
#[derive(Debug, Clone)]
pub struct RetentionConfig {
    /// Raw rows older than this are deleted.
    pub raw_retention: Duration,
    /// Rollup rows older than this are deleted.
    pub rollup_retention: Duration,
    /// High-water cap (file + WAL) that triggers oldest-first eviction.
    pub max_bytes: u64,
    /// Eviction runs down to this fraction of `max_bytes`.
    pub low_water_ratio: f64,
    /// How often a maintenance pass runs.
    pub maintenance_interval: Duration,
    /// How often a periodic `VACUUM` runs.
    pub vacuum_interval: Duration,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            raw_retention: DEFAULT_RAW_RETENTION,
            rollup_retention: DEFAULT_ROLLUP_RETENTION,
            max_bytes: DEFAULT_MAX_BYTES,
            low_water_ratio: DEFAULT_LOW_WATER_RATIO,
            maintenance_interval: DEFAULT_MAINTENANCE_INTERVAL,
            vacuum_interval: DEFAULT_VACUUM_INTERVAL,
        }
    }
}

/// The smallest cap the store is allowed to enforce. A cap below this would
/// evict so aggressively the store stops being a usable history cache; the
/// config value is clamped up to this.
pub const MIN_MAX_BYTES: u64 = 16_000_000;

/// The low-water ratio is clamped into this inclusive band so a misconfigured
/// value cannot make eviction either never finish (ratio at/above 1.0) or rewrite
/// the whole store on every trigger (ratio near 0).
pub const LOW_WATER_RATIO_MIN: f64 = 0.5;
/// Upper bound on the low-water ratio.
pub const LOW_WATER_RATIO_MAX: f64 = 0.95;

impl RetentionConfig {
    /// Clamp the knobs to sane bounds, returning the corrected config. Called
    /// once at writer start so the rest of the module can trust its fields.
    pub fn clamped(mut self) -> Self {
        self.max_bytes = self.max_bytes.max(MIN_MAX_BYTES);
        if !self.low_water_ratio.is_finite() {
            self.low_water_ratio = DEFAULT_LOW_WATER_RATIO;
        }
        self.low_water_ratio = self
            .low_water_ratio
            .clamp(LOW_WATER_RATIO_MIN, LOW_WATER_RATIO_MAX);
        self
    }

    /// The low-water target in bytes: eviction stops once the store is at or
    /// below this.
    fn low_water_bytes(&self) -> u64 {
        (self.max_bytes as f64 * self.low_water_ratio) as u64
    }
}

/// What a maintenance pass did, returned so the writer can emit a
/// `retention.evicted` event and log a one-line summary. All counts are for the
/// pass just run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MaintenanceReport {
    /// Rollup rows written/updated into the 1m and 1h tables.
    pub rolled_up_rows: u64,
    /// Raw rows deleted by the TTL window across all raw tables.
    pub ttl_deleted_rows: u64,
    /// Rollup rows deleted by the rollup TTL window.
    pub rollup_ttl_deleted_rows: u64,
    /// Raw rows evicted by the size cap. Zero when the store was under the cap.
    pub evicted_rows: u64,
    /// Oldest timestamp (µs) freed by eviction, if any rows were evicted.
    pub evicted_from_us: Option<i64>,
    /// Newest timestamp (µs) freed by eviction, if any rows were evicted.
    pub evicted_to_us: Option<i64>,
    /// Whether this pass ran a full `VACUUM`. Only ever true on the periodic
    /// cadence — eviction never triggers one.
    pub vacuumed: bool,
    /// Pages returned to the filesystem by the bounded `incremental_vacuum` this
    /// pass ran after an eviction. Zero when nothing was evicted, or when the
    /// store is not in incremental auto-vacuum mode.
    pub reclaimed_pages: u64,
}

impl MaintenanceReport {
    /// True if the size cap evicted any rows this pass, so the writer should
    /// emit the `retention.evicted` event.
    pub fn had_eviction(&self) -> bool {
        self.evicted_rows > 0
    }
}

/// The raw tables retention sweeps over. Listed once so TTL and eviction agree
/// on the set. Every one carries a `ts_us` and an integer `id` primary key.
const RAW_TABLES: [&str; 4] = ["logs", "metrics", "events", "hw"];

/// Run one full maintenance pass on the writer's connection. `now_us` is the
/// current microsecond-epoch time, passed in so a test drives deterministic
/// windows. `db_path` is the store file, used to measure the on-disk size for
/// the cap. `do_vacuum` is decided by the caller's vacuum cadence; when true the
/// pass ends with a `VACUUM`.
///
/// Order is rollup → raw TTL → rollup TTL → size-cap eviction → optional vacuum.
/// Rollup runs first so no metric row is deleted before it is downsampled.
///
/// `stop` is the daemon's shutdown-pending flag: the cheap deletes always run, but
/// the one long, uninterruptible step (`VACUUM`) is skipped when a stop is in
/// flight so it can never overrun the writer-join bound and get torn mid-rewrite
/// (which would leave the WAL needing recovery on the next start). The file simply
/// stays above the low-water mark until the next pass after a clean start vacuums.
///
/// `clock` decides whether the two TTL steps run at all. They are the only
/// steps that treat a stored timestamp as an *age*, and an age is meaningless
/// on a board whose clock has not been established or has stepped since the
/// rows were stamped — the first pass after an RTC-less board first reaches
/// NTP would otherwise delete that flight's whole boot window. Rollup and the
/// size cap are unaffected: they order rows, they do not date them, so the
/// store stays bounded even on a node that never sees a time source.
///
/// `fs` is the free space of the filesystem holding the store, measured by the
/// caller ([`read_fs_usage`] in production, a fixed value under test). Below the
/// reserve it arms eviction even under the byte cap. `None` (an unreadable
/// filesystem, or a host without `statvfs`) leaves only the byte cap armed —
/// a measurement failure never evicts.
#[allow(clippy::too_many_arguments)]
pub fn run_maintenance(
    conn: &Connection,
    cfg: &RetentionConfig,
    now_us: i64,
    db_path: &Path,
    do_vacuum: bool,
    stop: &AtomicBool,
    clock: ClockTrust,
    fs: Option<FsUsage>,
) -> Result<MaintenanceReport, rusqlite::Error> {
    // Step 1: roll up closed metric windows before any raw row can age out.
    let rolled_up_rows = roll_up(conn, now_us)?;

    // Steps 2 and 3: TTL-delete aged raw rows, then aged rollup rows (a much
    // longer window). Both are refused outright on an untrustworthy clock.
    let (ttl_deleted_rows, rollup_ttl_deleted_rows) = if clock.allows_ttl() {
        let raw_cutoff = now_us.saturating_sub(cfg.raw_retention.as_micros() as i64);
        let rollup_cutoff = now_us.saturating_sub(cfg.rollup_retention.as_micros() as i64);
        (
            ttl_delete_raw(conn, raw_cutoff)?,
            ttl_delete_rollups(conn, rollup_cutoff)?,
        )
    } else {
        (0, 0)
    };

    // Step 4: size-cap and free-space eviction, the safety net.
    let eviction = enforce_size_cap(conn, cfg, db_path, fs)?;

    // Reclaim. Two different mechanisms, deliberately not interchangeable:
    //
    // - After an eviction, a **bounded** `incremental_vacuum`. Eviction happens
    //   every 45 to 100 minutes once the store is parked at its cap, so whatever
    //   runs here runs on that cadence forever. A full `VACUUM` here rewrote the
    //   entire ~900 MB file each time, which on flash is both the dominant write
    //   load on the box and the window in which a power cut tears the store.
    // - On the caller's own long cadence, the full `VACUUM`, which is the only
    //   thing that truly compacts a file whose workload has shrunk.
    //
    // The `TRUNCATE` checkpoint after either one flushes the WAL frames the
    // rewrite produced back into the main file and shrinks the `-wal` sidecar;
    // without it the on-disk footprint would stay high even though the logical
    // data is small.
    let stopping = stop.load(Ordering::Relaxed);
    // A low-headroom pass reclaims even when it evicted nothing: the free list
    // may already hold the pages the filesystem is short of.
    let reclaim = eviction.rows > 0 || eviction.low_headroom;
    let reclaimed_pages = if reclaim && !stopping {
        incremental_vacuum(conn, eviction.reclaim_pages)?
    } else {
        0
    };
    let vacuumed = if do_vacuum && !stopping && full_vacuum_warranted(conn)? {
        // A legacy store adopts incremental auto-vacuum across a VACUUM, and
        // this is the only VACUUM that runs. Asking for it here means the
        // conversion rides a rewrite that was going to happen anyway, instead
        // of being a whole-file rewrite in front of daemon readiness — which,
        // when it was tried there, put the unit minutes from a start-timeout
        // kill mid-rewrite on a real node.
        let _ = crate::db::request_incremental_auto_vacuum(conn);
        conn.execute_batch("VACUUM")?;
        true
    } else {
        false
    };
    if (vacuumed || reclaim) && !stopping {
        conn.pragma_update(None, "wal_checkpoint", "TRUNCATE")?;
    }

    Ok(MaintenanceReport {
        rolled_up_rows,
        ttl_deleted_rows,
        rollup_ttl_deleted_rows,
        evicted_rows: eviction.rows,
        evicted_from_us: eviction.from_us,
        evicted_to_us: eviction.to_us,
        vacuumed,
        reclaimed_pages,
    })
}

/// The free-list fraction above which a full `VACUUM` is worth its cost.
///
/// Below this, incremental reclaim is keeping up and a whole-file rewrite would
/// buy nothing for a large, and dangerous, price.
pub const FULL_VACUUM_FREELIST_RATIO: f64 = 0.25;

/// Whether the periodic full `VACUUM` still has a job to do.
///
/// Once the store is in incremental auto-vacuum mode, it usually does not. That
/// matters for more than write volume: a `VACUUM` runs as one transaction, so
/// `wal_autocheckpoint` cannot fire inside it and the whole rewrite lands in the
/// WAL. On a ~1 GB store that produced a **955 MB WAL** on a real node, and the
/// next start had to recover it — inside a `TimeoutStartSec` whose expiry would
/// restart the daemon straight back into the same recovery. Every full `VACUUM`
/// is therefore a scheduled opportunity to create that condition.
///
/// So it now runs only when it is actually warranted:
///
/// - the store has not adopted incremental mode yet, and needs one rewrite to
///   (this is the conversion path, and it happens once); or
/// - the free list has grown past [`FULL_VACUUM_FREELIST_RATIO`] of the file,
///   meaning incremental reclaim has genuinely fallen behind and the file is
///   carrying real slack.
///
/// A store in incremental mode with a healthy free list skips it entirely, which
/// removes the last whole-file rewrite from the steady state.
fn full_vacuum_warranted(conn: &Connection) -> Result<bool, rusqlite::Error> {
    if crate::db::auto_vacuum_mode(conn).unwrap_or(0) != crate::db::AUTO_VACUUM_INCREMENTAL {
        return Ok(true);
    }
    let page_count: i64 = conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
    let freelist: i64 = conn.query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
    if page_count <= 0 {
        return Ok(false);
    }
    Ok(freelist as f64 / page_count as f64 >= FULL_VACUUM_FREELIST_RATIO)
}

/// Return up to `max_pages` free pages to the filesystem, and report how many
/// actually went back.
///
/// `PRAGMA incremental_vacuum` is a no-op on a store that is not in incremental
/// auto-vacuum mode, and SQLite reports no count, so the freed page total is
/// measured as the drop in `freelist_count` across the call. That also makes the
/// no-op case read honestly as zero rather than as an assumed success.
///
/// The pragma frees one page per VDBE step (it yields a row per page), so it is
/// stepped to completion here; `execute_batch` steps a statement only once and
/// would hand back a single page whatever `max_pages` said.
fn incremental_vacuum(conn: &Connection, max_pages: i64) -> Result<u64, rusqlite::Error> {
    let before: i64 = conn.query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
    if before <= 0 {
        return Ok(0);
    }
    {
        let mut stmt = conn.prepare(&format!("PRAGMA incremental_vacuum({max_pages})"))?;
        let mut rows = stmt.query([])?;
        while rows.next()?.is_some() {}
    }
    let after: i64 = conn.query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
    Ok((before - after).max(0) as u64)
}

#[cfg(test)]
mod tests;
