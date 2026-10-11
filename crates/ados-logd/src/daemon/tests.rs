use super::*;
use ados_protocol::logd::{IngestFrame, Level, LogFrame};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;

fn temp_paths(dir: &Path) -> DaemonPaths {
    // Point the hardware collector at an empty subtree so the daemon test
    // exercises its lifecycle wiring without reading the host's `/sys`.
    let hw_root = dir.join("hwroot");
    std::fs::create_dir_all(&hw_root).unwrap();
    // Point the taps at the tempdir with no sockets or sidecars present, so
    // the daemon test exercises their spawn + absent-seam backoff + shutdown
    // wiring without touching the host's runtime directory.
    DaemonPaths {
        db: dir.join("logs.db"),
        ingest_socket: dir.join("logd.sock"),
        query_socket: dir.join("logd-query.sock"),
        // Port 0 asks the OS for an ephemeral free port so the daemon test
        // never collides with a real listener on the bench TCP port.
        query_tcp_port: 0,
        pairing_path: dir.join("pairing.json"),
        hw_root,
        taps: TapPaths {
            state_socket: dir.join("state.sock"),
            mavlink_socket: dir.join("mavlink.sock"),
            sidecar_root: dir.to_path_buf(),
        },
    }
}

#[tokio::test]
async fn end_to_end_bring_up_ingest_and_clean_shutdown() {
    let dir = tempfile::tempdir().unwrap();
    let paths = temp_paths(dir.path());
    let socket_path = paths.ingest_socket.clone();
    let db_path = paths.db.clone();

    // The stop trigger the daemon awaits; the test fires it after sending.
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let daemon = tokio::spawn(run_with_paths(paths, async move {
        let _ = stop_rx.await;
    }));

    // Wait for the ingest socket to appear, then connect and send a frame
    // carrying a secret field.
    for _ in 0..200 {
        if socket_path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let mut client = UnixStream::connect(&socket_path).await.unwrap();
    let mut log = LogFrame::new(now_us(), "test-producer", Level::Warn, "hello");
    log.fields.insert(
        "api_key".to_string(),
        rmpv::Value::from("secret_value_12345"),
    );
    let wire = IngestFrame::Log(log).encode().unwrap();
    client.write_all(&wire).await.unwrap();
    client.flush().await.unwrap();
    drop(client);

    // Give the writer a moment to drain and commit the batch, then stop.
    tokio::time::sleep(Duration::from_millis(250)).await;
    let _ = stop_tx.send(());
    daemon.await.unwrap().unwrap();

    // The row landed, redacted, and the boot session closed on shutdown.
    let ro = db::open_readonly(&db_path).unwrap();
    let (count, redacted): (i64, i64) = ro
        .query_row(
            "SELECT count(*), coalesce(sum(redacted),0) FROM logs WHERE source='test-producer'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(count, 1);
    assert_eq!(redacted, 1);

    let fields_blob: Vec<u8> = ro
        .query_row(
            "SELECT fields FROM logs WHERE source='test-producer'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let fields: ados_protocol::logd::Fields = rmp_serde::from_slice(&fields_blob).unwrap();
    let api_key = fields.get("api_key").and_then(|v| v.as_str()).unwrap();
    assert!(
        api_key.starts_with("redacted:"),
        "secret must never reach disk in the clear: {api_key}"
    );

    let (ended, reason): (Option<i64>, Option<String>) = ro
        .query_row(
            "SELECT ended_us, reason FROM sessions WHERE kind='boot'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert!(ended.is_some());
    assert_eq!(reason.as_deref(), Some("shutdown"));
    db::integrity_check(&ro).unwrap();

    // The sockets were unlinked on shutdown.
    assert!(!socket_path.exists(), "ingest socket should be unlinked");
}

#[test]
fn open_and_verify_quarantines_a_non_sqlite_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("logs.db");
    // Write a file that is not a valid SQLite database. This is the
    // realistic failure: a truncated or garbage header makes db::open()
    // error on its first PRAGMA, before any query runs.
    std::fs::write(&path, b"this is not a sqlite database, it is garbage bytes").unwrap();
    // The broken store must self-heal in-process: no error bubbles up, the
    // file is quarantined, and a fresh healthy store is recreated in place.
    open_and_verify(&path).expect("a broken store must be recreated, not propagated as fatal");

    let conn = db::open(&path).unwrap();
    db::integrity_check(&conn).unwrap();

    // A quarantine copy was left behind, and it still holds the garbage.
    let quarantined: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains("db.corrupt-"))
        .collect();
    assert_eq!(quarantined.len(), 1, "exactly one quarantine copy expected");
    let saved = std::fs::read(quarantined[0].path()).unwrap();
    assert_eq!(saved, b"this is not a sqlite database, it is garbage bytes");
}

#[test]
fn repeated_corruption_does_not_accumulate_quarantines() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("logs.db");

    // Corrupt and recover several times over, as a unit that keeps losing
    // power mid-write would. Each round quarantines the broken store.
    for round in 0..4u32 {
        std::fs::write(&path, format!("garbage from round {round}")).unwrap();
        open_and_verify(&path).expect("a broken store must be recreated");
        // The timestamped suffix has microsecond resolution; make sure two
        // rounds cannot land on the same name or share a modified time.
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    let quarantined: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains("db.corrupt-"))
        .collect();

    // Without pruning this grew one dead copy per corruption, forever. On a
    // field unit that reached 1.5 GB of quarantines beside the live store.
    assert_eq!(
        quarantined.len(),
        QUARANTINE_KEEP,
        "quarantines must not accumulate across repeated corruption"
    );

    // The one kept is the most recent, which is the one worth a post-mortem.
    let kept = std::fs::read(quarantined[0].path()).unwrap();
    assert_eq!(
        kept, b"garbage from round 3",
        "the newest quarantine is the one retained"
    );
}

#[test]
fn a_healthy_start_reclaims_quarantines_left_by_an_older_build() {
    // Pruning used to run only as part of quarantining, so a node that
    // corrupted twice under a build that predated it carried both dead
    // copies — up to two gigabytes of unreadable file — until it corrupted
    // a *third* time. That is the worst possible moment to be short of
    // space, and being short of space is a fair way to cause it.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("logs.db");

    // A healthy live store, with two stale quarantines beside it.
    open_and_verify(&path).expect("a fresh store comes up healthy");
    for (i, name) in ["logs.db.corrupt-100", "logs.db.corrupt-200"]
        .into_iter()
        .enumerate()
    {
        std::fs::write(dir.path().join(name), format!("dead copy {i}")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    // Starting again, with nothing wrong, must still reclaim them.
    open_and_verify(&path).expect("the healthy store still verifies");

    let quarantined: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains("db.corrupt-"))
        .collect();
    assert_eq!(
        quarantined.len(),
        QUARANTINE_KEEP,
        "a healthy start must reclaim superseded quarantines, not wait for the next corruption"
    );
    assert_eq!(
        std::fs::read(quarantined[0].path()).unwrap(),
        b"dead copy 1",
        "the newest is the one kept for a post-mortem"
    );
}

#[test]
fn open_and_verify_creates_a_fresh_store_when_absent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("logs.db");
    // No file present: open_and_verify creates a healthy store and leaves no
    // quarantine copy (there was nothing to move aside).
    open_and_verify(&path).expect("an absent store should be created cleanly");
    let conn = db::open(&path).unwrap();
    db::integrity_check(&conn).unwrap();
    let quarantined = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .any(|e| e.file_name().to_string_lossy().contains("db.corrupt-"));
    assert!(
        !quarantined,
        "no quarantine copy when there was no prior file"
    );
}

#[tokio::test]
async fn the_watchdog_stops_being_fed_once_the_writer_ends() {
    use std::sync::atomic::AtomicUsize;

    // Real (short) intervals rather than a paused clock: the progress stamp
    // is measured on the standard monotonic clock, which tokio's test clock
    // does not advance, so pausing time would freeze the very thing under
    // test.
    let health = WriterHealth::new();
    let pings = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&pings);
    let task = tokio::spawn(watchdog_loop(
        Duration::from_millis(10),
        Duration::from_secs(60),
        health.clone(),
        move || {
            counter.fetch_add(1, Ordering::SeqCst);
        },
    ));

    // A live writer is fed.
    tokio::time::sleep(Duration::from_millis(120)).await;
    let fed = pings.load(Ordering::SeqCst);
    assert!(fed >= 3, "a live writer must be fed: {fed}");

    // The writer's run loop returns — SQLITE_FULL, an I/O error, a corrupt
    // store or a panic all end here.
    health.mark_ended();
    // Let any tick already in flight land, then take the baseline.
    tokio::time::sleep(Duration::from_millis(30)).await;
    let at_death = pings.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        pings.load(Ordering::SeqCst),
        at_death,
        "a dead writer must stop the watchdog so systemd restarts the unit"
    );
    task.abort();
}

