//! Spawn and supervise the kiosk child (cage, or the browser window itself).

use std::collections::VecDeque;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ados_proc::lines::BoundedLines;
use ados_proc::{GroupChild, Spec, StderrMode, StdoutMode};
use tokio::io::AsyncRead;
use tokio::sync::Notify;

use super::compositor::{apply_output_mode, browser_running, sweep_orphans};
use super::drm::DrmPaths;
use super::launcher::cage_runtime_dir;

/// Size of the rolling stderr tail kept for the downgrade heuristic.
pub const STDERR_TAIL_BYTES: usize = 2048;
/// Child stderr lines forwarded to the journal before the rest are suppressed.
pub const STDERR_LOG_LINE_LIMIT: usize = 40;

/// A sticky stop request, awaitable by any number of tasks.
#[derive(Debug, Default)]
pub struct Stop {
    flag: AtomicBool,
    notify: Notify,
}

impl Stop {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn request(&self) {
        self.flag.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    pub fn is_set(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// Resolve once a stop is requested (at once when it already was).
    pub async fn wait(&self) {
        loop {
            let notified = self.notify.notified();
            if self.is_set() {
                return;
            }
            notified.await;
        }
    }

    /// `true` when a stop arrives within `d`.
    pub async fn wait_timeout(&self, d: Duration) -> bool {
        tokio::time::timeout(d, self.wait()).await.is_ok()
    }
}

/// What to run and how.
#[derive(Debug, Clone, Default)]
pub struct ChildSpec {
    pub argv: Vec<String>,
    /// Overlay on the service environment.
    pub env: Vec<(String, String)>,
    /// Variables removed after the overlay (the cage path strips `DISPLAY` /
    /// `WAYLAND_DISPLAY` so cage uses its DRM backend instead of nesting).
    pub env_unset: Vec<String>,
    /// Drop to this uid and its primary gid before exec (the windowed path:
    /// the browser refuses to run as root without `--no-sandbox`).
    pub run_as_uid: Option<u32>,
    /// The cage path: sweep orphans on stop, watch the browser under the
    /// compositor and apply the output mode.
    pub sweep_orphans: bool,
    /// `WIDTHxHEIGHT` set on the outputs once cage is up.
    pub output_mode: Option<String>,
}

/// The supervisor's fixed intervals and crash-loop guard.
#[derive(Debug, Clone)]
pub struct Timing {
    /// Fixed pause between child restarts (no backoff).
    pub retry: Duration,
    /// Graceful-stop allowance before `SIGKILL`.
    pub grace: Duration,
    /// How long the browser may be absent after a cage launch (cage starts it).
    pub browser_start_grace: Duration,
    pub browser_poll: Duration,
    /// `crash_limit` exits inside `crash_window` mark a crash loop.
    pub crash_window: Duration,
    pub crash_limit: usize,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            retry: Duration::from_secs(3),
            grace: Duration::from_secs(10),
            browser_start_grace: Duration::from_secs(25),
            browser_poll: Duration::from_secs(10),
            crash_window: Duration::from_secs(60),
            crash_limit: 5,
        }
    }
}

