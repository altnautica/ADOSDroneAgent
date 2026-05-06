//! Respawn-on-crash supervisor for the RKMPI encoder subprocess.
//!
//! The single-spawn [`crate::rkmpi_subprocess::RkmpiEncoderSubprocess`]
//! API is unchanged: it owns one child wrapper, hands frames out, and
//! returns when the child exits. The supervisor wraps that lifecycle in
//! a respawn loop so a vendor-library segfault, OOM kill, or hang does
//! not take video off the air for the rest of the agent's runtime.
//!
//! Lifecycle of a single iteration:
//!
//! 1. Spawn the wrapper binary with piped stdin/stdout.
//! 2. Send a `Start` request, wait up to [`READY_TIMEOUT`] for `Ready`.
//! 3. While the child is alive, drain frames into the long-lived
//!    broadcast sender and watchdog the inter-frame gap.
//! 4. When the child exits or hangs, classify the failure mode and
//!    apply the right backoff.
//!
//! The broadcast sender is owned by the supervisor and survives across
//! respawns. Subscribers obtained via [`RkmpiSupervisor::frame_subscriber`]
//! see a continuous frame stream as long as the supervisor is running,
//! with a `Lagged` event if the child cycle drops too many frames during
//! a respawn window.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::rkmpi_subprocess::{
    parse_response, write_request, SubprocessRequest, SubprocessResponse, READY_TIMEOUT,
};
use crate::{EncodedFrame, EncoderConfig, EncoderError};

/// Capacity of the long-lived frame broadcast channel. Picked to absorb
/// a few seconds of 30 fps without lag for slow consumers; the supervisor
/// does not block the wrapper if a downstream stalls.
const FRAME_BROADCAST_CAPACITY: usize = 128;

/// Default base backoff for non-OOM crashes.
const DEFAULT_BACKOFF_BASE: Duration = Duration::from_secs(2);

/// Initial backoff after an OOM kill (SIGKILL). The doubled curve stretches
/// 8 / 16 / 32 / 60 (cap) so a board that is bouncing off its memory
/// budget gets longer headroom between retries than the routine-crash path.
const OOM_BACKOFF_BASE: Duration = Duration::from_secs(8);

/// Maximum backoff applied to any single retry.
const DEFAULT_BACKOFF_MAX: Duration = Duration::from_secs(60);

/// Inter-frame gap after which the supervisor declares the child hung
/// and SIGKILLs it without trying SIGTERM (proven unresponsive).
const DEFAULT_WATCHDOG_INTERVAL: Duration = Duration::from_secs(5);

/// Respawn count threshold that opens the circuit breaker. Once tripped,
/// the supervisor sleeps [`DEFAULT_CIRCUIT_BREAKER_HOLDOFF`], resets the
/// counter, and tries one more respawn.
const DEFAULT_CIRCUIT_BREAKER_THRESHOLD: u32 = 10;

/// Holdoff duration when the circuit breaker trips.
const DEFAULT_CIRCUIT_BREAKER_HOLDOFF: Duration = Duration::from_secs(60);

/// Window after which a successful frame run resets the restart counter.
/// The classifier counts a child as "healthy" once the first frame lands
/// within `READY_TIMEOUT + 5s` of spawn.
const HEALTH_RESET_GRACE: Duration = Duration::from_secs(5);

/// Snapshot of the supervisor's runtime state. Cheap to clone; intended
/// for the diag handler.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RkmpiSnapshot {
    /// True while a child wrapper is alive and producing frames.
    pub running: bool,
    /// Wall-clock seconds since the current child started. 0 when
    /// `running == false`.
    pub uptime_secs: u64,
    /// Number of respawns the supervisor has performed since either
    /// program start or the most recent circuit-breaker reset.
    pub restart_count: u32,
    /// Exit code reported by the most recently reaped child, or `None`
    /// if the child was killed by signal (or no child has exited yet).
    pub last_exit_code: Option<i32>,
    /// Symbolic name of the signal that killed the most recently reaped
    /// child (e.g. `"SIGKILL"`), or `None` when the child exited
    /// normally / no child has exited yet.
    pub last_exit_signal: Option<String>,
    /// UNIX seconds of the most recent respawn. 0 / `None` when the
    /// supervisor has not respawned since startup.
    pub last_restart_unix: Option<u64>,
    /// Resident-set-size of the current child in kilobytes, read live
    /// from `/proc/<pid>/status`. `None` when no child is running or
    /// the host does not expose `/proc`.
    pub rss_kb: Option<u64>,
    /// True when the circuit breaker is in its holdoff period.
    pub circuit_breaker_open: bool,
    /// UNIX seconds of the most recent `Frame` response observed. 0 /
    /// `None` until at least one frame has been delivered.
    pub last_frame_unix: Option<u64>,
}