#[test]
fn a_writer_that_stops_stamping_is_not_alive_even_before_it_exits() {
    // The wedge case, distinct from the exit case: a `commit_batch` blocked
    // forever inside SQLite never returns, so nothing marks it ended. The
    // stamp is what catches it.
    let health = WriterHealth::new();
    health.mark();
    assert!(health.is_alive(Duration::from_secs(60)));
    std::thread::sleep(Duration::from_millis(5));
    assert!(
        !health.is_alive(Duration::from_millis(1)),
        "a writer that has not stamped within the budget is not alive"
    );
}

#[tokio::test]
async fn the_daemon_exits_non_zero_when_its_writer_dies() {
    // The store's write path starts failing underneath a running writer —
    // an SQLITE_FULL, an I/O error or a damaged schema all look like this
    // from the loop's side. The writer's run loop returns Err, and the
    // daemon must come down non-zero so `Restart=always` cycles it,
    // instead of serving a read surface in front of a Black Box that
    // stopped recording. (A single bad ROW is deliberately skipped, not
    // fatal, so this fails the session write, which is not skippable.)
    let dir = tempfile::tempdir().unwrap();
    let paths = temp_paths(dir.path());
    let socket_path = paths.ingest_socket.clone();
    let db_path = paths.db.clone();

    // A stop trigger that is never fired: the only way out is the writer.
    let (_stop_tx, stop_rx) = oneshot::channel::<()>();
    let daemon = tokio::spawn(run_with_paths(paths, async move {
        let _ = stop_rx.await;
    }));

    for _ in 0..200 {
        if socket_path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Make the next session write fail, then send the frame that triggers
    // one (an arm event opens a flight session).
    {
        let conn = db::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TRIGGER fail_sessions BEFORE INSERT ON sessions \
                 BEGIN SELECT RAISE(ABORT, 'simulated store write failure'); END",
        )
        .unwrap();
    }
    let mut client = UnixStream::connect(&socket_path).await.unwrap();
    let mut arm =
        ados_protocol::logd::EventFrame::new(now_us(), "state.arm", "test-producer", Level::Info);
    arm.detail
        .insert("reason".to_string(), rmpv::Value::from("arm"));
    let wire = IngestFrame::Event(arm).encode().unwrap();
    client.write_all(&wire).await.unwrap();
    client.flush().await.unwrap();
    drop(client);

    let outcome = tokio::time::timeout(Duration::from_secs(20), daemon)
        .await
        .expect("the daemon must come down on its own when the writer dies")
        .unwrap();
    assert!(
        outcome.is_err(),
        "a writer death must be a non-zero exit, not a clean stop"
    );
}
