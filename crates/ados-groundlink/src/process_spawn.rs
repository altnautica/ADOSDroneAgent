//! Ground-side wfb subprocess spawn with process-group isolation.
//!
//! The receive side forks `wfb_rx`/`wfb_tx` C binaries with GS-specific args
//! (data RX on 5599, rx-control on 5803, tx-control on 5810) that differ from
//! the drone-side arg sets in `ados_radio::process`. The setsid/killpg
//! discipline is the same structural fix: the child becomes its own process
//! group leader so a terminate kills the whole group atomically and a `Drop`
//! killpg guarantees the C binary never outlives its Rust owner (the
//! orphan-child bug class is impossible). This is the receive-side sibling of
//! `ados_radio::process::WfbProcess`, kept in this crate because the args are
//! GS-specific and the drone crate exposes no generic spawn entry point.

/// stdout disposition for a spawned wfb child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stdout {
    /// Pipe stdout so the caller can read the per-second stats lines.
    Piped,
    /// Discard stdout (PKT stats would otherwise fill the pipe).
    Null,
}

/// A live wfb child in its own process group.
pub struct GsWfbProcess {
    #[cfg(target_os = "linux")]
    pgid: nix::unistd::Pid,
    inner: tokio::process::Child,
}

impl GsWfbProcess {
    /// Spawn `program` with `args` as a process-group leader (setsid). `stdout`
    /// selects pipe-vs-null; when `stderr_log` is `Some`, stderr is redirected
    /// to that truncated file (avoids the PIPE deadlock), else discarded.
    pub async fn spawn(
        program: &str,
        args: &[String],
        stdout: Stdout,
        stderr_log: Option<&str>,
    ) -> std::io::Result<Self> {
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(args);

        match stdout {
            Stdout::Piped => {
                cmd.stdout(std::process::Stdio::piped());
            }
            Stdout::Null => {
                cmd.stdout(std::process::Stdio::null());
            }
        }

        match stderr_log {
            Some(path) => {
                let file = std::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(path)?;
                cmd.stderr(std::process::Stdio::from(file));
            }
            None => {
                cmd.stderr(std::process::Stdio::null());
            }
        }

        // Move the child into its own session so killpg later kills it cleanly.
        #[cfg(target_os = "linux")]
        // Safety: setsid() is async-signal-safe and is the only call in the hook.
        unsafe {
            cmd.pre_exec(|| {
                nix::unistd::setsid().map_err(|e| std::io::Error::from_raw_os_error(e as i32))?;
                Ok(())
            });
        }

        let child = cmd.spawn()?;
        #[cfg(target_os = "linux")]
        let pgid = {
            let raw_pid = child
                .id()
                .ok_or_else(|| std::io::Error::other("wfb child has no PID yet"))?;
            // After setsid the child is its own process-group leader: PGID == PID.
            nix::unistd::Pid::from_raw(raw_pid as i32)
        };

        Ok(Self {
            #[cfg(target_os = "linux")]
            pgid,
            inner: child,
        })
    }

    /// Spawn `program` with `args` as a process-group leader (setsid), stdout
    /// discarded and **stderr piped** for the caller to read. The relay /
    /// receiver wfb_rx subprocesses print their `PKT` stats on stderr, so the
    /// stats tail reads from there.
    pub async fn spawn_stderr_piped(program: &str, args: &[String]) -> std::io::Result<Self> {
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped());

        #[cfg(target_os = "linux")]
        // Safety: setsid() is async-signal-safe and is the only call in the hook.
        unsafe {
            cmd.pre_exec(|| {
                nix::unistd::setsid().map_err(|e| std::io::Error::from_raw_os_error(e as i32))?;
                Ok(())
            });
        }

        let child = cmd.spawn()?;
        #[cfg(target_os = "linux")]
        let pgid = {
            let raw_pid = child
                .id()
                .ok_or_else(|| std::io::Error::other("wfb child has no PID yet"))?;
            nix::unistd::Pid::from_raw(raw_pid as i32)
        };

        Ok(Self {
            #[cfg(target_os = "linux")]
            pgid,
            inner: child,
        })
    }

    /// Take the child's stdout handle (for the stats reader). `None` if stdout
    /// was not piped or already taken.
    pub fn take_stdout(&mut self) -> Option<tokio::process::ChildStdout> {
        self.inner.stdout.take()
    }

    /// Take the child's stderr handle (for the relay/receiver stats tail).
    /// `None` if stderr was not piped or already taken.
    pub fn take_stderr(&mut self) -> Option<tokio::process::ChildStderr> {
        self.inner.stderr.take()
    }

    /// True if the process has not yet exited.
    pub fn is_running(&mut self) -> bool {
        matches!(self.inner.try_wait(), Ok(None))
    }

    /// The OS PID.
    pub fn pid(&self) -> Option<u32> {
        self.inner.id()
    }

    /// Kill the entire process group and wait for the child to exit.
    pub async fn kill(&mut self) {
        self.killpg_now();
        let _ = self.inner.wait().await;
    }

    /// Graceful shutdown: send SIGTERM to the whole process group, wait up to
    /// `grace` for the child to exit, then SIGKILL the group if it is still
    /// alive. Mirrors the Python `proc.terminate(); wait(3s); proc.kill()`
    /// sequence the relay/receiver loops use when a peer changes or shuts down,
    /// giving `wfb_rx` a chance to flush before the hard kill.
    pub async fn terminate_then_kill(&mut self, grace: std::time::Duration) {
        self.termpg_now();
        match tokio::time::timeout(grace, self.inner.wait()).await {
            Ok(_) => {}
            Err(_) => {
                // Still alive after the grace window: hard-kill the group.
                self.killpg_now();
                let _ = self.inner.wait().await;
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn termpg_now(&self) {
        use nix::sys::signal::{self, Signal};
        let _ = signal::killpg(self.pgid, Signal::SIGTERM);
    }

    #[cfg(not(target_os = "linux"))]
    fn termpg_now(&self) {
        // No-op off Linux.
    }

    #[cfg(target_os = "linux")]
    fn killpg_now(&self) {
        use nix::sys::signal::{self, Signal};
        let _ = signal::killpg(self.pgid, Signal::SIGKILL);
    }

    #[cfg(not(target_os = "linux"))]
    fn killpg_now(&self) {
        // No-op off Linux.
    }
}

impl Drop for GsWfbProcess {
    fn drop(&mut self) {
        self.killpg_now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn spawns_and_kills_a_process_group() {
        // `true` exits immediately; we just prove the spawn + kill path and the
        // PGID capture do not panic on the host.
        #[cfg(target_os = "linux")]
        {
            let mut p = GsWfbProcess::spawn("true", &[], Stdout::Null, None)
                .await
                .expect("spawn true");
            assert!(p.pid().is_some());
            p.kill().await;
        }
        // Off Linux there is no setsid hook; skip the spawn to keep the suite
        // host-portable (the dev host is macOS).
        #[cfg(not(target_os = "linux"))]
        {
            let _ = Stdout::Null;
        }
    }

    #[tokio::test]
    async fn terminate_then_kill_reaps_a_sleeper() {
        // A `sleep 60` ignores nothing but must be reaped well inside the grace
        // window once SIGTERM hits the group.
        #[cfg(target_os = "linux")]
        {
            let mut p = GsWfbProcess::spawn("sleep", &["60".to_string()], Stdout::Null, None)
                .await
                .expect("spawn sleep");
            assert!(p.is_running());
            p.terminate_then_kill(std::time::Duration::from_secs(3))
                .await;
            assert!(!p.is_running());
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = Stdout::Piped;
        }
    }
}
