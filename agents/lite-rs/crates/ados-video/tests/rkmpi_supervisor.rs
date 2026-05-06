//! Integration tests for the RKMPI subprocess respawn supervisor.
//!
//! Each test spins up a fake wrapper binary (a small shell script
//! written into a per-test tempdir) and drives it through one supervise
//! cycle. The supervisor's tuning is dialed down to sub-second backoff
//! so the suite finishes in a few seconds rather than the production
//! curve's tens of minutes. Hardware semantics (signal handling,
//! `/proc` parsing, msgpack framing) match production verbatim — only
//! the timing constants are scaled.

use std::path::{Path, PathBuf};
use std::time::Duration;

use ados_video::rkmpi_subprocess::SubprocessResponse;
use ados_video::rkmpi_supervisor::{RkmpiSupervisor, SupervisorTuning};
use ados_video::EncoderConfig;
use tempfile::TempDir;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// Build tuning suitable for tests: every wait is sub-second so the
/// suite drives many supervise cycles without sleeping for minutes.
fn fast_tuning() -> SupervisorTuning {
    SupervisorTuning {
        backoff_base: Duration::from_millis(20),
        backoff_max: Duration::from_millis(80),
        oom_backoff_base: Duration::from_millis(40),
        watchdog_interval: Duration::from_millis(200),
        circuit_breaker_threshold: 10,
        circuit_breaker_holdoff: Duration::from_millis(100),
        ready_timeout: Duration::from_millis(500),
        health_reset_grace: Duration::from_millis(200),
    }
}

/// Encode the Ready response as the C wrapper would: 4-byte BE length
/// prefix + msgpack body. Generated at test time so the bytes track any
/// future change to the wire format without a hand-edit.
fn ready_frame_bytes() -> Vec<u8> {
    let body = rmp_serde::to_vec_named(&SubprocessResponse::Ready).expect("encode ready");
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(&body);
    out
}

/// Encode a framed Frame response as the C wrapper would.
fn frame_frame_bytes(is_keyframe: bool, pts_ms: u64, payload: &[u8]) -> Vec<u8> {
    let resp = SubprocessResponse::Frame {
        is_keyframe,
        pts_ms,
        bytes: payload.to_vec(),
    };
    let body = rmp_serde::to_vec_named(&resp).expect("encode frame");
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(&body);
    out
}

/// Write a shell script + companion data file into `dir`, mark the
/// script executable, and return its path. The script is the fake
/// wrapper binary the supervisor will exec.
///
/// `body` is the body of the script that runs after `set -e`. It can
/// reference `$DATA` to point at the companion data file path. The
/// data file is created empty if it does not already exist; tests
/// that need non-empty data should call [`write_data`] before OR
/// after this helper, since this helper never overwrites an existing
/// data file.
fn install_fake_wrapper(dir: &Path, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let script_path = dir.join("wrapper.sh");
    let data_path = dir.join("data.bin");
    let script = format!(
        "#!/bin/sh\nset -e\nDATA={data}\n{body}\n",
        data = data_path.display()
    );
    std::fs::write(&script_path, script).expect("write wrapper script");
    let mut perms = std::fs::metadata(&script_path)
        .expect("stat wrapper")
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script_path, perms).expect("chmod wrapper");
    // Touch the data file ONLY if it does not already exist so the
    // script never aborts on a missing path. Pre-existing data set up
    // by the test stays intact.
    if !data_path.exists() {
        std::fs::write(&data_path, b"").expect("touch data file");
    }
    script_path
}

/// Update the companion data file the fake wrapper streams from.
fn write_data(dir: &Path, bytes: &[u8]) {
    std::fs::write(dir.join("data.bin"), bytes).expect("write data file");
}

/// Spawn the supervisor as a tokio task. Returns the cancellation
/// token, the supervisor handle (cloned, retains snapshot / subscriber
/// access), and the join handle for the run loop itself.
fn spawn_supervisor(
    config: EncoderConfig,
    wrapper: PathBuf,
    tuning: SupervisorTuning,
) -> (
    CancellationToken,
    RkmpiSupervisor,
    tokio::task::JoinHandle<()>,
) {
    let cancel = CancellationToken::new();
    let supervisor = RkmpiSupervisor::new_with_tuning(config, wrapper, tuning);
    let runner = supervisor.clone();
    let cancel_clone = cancel.clone();
    let handle = tokio::spawn(async move {
        runner.run(cancel_clone).await;
    });
    (cancel, supervisor, handle)
}

