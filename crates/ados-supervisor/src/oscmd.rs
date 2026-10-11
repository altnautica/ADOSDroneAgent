//! Bounded OS-command helpers for the monitor-pass network backends.
//!
//! Every `nmcli`, `ip`, `iw` and `rfkill` call the monitor pass makes runs
//! through here, and every one of them is wrapped in a hard timeout. This is
//! load-bearing, not hygiene: `monitor_pass` awaits the network backends
//! inline, so a single `nmcli device connect` on a flapping FullMAC adapter
//! that never returns stops death-detection, auto-restart and hot-plug
//! handling for the whole process. The systemd watchdog ping is coupled to
//! monitor-pass progress ([`crate::sdnotify`]), so an unbounded call would
//! also take the unit down instead of repairing the link.
//!
//! An elapsed timeout is reported exactly like a spawn failure or a non-zero
//! exit — `false` / `None` — so every caller's existing failure branch handles
//! it. Every child is spawned `kill_on_drop(true)` so the timeout path reaps
//! the hung process instead of leaking one per timed-out repair (an `nmcli`
//! stuck on a wedged driver otherwise survives the whole uptime).
//!
//! Compiled on every host (not `cfg(target_os = "linux")`) so the bound itself
//! is testable off the SBC.

use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;
use tokio::time::timeout;

/// Ceiling for one network-repair subprocess.
///
/// `nmcli connection up` on a re-associating station legitimately takes tens of
/// seconds (DHCP + 4-way handshake), so the bound has to sit above that or the
/// repair never completes; 20 s is the same order as
/// [`crate::process_manager`]'s 30 s action ceiling and well inside the
/// monitor-pass stall budget in [`crate::sdnotify`].
pub const CMD_TIMEOUT: Duration = Duration::from_secs(20);

/// Run a command with the default ceiling, returning true on a zero exit.
/// stdout/stderr are discarded. A spawn failure, a non-zero exit and a timeout
/// are all `false`.
pub async fn run_status(cmd: &str, args: &[&str]) -> bool {
    run_status_within(cmd, args, CMD_TIMEOUT).await
}

/// [`run_status`] with an explicit ceiling. Exists so the bound is testable
/// without holding a test open for [`CMD_TIMEOUT`].
pub async fn run_status_within(cmd: &str, args: &[&str], dur: Duration) -> bool {
    let child = Command::new(cmd)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .status();
    match timeout(dur, child).await {
        Ok(Ok(status)) => status.success(),
        Ok(Err(_)) => false, // spawn error (binary missing)
        Err(_) => {
            tracing::warn!(command = %cmd, timeout_s = dur.as_secs(), "oscmd_timeout");
            false
        }
    }
}

/// Run a command with the default ceiling and capture stdout, or `None` when it
/// could not be run or did not finish in time. A non-zero exit still returns
/// whatever was written to stdout, matching the callers' prior behaviour.
pub async fn run_output(cmd: &str, args: &[&str]) -> Option<String> {
    run_output_within(cmd, args, CMD_TIMEOUT).await
}

/// [`run_output`] with an explicit ceiling.
pub async fn run_output_within(cmd: &str, args: &[&str], dur: Duration) -> Option<String> {
    let child = Command::new(cmd).args(args).kill_on_drop(true).output();
    match timeout(dur, child).await {
        Ok(Ok(out)) => Some(String::from_utf8_lossy(&out.stdout).to_string()),
        Ok(Err(_)) => None, // spawn error (binary missing)
        Err(_) => {
            tracing::warn!(command = %cmd, timeout_s = dur.as_secs(), "oscmd_timeout");
            None
        }
    }
}

/// True when `name` resolves on PATH. Bounded like every other call here.
pub async fn binary_available(name: &str) -> bool {
    run_status("sh", &["-c", &format!("command -v {name}")]).await
}

#[cfg(test)]
mod tests;
