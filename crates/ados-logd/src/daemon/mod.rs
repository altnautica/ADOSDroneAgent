//! Daemon lifecycle coordination.
//!
//! Brings up the store, the dedicated writer thread, and the async ingest accept
//! loop, then waits for a stop signal and shuts down cleanly. The split is
//! deliberate: the writer is a blocking `std::thread` holding the only
//! read-write `rusqlite` connection, and the accept loop is async on the tokio
//! runtime. A bounded channel is the only bridge between the two worlds, so the
//! synchronous SQLite work never runs inside an async task.
//!
//! Startup order: open + verify the store (a fast `quick_check`, quarantining
//! and recreating on failure, since the store is a cache of history not flight
//! state — the boot path must not run the size-scaling full integrity check),
//! spawn the writer thread, bind the ingest socket, spawn the accept loop, then
//! notify systemd `READY`. Shutdown order on `SIGTERM`/`SIGINT`: notify
//! `STOPPING`, stop accepting, drop the ingest sender so the writer drains and
//! commits its final batch and closes the session, join the writer (bounded),
//! and unlink the sockets.

mod signals;
mod startup;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::db;
use crate::ingest::{run_accept_loop, IngestSocket, IngestStats};
use crate::taps::{spawn_all_taps, TapPaths};
use crate::writer::{now_us, Writer, WriterConfig, WriterHealth, WRITER_STALL_BUDGET};

/// Capacity of the bounded channel from the async accept loop to the blocking
/// writer thread. Bounds memory so a producer flood cannot grow the queue
/// without limit; the per-class drop policy sheds the overflow visibly.
pub const INGEST_QUEUE_CAPACITY: usize = 4096;

