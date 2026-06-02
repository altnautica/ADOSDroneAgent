//! Typed result rows and the read-only SQL that fills them.
//!
//! Every row type serializes to the JSON the read envelope wraps. The open
//! msgpack blob columns (`fields`, `tags`, `detail`, `signals`, session `meta`)
//! are decoded back into JSON values so a consumer never has to speak msgpack;
//! a blob that fails to decode degrades to `null` rather than failing the row.
//!
//! Reads use a read-only WAL connection (never the writer's connection), so a
//! query never blocks the single writer and the writer never blocks a query.
//! Pagination is keyset on `(ts_us, id)` descending: the cursor carries the
//! last `(ts_us, id)` and the next page selects strictly-earlier rows. The
//! handler asks for `limit + 1` rows so it can tell whether a further page
//! exists without a second count query.

use rusqlite::{Connection, Row};
use serde::Serialize;
use serde_json::Value as Json;

use ados_protocol::logd::{Fields, Level};

use super::pagination::Cursor;
use super::params::{QueryFilters, Table};

/// One log record as returned by `/v1/query?kind=logs`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct LogRow {
    /// Primary key (the keyset tiebreak).
    pub id: i64,
    /// Microsecond epoch timestamp.
    pub ts_us: i64,
    /// The session this row is attributed to, if any.
    pub session: Option<i64>,
    /// Emitting process or component.
    pub source: String,
    /// Severity level name.
    pub level: &'static str,
    /// Module or logger target.
    pub target: Option<String>,
    /// The rendered message.
    pub msg: String,
    /// Decoded structured fields (already redacted on disk).
    pub fields: Json,
    /// Whether redaction changed a value before this row was stored.
    pub redacted: bool,
}

/// One discrete event as returned by `/v1/query?kind=events`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct EventRow {
    /// Primary key.
    pub id: i64,
    /// Microsecond epoch timestamp.
    pub ts_us: i64,
    /// The session this row is attributed to, if any.
    pub session: Option<i64>,
    /// Dotted event classifier.
    pub kind: String,
    /// Emitting component.
    pub source: String,
    /// Severity level name.
    pub severity: &'static str,
    /// Decoded detail map.
    pub detail: Json,
}

/// One telemetry sample as returned by `/v1/query?kind=metrics`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct MetricRow {
    /// Primary key.
    pub id: i64,
    /// Microsecond epoch timestamp.
    pub ts_us: i64,
    /// The session this row is attributed to, if any.
    pub session: Option<i64>,
    /// Dotted metric key.
    pub metric: String,
    /// The numeric value.
    pub value: f64,
    /// Decoded tag map.
    pub tags: Json,
}

/// One hardware snapshot as returned by `/v1/query?kind=hw`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct HwRow {
    /// Primary key.
    pub id: i64,
    /// Microsecond epoch timestamp.
    pub ts_us: i64,
    /// The session this row is attributed to, if any.
    pub session: Option<i64>,
    /// Decoded open signal map.
    pub signals: Json,
}

/// One session as returned by `/v1/sessions`, with convenience counts.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SessionRow {
    /// Primary key.
    pub id: i64,
    /// When the session opened, microsecond epoch.
    pub started_us: i64,
    /// When it closed, or `null` while open.
    pub ended_us: Option<i64>,
    /// `flight` | `boot` | `manual`.
    pub kind: String,
    /// The opening/closing reason.
    pub reason: Option<String>,
    /// Decoded meta map.
    pub meta: Json,
    /// Log rows attributed to this session.
    pub log_count: i64,
    /// Event rows attributed to this session.
    pub event_count: i64,
    /// Span in microseconds, `null` while the session is open.
    pub span_us: Option<i64>,
}

/// The `(ts_us, id)` keyset boundary of the last row on a page, present only
/// when a further page may exist.
pub type Keyset = Option<(i64, i64)>;

/// A page of rows from one table plus the keyset boundary of the last row,
/// which the handler turns into the next cursor.
pub struct Page<T> {
    /// The rows, newest first.
    pub rows: Vec<T>,
    /// `(ts_us, id)` of the last row, when a further page may exist.
    pub last_key: Keyset,
}

/// A keyset page of session rows plus the boundary, returned by
/// [`query_sessions`].
pub type SessionPage = (Vec<SessionRow>, Keyset);

