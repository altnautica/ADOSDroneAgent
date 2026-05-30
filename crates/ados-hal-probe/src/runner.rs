//! Sandboxed probe runner.
//!
//! A trial-init probe spawns a real helper (e.g. `ffmpeg` initializing a V4L2
//! encoder). On a board whose advertised encoder is backed by no real device,
//! that init can wedge the kernel ioctl into an uninterruptible **D-state**: a
//! sleep that *no signal can reap*, not even SIGKILL. `kill_on_drop` and a bare
//! single-PID `kill()` are both useless against it, and a naive blocking
//! `wait()` would hang the agent forever.
//!
//! This runner is the structural defense:
//! - the child is spawned in its **own process group** (`setsid` in a pre-exec
//!   hook), so the trial and anything it forks share one group id;
//! - a watcher polls `try_wait` against a hard wall-clock budget;
//! - on timeout it `killpg(SIGKILL)`s the **whole group**, then does a
//!   **bounded** reap (a short poll window);
//! - if the child *still* will not reap, it is honestly D-state: the runner
//!   returns [`ProbeOutcome::TimedOutHung`] and **does not block** — the agent
//!   parent survives and treats the capability as a lying advertisement.
//!
//! No async runtime is involved: a probe runs on a worker thread off the hot
//! path, and a plain `try_wait` poll loop keeps the dependency surface minimal.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The result of a sandboxed probe run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// The child exited within the budget with a success status.
    ExitedOk { ms: u32 },
    /// The child exited within the budget with a *failure* status, after `ms`
    /// milliseconds (the device advertised itself but the trial init failed).
    ExitedAfterMs { ms: u32 },
    /// The child overran the budget. The runner SIGKILLed the process group and
    /// either reaped it within the bounded reap window or — if it stayed
    /// D-state — gave up so the agent survives.
    TimedOutHung,
}

/// How long to keep polling for a reap after `killpg(SIGKILL)` on timeout.
const REAP_BUDGET: Duration = Duration::from_millis(500);
/// `try_wait` poll cadence (both for the run budget and the reap budget).
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Spawn `argv` (program then args) as a process-group leader and wait up to
/// `timeout`. On overrun, SIGKILL the whole group, attempt a bounded reap, and
/// return [`ProbeOutcome::TimedOutHung`] regardless of whether the reap
/// succeeded — the point is that this call never blocks past
/// `timeout + REAP_BUDGET`.
///
/// `argv[0]` is the program; the rest are arguments. A spawn failure (the
/// program is absent) is reported as [`ProbeOutcome::ExitedAfterMs`] with `ms`
/// = 0, i.e. "it could not even start", which the probe layer treats the same
/// as a trial-init failure (no usable device).
pub fn run_probe_sandboxed(argv: &[&str], timeout: Duration) -> ProbeOutcome {
    let Some((program, args)) = argv.split_first() else {
        return ProbeOutcome::ExitedAfterMs { ms: 0 };
    };

    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(target_os = "linux")]
    // Safety: setsid() is async-signal-safe and is the only call in the hook.
    // It makes the child its own process-group leader so PGID == PID and the
    // timeout path can kill the whole group atomically.
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            nix::unistd::setsid().map_err(|e| std::io::Error::from_raw_os_error(e as i32))?;
            Ok(())
        });
    }

    let start = Instant::now();
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(program, error = %e, "probe_spawn_failed");
            return ProbeOutcome::ExitedAfterMs { ms: 0 };
        }
    };

    // After setsid the child leads its own group: PGID == PID.
    #[cfg(target_os = "linux")]
    let pgid = nix::unistd::Pid::from_raw(child.id() as i32);

    // Poll for exit within the run budget.
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let ms = elapsed_ms(start);
                return if status.success() {
                    ProbeOutcome::ExitedOk { ms }
                } else {
                    ProbeOutcome::ExitedAfterMs { ms }
                };
            }
            Ok(None) => {
                if start.elapsed() >= timeout {
                    break;
                }
                std::thread::sleep(POLL_INTERVAL);
            }
            Err(e) => {
                // Wait itself errored; treat as a failed trial, don't block.
                tracing::warn!(program, error = %e, "probe_wait_failed");
                return ProbeOutcome::ExitedAfterMs {
                    ms: elapsed_ms(start),
                };
            }
        }
    }

    // Budget overrun. SIGKILL the whole group, then do a *bounded* reap.
    tracing::warn!(
        program,
        timeout_ms = timeout.as_millis() as u64,
        "probe_timed_out; killing process group"
    );

    #[cfg(target_os = "linux")]
    {
        use nix::sys::signal::{killpg, Signal};
        let _ = killpg(pgid, Signal::SIGKILL);
    }
    #[cfg(not(target_os = "linux"))]
    {
        // No process groups on the dev host; single-PID best-effort kill.
        let _ = child.kill();
    }

    let reap_start = Instant::now();
    loop {
        match child.try_wait() {
            // Reaped after the kill — still report Hung: the device overran its
            // budget, which is the signal the probe layer needs.
            Ok(Some(_)) => return ProbeOutcome::TimedOutHung,
            Ok(None) => {
                if reap_start.elapsed() >= REAP_BUDGET {
                    // Truly D-state: it will not reap. Do NOT block the agent.
                    tracing::error!(
                        program,
                        "probe child not reapable after SIGKILL (likely D-state); abandoning"
                    );
                    return ProbeOutcome::TimedOutHung;
                }
                std::thread::sleep(POLL_INTERVAL);
            }
            Err(_) => return ProbeOutcome::TimedOutHung,
        }
    }
}

fn elapsed_ms(start: Instant) -> u32 {
    start.elapsed().as_millis().min(u32::MAX as u128) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exited_ok_for_true() {
        // `true` lives at /bin on Linux rigs and /usr/bin on the dev host; a
        // bare name lets Command resolve it via PATH on either.
        let out = run_probe_sandboxed(&["true"], Duration::from_secs(2));
        assert!(matches!(out, ProbeOutcome::ExitedOk { .. }), "{out:?}");
    }

    #[test]
    fn exited_after_ms_for_false() {
        // `false` exits non-zero promptly → a trial-init "failure".
        let out = run_probe_sandboxed(&["false"], Duration::from_secs(2));
        assert!(matches!(out, ProbeOutcome::ExitedAfterMs { .. }), "{out:?}");
    }

    #[test]
    fn timed_out_for_long_sleep() {
        // A sleep that vastly overruns a short budget must come back Hung
        // within timeout + reap budget, never block.
        let start = Instant::now();
        let out = run_probe_sandboxed(&["/bin/sleep", "30"], Duration::from_millis(100));
        let waited = start.elapsed();
        assert_eq!(out, ProbeOutcome::TimedOutHung);
        // It must not have waited anywhere near the 30s sleep.
        assert!(
            waited < Duration::from_secs(3),
            "runner blocked too long: {waited:?}"
        );
    }

    #[test]
    fn empty_argv_is_a_failure() {
        assert_eq!(
            run_probe_sandboxed(&[], Duration::from_secs(1)),
            ProbeOutcome::ExitedAfterMs { ms: 0 }
        );
    }

    #[test]
    fn missing_program_is_a_failure() {
        assert_eq!(
            run_probe_sandboxed(&["/nonexistent/probe-xyzzy"], Duration::from_secs(1)),
            ProbeOutcome::ExitedAfterMs { ms: 0 }
        );
    }
}
