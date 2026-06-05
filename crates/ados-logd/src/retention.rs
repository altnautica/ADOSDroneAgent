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
//!    producer from wedging the box.
//!
//! A periodic `VACUUM` runs on a long, separate cadence (and once after a large
//! eviction) to reclaim the freed pages; it is never on the hot path.
//!
//! Every window, cap, batch size, and cadence is a constant in this one place.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use rusqlite::{params, Connection};

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

/// A safety bound on the number of delete batches one maintenance pass runs, so
/// a pathologically large backlog cannot turn one pass into an unbounded loop.
/// The remainder is handled on the next pass.
pub const MAX_DELETE_BATCHES_PER_PASS: usize = 256;

/// The one-minute rollup bucket width in microseconds.
pub const BUCKET_1M_US: i64 = 60_000_000;

/// The one-hour rollup bucket width in microseconds.
pub const BUCKET_1H_US: i64 = 3_600_000_000;

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
    /// Whether this pass ran a `VACUUM`.
    pub vacuumed: bool,
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
pub fn run_maintenance(
    conn: &Connection,
    cfg: &RetentionConfig,
    now_us: i64,
    db_path: &Path,
    do_vacuum: bool,
    stop: &AtomicBool,
) -> Result<MaintenanceReport, rusqlite::Error> {
    // Step 1: roll up closed metric windows before any raw row can age out.
    let rolled_up_rows = roll_up(conn, now_us)?;

    // Step 2: TTL-delete aged raw rows.
    let raw_cutoff = now_us.saturating_sub(cfg.raw_retention.as_micros() as i64);
    let ttl_deleted_rows = ttl_delete_raw(conn, raw_cutoff)?;

    // Step 3: TTL-delete aged rollup rows (a much longer window).
    let rollup_cutoff = now_us.saturating_sub(cfg.rollup_retention.as_micros() as i64);
    let rollup_ttl_deleted_rows = ttl_delete_rollups(conn, rollup_cutoff)?;

    // Step 4: size-cap eviction, the safety net.
    let eviction = enforce_size_cap(conn, cfg, db_path)?;

    // VACUUM on the caller's cadence, or always after a large eviction freed
    // pages that would otherwise leave the file fragmented and oversized. The
    // VACUUM itself is written through the WAL, so a TRUNCATE checkpoint after it
    // flushes those frames back into the (now compact) main file and shrinks the
    // `-wal` sidecar — without it the on-disk footprint (main + WAL) would stay
    // above the cap even though the logical data is small.
    let vacuumed = if (do_vacuum || eviction.rows > 0) && !stop.load(Ordering::Relaxed) {
        conn.execute_batch("VACUUM")?;
        conn.pragma_update(None, "wal_checkpoint", "TRUNCATE")?;
        true
    } else {
        false
    };

    Ok(MaintenanceReport {
        rolled_up_rows,
        ttl_deleted_rows,
        rollup_ttl_deleted_rows,
        evicted_rows: eviction.rows,
        evicted_from_us: eviction.from_us,
        evicted_to_us: eviction.to_us,
        vacuumed,
    })
}

// --- step 1: rollup ------------------------------------------------------

/// Fold closed raw-metric windows into the 1m and 1h rollup tables. Returns the
/// number of rollup rows inserted or updated.
///
/// A bucket is "closed" once `now` has passed its end, so a bucket still
/// accumulating raw rows is never rolled (it would be rolled partially and then
/// be wrong). For each grain, the floor of the latest already-rolled bucket is
/// the lower bound, so the step is incremental: only raw rows at or after that
/// bound and inside a closed bucket are folded. Re-running over the same window
/// is idempotent because the per-grain `INSERT ... ON CONFLICT` recomputes the
/// closed bucket from the raw rows rather than accumulating onto the prior value.
fn roll_up(conn: &Connection, now_us: i64) -> Result<u64, rusqlite::Error> {
    let mut written = 0u64;
    written += roll_up_grain(conn, now_us, "metrics_1m", BUCKET_1M_US)?;
    written += roll_up_grain(conn, now_us, "metrics_1h", BUCKET_1H_US)?;
    Ok(written)
}