/// Internal tunables for backoff / watchdog / circuit-breaker timing.
/// Production code uses [`SupervisorTuning::default`]; tests inject
/// scaled-down values so the suite does not real-time-sleep through
/// minute-long waits.
#[doc(hidden)]
#[derive(Debug, Clone)]
pub struct SupervisorTuning {
    pub backoff_base: Duration,
    pub backoff_max: Duration,
    pub oom_backoff_base: Duration,
    pub watchdog_interval: Duration,
    pub circuit_breaker_threshold: u32,
    pub circuit_breaker_holdoff: Duration,
    pub ready_timeout: Duration,
    pub health_reset_grace: Duration,
}

impl Default for SupervisorTuning {
    fn default() -> Self {
        Self {
            backoff_base: DEFAULT_BACKOFF_BASE,
            backoff_max: DEFAULT_BACKOFF_MAX,
            oom_backoff_base: OOM_BACKOFF_BASE,
            watchdog_interval: DEFAULT_WATCHDOG_INTERVAL,
            circuit_breaker_threshold: DEFAULT_CIRCUIT_BREAKER_THRESHOLD,
            circuit_breaker_holdoff: DEFAULT_CIRCUIT_BREAKER_HOLDOFF,
            ready_timeout: READY_TIMEOUT,
            health_reset_grace: HEALTH_RESET_GRACE,
        }
    }
}

/// Classification of a single child's exit, used to drive backoff and
/// log selection.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ExitClass {
    /// Code 0 — operator-driven stop. Do not respawn.
    CleanExit,
    /// Non-zero exit code (the wrapper called `exit(n)` with n != 0).
    NonZeroExit(i32),
    /// SIGKILL — almost certainly an OOM kill on this class of board.
    OomKill,
    /// SIGSEGV / SIGABRT / SIGBUS — vendor library or wrapper fault.
    SignalCrash(i32),
    /// Hang — no `Frame` within the watchdog interval. Supervisor
    /// SIGKILLed the child after the timeout fired.
    Hung,
    /// Spawn failed before we could even talk to the child.
    SpawnFailed,
    /// Handshake failed (no Ready, or stdout closed before Ready).
    HandshakeFailed,
}

/// Shared inner state — held behind an `Arc` so the supervisor handle
/// can be cloned cheaply, snapshots can be read from any thread, and
/// the long-lived broadcast channel survives across child respawns.
struct SupervisorInner {
    wrapper_path: PathBuf,
    config: EncoderConfig,
    frame_tx: broadcast::Sender<EncodedFrame>,
    snapshot: Mutex<RkmpiSnapshot>,
    tuning: SupervisorTuning,
}

/// Respawn-on-crash supervisor for the RKMPI subprocess encoder.
///
/// Construction is cheap. [`RkmpiSupervisor::run`] consumes the handle
/// and drives the supervise loop until the supplied [`CancellationToken`]
/// is triggered. Clone the supervisor before calling `run` to retain
/// access to [`RkmpiSupervisor::frame_subscriber`] and
/// [`RkmpiSupervisor::snapshot`] from elsewhere in the agent.
#[derive(Clone)]
pub struct RkmpiSupervisor {
    inner: Arc<SupervisorInner>,
}

impl std::fmt::Debug for RkmpiSupervisor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RkmpiSupervisor")
            .field("wrapper_path", &self.inner.wrapper_path)
            .field("subscribers", &self.inner.frame_tx.receiver_count())
            .finish()
    }
}

impl RkmpiSupervisor {
    /// Build a supervisor with production tuning.
    pub fn new(config: EncoderConfig, wrapper_path: PathBuf) -> Self {
        Self::new_with_tuning(config, wrapper_path, SupervisorTuning::default())
    }