/// The level integer column decoded to its lowercase name.
fn level_name(n: i64) -> &'static str {
    match Level::from_u8(n.clamp(0, 255) as u8) {
        Level::Trace => "trace",
        Level::Debug => "debug",
        Level::Info => "info",
        Level::Warn => "warn",
        Level::Error => "error",
    }
}

/// Decode an optional msgpack blob column into a JSON value. A `NULL` column or
/// a blob that fails to decode degrades to JSON `null` rather than erroring the
/// whole row — a single corrupt blob must not sink a page.
fn decode_blob(blob: Option<Vec<u8>>) -> Json {
    match blob {
        None => Json::Null,
        Some(bytes) => match rmp_serde::from_slice::<Fields>(&bytes) {
            Ok(map) => serde_json::to_value(map).unwrap_or(Json::Null),
            Err(_) => Json::Null,
        },
    }
}

/// Build the shared `WHERE` clause and bound parameters for a table read. The
/// clause composes the time window, the keyset boundary, the per-table list
/// filters, the level floor, the text match, and the session restriction.
///
/// Returns the SQL fragment (without the leading `WHERE`) and the positional
/// parameter values in the order they appear. List filters use inlined,
/// integer-or-quoted-safe placeholders generated here; all user strings are
/// bound as parameters, never interpolated, so the builder is injection-safe.
struct WhereBuilder {
    clauses: Vec<String>,
    params: Vec<Box<dyn rusqlite::types::ToSql>>,
}

impl WhereBuilder {
    fn new() -> Self {
        Self {
            clauses: Vec::new(),
            params: Vec::new(),
        }
    }

    fn push<T: rusqlite::types::ToSql + 'static>(&mut self, clause: impl Into<String>, value: T) {
        self.clauses.push(clause.into());
        self.params.push(Box::new(value));
    }

    /// Push an `IN (...)` clause over a string list, binding each element.
    fn push_in(&mut self, column: &str, values: &[String]) {
        if values.is_empty() {
            return;
        }
        let placeholders = vec!["?"; values.len()].join(", ");
        self.clauses.push(format!("{column} IN ({placeholders})"));
        for v in values {
            self.params.push(Box::new(v.clone()));
        }
    }

    fn build(self) -> (String, Vec<Box<dyn rusqlite::types::ToSql>>) {
        let sql = if self.clauses.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", self.clauses.join(" AND "))
        };
        (sql, self.params)
    }
}

/// Assemble the `WHERE` builder shared by all four tables for a keyset read.
fn where_for(filters: &QueryFilters, after: Option<&Cursor>) -> WhereBuilder {
    let mut wb = WhereBuilder::new();
    if let Some(lo) = filters.from_us {
        wb.push("ts_us >= ?", lo);
    }
    if let Some(hi) = filters.to_us {
        wb.push("ts_us < ?", hi);
    }
    if let Some(sid) = filters.session {
        wb.push("session = ?", sid);
    }
    // Keyset boundary: strictly-earlier rows than the cursor's (ts_us, id).
    if let Some(c) = after {
        wb.clauses
            .push("(ts_us < ? OR (ts_us = ? AND id < ?))".to_string());
        wb.params.push(Box::new(c.ts_us));
        wb.params.push(Box::new(c.ts_us));
        wb.params.push(Box::new(c.id));
    }
    match filters.table {
        Table::Logs => {
            wb.push_in("source", &filters.sources);
            if let Some(level) = filters.min_level {
                wb.push("level >= ?", i64::from(level.as_u8()));
            }
            if let Some(text) = &filters.text {
                let like = format!("%{}%", escape_like(text));
                wb.clauses
                    .push("(msg LIKE ? ESCAPE '\\' OR target LIKE ? ESCAPE '\\')".to_string());
                wb.params.push(Box::new(like.clone()));
                wb.params.push(Box::new(like));
            }
        }
        Table::Events => {
            wb.push_in("source", &filters.sources);
            wb.push_in("kind", &filters.event_kinds);
            if let Some(level) = filters.min_level {
                wb.push("severity >= ?", i64::from(level.as_u8()));
            }
            if let Some(text) = &filters.text {
                let like = format!("%{}%", escape_like(text));
                wb.clauses.push("kind LIKE ? ESCAPE '\\'".to_string());
                wb.params.push(Box::new(like));
            }
        }
        Table::Metrics => {
            wb.push_in("metric", &filters.metrics);
        }
        Table::Hw => {}
    }
    wb
}

