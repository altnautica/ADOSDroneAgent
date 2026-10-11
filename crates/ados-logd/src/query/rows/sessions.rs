//! The session listing query.

use super::*;

/// Read the session list with per-session convenience counts. `open_only`
/// restricts to currently-open sessions; the window/kind filters apply against
/// `started_us`. Keyset paginates on `(started_us, id)` descending.
#[allow(clippy::too_many_arguments)]
pub fn query_sessions(
    conn: &Connection,
    from_us: Option<i64>,
    to_us: Option<i64>,
    kind: Option<&str>,
    open_only: bool,
    limit: u32,
    after: Option<&Cursor>,
) -> rusqlite::Result<SessionPage> {
    let mut wb = WhereBuilder::new();
    if let Some(lo) = from_us {
        wb.push("started_us >= ?", lo);
    }
    if let Some(hi) = to_us {
        wb.push("started_us < ?", hi);
    }
    if let Some(k) = kind {
        wb.push("kind = ?", k.to_string());
    }
    if open_only {
        wb.clauses.push("ended_us IS NULL".to_string());
    }
    if let Some(c) = after {
        wb.clauses
            .push("(started_us < ? OR (started_us = ? AND id < ?))".to_string());
        wb.params.push(Box::new(c.ts_us));
        wb.params.push(Box::new(c.ts_us));
        wb.params.push(Box::new(c.id));
    }
    let (where_sql, where_params) = wb.build();
    let sql = format!(
        "SELECT s.id, s.started_us, s.ended_us, s.kind, s.reason, s.meta, \
            (SELECT count(*) FROM logs   l WHERE l.session = s.id) AS log_count, \
            (SELECT count(*) FROM events e WHERE e.session = s.id) AS event_count \
         FROM sessions s {where_sql} ORDER BY s.started_us DESC, s.id DESC LIMIT ?"
    );
    let bound_limit = i64::from(limit) + 1;
    let mut stmt = conn.prepare(&sql)?;
    let mut params: Vec<&dyn rusqlite::types::ToSql> =
        where_params.iter().map(|b| b.as_ref()).collect();
    params.push(&bound_limit);
    let mut out: Vec<SessionRow> = Vec::new();
    let mut rows = stmt.query(params.as_slice())?;
    while let Some(row) = rows.next()? {
        let started_us: i64 = row.get(1)?;
        let ended_us: Option<i64> = row.get(2)?;
        out.push(SessionRow {
            id: row.get(0)?,
            started_us,
            ended_us,
            kind: row.get(3)?,
            reason: row.get(4)?,
            meta: decode_blob(row.get(5)?),
            log_count: row.get(6)?,
            event_count: row.get(7)?,
            span_us: ended_us.map(|e| e - started_us),
        });
    }
    let last_key = if out.len() as u32 > limit {
        out.truncate(limit as usize);
        out.last().map(|r| (r.started_us, r.id))
    } else {
        None
    };
    Ok((out, last_key))
}
