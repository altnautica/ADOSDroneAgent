//! The `/v1` endpoint handlers and the shared application state.
//!
//! Every handler reads from a fresh read-only WAL connection (never the
//! writer's connection), wraps its result in the shared envelope, and returns
//! a JSON response. The handlers are edge-agnostic: the same set is mounted on
//! the Unix socket and the TCP port, with auth applied at the edge layer in
//! `mod.rs`.
//!
//! Blocking SQLite work is moved off the async runtime with
//! `tokio::task::spawn_blocking`, so a slow query never stalls the reactor and
//! the streaming export runs on its own blocking thread.

use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{RawQuery, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures::stream::Stream;
use serde::Serialize;
use serde_json::Value as JsonValue;
use tokio::sync::{broadcast, mpsc, oneshot};

use ados_protocol::logd::{
    IngestFrame, Meta, Page, QueryResponse, SyncRequest, SyncResponse, ENVELOPE_VERSION,
};

use crate::writer::{ControlMsg, MarkResult};

use super::aggregate::{self, AggregateParams};
use super::export::{self, Format};
use super::openapi;
use super::pagination::{Cursor, CursorError};
use super::params::{ParamError, QueryFilters, QueryParams, Table};
use super::pool::{ConnPool, PooledConn};
use super::rows::{self, Page as RowPage};
use super::sse::{frame_matches, frame_to_json, ExportSlots, TailSlots};
use super::stats;
use crate::db;
use crate::ingest::IngestStats;
use crate::writer::now_us;

mod stream;
pub use stream::{export, tail};

/// The state shared across all handlers and both listeners. Everything here is
/// cheap to clone (paths and `Arc`s); a per-request read connection is opened
/// from `db_path`.
#[derive(Clone)]
pub struct AppState {
    /// The store path. The pool checks out read-only connections from it; the
    /// export thread and `healthz` open from it directly.
    pub db_path: PathBuf,
    /// The shared pool of warm read-only connections the handlers check out from.
    pub pool: Arc<ConnPool>,
    /// The writer's broadcast sender; `subscribe()` feeds the live tail.
    pub broadcast: broadcast::Sender<IngestFrame>,
    /// The live ingest counters surfaced by `stats`.
    pub ingest: Arc<IngestStats>,
    /// The concurrent-tail-subscriber cap.
    pub tail_slots: Arc<TailSlots>,
    /// The concurrent-export cap (bulk streams are far heavier than a query).
    pub export_slots: Arc<ExportSlots>,
    /// The control sender to the single writer. The mark-synced handler enqueues
    /// a request here and awaits the reply; it never writes the store itself.
    pub mark_synced: mpsc::Sender<crate::writer::ControlMsg>,
    /// The writer's liveness stamp. Read (never written) by `healthz` and
    /// `stats`.
    pub writer_health: crate::writer::WriterHealth,
    /// The store's latest structural-check verdict, refreshed in the
    /// background. `healthz` and `stats` report it; neither runs the check.
    pub integrity: stats::IntegrityVerdict,
}

impl AppState {
    /// Check out a read-only connection from the pool. Returns the API error
    /// shape on failure so a handler can surface a 503. The connection parks
    /// back into the pool when the returned guard drops at the end of the
    /// handler's blocking closure.
    fn open_ro(&self) -> Result<PooledConn, ApiErr> {
        self.pool.checkout().map_err(|e| {
            ApiErr::status(
                StatusCode::SERVICE_UNAVAILABLE,
                "db_unavailable",
                format!("store is not readable: {e}"),
            )
        })
    }

    /// Whether the writer is still persisting.
    ///
    /// Read from the writer's own progress stamp, not assumed: this used to
    /// return a hardcoded `true`, so a writer that exited on an `SQLITE_FULL`,
    /// an I/O error or a corrupt store left `/v1/healthz` answering 200
    /// `{ok:true, writer_alive:true}` while the Black Box recorded nothing.
    /// The daemon owns the writer thread for its whole lifetime and tears this
    /// surface down before joining it, so during a clean shutdown this still
    /// reads alive; what it now catches is the writer dying underneath a daemon
    /// that is still serving.
    fn writer_alive(&self) -> bool {
        self.writer_health
            .is_alive(crate::writer::WRITER_STALL_BUDGET)
    }
}

/// A handler error carrying an HTTP status and the `{ error: { code, message } }`
/// body. Converts `ParamError`/`CursorError` into a 400.
pub struct ApiErr {
    status: StatusCode,
    code: String,
    message: String,
}

impl ApiErr {
    fn status(status: StatusCode, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            status,
            code: code.into(),
            message: message.into(),
        }
    }

    fn bad_request(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::status(StatusCode::BAD_REQUEST, code, message)
    }
}

impl From<ParamError> for ApiErr {
    fn from(e: ParamError) -> Self {
        ApiErr::bad_request(e.code, e.message)
    }
}

impl From<CursorError> for ApiErr {
    fn from(e: CursorError) -> Self {
        ApiErr::bad_request("bad_cursor", e.to_string())
    }
}

