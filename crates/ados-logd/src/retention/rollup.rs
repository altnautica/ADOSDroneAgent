//! Retention steps 1-3: closed-window rollups and the TTL deletes.

use super::*;

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
pub(super) fn roll_up(conn: &Connection, now_us: i64) -> Result<u64, rusqlite::Error> {
    let mut written = 0u64;
    written += roll_up_grain(conn, now_us, "metrics_1m", BUCKET_1M_US)?;
    written += roll_up_grain(conn, now_us, "metrics_1h", BUCKET_1H_US)?;
    Ok(written)
}

/// Roll one grain. `bucket_us` is the bucket width; `table` is the destination
/// rollup table. Only buckets whose end is at or before `now` are rolled.
pub(super) fn roll_up_grain(
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
pub(super) fn ttl_delete_raw(conn: &Connection, cutoff_us: i64) -> Result<u64, rusqlite::Error> {
    let mut total = 0u64;
    for table in RAW_TABLES {
        total += delete_older_than(conn, table, cutoff_us)?;
    }
    Ok(total)
}

/// Delete rollup rows older than `cutoff_us` from both rollup tables. Rollup
/// tables key on `bucket_us`, not `ts_us`, and have no integer `id`, so the
/// delete is a direct ranged delete (already bounded by the long window).
pub(super) fn ttl_delete_rollups(
    conn: &Connection,
    cutoff_us: i64,
) -> Result<u64, rusqlite::Error> {
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
pub(super) fn delete_older_than(
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