/// How long the writer thread is given to drain and commit its final batch on
/// shutdown before the daemon stops waiting and exits anyway.
pub const WRITER_JOIN_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the orderly stop of the async components (read surface, accept loop,
/// hardware collector, seam taps) is given before they are abandoned and shutdown
/// presses on to close the ingest channel. Bounds a tap that blocks on a seam
/// whose provider is dying in the same control-group SIGTERM, so a `systemctl
/// stop` can never sit in `deactivating` until the unit's stop timeout.
pub const TEARDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// How often the daemon pings the systemd watchdog while running. Comfortably
/// under the unit's `WatchdogSec` (a ~3x margin) so a single missed tick from a
/// brief scheduler stall does not trip a restart, but a genuinely wedged async
/// runtime does.
pub const WATCHDOG_PING_INTERVAL: Duration = Duration::from_secs(10);

/// Default raw-frame sampling rate for the frame tap, in frames per second. The
/// frame trail is a low-rate diagnostic record, not the full firehose; the
/// config surface overrides this.
pub const DEFAULT_MAVLINK_SAMPLE_HZ: f64 = 1.0;

/// Resolved paths a daemon run needs: the store and the two sockets it owns,
/// plus the filesystem root the hardware collector samples under.
#[derive(Debug, Clone)]
pub struct DaemonPaths {
    /// The read-write store.
    pub db: PathBuf,
    /// The ingest socket producers write framed msgpack to.
    pub ingest_socket: PathBuf,
    /// The query socket the read API binds (the trusted local read plane).
    pub query_socket: PathBuf,
    /// The TCP port the read API binds (the LAN read plane).
    pub query_tcp_port: u16,
    /// The agent pairing-state file the LAN-edge auth reads. Injectable so a
    /// test points it at a tempdir.
    pub pairing_path: PathBuf,
    /// The filesystem root the hardware collector reads sysfs/proc under. `/` in
    /// production; a fixture tree in a test so the collector never touches the
    /// host's real `/sys` or `/proc`.
    pub hw_root: PathBuf,
    /// The seams the taps read: the state and frame sockets and the sidecar
    /// directory. Injectable so a test points them at a tempdir.
    pub taps: TapPaths,
}

impl Default for DaemonPaths {
    fn default() -> Self {
        Self {
            db: PathBuf::from(crate::paths::db_path()),
            ingest_socket: PathBuf::from(crate::paths::ingest_socket()),
            query_socket: PathBuf::from(crate::paths::query_socket()),
            query_tcp_port: crate::paths::QUERY_TCP_PORT,
            pairing_path: PathBuf::from(crate::query::auth::DEFAULT_PAIRING_PATH),
            hw_root: PathBuf::from(crate::hw::DEFAULT_ROOT),
            taps: TapPaths::default(),
        }
    }
}

pub use signals::sd_ready;
use signals::*;
use startup::*;

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

    // The shutdown-pending flag, shared with the writer thread. Set the instant a
    // stop begins so the writer never starts a maintenance pass (and its `VACUUM`)
    // during the drain window, which could overrun the bounded writer join.
    let stop = Arc::new(AtomicBool::new(false));

    // Build the writer on the daemon thread (so an open error surfaces here),
    // then move it onto its own dedicated OS thread to run the blocking loop.
    let writer = Writer::new(
        &paths.db,
        ingest_rx,
        WriterConfig::default(),
        Arc::clone(&stop),
    )
    .context("open writer connection")?;
    let boot_session = writer.boot_session();
    // The broadcast handle is the seam the live tail subscribes to; held so the
    // channel stays open for the daemon's lifetime, and cloned into the query
    // server so `/v1/tail` is fed by the writer fan-out rather than a DB poll.
    let broadcast = writer.broadcast_handle();
    // The control handle is the seam the read surface enqueues mark-synced
    // requests on. Cloned into the query server so the on-socket mark path can
    // reach the single writer; held open for the daemon's lifetime so the
    // writer's control channel never sees a closed sender while serving.
    let mark_synced = writer.control_handle();
    // The liveness stamp. The read surface answers `/v1/healthz` from it and
    // the watchdog ticker below feeds systemd only while it advances, so a
    // writer that wedges or dies is visible instead of being masked by a
    // process that is still scheduling tasks.
    let health = writer.health_handle();
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

    // Spawn the in-process hardware collector. It owns a clone of the ingest
    // sender and samples sysfs/proc at per-class cadences, pushing one snapshot
    // plus the key metrics per tick onto the same channel the socket producers
    // feed. It stops when its own shutdown signal fires (before the sender is
    // dropped) so it never sends into a closing channel.
    let (collector_stop_tx, collector_stop_rx) = oneshot::channel::<()>();
    let collector_task = tokio::spawn(crate::hw::run_collector(
        paths.hw_root.clone(),
        ingest_tx.clone(),
        collector_stop_rx,
    ));

    // Spawn the three seam taps, each with a clone of the ingest sender and a
    // subscription to one broadcast shutdown so a single fire stops them all,
    // symmetric with the collector and the accept loop. The taps read the frozen
    // state/frame sockets and the runtime sidecars; an absent seam is normal and
    // each tap retries on a backoff rather than failing.
    let (taps_stop_tx, _taps_keep) = broadcast::channel::<()>(1);
    let tap_tasks = spawn_all_taps(
        &paths.taps,
        ingest_tx.clone(),
        &taps_stop_tx,
        DEFAULT_MAVLINK_SAMPLE_HZ,
    );

    // Spawn the read surface: the same axum `/v1` Router on the trusted Unix
    // query socket and the LAN TCP port. It opens the store read-only per
    // request (never the writer's connection) and is fed live by the writer's
    // broadcast clone. Its own shutdown signal stops both listeners before the
    // writer is torn down.
    let (query_stop_tx, query_stop_rx) = oneshot::channel::<()>();
    let query_task = tokio::spawn(crate::query::spawn_query_server(
        paths.db.clone(),
        paths.query_socket.clone(),
        paths.query_tcp_port,
        broadcast,
        Arc::clone(&stats),
        paths.pairing_path.clone(),
        mark_synced,
        health.clone(),
        async move {
            let _ = query_stop_rx.await;
        },
    ));

    sd_ready();
    tracing::info!(
        boot_session,
        db = %paths.db.display(),
        ingest = %paths.ingest_socket.display(),
        "logging store ready"
    );

    // Feed the systemd watchdog on its own task, coupled to writer progress.
    // Liveness for this unit is not "the process exists" and not "the tokio
    // runtime still schedules tasks": a store whose writer has wedged
    // satisfies both while recording nothing, which is the one failure the
    // Black Box cannot have. A stalled writer stops the pings, `WatchdogSec`
    // expires, and systemd restarts the unit.
    let watchdog_task = tokio::spawn(watchdog_loop(
        WATCHDOG_PING_INTERVAL,
        WRITER_STALL_BUDGET,
        health,
        sd_watchdog,
    ));

    // Run until the stop trigger fires OR the writer exits on its own. The
    // second arm is what turns a dead writer into a dead unit: without it the
    // daemon kept serving a read surface in front of a store that had stopped
    // recording, and nothing ever noticed.
    tokio::pin!(shutdown);
    let mut writer_result_rx = writer_result_rx;
    // The writer's own exit, if it beats the stop trigger. Kept in the shape
    // `join_writer` already understands (the oneshot's own result) so the
    // teardown reports a panicked thread and a failed run the same way whether
    // the exit was observed here or waited for below.
    type WriterExit = Result<Result<(), crate::writer::WriterError>, oneshot::error::RecvError>;
    let mut writer_exit: Option<WriterExit> = None;
    tokio::select! {
        _ = &mut shutdown => {}
        res = &mut writer_result_rx => {
            // However it reported, the store has stopped recording, so the
            // daemon must not keep serving as though it had not.
            writer_exit = Some(res);
        }
    }
    watchdog_task.abort();
    tracing::info!("logging store stopping");
    // Signal the writer before any teardown await so it cannot start a new
    // maintenance pass while the channel is draining toward the bounded join.
    stop.store(true, Ordering::SeqCst);
    sd_stopping();

    // Signal every async component to stop up front (the sends are non-blocking),
    // then await their exit under one bound. The read surface holds no ingest
    // sender; the collector and taps must stop before the channel closes so they
    // never send into it. The bound matters because `systemd stop` kills the whole
    // control group at once: a seam tap reading a socket whose provider is dying
    // in the same SIGTERM can block its `await`, and an unbounded wait here would
    // hold shutdown open until the unit's stop timeout fires a SIGKILL (which also
    // tears the writer mid-write). If the orderly join overruns, abandon the
    // remaining tasks and press on — dropping the ingest sender next closes the
    // channel so the writer drains and exits cleanly regardless.
    let _ = query_stop_tx.send(());
    let _ = accept_stop_tx.send(());
    let _ = collector_stop_tx.send(());
    let _ = taps_stop_tx.send(());
    let orderly = async {
        let _ = query_task.await;
        let _ = accept_task.await;
        let _ = collector_task.await;
        for task in tap_tasks {
            let _ = task.await;
        }
    };
    if tokio::time::timeout(TEARDOWN_TIMEOUT, orderly)
        .await
        .is_err()
    {
        tracing::warn!(
            timeout_s = TEARDOWN_TIMEOUT.as_secs(),
            "async teardown overran its bound; abandoning the remaining tasks"
        );
    }

    // Drop every sender so the writer sees the channel close, drains the queue,
    // commits the final batch, closes the session, and truncates the WAL.
    drop(ingest_tx);

    // Join the writer thread off the async runtime, bounded so a stuck writer
    // cannot hang shutdown past the unit's stop timeout.
    let writer_died = writer_exit.is_some();
    join_writer(writer_thread, writer_result_rx, writer_exit).await;

    // tmpfs cleanup: a stale socket path confuses a producer probing for the
    // socket on the next start. The query server unlinks its own socket on the
    // way out; unlink it here too as a belt-and-suspenders guard.
    let _ = std::fs::remove_file(&paths.ingest_socket);
    let _ = std::fs::remove_file(&paths.query_socket);

    if writer_died {
        // Not a clean stop: the store stopped recording under a daemon that
        // was still serving. Exit non-zero so `Restart=always` cycles the
        // unit, rather than leaving a healthy-looking front over a dead store.
        tracing::error!("logging store stopped because its writer ended");
        return Err(anyhow::anyhow!(
            "logging store writer ended while the daemon was running"
        ));
    }
    tracing::info!("logging store stopped");
    Ok(())
}