    /// Build a supervisor with custom timing — intended for tests that
    /// need to drive crash / hang / circuit-breaker scenarios in well
    /// under a real-world cycle.
    #[doc(hidden)]
    pub fn new_with_tuning(
        config: EncoderConfig,
        wrapper_path: PathBuf,
        tuning: SupervisorTuning,
    ) -> Self {
        let (frame_tx, _) = broadcast::channel(FRAME_BROADCAST_CAPACITY);
        Self {
            inner: Arc::new(SupervisorInner {
                wrapper_path,
                config,
                frame_tx,
                snapshot: Mutex::new(RkmpiSnapshot::default()),
                tuning,
            }),
        }
    }

    /// Subscribe to the encoded-frame broadcast. The receiver survives
    /// across child respawns.
    pub fn frame_subscriber(&self) -> broadcast::Receiver<EncodedFrame> {
        self.inner.frame_tx.subscribe()
    }

    /// Snapshot the current observability fields. Cheap; the underlying
    /// mutex is held only for the duration of one struct copy.
    pub fn snapshot(&self) -> RkmpiSnapshot {
        let mut snap = self
            .inner
            .snapshot
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default();
        // Refresh uptime live; the supervise loop only writes the value
        // once per state change, but the diag handler should see ticks
        // between writes too.
        if snap.running {
            if let Some(last) = snap.last_restart_unix {
                let now = now_unix_seconds();
                snap.uptime_secs = now.saturating_sub(last);
            }
        }
        snap
    }

    /// Drive the supervise loop until `cancel` is triggered. Consumes
    /// the handle; clone the supervisor first if other code holds
    /// references.
    pub async fn run(self, cancel: CancellationToken) {
        let inner = self.inner;
        supervise_loop(inner, cancel).await;
    }
}

/// The actual supervise loop. Pulled out of the public type so the
/// borrow lifetimes stay simple.
async fn supervise_loop(inner: Arc<SupervisorInner>, cancel: CancellationToken) {
    loop {
        if cancel.is_cancelled() {
            tracing::info!("rkmpi supervisor exiting on cancellation");
            mark_stopped(&inner);
            return;
        }

        let class = run_one_child(&inner, &cancel).await;

        match class {
            ExitClass::CleanExit => {
                tracing::info!("rkmpi wrapper exited cleanly with code 0; not respawning");
                mark_stopped(&inner);
                return;
            }
            ExitClass::NonZeroExit(code) => {
                tracing::warn!(
                    code = code,
                    event = "wrapper_exited",
                    "rkmpi wrapper exited with non-zero status; will respawn"
                );
                apply_backoff_and_continue(&inner, &cancel, false).await;
            }
            ExitClass::SignalCrash(sig) => {
                tracing::warn!(
                    signal = %signal_name(sig),
                    event = "wrapper_signal_exit",
                    "rkmpi wrapper killed by signal; will respawn"
                );
                apply_backoff_and_continue(&inner, &cancel, false).await;
            }
            ExitClass::OomKill => {
                tracing::error!(
                    event = "wrapper_oom_killed",
                    "rkmpi wrapper killed by SIGKILL (likely OOM); will respawn with extended backoff"
                );
                apply_backoff_and_continue(&inner, &cancel, true).await;
            }
            ExitClass::Hung => {
                tracing::warn!(
                    event = "wrapper_hung",
                    "rkmpi wrapper produced no frames within watchdog interval; killed and will respawn"
                );
                apply_backoff_and_continue(&inner, &cancel, false).await;
            }
            ExitClass::SpawnFailed => {
                tracing::warn!(
                    event = "wrapper_spawn_failed",
                    "rkmpi wrapper failed to spawn; will respawn"
                );
                apply_backoff_and_continue(&inner, &cancel, false).await;
            }
            ExitClass::HandshakeFailed => {
                tracing::warn!(
                    event = "wrapper_handshake_failed",
                    "rkmpi wrapper failed to send Ready; will respawn"
                );
                apply_backoff_and_continue(&inner, &cancel, false).await;
            }
        }

        if cancel.is_cancelled() {
            mark_stopped(&inner);
            return;
        }
    }
}

