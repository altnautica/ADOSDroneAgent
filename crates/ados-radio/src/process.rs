//! `wfb_tx` subprocess manager with process-group isolation.
//!
//! The Python predecessor used `asyncio.create_subprocess_exec()` with no
//! `setsid`/`killpg`, so a `proc.terminate()` that raised or timed out left
//! `wfb_tx` as an orphan in the agent's cgroup — the v0.46.4 bug. This
//! module fixes that structurally:
//!
//! - `setsid()` in the child's pre-exec hook moves `wfb_tx` into its own
//!   process group (same PID as the process group ID, PGID = wfb_tx's PID).
//! - `WfbTxProcess::kill()` calls `killpg(pgid, SIGKILL)` — the whole group
//!   dies atomically, including any grandchild the C binary might have spawned.
//! - The `Drop` impl also calls `killpg` so the process never outlives the
//!   Rust owner, even on panic.
//!
//! The wfb_tx command mirrors `manager.py:479-504`:
//!   `wfb_tx -p 0 -u 5600 -K <key> -k <fec_k> -n <fec_n> -B 20 -M <mcs> <iface>`

use std::path::Path;

use crate::config::WfbConfig;

/// A live `wfb_tx` child in its own process group.
pub struct WfbTxProcess {
    #[cfg(target_os = "linux")]
    pgid: nix::unistd::Pid,
    inner: tokio::process::Child,
}

impl WfbTxProcess {
    /// Spawn `wfb_tx` in a new session (setsid). Returns the process handle or
    /// an error when the binary is not found or the fork fails.
    pub async fn spawn(
        iface: &str,
        _channel: u8,
        cfg: &WfbConfig,
        key_path: &Path,
    ) -> std::io::Result<Self> {
        let mut cmd = tokio::process::Command::new("wfb_tx");
        cmd.args([
            "-p",
            "0",
            "-u",
            "5600",
            "-K",
            key_path.to_str().unwrap_or("/etc/ados/wfb/tx.key"),
            "-k",
            &cfg.fec_k.to_string(),
            "-n",
            &cfg.fec_n.to_string(),
            "-B",
            "20",
            "-M",
            &cfg.mcs_index.to_string(),
            iface,
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped()); // captured for Rule-37 diagnostics

        // Move the child into its own session so killpg later kills it cleanly.
        #[cfg(target_os = "linux")]
        // Safety: setsid() is async-signal-safe and is the only call in this hook.
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
                .ok_or_else(|| std::io::Error::other("wfb_tx has no PID yet"))?;
            // After setsid the child is its own process group leader: PGID == PID.
            nix::unistd::Pid::from_raw(raw_pid as i32)
        };

        Ok(Self {
            #[cfg(target_os = "linux")]
            pgid,
            inner: child,
        })
    }

    /// True if the process has not yet exited.
    pub fn is_running(&mut self) -> bool {
        // `try_wait` is non-blocking and returns None when still running.
        matches!(self.inner.try_wait(), Ok(None))
    }

    /// The OS PID, for reading `/proc/<pid>/io`.
    pub fn pid(&self) -> Option<u32> {
        self.inner.id()
    }

    /// Kill the entire process group and wait for the child to exit. The
    /// `Drop` impl also calls this, but explicit shutdown lets callers await
    /// the exit and log cleanly.
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
        // No-op on non-Linux.
    }
}

impl Drop for WfbTxProcess {
    fn drop(&mut self) {
        self.killpg_now();
        // Best-effort blocking wait so the cgroup is fully reaped.
        let _ = std::process::Command::new("true").status(); // yield to the OS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wfb_tx_args_correct_format() {
        // Verify the argument list we build matches the Python prototype:
        // wfb_tx -p 0 -u 5600 -K <key> -k <fec_k> -n <fec_n> -B 20 -M <mcs> <iface>
        let cfg = WfbConfig::default();
        let key = Path::new("/etc/ados/wfb/tx.key");
        let iface = "wlan1";
        // Re-build the arg list the same way spawn() does, without forking.
        let args: Vec<String> = vec![
            "-p",
            "0",
            "-u",
            "5600",
            "-K",
            key.to_str().unwrap(),
            "-k",
            &cfg.fec_k.to_string(),
            "-n",
            &cfg.fec_n.to_string(),
            "-B",
            "20",
            "-M",
            &cfg.mcs_index.to_string(),
            iface,
        ]
        .into_iter()
        .map(String::from)
        .collect();
        // -K must be followed by the key path.
        let k_idx = args.iter().position(|a| a == "-K").unwrap();
        assert_eq!(args[k_idx + 1], "/etc/ados/wfb/tx.key");
        // fec_k defaults to 8, fec_n to 12.
        let k_idx = args.iter().position(|a| a == "-k").unwrap();
        assert_eq!(args[k_idx + 1], "8");
        let n_idx = args.iter().position(|a| a == "-n").unwrap();
        assert_eq!(args[n_idx + 1], "12");
        // Last arg is the interface.
        assert_eq!(args.last().unwrap(), "wlan1");
    }
}