/// Roll one grain. `bucket_us` is the bucket width; `table` is the destination
/// rollup table. Only buckets whose end is at or before `now` are rolled.
fn roll_up_grain(
    conn: &Connection,
    now_us: i64,
    table: &str,
    bucket_us: i64,
) -> Result<u64, rusqlite::Error> {
    // The newest fully-closed bucket start: the latest bucket whose end (start +
    // width) is at or before now. `floor(now/width)*width` is the bucket `now`
    // falls in; that one is still open, so the last closed bucket starts one
    // width earlier.
    let current_bucket = (now_us / bucket_us) * bucket_us;
    let last_closed_bucket = current_bucket - bucket_us;
    if last_closed_bucket < 0 {
        // Before the first full bucket of the epoch: nothing to roll. Guards the
        // synthetic-low-timestamp case a test can produce.
        return Ok(0);
    }

    // Lower bound: only re-roll buckets at or after the latest one already in the
    // table, so a steady run does bounded work. A fresh table rolls everything up
    // to the last closed bucket.
    let already_rolled_to: i64 = conn.query_row(
        &format!("SELECT coalesce(max(bucket_us), -1) FROM {table}"),
        [],
        |r| r.get(0),
    )?;
    // Start one bucket-width after the last fully-rolled bucket. The latest rolled
    // bucket is recomputed too in case raw rows landed late inside it.
    let from_bucket = already_rolled_to.max(0);
    let from_ts = if already_rolled_to < 0 {
        // No prior rollup: include all raw rows in closed buckets.
        i64::MIN / 2
    } else {
        from_bucket
    };

    // The upsert: aggregate raw `metrics` rows that fall into closed buckets at
    // or after the lower bound, grouped by (metric, tags_key, bucket). `tags_key`
    // groups distinct tag sets apart; raw tags are an opaque blob, so the stable
    // grouping key is the hex of the blob (NULL tags collapse to the empty key).
    //
    // A windowed inner query tags every row with the value at the latest
    // timestamp in its bucket (`last_value` over the bucket partition ordered by
    // ts, with an unbounded frame so the partition's final row is visible from
    // every row). The outer aggregate then takes count/sum/min/max plus that
    // already-resolved per-bucket last value (constant within the group, so
    // `max(last)` simply reads it). This is deterministic — it does not rely on
    // SQLite's single-aggregate bare-column rule, which would be ambiguous here
    // with both `min(value)` and `max(value)` present. The conflict target
    // recomputes the bucket fresh, so re-rolling a bucket that gained late rows
    // replaces the prior aggregate rather than double counting.
    let sql = format!(
        "INSERT INTO {table} \
           (bucket_us, metric, tags_key, count, sum, min, max, last, last_us) \
         SELECT \
           b, \
           metric, \
           tk, \
           count(*), \
           sum(value), \
           min(value), \
           max(value), \
           max(bucket_last), \
           max(ts_us) \
         FROM ( \
           SELECT \
             (ts_us / ?1) * ?1 AS b, \
             metric, \
             coalesce(lower(hex(tags)), '') AS tk, \
             value, \
             ts_us, \
             last_value(value) OVER ( \
               PARTITION BY metric, coalesce(lower(hex(tags)), ''), (ts_us / ?1) * ?1 \
               ORDER BY ts_us \
               ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING \
             ) AS bucket_last \
           FROM metrics \
           WHERE ts_us < ?2 AND ts_us >= ?3 \
         ) \
         GROUP BY metric, tk, b \
         ON CONFLICT (metric, tags_key, bucket_us) DO UPDATE SET \
           count = excluded.count, \
           sum   = excluded.sum, \
           min   = excluded.min, \
           max   = excluded.max, \
           last  = excluded.last, \
           last_us = excluded.last_us"
    );

    // Closed-bucket upper bound is the start of the current (open) bucket: any
    // ts_us strictly below it is in a closed bucket.
    let closed_upper = current_bucket;
    let changed = conn.execute(&sql, params![bucket_us, closed_upper, from_ts])?;
    Ok(changed as u64)
}

// --- step 2 + 3: TTL deletes --------------------------------------------

/// Delete raw rows older than `cutoff_us` from every raw table, in bounded
/// batches. Returns the total rows deleted across tables.
fn ttl_delete_raw(conn: &Connection, cutoff_us: i64) -> Result<u64, rusqlite::Error> {
    let mut total = 0u64;
    for table in RAW_TABLES {
        total += delete_older_than(conn, table, cutoff_us)?;
    }
    Ok(total)
}

