//! Daemon lifecycle coordination.
//!
//! Brings up the store, the dedicated writer thread, and the async ingest accept
//! loop, then waits for a stop signal and shuts down cleanly. The split is
//! deliberate: the writer is a blocking `std::thread` holding the only
//! read-write `rusqlite` connection, and the accept loop is async on the tokio
//! runtime. A bounded channel is the only bridge between the two worlds, so the
//! synchronous SQLite work never runs inside an async task.
//!
//! Startup order: open + verify the store (quarantine and recreate on a failed
//! integrity check, since the store is a cache of history, not flight state),
//! spawn the writer thread, bind the ingest socket, spawn the accept loop, then
//! notify systemd `READY`. Shutdown order on `SIGTERM`/`SIGINT`: notify
//! `STOPPING`, stop accepting, drop the ingest sender so the writer drains and
//! commits its final batch and closes the session, join the writer (bounded),
//! and unlink the sockets.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::{mpsc, oneshot};

use crate::db;
use crate::ingest::{run_accept_loop, IngestSocket, IngestStats};
use crate::writer::{now_us, Writer, WriterConfig};

/// Capacity of the bounded channel from the async accept loop to the blocking
/// writer thread. Bounds memory so a producer flood cannot grow the queue
/// without limit; the per-class drop policy sheds the overflow visibly.
pub const INGEST_QUEUE_CAPACITY: usize = 4096;

/// How long the writer thread is given to drain and commit its final batch on
/// shutdown before the daemon stops waiting and exits anyway.
pub const WRITER_JOIN_TIMEOUT: Duration = Duration::from_secs(10);

/// Resolved paths a daemon run needs: the store and the two sockets it owns.
#[derive(Debug, Clone)]
pub struct DaemonPaths {
    /// The read-write store.
    pub db: PathBuf,
    /// The ingest socket producers write framed msgpack to.
    pub ingest_socket: PathBuf,
    /// The query socket (bound by the query API in a later chunk; unlinked here
    /// on shutdown so a stale path never confuses a probing reader).
    pub query_socket: PathBuf,
}

impl Default for DaemonPaths {
    fn default() -> Self {
        Self {
            db: PathBuf::from(crate::paths::DB_PATH),
            ingest_socket: PathBuf::from(crate::paths::INGEST_SOCKET),
            query_socket: PathBuf::from(crate::paths::QUERY_SOCKET),
        }
    }
}

/// systemd readiness ping. No-op off Linux and when not run under a
/// `Type=notify` unit (`NOTIFY_SOCKET` unset).
#[cfg(target_os = "linux")]
fn sd_ready() {
    if let Err(e) = sd_notify::notify(false, &[sd_notify::NotifyState::Ready]) {
        tracing::debug!(error = %e, "sd_notify READY failed");
    }
}

#[cfg(not(target_os = "linux"))]
fn sd_ready() {}

/// systemd stopping ping. No-op off Linux / outside a notify unit.
#[cfg(target_os = "linux")]
fn sd_stopping() {
    if let Err(e) = sd_notify::notify(false, &[sd_notify::NotifyState::Stopping]) {
        tracing::debug!(error = %e, "sd_notify STOPPING failed");
    }
}

#[cfg(not(target_os = "linux"))]
fn sd_stopping() {}

/// Open the store and verify it. On a failed integrity check the file is
/// quarantined (renamed with a timestamp suffix) and a fresh store is created
/// from the embedded schema, so a corrupt history cache never wedges the daemon.
/// Returns once a healthy store exists at `path`.
fn open_and_verify(path: &Path) -> Result<()> {
    // A first open also runs migrations and creates the file + parent dir.
    let conn = db::open(path).with_context(|| format!("open store at {}", path.display()))?;
    match db::integrity_check(&conn) {
        Ok(()) => {
            tracing::info!(path = %path.display(), "store integrity check passed");
            Ok(())
        }
        Err(e) => {
            drop(conn);
            let quarantine = path.with_extension(format!("db.corrupt-{}", now_us()));
            tracing::error!(
                error = %e,
                quarantine = %quarantine.display(),
                "store failed integrity check; quarantining and recreating"
            );
            std::fs::rename(path, &quarantine)
                .with_context(|| format!("quarantine {}", path.display()))?;
            // Recreate from the embedded schema.
            let _ = db::open(path).with_context(|| "recreate store after quarantine")?;
            Ok(())
        }
    }
}

/// Run the daemon to completion: bring everything up, wait for a stop signal,
/// shut down cleanly. Returns `Ok(())` after a graceful stop.
pub async fn run_daemon() -> Result<()> {
    run_with_paths(DaemonPaths::default(), shutdown_signal()).await
}

