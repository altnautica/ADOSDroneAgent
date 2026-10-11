//! Retention step 4: size-cap eviction and the store size readers.

use super::*;

// --- step 4: size-cap and free-space eviction ---------------------------

/// The outcome of eviction: how many rows were evicted and the timestamp span
/// they covered (for the `retention.evicted` event), plus how the reclaim step
/// after it must run.
#[derive(Debug, Clone, Default)]
pub(super) struct Eviction {
    pub(super) rows: u64,
    pub(super) from_us: Option<i64>,
    pub(super) to_us: Option<i64>,
    /// The filesystem was below its free-space reserve this pass, so the
    /// reclaim step runs even if nothing was evicted.
    pub(super) low_headroom: bool,
    /// The page bound for the reclaim step: [`MAX_INCREMENTAL_VACUUM_PAGES`],
    /// raised to the filesystem shortfall on a low-headroom pass so the freed
    /// pages actually go back to the filesystem in the same pass.
    pub(super) reclaim_pages: i64,
}

/// Evict the oldest raw rows first, across all raw tables, until the store is at
/// or below its target, or there is nothing left to evict, or the per-pass
/// batch bound is hit. Returns the eviction span so the writer can record it.
///
/// Two independent triggers arm it, and the stricter target wins:
///
/// - the store is over its byte cap → the target is the low-water mark;
/// - the filesystem is below its free-space reserve (`fs`, see [`FsUsage`]) →
///   the target is the allocated file size minus the shortfall, i.e. evict
///   until the pages the reclaim step will hand back cover the deficit. Free
///   space itself is not re-measured inside the loop: a `DELETE` frees pages
///   onto SQLite's free list, not to the filesystem, so `statvfs` would not
///   move until the reclaim and would drain the store to empty.
///
/// Eviction is the safety net for a small card: TTL handles the steady state,
/// this handles a spike. It is strictly oldest-first by `ts_us`, so the rollups
/// (which are not evicted here) keep the long-horizon shape even after the raw
/// rows for that span are gone.
///
/// The byte-cap trigger and the stopping condition are measured against the
/// **logical** used size ([`logical_used_bytes`]: `(page_count -
/// freelist_count) * page_size`) plus the WAL — not the raw file size.
///
/// This is the counterpart to reclaim being incremental. A plain `DELETE` moves
/// pages onto SQLite's free list without shrinking the main file, and those
/// pages are then reused by subsequent inserts. So a healthy store at steady
/// state legitimately sits at its high-water file size forever, with the free
/// list absorbing the churn. Triggering on raw file size would read that
/// perfectly healthy state as "over the cap" on **every** pass and evict the
/// store down to nothing, one batch at a time, while never being able to shrink
/// the file that caused the trigger. Measuring what is actually *used* is both
/// the honest number and the only self-consistent one.
///
/// The raw on-disk footprint is still reported by [`store_size_bytes`] for
/// telemetry — the operator should be able to see what the store occupies on
/// the card — it just is not what arms eviction.
pub(super) fn enforce_size_cap(
    conn: &Connection,
    cfg: &RetentionConfig,
    db_path: &Path,
    fs: Option<FsUsage>,
) -> Result<Eviction, rusqlite::Error> {
    let over_cap = used_store_bytes(conn, db_path)? > cfg.max_bytes;
    let deficit = fs.map_or(0, |u| u.deficit_bytes());
    if !over_cap && deficit == 0 {
        return Ok(Eviction::default());
    }

    let mut target = if over_cap {
        cfg.low_water_bytes()
    } else {
        u64::MAX
    };
    let mut ev = Eviction {
        reclaim_pages: MAX_INCREMENTAL_VACUUM_PAGES,
        ..Eviction::default()
    };
    if deficit > 0 {
        let page_size = page_size_bytes(conn)?;
        let allocated = allocated_bytes(conn)?;
        target = target.min(allocated.saturating_sub(deficit));
        ev.low_headroom = true;
        ev.reclaim_pages = ev
            .reclaim_pages
            .max(headroom::pages_for(deficit, page_size));
        tracing::warn!(
            deficit_bytes = deficit,
            store_bytes = allocated,
            "filesystem below its free-space reserve; evicting oldest store rows"
        );
    }
    for _ in 0..MAX_DELETE_BATCHES_PER_PASS {
        if logical_used_bytes(conn)? <= target {
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

/// The store's *used* footprint: the logical used size plus the on-disk WAL.
///
/// The WAL is real bytes on the card that the cap has to account for, and unlike
/// the main file it genuinely does shrink (the `TRUNCATE` checkpoint after a
/// reclaim), so including it neither wedges the trigger nor lets an unbounded
/// WAL hide from the cap.
pub(super) fn used_store_bytes(conn: &Connection, db_path: &Path) -> Result<u64, rusqlite::Error> {
    Ok(logical_used_bytes(conn)? + wal_size_bytes(db_path))
}

/// The store's logical used size in bytes: `(page_count - freelist_count) *
/// page_size`. This is the size the file would have after a `VACUUM`, so it is
/// the right measure for both the eviction trigger and its stopping condition:
/// the file itself does not shrink when rows are deleted, but this does.
pub(super) fn logical_used_bytes(conn: &Connection) -> Result<u64, rusqlite::Error> {
    let page_count: i64 = conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
    let freelist: i64 = conn.query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
    let used_pages = (page_count - freelist).max(0);
    Ok(used_pages as u64 * page_size_bytes(conn)?)
}

/// The store's allocated size in bytes, free list included: `page_count *
/// page_size`. What the main file occupies once checkpointed, and so the most
/// a full reclaim can shrink it from.
pub(super) fn allocated_bytes(conn: &Connection) -> Result<u64, rusqlite::Error> {
    let page_count: i64 = conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
    Ok(page_count.max(0) as u64 * page_size_bytes(conn)?)
}

/// `PRAGMA page_size` in bytes.
pub(super) fn page_size_bytes(conn: &Connection) -> Result<u64, rusqlite::Error> {
    let page_size: i64 = conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
    Ok(page_size.max(0) as u64)
}

/// The span and count of one evicted batch.
#[derive(Debug, Clone, Default)]
pub(super) struct EvictBatch {
    pub(super) rows: u64,
    pub(super) from_us: i64,
    pub(super) to_us: i64,
}

/// Find the table holding the globally-oldest raw row and that row's `ts_us`.
/// Returns `None` when every raw table is empty.
pub(super) fn oldest_raw_row(
    conn: &Connection,
) -> Result<Option<(&'static str, i64)>, rusqlite::Error> {
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
pub(super) fn evict_batch(
    conn: &Connection,
    table: &'static str,
) -> Result<EvictBatch, rusqlite::Error> {
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
    main + wal_size_bytes(db_path)
}

/// The size of the `-wal` sidecar alone, or zero when it is absent or
/// unreadable.
pub(super) fn wal_size_bytes(db_path: &Path) -> u64 {
    wal_path(db_path)
        .and_then(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .unwrap_or(0)
}

/// The `-wal` companion path for a store file.
pub(super) fn wal_path(db_path: &Path) -> Option<std::path::PathBuf> {
    let name = db_path.file_name()?.to_str()?;
    Some(db_path.with_file_name(format!("{name}-wal")))
}