/// The last `limit` bytes of `s`, starting on a char boundary.
pub fn tail_str(s: &str, limit: usize) -> &str {
    let mut start = s.len().saturating_sub(limit);
    while !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

/// Forward the child's stderr to the journal while it runs (bounded) and keep
/// the rolling tail in `tail`. A compositor that stays up with a broken
/// browser never exits, so an exit-time read would never show its error.
pub async fn stream_stderr(stderr: impl AsyncRead + Unpin, tail: Arc<Mutex<String>>) {
    let mut lines = BoundedLines::new(stderr);
    let mut recent: VecDeque<String> = VecDeque::with_capacity(40);
    let mut seen = 0usize;
    while let Some(line) = lines.next_line().await {
        let line = line.trim_end().to_string();
        if line.is_empty() {
            continue;
        }
        if recent.len() == 40 {
            recent.pop_front();
        }
        recent.push_back(line.clone());
        let joined = recent.iter().cloned().collect::<Vec<_>>().join("\n");
        *tail.lock().unwrap_or_else(|e| e.into_inner()) =
            tail_str(&joined, STDERR_TAIL_BYTES).to_string();
        if seen < STDERR_LOG_LINE_LIMIT {
            tracing::warn!(line = %tail_str(&line, 400), "kiosk_child_stderr");
        } else if seen == STDERR_LOG_LINE_LIMIT {
            tracing::warn!(lines = seen, "kiosk_child_stderr_suppressed");
        }
        seen += 1;
    }
}

/// Resolve once the browser is gone while cage is still up. Cage path only:
/// there the child is the compositor and the browser its grandchild, so a dead
/// browser under a live cage is invisible to the child wait. On the windowed
/// path the child is the browser itself and this never resolves.
async fn watch_browser(enabled: bool, timing: &Timing) {
    if !enabled {
        return std::future::pending().await;
    }
    tokio::time::sleep(timing.browser_start_grace).await;
    while browser_running().await {
        tokio::time::sleep(timing.browser_poll).await;
    }
}

enum Ended {
    Exited(i32),
    Stopped,
    BrowserGone,
}

/// Supervises one child spec until stopped, a crash loop, or a spawn failure.
pub struct KioskSupervisor {
    pub spec: ChildSpec,
    pub timing: Timing,
    stop: Arc<Stop>,
    crash_times: VecDeque<Instant>,
    /// Set when the crash-loop guard trips: the caller's cue to downgrade a
    /// GPU launch to software.
    pub crash_looped: bool,
    tail: Arc<Mutex<String>>,
    drm: DrmPaths,
}

impl KioskSupervisor {
    pub fn new(spec: ChildSpec) -> Self {
        Self {
            spec,
            timing: Timing::default(),
            stop: Stop::new(),
            crash_times: VecDeque::new(),
            crash_looped: false,
            tail: Arc::default(),
            drm: DrmPaths::default(),
        }
    }

    /// The handle that stops this supervisor's [`run`](Self::run).
    pub fn stop_handle(&self) -> Arc<Stop> {
        self.stop.clone()
    }

    /// The last child's rolling stderr tail.
    pub fn last_stderr_tail(&self) -> String {
        self.tail.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Spawn the child as its own process group, stdout discarded and stderr
    /// piped, with the env overlay, removals and uid drop applied.
    pub(crate) fn spawn(&self) -> io::Result<GroupChild> {
        let (program, args) = self
            .spec
            .argv
            .split_first()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty argv"))?;
        tracing::info!(argv = ?self.spec.argv, run_as_uid = ?self.spec.run_as_uid, "kiosk_spawning");
        let gid = self
            .spec
            .run_as_uid
            .and_then(|uid| super::session::passwd_entry(uid).map(|(_, _, gid)| gid));
        let spec = Spec {
            program,
            args,
            stdout: StdoutMode::Null,
            stderr: StderrMode::Piped,
        };
        GroupChild::spawn_with("kiosk", spec, |cmd| {
            cmd.envs(self.spec.env.iter().map(|(k, v)| (k, v)));
            for key in &self.spec.env_unset {
                cmd.env_remove(key);
            }
            #[cfg(unix)]
            if let Some(uid) = self.spec.run_as_uid {
                cmd.uid(uid);
                if let Some(gid) = gid {
                    cmd.gid(gid);
                }
            }
            #[cfg(not(unix))]
            let _ = gid;
        })
    }

    /// Record an exit now; `true` while the window holds fewer than the limit.
    pub fn record_crash(&mut self) -> bool {
        let now = Instant::now();
        self.crash_times.push_back(now);
        let window = self.timing.crash_window;
        self.crash_times
            .retain(|t| now.duration_since(*t) <= window);
        self.crash_times.len() < self.timing.crash_limit
    }

    /// `SIGTERM` the child's group, `SIGKILL` after the grace, then (cage
    /// path) sweep orphans.
    async fn graceful_kill(&self, child: &mut GroupChild) {
        child.terminate(self.timing.grace).await;
        tracing::info!(label = child.label(), "kiosk_child_terminated");
        if self.spec.sweep_orphans {
            sweep_orphans().await;
        }
    }

    fn runtime_dir(&self) -> PathBuf {
        self.spec
            .env
            .iter()
            .find(|(k, _)| k == "XDG_RUNTIME_DIR")
            .map(|(_, v)| PathBuf::from(v))
            .unwrap_or_else(cage_runtime_dir)
    }

    /// Supervise until stopped (0), a missing binary (3), another spawn
    /// failure (4) or a crash loop (the last exit code, never 0: the unit's
    /// `RestartPreventExitStatus=0` would otherwise leave the panel dark).
    pub async fn run(&mut self) -> i32 {
        while !self.stop.is_set() {
            let mut child = match self.spawn() {
                Ok(child) => child,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    tracing::error!(%error, "kiosk_binary_missing");
                    return 3;
                }
                Err(error) => {
                    tracing::error!(%error, "kiosk_spawn_failed");
                    return 4;
                }
            };
            tracing::info!(pid = ?child.pid(), "kiosk_child_running");
            let drain = child
                .take_stderr()
                .map(|s| tokio::spawn(stream_stderr(s, self.tail.clone())));
            let mode_task = match (&self.spec.output_mode, self.spec.sweep_orphans) {
                (Some(mode), true) => {
                    let (mode, env, drm) = (mode.clone(), self.spec.env.clone(), self.drm.clone());
                    let runtime = self.runtime_dir();
                    Some(tokio::spawn(async move {
                        apply_output_mode(&mode, &runtime, &env, &drm).await;
                    }))
                }
                _ => None,
            };
            let stop = self.stop.clone();
            let ended = tokio::select! {
                biased;
                status = child.wait() => {
                    Ended::Exited(status.ok().and_then(|s| s.code()).unwrap_or(-1))
                }
                () = stop.wait() => Ended::Stopped,
                () = watch_browser(self.spec.sweep_orphans, &self.timing) => Ended::BrowserGone,
            };
            if let Some(task) = mode_task {
                task.abort();
            }
            match ended {
                Ended::Stopped => {
                    if let Some(task) = drain {
                        task.abort();
                    }
                    self.graceful_kill(&mut child).await;
                    tracing::info!("kiosk_supervisor_stopping");
                    return 0;
                }
                Ended::BrowserGone => {
                    tracing::error!(
                        stderr_tail = %self.last_stderr_tail(),
                        "kiosk_browser_vanished"
                    );
                    if let Some(task) = drain {
                        task.abort();
                    }
                    self.graceful_kill(&mut child).await;
                    self.record_crash();
                    if self.stop.wait_timeout(self.timing.retry).await {
                        return 0;
                    }
                }
                Ended::Exited(rc) => {
                    // Let the drain pick up the child's last lines.
                    if let Some(task) = drain {
                        let abort = task.abort_handle();
                        if tokio::time::timeout(Duration::from_secs(1), task)
                            .await
                            .is_err()
                        {
                            abort.abort();
                        }
                    }
                    let under_limit = self.record_crash();
                    tracing::warn!(
                        rc,
                        stderr_tail = %self.last_stderr_tail(),
                        crashes_in_window = self.crash_times.len(),
                        "kiosk_child_exited"
                    );
                    if !under_limit {
                        self.crash_looped = true;
                        tracing::error!(last_rc = rc, "kiosk_crash_loop_guard");
                        return if rc > 0 { rc } else { 5 };
                    }
                    if self.stop.wait_timeout(self.timing.retry).await {
                        return 0;
                    }
                }
            }
        }
        0
    }
}

#[cfg(test)]
mod tests;
