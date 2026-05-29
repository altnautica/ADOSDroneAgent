//! Forked-helper process manager with process-group isolation (Rule 37).
//!
//! The USB-gadget manager forks a long-lived `dnsmasq` to serve the single
//! tethered host its DHCP lease. The Python predecessor used a bare
//! `subprocess.Popen` + `terminate()`, so a `terminate()` that raised or timed
//! out could orphan the child. This is the same structural fix the radio crate
//! ships:
//!
//! - `setsid()` in the child's pre-exec hook makes the child its own process
//!   group leader (PGID == PID).
//! - `kill()` issues `killpg(pgid, SIGKILL)` so the whole group dies atomically
//!   (no orphaned dnsmasq lingering with a bound UDP socket on usb0).
//! - `Drop` also kills the group so the helper never outlives its Rust owner.

use std::process::Stdio;

/// A live helper child in its own process group.
pub struct ManagedProcess {
    #[cfg(target_os = "linux")]
    pgid: nix::unistd::Pid,
    inner: tokio::process::Child,
}

impl ManagedProcess {
    /// Spawn `program` with `args` as a process-group leader. stdout is
    /// discarded; stderr is piped so the caller may drain it. The child is
    /// `setsid`'d so [`kill`](Self::kill) can `killpg` the whole group.
    pub fn spawn(program: &str, args: &[&str]) -> std::io::Result<Self> {
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());

        // Move the child into its own session so killpg later kills it cleanly.
        // `pre_exec` is an inherent method on tokio's unix Command.
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
                .ok_or_else(|| std::io::Error::other("helper child has no PID yet"))?;
            // After setsid the child is its own process-group leader: PGID == PID.
            nix::unistd::Pid::from_raw(raw_pid as i32)
        };

        Ok(Self {
            #[cfg(target_os = "linux")]
            pgid,
            inner: child,
        })
    }

    /// The OS PID, if the child has one.
    pub fn pid(&self) -> Option<u32> {
        self.inner.id()
    }

    /// True if the process has not yet exited.
    pub fn is_running(&mut self) -> bool {
        matches!(self.inner.try_wait(), Ok(None))
    }

    /// Kill the entire process group and reap the child. Idempotent.
    pub async fn kill(&mut self) {
        self.killpg_now();
        let _ = self.inner.wait().await;
    }

    #[cfg(target_os = "linux")]
    fn killpg_now(&self) {
        use nix::sys::signal::{self, Signal};
        let _ = signal::killpg(self.pgid, Signal::SIGKILL);
    }

    #[cfg(not(target_os = "linux"))]
    fn killpg_now(&self) {
        // No-op off Linux (no setsid group was created).
    }
}

impl Drop for ManagedProcess {
    fn drop(&mut self) {
        self.killpg_now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn spawn_and_kill_a_short_lived_helper() {
        // `sleep 30` stands in for dnsmasq: spawn it in its own group, confirm
        // it runs, then killpg it. Available on both macOS and Linux.
        let mut p = match ManagedProcess::spawn("sleep", &["30"]) {
            Ok(p) => p,
            // No `sleep` on PATH in some sandboxes; skip rather than fail.
            Err(_) => return,
        };
        assert!(p.pid().is_some());
        assert!(p.is_running());
        p.kill().await;
        assert!(!p.is_running());
    }
}