/// Delete rollup rows older than `cutoff_us` from both rollup tables. Rollup
/// tables key on `bucket_us`, not `ts_us`, and have no integer `id`, so the
/// delete is a direct ranged delete (already bounded by the long window).
fn ttl_delete_rollups(conn: &Connection, cutoff_us: i64) -> Result<u64, rusqlite::Error> {
    let mut total = 0u64;
    for table in ["metrics_1m", "metrics_1h"] {
        let n = conn.execute(
            &format!("DELETE FROM {table} WHERE bucket_us < ?1"),
            params![cutoff_us],
        )?;
        total += n as u64;
    }
    Ok(total)
}

/// Delete rows of one raw table older than `cutoff_us` in batches of at most
/// [`DELETE_BATCH_ROWS`], committing implicitly per statement so no single
/// delete holds a long write lock. Stops when a batch deletes nothing (the table
/// is clear of the window) or the per-pass batch bound is hit.
fn delete_older_than(
    conn: &Connection,
    table: &str,
    cutoff_us: i64,
) -> Result<u64, rusqlite::Error> {
    let sql = format!(
        "DELETE FROM {table} WHERE id IN \
         (SELECT id FROM {table} WHERE ts_us < ?1 ORDER BY ts_us LIMIT ?2)"
    );
    let mut total = 0u64;
    for _ in 0..MAX_DELETE_BATCHES_PER_PASS {
        let n = conn.execute(&sql, params![cutoff_us, DELETE_BATCH_ROWS as i64])?;
        total += n as u64;
        if n < DELETE_BATCH_ROWS {
            break;
        }
    }
    Ok(total)
}

// --- step 4: size-cap eviction ------------------------------------------

/// The outcome of size-cap eviction: how many rows were evicted and the
/// timestamp span they covered (for the `retention.evicted` event).
#[derive(Debug, Clone, Default)]
struct Eviction {
    rows: u64,
    from_us: Option<i64>,
    to_us: Option<i64>,
}

/// Evict the oldest raw rows first, across all raw tables, until the store is at
/// or below the low-water mark, or there is nothing left to evict, or the
/// per-pass batch bound is hit. Returns the eviction span so the writer can
/// record it.
///
/// Eviction is the safety net for a small card: TTL handles the steady state,
/// this handles a spike. It is strictly oldest-first by `ts_us`, so the rollups
/// (which are not evicted here) keep the long-horizon shape even after the raw
/// rows for that span are gone.
///
/// Two distinct size measures are used, deliberately:
///
/// - The **trigger** is the on-disk footprint ([`store_size_bytes`]: main file +
///   WAL). That is the honest "how big is the store on the card" the cap is
///   meant to bound, and it is what crosses the high-water mark.
/// - The **stopping condition** is the *logical used size* ([`logical_used_bytes`]:
///   `(page_count - freelist_count) * page_size`). A plain `DELETE` moves pages
///   onto SQLite's free list but does **not** shrink the main file; only the
///   trailing `VACUUM` reclaims them. So measuring the file size inside the loop
///   would never drop below low-water and the loop would evict everything. The
///   logical used size *does* fall as rows are deleted, and it is exactly what
///   the trailing `VACUUM` will shrink the file down to — so it is the correct
///   stop. The caller always vacuums after an eviction, making the on-disk file
///   match the logical size again.
fn enforce_size_cap(
    conn: &Connection,
    cfg: &RetentionConfig,
    db_path: &Path,
) -> Result<Eviction, rusqlite::Error> {
    if store_size_bytes(db_path) <= cfg.max_bytes {
        return Ok(Eviction::default());
    }

    let low_water = cfg.low_water_bytes();
    let mut ev = Eviction::default();
    for _ in 0..MAX_DELETE_BATCHES_PER_PASS {
        if logical_used_bytes(conn)? <= low_water {
            break;
        }
        // Find the oldest ts_us across all raw tables and evict one batch from
        // the table holding it, oldest-first. One table at a time keeps each
        // delete simple and the global ordering correct (the global oldest is
        // always evicted next).
        let Some((table, _oldest)) = oldest_raw_row(conn)? else {
            break; // every raw table is empty
        };
        let batch = evict_batch(conn, table)?;
        if batch.rows == 0 {
            break;
        }
        ev.rows += batch.rows;
        ev.from_us = Some(match ev.from_us {
            Some(existing) => existing.min(batch.from_us),
            None => batch.from_us,
        });
        ev.to_us = Some(match ev.to_us {
            Some(existing) => existing.max(batch.to_us),
            None => batch.to_us,
        });
    }
    Ok(ev)
}