/// Escape the LIKE metacharacters in a user substring so a literal `%` or `_`
/// in the search text is matched literally (with the `ESCAPE '\'` clause above).
fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// Read one keyset page of log rows.
pub fn query_logs(
    conn: &Connection,
    filters: &QueryFilters,
    after: Option<&Cursor>,
) -> rusqlite::Result<Page<LogRow>> {
    let (where_sql, params) = where_for(filters, after).build();
    let sql = format!(
        "SELECT id, ts_us, session, source, level, target, msg, fields, redacted \
         FROM logs {where_sql} ORDER BY ts_us DESC, id DESC LIMIT ?"
    );
    let limit = i64::from(filters.limit) + 1;
    read_page(conn, &sql, params, limit, filters.limit, |row| {
        Ok(LogRow {
            id: row.get(0)?,
            ts_us: row.get(1)?,
            session: row.get(2)?,
            source: row.get(3)?,
            level: level_name(row.get::<_, i64>(4)?),
            target: row.get(5)?,
            msg: row.get(6)?,
            fields: decode_blob(row.get(7)?),
            redacted: row.get::<_, i64>(8)? != 0,
        })
    })
    .map(|(rows, last_key)| Page { rows, last_key })
}

/// Read one keyset page of event rows.
pub fn query_events(
    conn: &Connection,
    filters: &QueryFilters,
    after: Option<&Cursor>,
) -> rusqlite::Result<Page<EventRow>> {
    let (where_sql, params) = where_for(filters, after).build();
    let sql = format!(
        "SELECT id, ts_us, session, kind, source, severity, detail \
         FROM events {where_sql} ORDER BY ts_us DESC, id DESC LIMIT ?"
    );
    let limit = i64::from(filters.limit) + 1;
    read_page(conn, &sql, params, limit, filters.limit, |row| {
        Ok(EventRow {
            id: row.get(0)?,
            ts_us: row.get(1)?,
            session: row.get(2)?,
            kind: row.get(3)?,
            source: row.get(4)?,
            severity: level_name(row.get::<_, i64>(5)?),
            detail: decode_blob(row.get(6)?),
        })
    })
    .map(|(rows, last_key)| Page { rows, last_key })
}

/// Read one keyset page of metric rows.
pub fn query_metrics(
    conn: &Connection,
    filters: &QueryFilters,
    after: Option<&Cursor>,
) -> rusqlite::Result<Page<MetricRow>> {
    let (where_sql, params) = where_for(filters, after).build();
    let sql = format!(
        "SELECT id, ts_us, session, metric, value, tags \
         FROM metrics {where_sql} ORDER BY ts_us DESC, id DESC LIMIT ?"
    );
    let limit = i64::from(filters.limit) + 1;
    read_page(conn, &sql, params, limit, filters.limit, |row| {
        Ok(MetricRow {
            id: row.get(0)?,
            ts_us: row.get(1)?,
            session: row.get(2)?,
            metric: row.get(3)?,
            value: row.get(4)?,
            tags: decode_blob(row.get(5)?),
        })
    })
    .map(|(rows, last_key)| Page { rows, last_key })
}

/// Read one keyset page of hardware snapshots.
pub fn query_hw(
    conn: &Connection,
    filters: &QueryFilters,
    after: Option<&Cursor>,
) -> rusqlite::Result<Page<HwRow>> {
    let (where_sql, params) = where_for(filters, after).build();
    let sql = format!(
        "SELECT id, ts_us, session, signals \
         FROM hw {where_sql} ORDER BY ts_us DESC, id DESC LIMIT ?"
    );
    let limit = i64::from(filters.limit) + 1;
    read_page(conn, &sql, params, limit, filters.limit, |row| {
        Ok(HwRow {
            id: row.get(0)?,
            ts_us: row.get(1)?,
            session: row.get(2)?,
            signals: decode_blob(row.get(3)?),
        })
    })
    .map(|(rows, last_key)| Page { rows, last_key })
}

