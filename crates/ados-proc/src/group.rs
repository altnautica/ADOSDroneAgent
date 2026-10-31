//! A child that leads its own process group.
//!
//! The daemons fork C binaries and shell pipelines whose members must never
//! outlive their owner: an orphaned encoder bridge holds a stream publisher
//! slot, an orphaned `wfb_rx` holds its UDP port, an orphaned `dnsmasq` holds
//! a lease socket. [`GroupChild`] makes the cleanup structural:
//!
//! - `setsid()` in the pre-exec hook makes the child its own process-group
//!   leader (PGID == PID), so one `killpg` reaches every member it forks;
//! - [`terminate`](GroupChild::terminate) is `SIGTERM` to the group, a bounded
//!   wait, then `SIGKILL` to the group, and a final `SIGKILL` sweep for any
//!   member that outlived the leader;
//! - `Drop` kills the group unless a terminate or kill already did, so a group
//!   whose leader was reaped is never signalled again (its PGID may have been
//!   reused by then);
//! - every child gets a null stdin and `kill_on_drop` as a single-PID backstop.
//!
//! Off Linux there is no group: terminate and kill signal the child itself.

use std::io;
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use tokio::process::{Child, ChildStderr, ChildStdout, Command};

/// Where a child's stdout goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StdoutMode {
    /// Discarded.
    Null,
    /// Piped; take it with [`GroupChild::take_stdout`] and drain it, or the
    /// child blocks once the 64 KiB pipe fills.
    Piped,
}

/// Where a child's stderr goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StderrMode {
    /// Discarded.
    Null,
    /// Piped; take it with [`GroupChild::take_stderr`] and drain it.
    Piped,
    /// Inherited from the owning daemon (lands in its journal).
    Inherit,
    /// Appended to this file, emptied at spawn. The `O_APPEND` descriptor lets
    /// [`crate::logcap::cap_log`] truncate the file under a running child: every
    /// write lands at the current end, so the file restarts small.
    AppendLog(PathBuf),
}

/// What to spawn.
#[derive(Debug, Clone)]
pub struct Spec<'a> {
    pub program: &'a str,
    pub args: &'a [String],
    pub stdout: StdoutMode,
    pub stderr: StderrMode,
}

impl Spec<'_> {
    fn command(&self) -> io::Result<Command> {
        let mut cmd = Command::new(self.program);
        cmd.args(self.args)
            .stdin(Stdio::null())
            .stdout(match self.stdout {
                StdoutMode::Null => Stdio::null(),
                StdoutMode::Piped => Stdio::piped(),
            })
            .stderr(match &self.stderr {
                StderrMode::Null => Stdio::null(),
                StderrMode::Piped => Stdio::piped(),
                StderrMode::Inherit => Stdio::inherit(),
                StderrMode::AppendLog(path) => Stdio::from(append_log(path)?),
            })
            .kill_on_drop(true);
        Ok(cmd)
    }
}

fn append_log(path: &std::path::Path) -> io::Result<std::fs::File> {
    let file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)?;
    file.set_len(0)?;
    Ok(file)
}

/// A live child leading its own process group.
pub struct GroupChild {
    label: String,
    #[cfg(target_os = "linux")]
    pgid: nix::unistd::Pid,
    inner: Child,
    /// Set once [`terminate`](Self::terminate) or [`kill`](Self::kill) has
    /// killed the group and reaped the leader; `Drop` then leaves it alone.
    group_reaped: bool,
}

impl GroupChild {
    /// Spawn `spec` as a new process-group leader. `label` names the child in
    /// log lines.
    pub fn spawn(label: &str, spec: Spec<'_>) -> io::Result<Self> {
        Self::spawn_with(label, spec, |_| {})
    }

