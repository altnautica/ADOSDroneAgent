//! End-to-end ingestion test: bring the daemon up against a temp store, connect
//! a client to the ingest socket, send each frame kind plus an arm/disarm
//! sequence, stop the daemon, and assert every row landed (redacted where it
//! must be) with the sessions opened and closed correctly.
//!
//! This exercises the production path: the async accept loop, the bounded
//! channel, and the blocking single-writer thread, with no test-only shortcut
//! into the writer.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::sync::oneshot;

use ados_logd::daemon::{run_with_paths, DaemonPaths};
use ados_logd::db;
use ados_logd::taps::TapPaths;
use ados_logd::writer::now_us;
use ados_protocol::logd::{EventFrame, HwSnapshot, IngestFrame, Level, LogFrame, TelemetryFrame};

/// Connect to the ingest socket once it appears, then write framed msgpack for
/// each frame in order on one connection.
async fn send_frames(socket_path: &Path, frames: &[IngestFrame]) {
    for _ in 0..300 {
        if socket_path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let mut client = UnixStream::connect(socket_path)
        .await
        .expect("connect to the ingest socket");
    for frame in frames {
        let wire = frame.encode().expect("encode frame");
        client.write_all(&wire).await.expect("write frame");
    }
    client.flush().await.expect("flush");
    // Closing the connection signals EOF to the per-client read task.
    drop(client);
}

#[tokio::test]
async fn ingest_socket_accepts_frames_and_writer_inserts_them() {
    let dir = tempfile::tempdir().unwrap();
    // Point the hardware collector at an empty subtree so this integration test
    // exercises the socket + writer path without reading the host's `/sys`.
    let hw_root = dir.path().join("hwroot");
    std::fs::create_dir_all(&hw_root).unwrap();
    // Point the taps at the tempdir too (no sockets or sidecars present), so the
    // integration test never reaches for the host's real runtime directory and
    // the daemon's tap wiring is exercised on the absent-seam path.
    let paths = DaemonPaths {
        db: dir.path().join("logs.db"),
        ingest_socket: dir.path().join("logd.sock"),
        query_socket: dir.path().join("logd-query.sock"),
        // Port 0 asks the OS for an ephemeral free port so this test never
        // collides with a real listener on the bench TCP port.
        query_tcp_port: 0,
        pairing_path: dir.path().join("pairing.json"),
        hw_root,
        taps: TapPaths {
            state_socket: dir.path().join("state.sock"),
            mavlink_socket: dir.path().join("mavlink.sock"),
            sidecar_root: dir.path().to_path_buf(),
        },
    };
    let socket_path = paths.ingest_socket.clone();
    let db_path = paths.db.clone();

    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let daemon = tokio::spawn(run_with_paths(paths, async move {
        let _ = stop_rx.await;
    }));

    // A log carrying a secret field, a telemetry sample, an event, a hardware
    // snapshot, and an arm/disarm flight session bracketing one in-flight log.
    let base = now_us();
    let mut secret_log = LogFrame::new(base, "test-producer", Level::Warn, "test message");
    secret_log.fields.insert(
        "api_key".to_string(),
        rmpv::Value::from("secret_value_12345"),
    );

    let mut tele = TelemetryFrame::new(base + 1, "cpu.load", 0.42);
    tele.tags
        .insert("core".to_string(), rmpv::Value::from(0u64));

    let evt = EventFrame::new(base + 2, "radio.lock", "test-producer", Level::Info);

    let mut hw = HwSnapshot::new(base + 3);
    hw.signals
        .insert("thermal.soc_c".to_string(), rmpv::Value::from(54.5));

    let mut arm = EventFrame::new(base + 4, "service.transition", "state", Level::Info);
    arm.detail
        .insert("reason".to_string(), rmpv::Value::from("arm"));
    let in_flight = LogFrame::new(base + 5, "test-producer", Level::Info, "in flight now");
    let mut disarm = EventFrame::new(base + 6, "service.transition", "state", Level::Info);
    disarm
        .detail
        .insert("reason".to_string(), rmpv::Value::from("disarm"));

    let frames = vec![
        IngestFrame::Log(secret_log),
        IngestFrame::Telemetry(tele),
        IngestFrame::Event(evt),
        IngestFrame::Hw(hw),
        IngestFrame::Event(arm),
        IngestFrame::Log(in_flight),
        IngestFrame::Event(disarm),
    ];

    send_frames(&socket_path, &frames).await;

    // Let the writer drain and commit the batch, then stop the daemon cleanly.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let _ = stop_tx.send(());
    daemon.await.unwrap().unwrap();

    // --- assertions against the read-only store ---
    let ro = db::open_readonly(&db_path).unwrap();

    // The secret log landed exactly once and is flagged redacted.
    let (log_count, redacted): (i64, i64) = ro
        .query_row(
            "SELECT count(*), coalesce(sum(redacted),0) FROM logs \
             WHERE source='test-producer' AND msg='test message'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(log_count, 1);
    assert_eq!(
        redacted, 1,
        "the log with a secret field must be flagged redacted"
    );

    // The secret value never reached disk in the clear.
    let fields_blob: Vec<u8> = ro
        .query_row(
            "SELECT fields FROM logs WHERE source='test-producer' AND msg='test message'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let fields: BTreeMap<String, rmpv::Value> = rmp_serde::from_slice(&fields_blob).unwrap();
    let api_key = fields.get("api_key").and_then(|v| v.as_str()).unwrap();
    assert!(
        api_key.starts_with("redacted:"),
        "api_key should be redacted on disk: {api_key}"
    );

    // The telemetry sample and the event landed.
    let metric_count: i64 = ro
        .query_row(
            "SELECT count(*) FROM metrics WHERE metric='cpu.load'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(metric_count, 1);
    let event_count: i64 = ro
        .query_row(
            "SELECT count(*) FROM events WHERE kind='radio.lock'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(event_count, 1);

    // The hardware snapshot landed with its signal preserved in the blob.
    let hw_blob: Vec<u8> = ro
        .query_row("SELECT signals FROM hw", [], |r| r.get(0))
        .unwrap();
    let signals: BTreeMap<String, rmpv::Value> = rmp_serde::from_slice(&hw_blob).unwrap();
    assert_eq!(
        signals.get("thermal.soc_c").and_then(|v| v.as_f64()),
        Some(54.5)
    );

    // A flight session opened on arm and closed on disarm.
    let (flight_id, flight_ended): (i64, Option<i64>) = ro
        .query_row(
            "SELECT id, ended_us FROM sessions WHERE kind='flight'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert!(
        flight_ended.is_some(),
        "flight session must close on disarm"
    );

    // The in-flight log is attributed to the flight session.
    let in_flight_count: i64 = ro
        .query_row(
            "SELECT count(*) FROM logs WHERE session=?1 AND msg='in flight now'",
            [flight_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(in_flight_count, 1);

    // The boot session closed with reason 'shutdown'.
    let (boot_ended, boot_reason): (Option<i64>, Option<String>) = ro
        .query_row(
            "SELECT ended_us, reason FROM sessions WHERE kind='boot' LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert!(boot_ended.is_some());
    assert_eq!(boot_reason.as_deref(), Some("shutdown"));

    // The store is structurally sound after the full round trip.
    db::integrity_check(&ro).unwrap();

    // The ingest socket was unlinked on shutdown.
    assert!(
        !socket_path.exists(),
        "ingest socket should be unlinked on stop"
    );
}