/// Drive one child end-to-end: spawn, handshake, frame loop, reap.
async fn run_one_child(inner: &Arc<SupervisorInner>, cancel: &CancellationToken) -> ExitClass {
    let mut child = match tokio::process::Command::new(&inner.wrapper_path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, wrapper = ?inner.wrapper_path, "rkmpi wrapper spawn failed");
            return ExitClass::SpawnFailed;
        }
    };

    let pid = child.id();
    let spawn_at = now_unix_seconds();

    {
        let mut snap = inner.snapshot.lock().expect("snapshot mutex poisoned");
        snap.running = true;
        snap.uptime_secs = 0;
        snap.last_restart_unix = Some(spawn_at);
        snap.rss_kb = pid.and_then(read_rss_kb_for_pid);
    }

    let mut stdin = match child.stdin.take() {
        Some(s) => s,
        None => {
            tracing::warn!("rkmpi wrapper child stdin missing; killing");
            let _ = child.start_kill();
            let _ = child.wait().await;
            return ExitClass::SpawnFailed;
        }
    };
    let mut stdout = match child.stdout.take() {
        Some(s) => s,
        None => {
            tracing::warn!("rkmpi wrapper child stdout missing; killing");
            let _ = child.start_kill();
            let _ = child.wait().await;
            return ExitClass::SpawnFailed;
        }
    };

    // Send the Start request. A write failure here is a handshake fault.
    if let Err(e) = write_request(
        &mut stdin,
        &SubprocessRequest::Start(inner.config.clone()),
    )
    .await
    {
        tracing::warn!(error = %e, "rkmpi wrapper start request write failed");
        let _ = child.start_kill();
        let _ = child.wait().await;
        return ExitClass::HandshakeFailed;
    }

    // Phase 1: wait for Ready (or stdout close, or timeout). This reads
    // straight from the child's stdout so we keep one byte buffer for the
    // entire child's lifetime.
    let mut stdout_buf: Vec<u8> = Vec::with_capacity(64 * 1024);
    let mut scratch = vec![0u8; 64 * 1024];

    let ready_outcome = tokio::time::timeout(inner.tuning.ready_timeout, async {
        loop {
            if let Ok((resp, consumed)) = parse_response(&stdout_buf) {
                stdout_buf.drain(..consumed);
                match resp {
                    SubprocessResponse::Ready => return Ok::<(), EncoderError>(()),
                    SubprocessResponse::Frame { .. } => {
                        // Spec violation but recoverable — child shouldn't
                        // emit Frame before Ready. Treat as if Ready had
                        // already arrived.
                        return Ok(());
                    }
                    SubprocessResponse::Error { message } => {
                        return Err(EncoderError::Protocol(message));
                    }
                }
            }
            match stdout.read(&mut scratch).await {
                Ok(0) => {
                    return Err(EncoderError::Protocol(
                        "child closed stdout before Ready".into(),
                    ));
                }
                Ok(n) => stdout_buf.extend_from_slice(&scratch[..n]),
                Err(e) => return Err(EncoderError::Io(e.to_string())),
            }
        }
    })
    .await;

    match ready_outcome {
        Ok(Ok(())) => {
            tracing::info!(wrapper = ?inner.wrapper_path, "rkmpi wrapper ready");
        }
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "rkmpi wrapper handshake failed");
            let _ = child.start_kill();
            let exit_class = wait_and_classify(&mut child, ExitClass::HandshakeFailed).await;
            update_snapshot_on_exit(inner, &exit_class);
            return exit_class;
        }
        Err(_) => {
            tracing::warn!(
                timeout_secs = inner.tuning.ready_timeout.as_secs(),
                "rkmpi wrapper handshake timed out"
            );
            let _ = child.start_kill();
            let exit_class = wait_and_classify(&mut child, ExitClass::HandshakeFailed).await;
            update_snapshot_on_exit(inner, &exit_class);
            return exit_class;
        }
    }

    // Phase 2: frame loop. The supervise loop owns the child and
    // forwards frames into the long-lived broadcast sender. A watchdog
    // timer fires whenever the inter-frame gap exceeds the configured
    // interval; a cancellation drops the child via `kill_on_drop`.
    let exit_class = run_frame_loop(inner, cancel, &mut child, stdout, stdout_buf, spawn_at).await;
    update_snapshot_on_exit(inner, &exit_class);
    exit_class
}

