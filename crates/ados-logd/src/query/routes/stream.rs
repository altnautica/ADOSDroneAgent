//! The export and live-tail handlers.

use super::*;

/// Streamed bulk export. Reads in keyset batches on a blocking thread and
/// streams the (optionally zstd-compressed) JSONL body as it goes.
pub async fn export(
    State(state): State<AppState>,
    RawQuery(q): RawQuery,
) -> Result<Response, ApiErr> {
    let params = QueryParams::parse(q.as_deref().unwrap_or(""));
    let filters = QueryFilters::parse(&params, now_us())?;
    let format = Format::parse(params.get("format"))?;

    // Claim a bulk-export slot or refuse with 429. An export is far heavier than
    // a query (a dedicated thread + a read-only connection + a zstd encoder held
    // for its whole lifetime), so the concurrency is capped low independently of
    // the per-second read budget. The guard rides the stream and frees the slot
    // when the export ends (completes or the client disconnects).
    let Some(guard) = state.export_slots.try_acquire() else {
        return Err(ApiErr::status(
            StatusCode::TOO_MANY_REQUESTS,
            "export_busy",
            "the concurrent-export limit is reached; try again shortly",
        ));
    };

    // A bounded channel from the blocking export thread to the async body; a
    // slow client backpressures the read pace, never the writer.
    let (tx, rx) = mpsc::channel::<Vec<u8>>(8);
    let db_path = state.db_path.clone();
    std::thread::Builder::new()
        .name("ados-logd-export".to_string())
        .spawn(move || export::run_export(db_path, filters, format, tx))
        .map_err(|e| {
            ApiErr::status(
                StatusCode::SERVICE_UNAVAILABLE,
                "export_unavailable",
                format!("could not start export: {e}"),
            )
        })?;

    // Carry the slot guard in the unfold state so it is dropped only when the
    // body stream ends, holding the slot for the export's whole lifetime.
    let stream = futures::stream::unfold((rx, guard), |(mut rx, guard)| async move {
        rx.recv()
            .await
            .map(|chunk| (Ok::<_, Infallible>(chunk), (rx, guard)))
    });
    let body = axum::body::Body::from_stream(stream);

    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, format.content_type().parse().unwrap());
    headers.insert(
        header::CONTENT_DISPOSITION,
        format!("attachment; filename=\"{}\"", format.filename())
            .parse()
            .unwrap(),
    );
    Ok((headers, body).into_response())
}

// --- GET /v1/tail (SSE) -------------------------------------------------

/// Live Server-Sent-Events tail. Sends a bounded replay of recent rows, then
/// switches to the writer's broadcast, filtering each fanned-out frame against
/// the request filters. Slow subscribers are told they lagged rather than
/// blocking the writer, and the subscriber count is capped.
pub async fn tail(
    State(state): State<AppState>,
    RawQuery(q): RawQuery,
) -> Result<Response, ApiErr> {
    let params = QueryParams::parse(q.as_deref().unwrap_or(""));
    let filters = QueryFilters::parse(&params, now_us())?;
    let replay: u32 = params
        .get("replay")
        .and_then(|r| r.parse::<u32>().ok())
        .unwrap_or(0)
        .min(crate::query::params::MAX_LIMIT);

    // Claim a subscriber slot or refuse with 429.
    let Some(guard) = state.tail_slots.try_acquire() else {
        return Err(ApiErr::status(
            StatusCode::TOO_MANY_REQUESTS,
            "tail_busy",
            "the live-tail subscriber limit is reached; try again shortly",
        ));
    };

    // Subscribe to the live broadcast BEFORE reading the replay so no frame
    // between the replay read and the live switch is lost.
    let rx = state.broadcast.subscribe();

    // Read the bounded replay backlog (newest-first, then reversed so the client
    // sees oldest-of-the-replay first and the live tail continues forward).
    let replay_rows = if replay > 0 {
        let mut replay_filters = filters.clone();
        replay_filters.limit = replay;
        let st = state.clone();
        run_blocking(move || {
            let conn = st.open_ro()?;
            Ok(read_replay_json(&conn, &replay_filters))
        })
        .await?
        .unwrap_or_default()
    } else {
        Vec::new()
    };

    let stream = tail_stream(rx, filters, replay_rows, guard);
    let sse = Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(std::time::Duration::from_secs(15))
            .text("keep-alive"),
    );
    Ok(sse.into_response())
}

/// Read the replay backlog for a tail as JSON rows (newest-first from the DB,
/// reversed so the emitted order is oldest→newest and the live tail follows on).
pub(super) fn read_replay_json(
    conn: &rusqlite::Connection,
    filters: &QueryFilters,
) -> Vec<JsonValue> {
    let mut rows: Vec<JsonValue> = match filters.table {
        Table::Logs => rows::query_logs(conn, filters, None)
            .map(|p| {
                p.rows
                    .iter()
                    .filter_map(|r| serde_json::to_value(r).ok())
                    .collect()
            })
            .unwrap_or_default(),
        Table::Events => rows::query_events(conn, filters, None)
            .map(|p| {
                p.rows
                    .iter()
                    .filter_map(|r| serde_json::to_value(r).ok())
                    .collect()
            })
            .unwrap_or_default(),
        Table::Metrics => rows::query_metrics(conn, filters, None)
            .map(|p| {
                p.rows
                    .iter()
                    .filter_map(|r| serde_json::to_value(r).ok())
                    .collect()
            })
            .unwrap_or_default(),
        Table::Hw => rows::query_hw(conn, filters, None)
            .map(|p| {
                p.rows
                    .iter()
                    .filter_map(|r| serde_json::to_value(r).ok())
                    .collect()
            })
            .unwrap_or_default(),
    };
    rows.reverse();
    rows
}

/// The SSE event stream: emit each replay row, then each matching live frame.
/// A broadcast lag is surfaced as a `lagged` event rather than dropping the
/// connection or blocking the writer; the stream ends when the channel closes
/// or the client disconnects (the `guard` frees the slot on drop).
pub(super) fn tail_stream(
    mut rx: broadcast::Receiver<IngestFrame>,
    filters: QueryFilters,
    replay_rows: Vec<JsonValue>,
    guard: crate::query::sse::TailGuard,
) -> impl Stream<Item = Result<Event, Infallible>> {
    async_stream::stream! {
        // Move the guard into the stream so the slot is held for the whole
        // lifetime of the SSE connection and freed when the stream is dropped.
        let _guard = guard;
        for row in replay_rows {
            yield Ok(Event::default().json_data(row).unwrap_or_else(|_| Event::default().comment("skip")));
        }
        loop {
            match rx.recv().await {
                Ok(frame) => {
                    if frame_matches(&frame, &filters) {
                        if let Some(json) = frame_to_json(&frame) {
                            match Event::default().json_data(json) {
                                Ok(ev) => yield Ok(ev),
                                Err(_) => continue,
                            }
                        }
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    // The subscriber fell behind; tell it how many it missed and
                    // keep going on the live edge. The writer was never blocked.
                    let note = serde_json::json!({ "kind": "lagged", "dropped": n });
                    if let Ok(ev) = Event::default().event("lagged").json_data(note) {
                        yield Ok(ev);
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    }
}