/// Wait for the writer thread to finish, bounded by [`WRITER_JOIN_TIMEOUT`]. The
/// writer signals its result over a oneshot the moment `run` returns; the join
/// of the OS thread itself is then immediate. If the writer overruns the bound
/// (a wedged commit), the daemon logs and exits rather than hang.
///
/// `observed` carries an exit the run loop already saw on its own select arm,
/// so the writer's result is reported identically whether it ended on its own
/// or was drained by a requested shutdown.
async fn join_writer(
    handle: std::thread::JoinHandle<()>,
    result_rx: oneshot::Receiver<Result<(), crate::writer::WriterError>>,
    observed: Option<Result<Result<(), crate::writer::WriterError>, oneshot::error::RecvError>>,
) {
    let outcome = match observed {
        Some(already) => Ok(already),
        None => tokio::time::timeout(WRITER_JOIN_TIMEOUT, result_rx).await,
    };
    match outcome {
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

/// The `ados-logd` binary entry: run the daemon to completion. A fatal error
/// is printed the same way a `main` returning `Err` prints it, with exit code 1.
pub fn run() -> std::process::ExitCode {
    match run_async() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Error: {e:?}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn init_logging() {
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string());

    #[cfg(target_os = "linux")]
    {
        use tracing_subscriber::prelude::*;
        use tracing_subscriber::EnvFilter;
        if let Ok(journald) = tracing_journald::layer() {
            let _ = tracing_subscriber::registry()
                .with(EnvFilter::new(&filter))
                .with(journald)
                .try_init();
            return;
        }
    }

    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(&filter))
        .try_init();
}

#[tokio::main]
async fn run_async() -> Result<()> {
    init_logging();

    // The store is opt-in. It is ~96% of everything this box writes to its card
    // and the largest single lump of space it occupies, so it does not run
    // unless somebody asked for it. Exiting BEFORE `run_daemon` is what makes
    // "off" mean no store file is created, rather than an empty one that grows
    // the moment anything connects.
    //
    // The installer also declines to enable the unit when the key is off; this
    // second read covers the unit being started by hand or left enabled by an
    // older install. Exit 0, because declining to run is not a failure, and the
    // unit's `RestartPreventExitStatus=0` keeps `Restart=always` from looping it.
    if !crate::gate::store_enabled() {
        tracing::info!(
            key = "logging.store.enabled",
            "logging store is disabled; not starting (journalctl is the log of record)"
        );
        // Announce readiness before exiting. The unit is `Type=notify`, so
        // systemd waits for this and treats a process that exits without ever
        // sending it as `result 'protocol'` — a FAILURE — no matter that the
        // exit code is 0. Both rigs therefore carried a permanently failed
        // ados-logd after the store was switched off by default, which is worse
        // than useless: a node that always shows a failed unit teaches its
        // operator to stop reading failed units, and the next one that matters
        // is read the same way.
        //
        // READY then exit is the correct handshake for "started successfully,
        // and there is nothing to do" — systemd records a clean start and a
        // clean stop, and exit 0 is the one status the unit does not restart.
        crate::daemon::sd_ready();
        return Ok(());
    }

    tracing::info!(
        db = %crate::paths::db_path(),
        ingest = %crate::paths::ingest_socket(),
        "logging store starting"
    );
    match crate::daemon::run_daemon().await {
        Ok(()) => {
            tracing::info!("logging store exited cleanly");
            Ok(())
        }
        Err(e) => {
            tracing::error!(error = %e, "logging store fatal error");
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests;