/// Run a keyset page query: bind the `WHERE` params plus the `limit + 1`, map
/// each row, and split off the overflow row to derive the next-page boundary.
fn read_page<T, F>(
    conn: &Connection,
    sql: &str,
    where_params: Vec<Box<dyn rusqlite::types::ToSql>>,
    bound_limit: i64,
    page_limit: u32,
    map: F,
) -> rusqlite::Result<(Vec<T>, Keyset)>
where
    F: Fn(&Row<'_>) -> rusqlite::Result<T>,
    T: HasKey,
{
    let mut stmt = conn.prepare(sql)?;
    // Build the positional parameter slice: the WHERE params, then the limit.
    let mut params: Vec<&dyn rusqlite::types::ToSql> =
        where_params.iter().map(|b| b.as_ref()).collect();
    params.push(&bound_limit);
    let mut out: Vec<T> = Vec::new();
    let mut rows = stmt.query(params.as_slice())?;
    while let Some(row) = rows.next()? {
        out.push(map(row)?);
    }
    // If the query returned the overflow row, a further page exists; the
    // boundary is the last row WITHIN the page, and the overflow is dropped.
    let last_key = if out.len() as u32 > page_limit {
        out.truncate(page_limit as usize);
        out.last().map(|r| r.key())
    } else {
        None
    };
    Ok((out, last_key))
}

/// A row that exposes its keyset key for cursor derivation.
trait HasKey {
    fn key(&self) -> (i64, i64);
}
impl HasKey for LogRow {
    fn key(&self) -> (i64, i64) {
        (self.ts_us, self.id)
    }
}
impl HasKey for EventRow {
    fn key(&self) -> (i64, i64) {
        (self.ts_us, self.id)
    }
}
impl HasKey for MetricRow {
    fn key(&self) -> (i64, i64) {
        (self.ts_us, self.id)
    }
}
impl HasKey for HwRow {
    fn key(&self) -> (i64, i64) {
        (self.ts_us, self.id)
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use ados_protocol::logd::{EventFrame, HwSnapshot, LogFrame, TelemetryFrame};
    use rmpv::Value as MpVal;

    /// Build a store with a writer-shaped row set so the read path is tested
    /// against the real schema (and a real session row the foreign key needs).
    fn seed(path: &std::path::Path) {
        let conn = db::open(path).unwrap();
        conn.execute(
            "INSERT INTO sessions (started_us, kind, reason) VALUES (?1, 'boot', 'start')",
            [1_000i64],
        )
        .unwrap();
        let session = conn.last_insert_rowid();
        for i in 0..5i64 {
            let mut l = LogFrame::new(
                2_000 + i,
                "api",
                Level::from_u8((i % 5) as u8),
                format!("m{i}"),
            );
            l.target = Some(format!("mod{i}"));
            l.fields
                .insert("attempt".to_string(), MpVal::from(i as u64));
            let fields = rmp_serde::to_vec_named(&l.fields).unwrap();
            conn.execute(
                "INSERT INTO logs (ts_us, session, source, level, target, msg, fields, redacted) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0)",
                rusqlite::params![
                    l.ts_us,
                    session,
                    l.source,
                    i64::from(l.level.as_u8()),
                    l.target,
                    l.msg,
                    fields
                ],
            )
            .unwrap();
        }
        // One event, one metric, one hw row to exercise the other tables.
        let evt = EventFrame::new(3_000, "radio.lock", "ados-radio", Level::Info);
        conn.execute(
            "INSERT INTO events (ts_us, session, kind, source, severity) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![evt.ts_us, session, evt.kind, evt.source, i64::from(evt.severity.as_u8())],
        )
        .unwrap();
        let m = TelemetryFrame::new(3_100, "cpu.load", 0.42);
        conn.execute(
            "INSERT INTO metrics (ts_us, session, metric, value) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![m.ts_us, session, m.metric, m.value],
        )
        .unwrap();
        let mut hw = HwSnapshot::new(3_200);
        hw.signals
            .insert("thermal.soc_c".to_string(), MpVal::from(48.0));
        let signals = rmp_serde::to_vec_named(&hw.signals).unwrap();
        conn.execute(
            "INSERT INTO hw (ts_us, session, signals) VALUES (?1, ?2, ?3)",
            rusqlite::params![hw.ts_us, session, signals],
        )
        .unwrap();
    }

    fn filters(query: &str) -> QueryFilters {
        QueryFilters::parse(&super::super::params::QueryParams::parse(query), 0).unwrap()
    }

    #[test]
    fn logs_come_back_newest_first_and_decode_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs.db");
        seed(&path);
        let ro = db::open_readonly(&path).unwrap();
        let page = query_logs(&ro, &filters("kind=logs&limit=10"), None).unwrap();
        assert_eq!(page.rows.len(), 5);
        // Newest first: ts_us strictly decreasing.
        assert!(page.rows.windows(2).all(|w| w[0].ts_us > w[1].ts_us));
        // Fields decoded back to JSON.
        assert!(page.rows[0].fields.get("attempt").is_some());
        // No further page.
        assert!(page.last_key.is_none());
    }

    #[test]
    fn keyset_pagination_returns_disjoint_pages() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs.db");
        seed(&path);
        let ro = db::open_readonly(&path).unwrap();
        let f = filters("kind=logs&limit=2");
        let p1 = query_logs(&ro, &f, None).unwrap();
        assert_eq!(p1.rows.len(), 2);
        let key = p1.last_key.unwrap();
        let cursor = Cursor::new(key.0, key.1, f.fingerprint());
        let p2 = query_logs(&ro, &f, Some(&cursor)).unwrap();
        assert_eq!(p2.rows.len(), 2);
        // No overlap between page 1 and page 2.
        let ids1: Vec<i64> = p1.rows.iter().map(|r| r.id).collect();
        let ids2: Vec<i64> = p2.rows.iter().map(|r| r.id).collect();
        assert!(ids1.iter().all(|id| !ids2.contains(id)));
    }

    #[test]
    fn source_and_level_and_text_filters_apply() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs.db");
        seed(&path);
        let ro = db::open_readonly(&path).unwrap();
        // Only WARN+ (levels 3,4) survive the floor.
        let warn = query_logs(&ro, &filters("kind=logs&level=warn"), None).unwrap();
        assert!(warn
            .rows
            .iter()
            .all(|r| r.level == "warn" || r.level == "error"));
        // Text match against the message.
        let m3 = query_logs(&ro, &filters("kind=logs&text=m3"), None).unwrap();
        assert_eq!(m3.rows.len(), 1);
        assert_eq!(m3.rows[0].msg, "m3");
        // A source that does not exist returns nothing.
        let none = query_logs(&ro, &filters("kind=logs&source=does-not-exist"), None).unwrap();
        assert!(none.rows.is_empty());
    }

    #[test]
    fn like_metacharacters_are_escaped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs.db");
        let conn = db::open(&path).unwrap();
        conn.execute(
            "INSERT INTO sessions (started_us, kind) VALUES (1, 'boot')",
            [],
        )
        .unwrap();
        let s = conn.last_insert_rowid();
        for (i, msg) in ["100% full", "plain text"].iter().enumerate() {
            conn.execute(
                "INSERT INTO logs (ts_us, session, source, level, msg) VALUES (?1, ?2, 'api', 2, ?3)",
                rusqlite::params![10 + i as i64, s, msg],
            )
            .unwrap();
        }
        drop(conn);
        let ro = db::open_readonly(&path).unwrap();
        // A literal '%' must match only the row that actually contains it.
        let hit = query_logs(&ro, &filters("kind=logs&text=100%25"), None).unwrap();
        assert_eq!(hit.rows.len(), 1);
        assert_eq!(hit.rows[0].msg, "100% full");
    }

    #[test]
    fn events_metrics_and_hw_tables_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs.db");
        seed(&path);
        let ro = db::open_readonly(&path).unwrap();
        assert_eq!(
            query_events(&ro, &filters("kind=events&limit=10"), None)
                .unwrap()
                .rows
                .len(),
            1
        );
        assert_eq!(
            query_metrics(&ro, &filters("kind=metrics&limit=10"), None)
                .unwrap()
                .rows
                .len(),
            1
        );
        let hw = query_hw(&ro, &filters("kind=hw&limit=10"), None).unwrap();
        assert_eq!(hw.rows.len(), 1);
        assert!(hw.rows[0].signals.get("thermal.soc_c").is_some());
    }

    #[test]
    fn sessions_carry_counts_and_span() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs.db");
        seed(&path);
        // Close the seeded session so the span is computed.
        {
            let w = db::open(&path).unwrap();
            w.execute("UPDATE sessions SET ended_us = 9000 WHERE kind='boot'", [])
                .unwrap();
        }
        let ro = db::open_readonly(&path).unwrap();
        let (rows, _) = query_sessions(&ro, None, None, None, false, 10, None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].log_count, 5);
        assert_eq!(rows[0].event_count, 1);
        assert_eq!(rows[0].span_us, Some(9000 - 1000));
    }
}
