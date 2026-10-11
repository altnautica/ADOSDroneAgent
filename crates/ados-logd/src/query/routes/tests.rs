use super::*;

#[test]
fn meta_reports_zero_lag_for_a_just_now_row() {
    let m = meta("logd", Some(now_us()));
    assert_eq!(m.source, "logd");
    assert_eq!(m.v, ENVELOPE_VERSION);
    assert!(m.db_lag_ms < 1000, "a fresh row reads as near-zero lag");
}

#[test]
fn api_error_renders_the_error_envelope() {
    let resp = ApiErr::bad_request("bad_cursor", "nope").into_response();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

/// An `AppState` over a real store file, wired to `health`. The control
/// receiver comes back with it so the caller keeps the channel open (these
/// tests never post `/v1/synced`).
fn state_for(
    db_path: std::path::PathBuf,
    health: crate::writer::WriterHealth,
) -> (AppState, mpsc::Receiver<crate::writer::ControlMsg>) {
    let (tx, rx) = mpsc::channel::<crate::writer::ControlMsg>(1);
    let state = AppState {
        db_path: db_path.clone(),
        pool: super::super::pool::ConnPool::new(db_path, super::super::pool::DEFAULT_MAX_IDLE),
        broadcast: broadcast::channel(1).0,
        ingest: Arc::new(crate::ingest::IngestStats::default()),
        tail_slots: Arc::new(super::super::sse::TailSlots::default()),
        export_slots: Arc::new(super::super::sse::ExportSlots::default()),
        mark_synced: tx,
        integrity: super::super::stats::IntegrityVerdict::new(),
        writer_health: health,
    };
    (state, rx)
}

#[tokio::test]
async fn synced_rejects_a_backwards_range_before_touching_the_writer() {
    // A control sender whose receiver is dropped: if the handler tried to
    // enqueue, the send would fail. The backwards-range check must short out
    // first with a 400, so the channel is never touched.
    let (tx, rx) = mpsc::channel::<crate::writer::ControlMsg>(1);
    drop(rx);
    let state = AppState {
        db_path: std::path::PathBuf::from("/nonexistent/logs.db"),
        pool: super::super::pool::ConnPool::new(
            std::path::PathBuf::from("/nonexistent/logs.db"),
            super::super::pool::DEFAULT_MAX_IDLE,
        ),
        broadcast: broadcast::channel(1).0,
        ingest: Arc::new(crate::ingest::IngestStats::default()),
        tail_slots: Arc::new(super::super::sse::TailSlots::default()),
        export_slots: Arc::new(super::super::sse::ExportSlots::default()),
        mark_synced: tx,
        integrity: super::super::stats::IntegrityVerdict::new(),
        writer_health: crate::writer::WriterHealth::new(),
    };
    let req = SyncRequest {
        from_us: Some(200),
        to_us: Some(100),
        ..SyncRequest::default()
    };
    let err = synced(State(state), Json(req)).await.unwrap_err();
    let resp = err.into_response();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn healthz_reports_unhealthy_once_the_writer_has_died() {
    // The store file itself stays perfectly readable — that is the whole
    // point. A dead writer used to leave this endpoint answering 200
    // {ok:true, writer_alive:true} while the Black Box recorded nothing,
    // so an operator (and any probe) read a store that had silently
    // stopped as healthy.
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("logs.db");
    crate::db::open(&db_path).unwrap();

    let health = crate::writer::WriterHealth::new();
    let (state, _control_rx) = state_for(db_path, health.clone());

    let resp = healthz(State(state.clone())).await;
    assert_eq!(resp.status(), StatusCode::OK, "a live writer reads healthy");

    // The writer's run loop returns (an SQLITE_FULL, an I/O error, a
    // corrupt store, or a panic all land here).
    health.mark_ended();

    let resp = healthz(State(state)).await;
    assert_eq!(
        resp.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "a dead writer must not be reported as healthy"
    );
    let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .unwrap();
    let health_json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(health_json["ok"], serde_json::json!(false));
    assert_eq!(health_json["writer_alive"], serde_json::json!(false));
    assert_eq!(
        health_json["db_open"],
        serde_json::json!(true),
        "the store is still readable; it is the writer that died"
    );
}