/// The store's logical used size in bytes: `(page_count - freelist_count) *
/// page_size`. This is the size the file would have after a `VACUUM`, so it is
/// the right measure for the eviction stopping condition (file size does not
/// shrink until the vacuum). Page count includes the WAL frames not yet
/// checkpointed, so this is conservative.
fn logical_used_bytes(conn: &Connection) -> Result<u64, rusqlite::Error> {
    let page_count: i64 = conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
    let freelist: i64 = conn.query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
    let page_size: i64 = conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
    let used_pages = (page_count - freelist).max(0);
    Ok(used_pages as u64 * page_size.max(0) as u64)
}

/// The span and count of one evicted batch.
#[derive(Debug, Clone, Default)]
struct EvictBatch {
    rows: u64,
    from_us: i64,
    to_us: i64,
}

/// Find the table holding the globally-oldest raw row and that row's `ts_us`.
/// Returns `None` when every raw table is empty.
fn oldest_raw_row(conn: &Connection) -> Result<Option<(&'static str, i64)>, rusqlite::Error> {
    let mut best: Option<(&'static str, i64)> = None;
    for table in RAW_TABLES {
        let min_ts: Option<i64> =
            conn.query_row(&format!("SELECT min(ts_us) FROM {table}"), [], |r| r.get(0))?;
        if let Some(ts) = min_ts {
            // Keep the strictly-older candidate; the existing best wins ties so
            // the scan is stable across the fixed table order.
            let replace = match best {
                Some((_, b)) => ts < b,
                None => true,
            };
            if replace {
                best = Some((table, ts));
            }
        }
    }
    Ok(best)
}

/// Evict one oldest-first batch from `table`, returning the count and the span
/// of the rows removed.
fn evict_batch(conn: &Connection, table: &'static str) -> Result<EvictBatch, rusqlite::Error> {
    // The ids of the oldest batch, captured first so the span can be measured
    // before the rows are gone.
    let span: Option<(i64, i64)> = conn
        .query_row(
            &format!(
                "SELECT min(ts_us), max(ts_us) FROM \
                 (SELECT ts_us FROM {table} ORDER BY ts_us LIMIT ?1)"
            ),
            params![DELETE_BATCH_ROWS as i64],
            |r| Ok((r.get::<_, Option<i64>>(0)?, r.get::<_, Option<i64>>(1)?)),
        )
        .map(|(a, b)| match (a, b) {
            (Some(a), Some(b)) => Some((a, b)),
            _ => None,
        })?;
    let Some((from_us, to_us)) = span else {
        return Ok(EvictBatch::default());
    };
    let n = conn.execute(
        &format!(
            "DELETE FROM {table} WHERE id IN \
             (SELECT id FROM {table} ORDER BY ts_us LIMIT ?1)"
        ),
        params![DELETE_BATCH_ROWS as i64],
    )?;
    Ok(EvictBatch {
        rows: n as u64,
        from_us,
        to_us,
    })
}

/// The on-disk store size: the main file plus its `-wal` sidecar. Pages freed by
/// a delete sit in the file (or the WAL) until a checkpoint/vacuum reclaims them,
/// so this is the honest "how big is the store on the card" measure the cap
/// enforces. Returns zero for an unreadable path (the cap then never triggers,
/// which is the safe direction — it never evicts on a measurement error).
pub fn store_size_bytes(db_path: &Path) -> u64 {
    let main = std::fs::metadata(db_path).map(|m| m.len()).unwrap_or(0);
    let wal = wal_path(db_path)
        .and_then(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .unwrap_or(0);
    main + wal
}

/// The `-wal` companion path for a store file.
fn wal_path(db_path: &Path) -> Option<std::path::PathBuf> {
    let name = db_path.file_name()?.to_str()?;
    Some(db_path.with_file_name(format!("{name}-wal")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    /// One microsecond-epoch base far enough into 2023 that bucket math is
    /// well-defined and the synthetic-low-timestamp guard is not in play.
    const BASE_US: i64 = 1_700_000_000_000_000;

    fn temp_store() -> (tempfile::TempDir, std::path::PathBuf, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs.db");
        let conn = db::open(&path).unwrap();
        let session = open_boot(&conn);
        // Stash the session id where the seed helpers can reach it via a thread
        // local would be overkill; instead seed helpers take the session.
        let _ = session;
        (dir, path, conn)
    }

    fn open_boot(conn: &Connection) -> i64 {
        conn.execute(
            "INSERT INTO sessions (started_us, kind, reason) VALUES (?1, 'boot', 'start')",
            params![BASE_US],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn seed_metric(conn: &Connection, ts_us: i64, metric: &str, value: f64) {
        conn.execute(
            "INSERT INTO metrics (ts_us, metric, value) VALUES (?1, ?2, ?3)",
            params![ts_us, metric, value],
        )
        .unwrap();
    }

    fn seed_log(conn: &Connection, ts_us: i64) {
        conn.execute(
            "INSERT INTO logs (ts_us, source, level, msg) VALUES (?1, 'test', 2, 'm')",
            params![ts_us],
        )
        .unwrap();
    }

    fn count(conn: &Connection, table: &str) -> i64 {
        conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn rollup_aggregates_closed_minute_buckets_and_leaves_the_open_one() {
        let (_dir, _path, conn) = temp_store();
        // Three samples in one closed minute bucket, one in the open (current)
        // bucket. now sits inside the bucket after the closed one.
        let closed_bucket = (BASE_US / BUCKET_1M_US) * BUCKET_1M_US;
        seed_metric(&conn, closed_bucket + 1_000, "cpu.util.all", 10.0);
        seed_metric(&conn, closed_bucket + 2_000, "cpu.util.all", 30.0);
        seed_metric(&conn, closed_bucket + 3_000, "cpu.util.all", 20.0);
        // A sample in the still-open next bucket must NOT be rolled.
        let open_bucket = closed_bucket + BUCKET_1M_US;
        seed_metric(&conn, open_bucket + 1_000, "cpu.util.all", 99.0);

        // now is inside the open bucket, so only the first bucket is closed.
        let now = open_bucket + 5_000;
        let written = roll_up(&conn, now).unwrap();
        assert!(written >= 1, "at least the 1m closed bucket was rolled");

        let (cnt, sum, mn, mx, last): (i64, f64, f64, f64, f64) = conn
            .query_row(
                "SELECT count, sum, min, max, last FROM metrics_1m \
                 WHERE metric='cpu.util.all' AND bucket_us=?1",
                params![closed_bucket],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!(cnt, 3, "three raw rows folded");
        assert_eq!(sum, 60.0);
        assert_eq!(mn, 10.0);
        assert_eq!(mx, 30.0);
        assert_eq!(
            last, 20.0,
            "last is the value at the latest ts in the bucket"
        );

        // The open bucket was not rolled.
        let open_rolled: i64 = conn
            .query_row(
                "SELECT count(*) FROM metrics_1m WHERE bucket_us=?1",
                params![open_bucket],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(open_rolled, 0, "the open bucket must not be rolled");
    }

    #[test]
    fn rollup_is_idempotent_and_groups_distinct_tag_sets() {
        let (_dir, _path, conn) = temp_store();
        let bucket = (BASE_US / BUCKET_1M_US) * BUCKET_1M_US;
        // Two metrics that share a name but differ by tags must roll into two
        // distinct rollup rows.
        conn.execute(
            "INSERT INTO metrics (ts_us, metric, value, tags) VALUES (?1, 'net.rx', 100.0, ?2)",
            params![
                bucket + 1_000,
                rmp_serde::to_vec_named(&tag("wlan0")).unwrap()
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO metrics (ts_us, metric, value, tags) VALUES (?1, 'net.rx', 200.0, ?2)",
            params![
                bucket + 2_000,
                rmp_serde::to_vec_named(&tag("eth0")).unwrap()
            ],
        )
        .unwrap();
        let now = bucket + BUCKET_1M_US + 5_000;

        roll_up(&conn, now).unwrap();
        let rows_1: i64 = conn
            .query_row(
                "SELECT count(*) FROM metrics_1m WHERE metric='net.rx'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rows_1, 2, "two distinct tag sets roll into two rows");

        // Running rollup again does not double-count: same row count, same sums.
        roll_up(&conn, now).unwrap();
        let rows_2: i64 = conn
            .query_row(
                "SELECT count(*) FROM metrics_1m WHERE metric='net.rx'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rows_2, 2, "re-roll is idempotent on row count");
        let total_count: i64 = conn
            .query_row(
                "SELECT sum(count) FROM metrics_1m WHERE metric='net.rx'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(total_count, 2, "re-roll did not double the folded count");
    }

    fn tag(iface: &str) -> super::super::wire::Fields {
        let mut f = super::super::wire::Fields::new();
        f.insert("iface".to_string(), rmpv::Value::from(iface));
        f
    }

    #[test]
    fn raw_ttl_deletes_old_rows_and_keeps_recent_ones() {
        let (_dir, path, conn) = temp_store();
        let cfg = RetentionConfig::default();
        let now = BASE_US;
        let old = now - (40 * 24 * 60 * 60) * 1_000_000; // 40 days old
        let recent = now - (60 * 1_000_000); // a minute old

        seed_metric(&conn, old, "cpu.util.all", 1.0);
        seed_log(&conn, old);
        seed_metric(&conn, recent, "cpu.util.all", 2.0);
        seed_log(&conn, recent);

        let report =
            run_maintenance(&conn, &cfg, now, &path, false, &AtomicBool::new(false)).unwrap();
        assert!(report.ttl_deleted_rows >= 2, "old rows were TTL-deleted");
        // The old metric was rolled up before deletion, so its long-horizon shape
        // survives even though the raw row is gone.
        assert!(
            report.rolled_up_rows >= 1,
            "the old metric was rolled before being TTL-deleted"
        );
        // The recent rows survive; the old rows are gone.
        assert_eq!(count(&conn, "metrics"), 1, "only the recent metric remains");
        assert_eq!(count(&conn, "logs"), 1, "only the recent log remains");
        let surviving_ts: i64 = conn
            .query_row("SELECT ts_us FROM metrics", [], |r| r.get(0))
            .unwrap();
        assert_eq!(surviving_ts, recent);
        // The rolled-up bucket for the old metric is still present (rollup TTL is
        // a year, far beyond the 40-day-old row).
        let rolled: i64 = conn
            .query_row("SELECT count(*) FROM metrics_1m", [], |r| r.get(0))
            .unwrap();
        assert!(rolled >= 1, "rolled bucket survives raw TTL");
    }

    #[test]
    fn rollup_ttl_deletes_buckets_older_than_the_rollup_window() {
        let (_dir, path, conn) = temp_store();
        let cfg = RetentionConfig::default();
        let now = BASE_US;
        // A rollup bucket two years old, inserted directly, must be reaped by the
        // one-year rollup TTL.
        let ancient_bucket =
            ((now - (730 * 24 * 60 * 60) * 1_000_000) / BUCKET_1M_US) * BUCKET_1M_US;
        conn.execute(
            "INSERT INTO metrics_1m (bucket_us, metric, tags_key, count, sum, min, max, last, last_us) \
             VALUES (?1, 'cpu.util.all', '', 5, 50.0, 5.0, 15.0, 10.0, ?1)",
            params![ancient_bucket],
        )
        .unwrap();
        // A recent rollup bucket must survive.
        let recent_bucket = (now / BUCKET_1M_US) * BUCKET_1M_US - BUCKET_1M_US;
        conn.execute(
            "INSERT INTO metrics_1m (bucket_us, metric, tags_key, count, sum, min, max, last, last_us) \
             VALUES (?1, 'cpu.util.all', '', 5, 50.0, 5.0, 15.0, 10.0, ?1)",
            params![recent_bucket],
        )
        .unwrap();

        let report =
            run_maintenance(&conn, &cfg, now, &path, false, &AtomicBool::new(false)).unwrap();
        assert!(
            report.rollup_ttl_deleted_rows >= 1,
            "the two-year-old rollup bucket was reaped"
        );
        let remaining: i64 = conn
            .query_row(
                "SELECT count(*) FROM metrics_1m WHERE bucket_us=?1",
                params![recent_bucket],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(remaining, 1, "the recent rollup bucket survives");
        let ancient: i64 = conn
            .query_row(
                "SELECT count(*) FROM metrics_1m WHERE bucket_us=?1",
                params![ancient_bucket],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(ancient, 0, "the ancient rollup bucket is gone");
    }

    /// Insert `n` fat log rows spanning microsecond timestamps below `now`, the
    /// oldest first, so the store crosses a small cap and the size guard has an
    /// oldest-first ordering to evict by. Returns the timestamp of the oldest row.
    /// All rows are recent enough that the TTL window never touches them, so the
    /// size cap is the only mechanism that can evict here.
    fn seed_bulk_logs(conn: &Connection, now: i64, n: i64, msg_len: usize) -> i64 {
        let tx = conn.unchecked_transaction().unwrap();
        let oldest = now - n * 1_000;
        for i in 0..n {
            tx.execute(
                "INSERT INTO logs (ts_us, source, level, msg) VALUES (?1, 'bulk', 2, ?2)",
                params![now - (n - i) * 1_000, "x".repeat(msg_len)],
            )
            .unwrap();
        }
        tx.commit().unwrap();
        oldest
    }

    #[test]
    fn size_cap_evicts_oldest_first_down_to_low_water_and_reports_the_span() {
        let (_dir, path, conn) = temp_store();
        let now = BASE_US;
        // Cross the real floored cap deterministically: ~70k rows of half-kilobyte
        // messages is comfortably above the 16 MB floor, so eviction must run
        // regardless of host. Using the production floor as the cap exercises the
        // real clamp path, not a synthetic tiny cap.
        let oldest = seed_bulk_logs(&conn, now, 70_000, 512);
        conn.pragma_update(None, "wal_checkpoint", "TRUNCATE")
            .unwrap();

        let before = count(&conn, "logs");
        let cfg = RetentionConfig {
            max_bytes: MIN_MAX_BYTES,
            low_water_ratio: 0.5,
            ..RetentionConfig::default()
        }
        .clamped();

        // The seed is sized to exceed the cap; assert that precondition so the
        // test fails loudly rather than silently skipping the eviction path.
        let store_size = store_size_bytes(&path);
        assert!(
            store_size > cfg.max_bytes,
            "the bulk seed must exceed the cap to exercise eviction: {store_size} <= {}",
            cfg.max_bytes
        );

        let report =
            run_maintenance(&conn, &cfg, now, &path, false, &AtomicBool::new(false)).unwrap();
        assert!(report.had_eviction(), "eviction ran when over the cap");
        assert!(report.evicted_rows > 0);
        // Oldest-first: the freed span starts at the very oldest row.
        assert_eq!(
            report.evicted_from_us,
            Some(oldest),
            "eviction started at the globally oldest row"
        );
        let surviving_min: i64 = conn
            .query_row("SELECT min(ts_us) FROM logs", [], |r| r.get(0))
            .unwrap();
        assert!(
            surviving_min > report.evicted_from_us.unwrap(),
            "the oldest rows were the ones evicted"
        );
        assert!(count(&conn, "logs") < before, "rows were removed");
        // After eviction plus the trailing vacuum + checkpoint, the on-disk store
        // (main file + WAL) is back at or below the cap.
        assert!(
            store_size_bytes(&path) <= cfg.max_bytes,
            "eviction brought the store back under the cap"
        );
        // A vacuum runs after an eviction freed pages.
        assert!(report.vacuumed, "vacuum runs after eviction");
    }

    #[test]
    fn no_eviction_when_under_the_cap() {
        let (_dir, path, conn) = temp_store();
        let now = BASE_US;
        seed_metric(&conn, now - 1_000, "cpu.util.all", 1.0);
        // The default 1 GB cap is far above a tiny store: no eviction.
        let cfg = RetentionConfig::default();
        let report =
            run_maintenance(&conn, &cfg, now, &path, false, &AtomicBool::new(false)).unwrap();
        assert!(!report.had_eviction());
        assert_eq!(report.evicted_rows, 0);
        assert_eq!(report.evicted_from_us, None);
    }

    #[test]
    fn vacuum_runs_when_requested() {
        let (_dir, path, conn) = temp_store();
        let now = BASE_US;
        seed_metric(&conn, now - 1_000, "cpu.util.all", 1.0);
        let cfg = RetentionConfig::default();
        let report =
            run_maintenance(&conn, &cfg, now, &path, true, &AtomicBool::new(false)).unwrap();
        assert!(report.vacuumed, "vacuum runs on the periodic cadence");
        // The store still passes its integrity check after a vacuum.
        db::integrity_check(&conn).unwrap();
    }

    #[test]
    fn periodic_vacuum_skipped_when_stopping() {
        let (_dir, path, conn) = temp_store();
        let now = BASE_US;
        seed_metric(&conn, now - 1_000, "cpu.util.all", 1.0);
        let cfg = RetentionConfig::default();
        // Even with the periodic cadence due (`do_vacuum = true`), a stop in flight
        // must skip the rewrite so it cannot overrun the shutdown join.
        let report =
            run_maintenance(&conn, &cfg, now, &path, true, &AtomicBool::new(true)).unwrap();
        assert!(
            !report.vacuumed,
            "the periodic vacuum is skipped while stopping"
        );
    }

    #[test]
    fn eviction_runs_but_vacuum_is_skipped_when_stopping() {
        let (_dir, path, conn) = temp_store();
        let now = BASE_US;
        // Same over-cap seed as the eviction test, so eviction must trigger.
        seed_bulk_logs(&conn, now, 70_000, 512);
        conn.pragma_update(None, "wal_checkpoint", "TRUNCATE")
            .unwrap();
        let cfg = RetentionConfig {
            max_bytes: MIN_MAX_BYTES,
            low_water_ratio: 0.5,
            ..RetentionConfig::default()
        }
        .clamped();
        assert!(
            store_size_bytes(&path) > cfg.max_bytes,
            "the bulk seed must exceed the cap to exercise eviction"
        );
        // The cheap deletes still run; only the trailing VACUUM is withheld, so the
        // file stays above the low-water mark until the next pass after a clean start.
        let report =
            run_maintenance(&conn, &cfg, now, &path, false, &AtomicBool::new(true)).unwrap();
        assert!(report.had_eviction(), "eviction still runs while stopping");
        assert!(report.evicted_rows > 0);
        assert!(
            !report.vacuumed,
            "the post-eviction vacuum is skipped while stopping"
        );
    }

    #[test]
    fn config_clamp_floors_the_cap_and_bounds_the_ratio() {
        let cfg = RetentionConfig {
            max_bytes: 1_000, // absurdly small
            low_water_ratio: 2.0,
            ..RetentionConfig::default()
        }
        .clamped();
        assert_eq!(cfg.max_bytes, MIN_MAX_BYTES, "cap floored");
        assert!(cfg.low_water_ratio <= LOW_WATER_RATIO_MAX);
        assert!(cfg.low_water_ratio >= LOW_WATER_RATIO_MIN);

        let nan_cfg = RetentionConfig {
            low_water_ratio: f64::NAN,
            ..RetentionConfig::default()
        }
        .clamped();
        assert!(nan_cfg.low_water_ratio.is_finite(), "NaN ratio corrected");
    }

    #[test]
    fn maintenance_on_an_empty_store_is_a_clean_no_op() {
        let (_dir, path, conn) = temp_store();
        let cfg = RetentionConfig::default();
        let report =
            run_maintenance(&conn, &cfg, BASE_US, &path, false, &AtomicBool::new(false)).unwrap();
        assert_eq!(report, MaintenanceReport::default());
    }

    #[test]
    fn oldest_raw_row_picks_the_global_minimum_across_tables() {
        let (_dir, _path, conn) = temp_store();
        seed_log(&conn, BASE_US + 500);
        seed_metric(&conn, BASE_US + 100, "m", 1.0); // the global oldest
        conn.execute(
            "INSERT INTO events (ts_us, kind, source, severity) VALUES (?1, 'k', 's', 2)",
            params![BASE_US + 900],
        )
        .unwrap();
        let (table, ts) = oldest_raw_row(&conn).unwrap().unwrap();
        assert_eq!(table, "metrics");
        assert_eq!(ts, BASE_US + 100);
    }
}