    /// [`spawn`](Self::spawn), with `configure` applied to the command before
    /// it is started: an environment overlay, removed variables, or a uid/gid
    /// to drop to. The group setup is installed after `configure` runs.
    pub fn spawn_with(
        label: &str,
        spec: Spec<'_>,
        configure: impl FnOnce(&mut Command),
    ) -> io::Result<Self> {
        let mut cmd = spec.command()?;
        configure(&mut cmd);
        #[cfg(target_os = "linux")]
        // Safety: setsid() is async-signal-safe and is the only call in the hook.
        unsafe {
            cmd.pre_exec(|| {
                nix::unistd::setsid().map_err(|e| io::Error::from_raw_os_error(e as i32))?;
                Ok(())
            });
        }
        let inner = cmd.spawn()?;
        #[cfg(target_os = "linux")]
        let pgid = {
            let raw = inner
                .id()
                .ok_or_else(|| io::Error::other("group child has no PID yet"))?;
            // After setsid the child leads its own group: PGID == PID.
            nix::unistd::Pid::from_raw(raw as i32)
        };
        Ok(Self {
            label: label.to_string(),
            #[cfg(target_os = "linux")]
            pgid,
            inner,
            group_reaped: false,
        })
    }

    /// Spawn `spec` as a member of this child's process group, so the group
    /// terminate, kill and `Drop` reach it too. `stdin_piped` pipes its stdin
    /// (otherwise null). The returned child is the caller's to wait on.
    pub fn spawn_joined(&self, spec: Spec<'_>, stdin_piped: bool) -> io::Result<Child> {
        let mut cmd = spec.command()?;
        if stdin_piped {
            cmd.stdin(Stdio::piped());
        }
        #[cfg(target_os = "linux")]
        {
            let pgid = self.pgid;
            // Safety: setpgid() is async-signal-safe and is the only call in
            // the hook.
            unsafe {
                cmd.pre_exec(move || {
                    nix::unistd::setpgid(nix::unistd::Pid::from_raw(0), pgid)
                        .map_err(|e| io::Error::from_raw_os_error(e as i32))?;
                    Ok(())
                });
            }
        }
        cmd.spawn()
    }

    /// The label given at spawn.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// The leader's PID, or `None` once it has been reaped.
    pub fn pid(&self) -> Option<u32> {
        self.inner.id()
    }

    /// Take the piped stdout (only with [`StdoutMode::Piped`]).
    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.inner.stdout.take()
    }

    /// Take the piped stderr (only with [`StderrMode::Piped`]).
    pub fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.inner.stderr.take()
    }

    /// True while the leader has not exited.
    pub fn is_running(&mut self) -> bool {
        matches!(self.inner.try_wait(), Ok(None))
    }

    /// Wait for the leader to exit. The group is left as it is.
    pub async fn wait(&mut self) -> io::Result<ExitStatus> {
        self.inner.wait().await
    }

    /// Graceful teardown: `SIGTERM` the group, wait up to `grace` for the
    /// leader, then `SIGKILL` the group and reap the leader; finally sweep the
    /// group with `SIGKILL` so no member outlives the leader.
    pub async fn terminate(&mut self, grace: Duration) {
        #[cfg(target_os = "linux")]
        {
            use nix::sys::signal::{killpg, Signal};
            let _ = killpg(self.pgid, Signal::SIGTERM);
            if tokio::time::timeout(grace, self.inner.wait())
                .await
                .is_err()
            {
                tracing::warn!(label = %self.label, "process group did not exit on SIGTERM; SIGKILL");
                let _ = killpg(self.pgid, Signal::SIGKILL);
                let _ = self.inner.wait().await;
            }
            let _ = killpg(self.pgid, Signal::SIGKILL);
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = grace;
            let _ = self.inner.start_kill();
            let _ = self.inner.wait().await;
        }
        self.group_reaped = true;
    }

    /// `SIGKILL` the group and reap the leader.
    pub async fn kill(&mut self) {
        #[cfg(target_os = "linux")]
        {
            use nix::sys::signal::{killpg, Signal};
            let _ = killpg(self.pgid, Signal::SIGKILL);
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = self.inner.start_kill();
        }
        let _ = self.inner.wait().await;
        self.group_reaped = true;
    }
}

impl Drop for GroupChild {
    fn drop(&mut self) {
        // A leader that merely exited can leave pipeline members running, so
        // only a terminate/kill (which swept the group) skips this.
        #[cfg(target_os = "linux")]
        if !self.group_reaped {
            use nix::sys::signal::{killpg, Signal};
            let _ = killpg(self.pgid, Signal::SIGKILL);
        }
    }
}

#[cfg(test)]
mod tests;