/// Drive the post-Ready frame loop until the child exits, hangs, or the
/// supervisor is cancelled.
async fn run_frame_loop(
    inner: &Arc<SupervisorInner>,
    cancel: &CancellationToken,
    child: &mut tokio::process::Child,
    mut stdout: tokio::process::ChildStdout,
    mut stdout_buf: Vec<u8>,
    spawn_at: u64,
) -> ExitClass {
    let mut scratch = vec![0u8; 64 * 1024];
    let mut last_frame_at = tokio::time::Instant::now();
    let mut first_frame_seen = false;
    let watchdog = inner.tuning.watchdog_interval;
    let pid = child.id();

    loop {
        // Drain any complete frames sitting in the buffer first, so a
        // burst of stdout bytes that landed in one read does not get
        // gated on watchdog ticks.
        loop {
            match parse_response(&stdout_buf) {
                Ok((resp, consumed)) => {
                    stdout_buf.drain(..consumed);
                    match resp {
                        SubprocessResponse::Frame {
                            is_keyframe,
                            pts_ms,
                            bytes,
                        } => {
                            last_frame_at = tokio::time::Instant::now();
                            // Reset restart count once we've seen the first
                            // healthy frame within the grace window.
                            if !first_frame_seen {
                                first_frame_seen = true;
                                let now = now_unix_seconds();
                                let alive_for = now.saturating_sub(spawn_at);
                                let grace_total = inner.tuning.ready_timeout.as_secs()
                                    + inner.tuning.health_reset_grace.as_secs();
                                if alive_for <= grace_total {
                                    let mut snap =
                                        inner.snapshot.lock().expect("snapshot mutex poisoned");
                                    snap.restart_count = 0;
                                }
                            }
                            {
                                let mut snap =
                                    inner.snapshot.lock().expect("snapshot mutex poisoned");
                                snap.last_frame_unix = Some(now_unix_seconds());
                                if let Some(p) = pid {
                                    snap.rss_kb = read_rss_kb_for_pid(p);
                                }
                            }
                            // Best-effort send; if no subscribers, just drop.
                            let frame = EncodedFrame {
                                bytes,
                                is_keyframe,
                                pts_ms,
                            };
                            let _ = inner.frame_tx.send(frame);
                        }
                        SubprocessResponse::Ready => {
                            // Late or duplicate Ready — ignore.
                        }
                        SubprocessResponse::Error { message } => {
                            tracing::warn!(message = %message, "rkmpi wrapper reported error");
                        }
                    }
                }
                Err(EncoderError::Incomplete(_)) => break,
                Err(e) => {
                    tracing::warn!(error = %e, "rkmpi wrapper protocol error; tearing down");
                    let _ = child.start_kill();
                    return wait_and_classify(child, ExitClass::HandshakeFailed).await;
                }
            }
        }

        // Now wait for the next stdout chunk, child exit, watchdog, or
        // cancel — whichever fires first.
        let watchdog_deadline = last_frame_at + watchdog;
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("rkmpi supervisor cancelled; killing child");
                let _ = child.start_kill();
                let _ = child.wait().await;
                return ExitClass::CleanExit;
            }
            res = stdout.read(&mut scratch) => {
                match res {
                    Ok(0) => {
                        // EOF — child closed stdout. Reap to read exit
                        // status and classify.
                        return wait_and_classify(child, ExitClass::HandshakeFailed).await;
                    }
                    Ok(n) => stdout_buf.extend_from_slice(&scratch[..n]),
                    Err(e) => {
                        tracing::warn!(error = %e, "rkmpi wrapper stdout read failed");
                        let _ = child.start_kill();
                        return wait_and_classify(child, ExitClass::HandshakeFailed).await;
                    }
                }
            }
            status = child.wait() => {
                // Child exited on its own. Classify directly.
                return classify_status(status);
            }
            _ = tokio::time::sleep_until(watchdog_deadline) => {
                tracing::warn!(
                    watchdog_secs = watchdog.as_secs(),
                    "rkmpi wrapper produced no frames within watchdog window; killing"
                );
                let _ = child.start_kill();
                let _ = child.wait().await;
                return ExitClass::Hung;
            }
        }
    }
}

/// Wait for the child to terminate and convert its exit status into an
/// [`ExitClass`]. Used when the supervisor has already initiated a kill
/// or the child has closed its stdout. `fallback` is returned only if
/// the wait itself fails.
async fn wait_and_classify(
    child: &mut tokio::process::Child,
    fallback: ExitClass,
) -> ExitClass {
    let status = child.wait().await;
    match status {
        Ok(_) => classify_status(status),
        Err(e) => {
            tracing::warn!(error = %e, "rkmpi wrapper wait failed");
            fallback
        }
    }
}