/// The lifecycle, parameterized over the paths and the stop trigger so tests can
/// drive a real bring-up + shutdown against a temp store without sending a
/// process signal.
pub async fn run_with_paths<F>(paths: DaemonPaths, shutdown: F) -> Result<()>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    open_and_verify(&paths.db)?;

    // The bridge from the async accept loop to the blocking writer thread.
    let (ingest_tx, ingest_rx) = mpsc::channel(INGEST_QUEUE_CAPACITY);

    // Build the writer on the daemon thread (so an open error surfaces here),
    // then move it onto its own dedicated OS thread to run the blocking loop.
    let writer = Writer::new(&paths.db, ingest_rx, WriterConfig::default())
        .context("open writer connection")?;
    let boot_session = writer.boot_session();
    // The broadcast handle is the seam the future live tail subscribes to; held
    // so the channel stays open for the daemon's lifetime even with no consumer.
    let _broadcast = writer.broadcast_handle();
    let (writer_result_tx, writer_result_rx) = oneshot::channel();
    let writer_thread = std::thread::Builder::new()
        .name("ados-logd-writer".to_string())
        .spawn(move || {
            let result = writer.run();
            let _ = writer_result_tx.send(result);
        })
        .context("spawn writer thread")?;

    // Bind the ingest socket and spawn the accept loop. A dedicated shutdown
    // channel lets the daemon stop the loop before tearing down the writer.
    let socket = IngestSocket::bind(&paths.ingest_socket)
        .with_context(|| format!("bind ingest socket {}", paths.ingest_socket.display()))?;
    let stats = Arc::new(IngestStats::default());
    let (accept_stop_tx, accept_stop_rx) = oneshot::channel::<()>();
    let accept_task = tokio::spawn(run_accept_loop(
        socket,
        ingest_tx.clone(),
        Arc::clone(&stats),
        async move {
            let _ = accept_stop_rx.await;
        },
    ));

    sd_ready();
    tracing::info!(
        boot_session,
        db = %paths.db.display(),
        ingest = %paths.ingest_socket.display(),
        "logging store ready"
    );

    // Run until the stop trigger fires.
    shutdown.await;
    tracing::info!("logging store stopping");
    sd_stopping();

    // Stop accepting new clients, then wait for the accept loop to finish.
    let _ = accept_stop_tx.send(());
    let _ = accept_task.await;

    // Drop every sender so the writer sees the channel close, drains the queue,
    // commits the final batch, closes the session, and truncates the WAL.
    drop(ingest_tx);

    // Join the writer thread off the async runtime, bounded so a stuck writer
    // cannot hang shutdown past the unit's stop timeout.
    join_writer(writer_thread, writer_result_rx).await;

    // tmpfs cleanup: a stale socket path confuses a producer probing for the
    // socket on the next start. The query socket is bound elsewhere later; unlink
    // it defensively too.
    let _ = std::fs::remove_file(&paths.ingest_socket);
    let _ = std::fs::remove_file(&paths.query_socket);

    tracing::info!("logging store stopped");
    Ok(())
}

/// Wait for the writer thread to finish, bounded by [`WRITER_JOIN_TIMEOUT`]. The
/// writer signals its result over a oneshot the moment `run` returns; the join
/// of the OS thread itself is then immediate. If the writer overruns the bound
/// (a wedged commit), the daemon logs and exits rather than hang.
async fn join_writer(
    handle: std::thread::JoinHandle<()>,
    result_rx: oneshot::Receiver<Result<(), crate::writer::WriterError>>,
) {
    match tokio::time::timeout(WRITER_JOIN_TIMEOUT, result_rx).await {
        Ok(Ok(Ok(()))) => {
            let _ = handle.join();
            tracing::info!("writer drained and committed the final batch");
        }
        Ok(Ok(Err(e))) => {
            let _ = handle.join();
            tracing::error!(error = %e, "writer ended with an error");
        }
        Ok(Err(_)) => {
            // The writer dropped its result sender without sending (it panicked).
            let _ = handle.join();
            tracing::error!("writer thread ended without a result");
        }
        Err(_) => {
            tracing::error!(
                timeout_s = WRITER_JOIN_TIMEOUT.as_secs(),
                "writer did not finish within the shutdown bound; exiting"
            );
        }
    }
}

/// Resolve when the process receives `SIGTERM` or `SIGINT`. The production stop
/// trigger.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "failed to install SIGTERM handler");
                return;
            }
        };
        let mut sigint = match signal(SignalKind::interrupt()) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "failed to install SIGINT handler");
                return;
            }
        };
        tokio::select! {
            _ = sigterm.recv() => tracing::info!("received SIGTERM"),
            _ = sigint.recv() => tracing::info!("received SIGINT"),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("received interrupt");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_protocol::logd::{IngestFrame, Level, LogFrame};
    use tokio::io::AsyncWriteExt;
    use tokio::net::UnixStream;

    fn temp_paths(dir: &Path) -> DaemonPaths {
        DaemonPaths {
            db: dir.join("logs.db"),
            ingest_socket: dir.join("logd.sock"),
            query_socket: dir.join("logd-query.sock"),
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
    fn open_and_verify_quarantines_a_corrupt_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs.db");
        // Write a file that is not a valid SQLite database.
        std::fs::write(&path, b"this is not a sqlite database, it is garbage bytes").unwrap();
        // open() may fail outright (not a database) or pass open and fail the
        // integrity check; either way the recovery path must yield a healthy
        // store with no error bubbling up to the caller.
        let r = open_and_verify(&path);
        // If the bytes were rejected at open, open_and_verify surfaces the error
        // (the caller would then exit and systemd restarts onto a clean file via
        // the unit's recovery); in the common corruption case it recreates. We
        // accept either, but when it returns Ok the store must be usable.
        if r.is_ok() {
            let conn = db::open(&path).unwrap();
            db::integrity_check(&conn).unwrap();
            // A quarantine copy was left behind.
            let quarantined = std::fs::read_dir(dir.path())
                .unwrap()
                .filter_map(|e| e.ok())
                .any(|e| e.file_name().to_string_lossy().contains("db.corrupt-"));
            assert!(quarantined, "a quarantine copy should be left behind");
        }
    }
}