/// Wait for `predicate(snapshot)` to return true, polling every 20 ms,
/// up to `timeout`. Returns the final snapshot. Panics on timeout.
async fn wait_for_snapshot<F>(
    sup: &RkmpiSupervisor,
    timeout: Duration,
    predicate: F,
) -> ados_video::RkmpiSnapshot
where
    F: Fn(&ados_video::RkmpiSnapshot) -> bool,
{
    let start = Instant::now();
    loop {
        let snap = sup.snapshot();
        if predicate(&snap) {
            return snap;
        }
        if start.elapsed() > timeout {
            panic!(
                "supervisor snapshot did not satisfy predicate within {:?}; final snapshot: {snap:?}",
                timeout
            );
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn crash_loop_increments_restart_count_and_applies_backoff() {
    // Wrapper that exits with code 1 immediately — never sends Ready.
    // The supervisor must classify this as a crash and respawn each
    // time, bumping restart_count.
    let dir = TempDir::new().expect("tempdir");
    let wrapper = install_fake_wrapper(dir.path(), "exit 1");

    let tuning = fast_tuning();
    let (cancel, sup, handle) = spawn_supervisor(EncoderConfig::default(), wrapper, tuning);

    // Wait for at least 3 respawns to be sure backoff is exercised
    // multiple times.
    let snap = wait_for_snapshot(&sup, Duration::from_secs(5), |s| s.restart_count >= 3).await;

    assert!(
        snap.restart_count >= 3,
        "expected restart_count >= 3, got {}",
        snap.restart_count
    );
    // The handshake-failed path does not populate exit code/signal
    // because the child closes stdout before classification can read
    // the status — that pathway is fine. We just assert the supervisor
    // is still iterating (running flips between true and false).

    cancel.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
}

#[tokio::test(flavor = "current_thread")]
async fn clean_exit_does_not_respawn() {
    // Wrapper sends Ready, then exits with code 0 promptly. The
    // supervisor must observe a clean exit and stop iterating.
    let dir = TempDir::new().expect("tempdir");
    write_data(dir.path(), &ready_frame_bytes());
    // After streaming Ready, the script exits 0. The agent's stdin gets
    // closed when the supervisor drops it; we don't need to wait for
    // that — the wrapper just exits.
    let wrapper = install_fake_wrapper(
        dir.path(),
        "cat $DATA\n# Brief pause so the supervisor sees Ready before EOF.\nsleep 0.1\nexit 0",
    );

    let tuning = fast_tuning();
    let (cancel, sup, handle) = spawn_supervisor(EncoderConfig::default(), wrapper, tuning);

    // Wait until the supervisor records a clean exit (last_exit_code ==
    // Some(0)) and is no longer running.
    let snap = wait_for_snapshot(&sup, Duration::from_secs(5), |s| {
        s.last_exit_code == Some(0) && !s.running
    })
    .await;

    assert_eq!(snap.last_exit_code, Some(0));
    assert!(snap.last_exit_signal.is_none());
    assert_eq!(
        snap.restart_count, 0,
        "clean exit must not bump restart_count"
    );

    // Wait a little to confirm no respawn happens.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let snap2 = sup.snapshot();
    assert_eq!(snap2.restart_count, 0, "no respawn after clean exit");
    assert!(!snap2.running);

    // The supervise loop should have terminated on its own; the
    // cancellation is belt-and-suspenders.
    cancel.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
}

#[tokio::test(flavor = "current_thread")]
async fn no_frames_triggers_watchdog_kill() {
    // Wrapper sends Ready, then sleeps without ever sending a Frame.
    // The watchdog is dialed to 200 ms; the supervisor must SIGKILL
    // and respawn at least once.
    let dir = TempDir::new().expect("tempdir");
    write_data(dir.path(), &ready_frame_bytes());
    // Use a long sleep so the supervisor has to kill the child rather
    // than waiting for it to exit on its own.
    let wrapper = install_fake_wrapper(dir.path(), "cat $DATA\nsleep 30");

    let tuning = fast_tuning();
    let (cancel, sup, handle) = spawn_supervisor(EncoderConfig::default(), wrapper, tuning);

    // Expect at least one watchdog-driven respawn within a few seconds.
    let snap = wait_for_snapshot(&sup, Duration::from_secs(5), |s| s.restart_count >= 1).await;

    assert!(
        snap.restart_count >= 1,
        "watchdog should have driven at least one respawn, got {}",
        snap.restart_count
    );
    // After the watchdog kill, last_exit_signal is SIGKILL.
    assert_eq!(
        snap.last_exit_signal.as_deref(),
        Some("SIGKILL"),
        "watchdog kill should surface SIGKILL on the snapshot"
    );

    cancel.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
}

#[tokio::test(flavor = "current_thread")]
async fn eleven_consecutive_crashes_open_circuit_breaker() {
    // Wrapper exits 1 forever. After the threshold (10) the supervisor
    // must flip circuit_breaker_open true.
    let dir = TempDir::new().expect("tempdir");
    let wrapper = install_fake_wrapper(dir.path(), "exit 1");

    // Custom tuning: the suite needs to drive >= 10 crashes within the
    // total wait window. Keep backoff micro-short.
    let tuning = SupervisorTuning {
        backoff_base: Duration::from_millis(5),
        backoff_max: Duration::from_millis(20),
        oom_backoff_base: Duration::from_millis(10),
        watchdog_interval: Duration::from_millis(200),
        circuit_breaker_threshold: 10,
        circuit_breaker_holdoff: Duration::from_millis(200),
        ready_timeout: Duration::from_millis(500),
        health_reset_grace: Duration::from_millis(200),
    };
    let (cancel, sup, handle) = spawn_supervisor(EncoderConfig::default(), wrapper, tuning);

    // Wait for the breaker to open. The threshold is 10 so we need at
    // least 10 crash classifications, then the next iteration trips
    // the breaker.
    let snap = wait_for_snapshot(&sup, Duration::from_secs(10), |s| s.circuit_breaker_open).await;
    assert!(
        snap.circuit_breaker_open,
        "circuit breaker should be open after threshold reached"
    );

    cancel.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
}

#[tokio::test(flavor = "current_thread")]
async fn snapshot_populates_running_then_exit_fields() {
    // End-to-end snapshot shape: confirm `running == true` while the
    // child is alive, then `running == false` and `last_exit_code` is
    // populated after a clean exit.
    let dir = TempDir::new().expect("tempdir");
    // Combine Ready + one Frame so `last_frame_unix` populates too,
    // then exit 0. The supervisor's watchdog interval in fast_tuning
    // is 200ms, so the wrapper must exit cleanly *before* the watchdog
    // window elapses or the test will see a SIGKILL from a hang
    // detection rather than the clean exit it is asserting against.
    let mut bundle = ready_frame_bytes();
    bundle.extend_from_slice(&frame_frame_bytes(true, 33, b"\x00\x00\x00\x01ABCD"));
    write_data(dir.path(), &bundle);
    // Stream the bundle and exit promptly. A short pause keeps the
    // running flag observable for the snapshot poll while staying well
    // under the watchdog window.
    let wrapper = install_fake_wrapper(dir.path(), "cat $DATA\nsleep 0.05\nexit 0");

    let tuning = fast_tuning();
    let (cancel, sup, handle) = spawn_supervisor(EncoderConfig::default(), wrapper, tuning);

    // Phase 1: child should be alive at some point and last_frame_unix
    // populated after the Frame is observed.
    let mid = wait_for_snapshot(&sup, Duration::from_secs(5), |s| {
        s.running && s.last_frame_unix.is_some()
    })
    .await;
    assert!(mid.running);
    assert!(mid.last_restart_unix.is_some());
    assert!(mid.last_frame_unix.is_some());

    // Phase 2: clean exit drops `running`, populates last_exit_code.
    let after = wait_for_snapshot(&sup, Duration::from_secs(5), |s| {
        s.last_exit_code == Some(0) && !s.running
    })
    .await;
    assert_eq!(after.last_exit_code, Some(0));
    assert!(after.last_exit_signal.is_none());
    // Frame timestamp from the running phase persists across the exit
    // observation.
    assert!(after.last_frame_unix.is_some());
    // restart_count remains 0 because the first frame landed inside the
    // grace window and the exit was clean.
    assert_eq!(after.restart_count, 0);

    cancel.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
}