/// Map a `Result<ExitStatus>` into the exit-class taxonomy.
fn classify_status(status: std::io::Result<std::process::ExitStatus>) -> ExitClass {
    match status {
        Ok(s) => {
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                if let Some(sig) = s.signal() {
                    if sig == 9 {
                        return ExitClass::OomKill;
                    }
                    return ExitClass::SignalCrash(sig);
                }
            }
            match s.code() {
                Some(0) => ExitClass::CleanExit,
                Some(code) => ExitClass::NonZeroExit(code),
                None => ExitClass::SignalCrash(0),
            }
        }
        Err(_) => ExitClass::HandshakeFailed,
    }
}

/// Compute the next backoff and sleep, also incrementing restart_count
/// and tripping the circuit breaker when needed. `oom` doubles the base
/// of the exponential curve.
async fn apply_backoff_and_continue(
    inner: &Arc<SupervisorInner>,
    cancel: &CancellationToken,
    oom: bool,
) {
    let (count, threshold, base, max, holdoff) = {
        let mut snap = inner.snapshot.lock().expect("snapshot mutex poisoned");
        snap.restart_count = snap.restart_count.saturating_add(1);
        snap.running = false;
        snap.rss_kb = None;
        snap.uptime_secs = 0;
        let count = snap.restart_count;
        let threshold = inner.tuning.circuit_breaker_threshold;
        let base = if oom {
            inner.tuning.oom_backoff_base
        } else {
            inner.tuning.backoff_base
        };
        let max = inner.tuning.backoff_max;
        let holdoff = inner.tuning.circuit_breaker_holdoff;
        (count, threshold, base, max, holdoff)
    };

    if count >= threshold {
        tracing::error!(
            event = "wrapper_circuit_breaker_open",
            restart_count = count,
            holdoff_secs = holdoff.as_secs(),
            "rkmpi supervisor opening circuit breaker; sleeping then resetting count"
        );
        {
            let mut snap = inner.snapshot.lock().expect("snapshot mutex poisoned");
            snap.circuit_breaker_open = true;
        }
        tokio::select! {
            _ = cancel.cancelled() => {}
            _ = tokio::time::sleep(holdoff) => {}
        }
        let mut snap = inner.snapshot.lock().expect("snapshot mutex poisoned");
        snap.circuit_breaker_open = false;
        snap.restart_count = 0;
        return;
    }

    let delay = exp_backoff(base, max, count);
    tracing::info!(
        backoff_secs = delay.as_secs(),
        restart_count = count,
        "rkmpi supervisor sleeping before respawn"
    );
    tokio::select! {
        _ = cancel.cancelled() => {}
        _ = tokio::time::sleep(delay) => {}
    }
}

/// `base * 2^(n - 1)` capped at `max`. Saturating arithmetic so a long
/// run does not wrap.
fn exp_backoff(base: Duration, max: Duration, n: u32) -> Duration {
    if n == 0 {
        return base.min(max);
    }
    // shift cap: 2^32 ms is already astronomical, but bound the shift
    // to avoid pathological inputs. The cap below trims to `max` anyway.
    let shift = (n - 1).min(31);
    let factor = 1u64 << shift;
    let base_ms = base.as_millis() as u64;
    let scaled_ms = base_ms.saturating_mul(factor);
    let dur = Duration::from_millis(scaled_ms);
    dur.min(max)
}

/// Map a small subset of common signal numbers to their POSIX names.
/// Unknown signals fall back to a numeric label so the log surface is
/// always populated.
fn signal_name(sig: i32) -> String {
    match sig {
        1 => "SIGHUP".into(),
        2 => "SIGINT".into(),
        3 => "SIGQUIT".into(),
        4 => "SIGILL".into(),
        6 => "SIGABRT".into(),
        7 => "SIGBUS".into(),
        8 => "SIGFPE".into(),
        9 => "SIGKILL".into(),
        11 => "SIGSEGV".into(),
        13 => "SIGPIPE".into(),
        14 => "SIGALRM".into(),
        15 => "SIGTERM".into(),
        other => format!("SIG{other}"),
    }
}