impl IntoResponse for ApiErr {
    fn into_response(self) -> Response {
        let body = serde_json::json!({
            "error": { "code": self.code, "message": self.message }
        });
        (self.status, Json(body)).into_response()
    }
}

/// Build the response metadata block. `db_lag_ms` is the gap between the newest
/// row in the result window and now, a coarse "how stale is this read" hint.
fn meta(source: &str, newest_ts_us: Option<i64>) -> Meta {
    let now = now_us();
    let db_lag_ms = newest_ts_us
        .map(|ts| ((now - ts).max(0)) / 1000)
        .unwrap_or(0);
    Meta {
        source: source.to_string(),
        v: ENVELOPE_VERSION,
        ts: now,
        db_lag_ms,
    }
}

/// Wrap typed rows in the shared success envelope.
fn envelope<T: Serialize>(
    data: T,
    count: usize,
    next_cursor: Option<String>,
    newest: Option<i64>,
) -> Json<QueryResponse<T>> {
    Json(QueryResponse {
        data,
        page: Page {
            next_cursor,
            count: count as u32,
        },
        meta: meta("logd", newest),
    })
}

/// Decode an optional cursor against the filter fingerprint.
fn decode_cursor(filters: &QueryFilters) -> Result<Option<Cursor>, ApiErr> {
    match &filters.cursor {
        None => Ok(None),
        Some(token) => Ok(Some(Cursor::decode(token, filters.fingerprint())?)),
    }
}

/// Derive the next-page cursor from a row page's keyset boundary.
fn next_cursor(filters: &QueryFilters, last_key: Option<(i64, i64)>) -> Option<String> {
    last_key.map(|(ts, id)| Cursor::new(ts, id, filters.fingerprint()).encode())
}

// --- GET /v1/query ------------------------------------------------------

/// Keyset-paginated rows across the chosen table.
pub async fn query(
    State(state): State<AppState>,
    RawQuery(q): RawQuery,
) -> Result<Response, ApiErr> {
    let params = QueryParams::parse(q.as_deref().unwrap_or(""));
    let filters = QueryFilters::parse(&params, now_us())?;
    let after = decode_cursor(&filters)?;

    let resp = run_blocking(move || {
        let conn = state.open_ro()?;
        Ok(read_table_page(&conn, &filters, after.as_ref()))
    })
    .await??;
    Ok(resp)
}

/// Read one page from the table the filters select and build its envelope. The
/// row JSON, the newest timestamp, and the next cursor are all derived here so
/// the four table arms share one shape.
fn read_table_page(
    conn: &rusqlite::Connection,
    filters: &QueryFilters,
    after: Option<&Cursor>,
) -> Response {
    macro_rules! page_response {
        ($read:path) => {{
            match $read(conn, filters, after) {
                Ok(RowPage { rows, last_key }) => {
                    let newest = rows.first().map(|r| r.ts_us);
                    let count = rows.len();
                    let cursor = next_cursor(filters, last_key);
                    envelope(rows, count, cursor, newest).into_response()
                }
                Err(e) => read_error(e).into_response(),
            }
        }};
    }
    match filters.table {
        Table::Logs => page_response!(rows::query_logs),
        Table::Events => page_response!(rows::query_events),
        Table::Metrics => page_response!(rows::query_metrics),
        Table::Hw => page_response!(rows::query_hw),
    }
}

// --- GET /v1/aggregate --------------------------------------------------

/// Downsampled metric series.
pub async fn aggregate(
    State(state): State<AppState>,
    RawQuery(q): RawQuery,
) -> Result<Response, ApiErr> {
    let params = QueryParams::parse(q.as_deref().unwrap_or(""));
    let agg_params = AggregateParams::parse(&params, now_us())?;
    let resp = run_blocking(move || {
        let conn = state.open_ro()?;
        match aggregate::aggregate(&conn, &agg_params) {
            Ok(buckets) => {
                let newest = buckets.iter().map(|b| b.bucket_us).max();
                let count = buckets.len();
                Ok(envelope(buckets, count, None, newest).into_response())
            }
            Err(e) => Ok(read_error(e).into_response()),
        }
    })
    .await??;
    Ok(resp)
}

// --- GET /v1/sessions ---------------------------------------------------

/// The session list with per-session counts.
pub async fn sessions(
    State(state): State<AppState>,
    RawQuery(q): RawQuery,
) -> Result<Response, ApiErr> {
    let params = QueryParams::parse(q.as_deref().unwrap_or(""));
    let now = now_us();
    let from_us = super::params::parse_time(params.get("from"), now, "from")?;
    let to_us = super::params::parse_time(params.get("to"), now, "to")?;
    let kind = params.get("kind").map(str::to_string);
    let open_only = matches!(params.get("open"), Some("1") | Some("true"));
    let limit = params
        .get("limit")
        .and_then(|l| l.parse::<u32>().ok())
        .unwrap_or(super::params::DEFAULT_LIMIT)
        .clamp(1, super::params::MAX_LIMIT);
    // The sessions cursor is fingerprinted over its own filter set.
    let mut fp = super::pagination::FilterFingerprint::new();
    fp.add_str("sessions")
        .add_opt_i64(from_us)
        .add_opt_i64(to_us)
        .add_opt_str(kind.as_deref())
        .add_i64(open_only as i64);
    let fingerprint = fp.finish();
    let after = match params.get("cursor") {
        Some(token) => Some(Cursor::decode(token, fingerprint)?),
        None => None,
    };

    let resp = run_blocking(move || {
        let conn = state.open_ro()?;
        match rows::query_sessions(
            &conn,
            from_us,
            to_us,
            kind.as_deref(),
            open_only,
            limit,
            after.as_ref(),
        ) {
            Ok((rows, last_key)) => {
                let newest = rows.first().map(|r| r.started_us);
                let count = rows.len();
                let cursor = last_key.map(|(ts, id)| Cursor::new(ts, id, fingerprint).encode());
                Ok(envelope(rows, count, cursor, newest).into_response())
            }
            Err(e) => Ok(read_error(e).into_response()),
        }
    })
    .await??;
    Ok(resp)
}