/// Update the snapshot with the most recent exit metadata. Only the
/// post-mortem fields move; counters and timings are owned by the
/// backoff path.
fn update_snapshot_on_exit(inner: &Arc<SupervisorInner>, class: &ExitClass) {
    let mut snap = inner.snapshot.lock().expect("snapshot mutex poisoned");
    snap.running = false;
    snap.rss_kb = None;
    snap.uptime_secs = 0;
    match class {
        ExitClass::CleanExit => {
            snap.last_exit_code = Some(0);
            snap.last_exit_signal = None;
        }
        ExitClass::NonZeroExit(code) => {
            snap.last_exit_code = Some(*code);
            snap.last_exit_signal = None;
        }
        ExitClass::OomKill => {
            snap.last_exit_code = None;
            snap.last_exit_signal = Some(signal_name(9));
        }
        ExitClass::SignalCrash(sig) => {
            snap.last_exit_code = None;
            snap.last_exit_signal = Some(signal_name(*sig));
        }
        ExitClass::Hung => {
            // Supervisor SIGKILLed the child after the watchdog fired.
            snap.last_exit_code = None;
            snap.last_exit_signal = Some(signal_name(9));
        }
        ExitClass::SpawnFailed | ExitClass::HandshakeFailed => {
            snap.last_exit_code = None;
            snap.last_exit_signal = None;
        }
    }
}

/// Stamp a stopped state onto the snapshot for the cancellation path.
fn mark_stopped(inner: &Arc<SupervisorInner>) {
    let mut snap = inner.snapshot.lock().expect("snapshot mutex poisoned");
    snap.running = false;
    snap.rss_kb = None;
    snap.uptime_secs = 0;
}

/// Read VmRSS for an arbitrary pid from `/proc/<pid>/status`. Returns
/// kilobytes (the unit `/proc` itself reports). `None` on non-Linux
/// hosts, when the pid has gone away, or when the VmRSS line is absent.
fn read_rss_kb_for_pid(pid: u32) -> Option<u64> {
    let raw = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let trimmed = rest.trim();
            let kb_str = trimmed.split_whitespace().next()?;
            return kb_str.parse().ok();
        }
    }
    None
}

/// Wall-clock UNIX seconds. Same source the rest of the supervisor uses
/// so timestamps line up across snapshot fields.
fn now_unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exp_backoff_doubles_to_cap() {
        let base = Duration::from_secs(2);
        let max = Duration::from_secs(60);
        assert_eq!(exp_backoff(base, max, 1), Duration::from_secs(2));
        assert_eq!(exp_backoff(base, max, 2), Duration::from_secs(4));
        assert_eq!(exp_backoff(base, max, 3), Duration::from_secs(8));
        assert_eq!(exp_backoff(base, max, 4), Duration::from_secs(16));
        assert_eq!(exp_backoff(base, max, 5), Duration::from_secs(32));
        assert_eq!(exp_backoff(base, max, 6), Duration::from_secs(60));
        assert_eq!(exp_backoff(base, max, 100), Duration::from_secs(60));
    }

    #[test]
    fn exp_backoff_oom_starts_at_doubled_base() {
        let base = Duration::from_secs(8);
        let max = Duration::from_secs(60);
        assert_eq!(exp_backoff(base, max, 1), Duration::from_secs(8));
        assert_eq!(exp_backoff(base, max, 2), Duration::from_secs(16));
        assert_eq!(exp_backoff(base, max, 3), Duration::from_secs(32));
        assert_eq!(exp_backoff(base, max, 4), Duration::from_secs(60));
    }

    #[test]
    fn signal_name_known_signals() {
        assert_eq!(signal_name(9), "SIGKILL");
        assert_eq!(signal_name(11), "SIGSEGV");
        assert_eq!(signal_name(6), "SIGABRT");
        assert_eq!(signal_name(7), "SIGBUS");
        assert_eq!(signal_name(15), "SIGTERM");
        assert_eq!(signal_name(99), "SIG99");
    }

    #[test]
    fn snapshot_default_is_idle() {
        let snap = RkmpiSnapshot::default();
        assert!(!snap.running);
        assert!(!snap.circuit_breaker_open);
        assert_eq!(snap.restart_count, 0);
        assert!(snap.last_exit_code.is_none());
        assert!(snap.last_exit_signal.is_none());
        assert!(snap.last_restart_unix.is_none());
        assert!(snap.last_frame_unix.is_none());
        assert!(snap.rss_kb.is_none());
        assert_eq!(snap.uptime_secs, 0);
    }
}