// --- GET /v1/stats ------------------------------------------------------

/// Store and ingest health.
pub async fn stats(State(state): State<AppState>) -> Result<Response, ApiErr> {
    let resp = run_blocking(move || {
        let conn = state.open_ro()?;
        match stats::gather(&conn, &state.db_path, &state.ingest, state.integrity.get()) {
            Ok(s) => Ok(envelope(s, 1, None, None).into_response()),
            Err(e) => Ok(read_error(e).into_response()),
        }
    })
    .await??;
    Ok(resp)
}

// --- POST /v1/synced (trusted socket only) ------------------------------

/// Mark the rows in the request's window as synced. Reachable ONLY on the
/// trusted local socket; the TCP edge rejects this method+path before this
/// handler is ever reached (see the edge gate in `mod.rs`). The handler does NO
/// DB write of its own: it enqueues a control message on the single writer's
/// channel and awaits the reply, so the only place a `synced` flag flips stays
/// the writer thread.
pub async fn synced(
    State(state): State<AppState>,
    Json(req): Json<SyncRequest>,
) -> Result<Response, ApiErr> {
    // Validate the window before enqueuing: a backwards range is a client error,
    // not a writer task.
    if let (Some(lo), Some(hi)) = (req.from_us, req.to_us) {
        if lo > hi {
            return Err(ApiErr::bad_request("bad_range", "from_us is after to_us"));
        }
    }
    let (ack_tx, ack_rx) = oneshot::channel::<MarkResult>();
    state
        .mark_synced
        .send(ControlMsg::MarkSynced { req, ack: ack_tx })
        .await
        .map_err(|_| {
            ApiErr::status(
                StatusCode::SERVICE_UNAVAILABLE,
                "writer_unavailable",
                "the writer is not accepting control messages",
            )
        })?;
    // Bound the wait so a wedged writer cannot hang the request. A timeout and a
    // dropped sender both map to a service-unavailable error; the timeout uses a
    // distinct code so the caller can tell a slow writer from a closed channel.
    let res = tokio::time::timeout(std::time::Duration::from_secs(5), ack_rx)
        .await
        .map_err(|_| {
            ApiErr::status(
                StatusCode::SERVICE_UNAVAILABLE,
                "mark_timeout",
                "the writer did not acknowledge in time",
            )
        })?
        .map_err(|_| {
            ApiErr::status(
                StatusCode::SERVICE_UNAVAILABLE,
                "writer_unavailable",
                "the writer dropped the request",
            )
        })?;
    Ok(envelope(
        SyncResponse {
            marked: res.marked,
            unsynced_after: res.unsynced_after,
        },
        1,
        None,
        None,
    )
    .into_response())
}

// --- GET /v1/healthz (public) -------------------------------------------

/// Liveness/readiness. Public on both edges.
pub async fn healthz(State(state): State<AppState>) -> Response {
    let writer_alive = state.writer_alive();
    let integrity = state.integrity.get();
    let db_open = tokio::task::spawn_blocking(move || db::open_readonly(&state.db_path).is_ok())
        .await
        .unwrap_or(false);
    let health = stats::health(db_open, writer_alive, integrity);

    let status = if health.ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(health)).into_response()
}

// --- GET /v1/openapi.json (public) --------------------------------------

/// The generated OpenAPI document. Public on both edges.
pub async fn openapi() -> Json<JsonValue> {
    Json(openapi::document())
}

/// Run a blocking closure on the blocking pool, converting a join failure into a
/// 503. The closure returns the handler's `Result<_, ApiErr>`.
async fn run_blocking<T, F>(f: F) -> Result<Result<T, ApiErr>, ApiErr>
where
    F: FnOnce() -> Result<T, ApiErr> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f).await.map_err(|_| {
        ApiErr::status(
            StatusCode::SERVICE_UNAVAILABLE,
            "internal",
            "the read task did not complete",
        )
    })
}

/// Map a rusqlite read error to the 503 envelope; a read error is a degraded
/// store, not a client error.
fn read_error(e: rusqlite::Error) -> ApiErr {
    ApiErr::status(
        StatusCode::SERVICE_UNAVAILABLE,
        "read_failed",
        format!("read failed: {e}"),
    )
}

#[cfg(test)]
mod tests;
