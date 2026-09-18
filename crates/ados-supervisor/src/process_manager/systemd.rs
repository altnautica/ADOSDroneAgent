//! systemd backend: thin async wrappers over the `systemctl` binary.
//!
//! The supervisor orchestrates systemd and never spawns a service process
//! itself, so every lifecycle action funnels through here. A missing
//! `systemctl` (e.g. a non-Linux dev host) or a timeout is treated as a soft
//! failure: the wrapper returns `false`/`None` and the caller logs and
//! proceeds, matching the Python wrapper's behavior under pytest.

use std::time::Duration;

use async_trait::async_trait;
use tokio::process::Command;
use tokio::time::timeout;

use super::ProcessManager;

const ACT_TIMEOUT: Duration = Duration::from_secs(30);
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

async fn run(args: &[&str], dur: Duration) -> Option<std::process::Output> {
    match timeout(dur, Command::new("systemctl").args(args).output()).await {
        Ok(Ok(out)) => Some(out),
        Ok(Err(_)) => None, // spawn error (systemctl missing)
        Err(_) => None,     // timed out
    }
}

fn ok(out: &Option<std::process::Output>) -> bool {
    out.as_ref().map(|o| o.status.success()).unwrap_or(false)
}

/// Drives service units via the `systemctl` binary.
pub struct SystemdManager;

#[async_trait]
impl ProcessManager for SystemdManager {
    /// `systemctl start <unit>`.
    async fn start(&self, unit: &str) -> bool {
        ok(&run(&["start", unit], ACT_TIMEOUT).await)
    }

    /// `systemctl stop <unit>`.
    async fn stop(&self, unit: &str) -> bool {
        ok(&run(&["stop", unit], ACT_TIMEOUT).await)
    }

    /// `systemctl restart <unit>` — the prompt path to a fresh spawn cycle (used
    /// after a key write so the wfb unit reloads the new key).
    async fn restart(&self, unit: &str) -> bool {
        ok(&run(&["restart", unit], ACT_TIMEOUT).await)
    }

    /// `systemctl reset-failed <unit>` — clears a `failed (start-limit-hit)`
    /// state + the burst counter so a following `start` is not a no-op.
    async fn reset_failed(&self, unit: &str) {
        let _ = run(&["reset-failed", unit], PROBE_TIMEOUT).await;
    }

    /// True only when `systemctl is-active <unit>` prints exactly `active`.
    async fn is_active(&self, unit: &str) -> bool {
        match run(&["is-active", unit], PROBE_TIMEOUT).await {
            Some(out) => String::from_utf8_lossy(&out.stdout).trim() == "active",
            None => false,
        }
    }

    /// `systemctl show -p MainPID` then `/proc/<pid>/io`, summing `rchar` and
    /// `wchar` (bytes handed to read/write syscalls, which covers both the
    /// serial/UART lanes and the unix-socket ones).
    ///
    /// Every failure mode answers `None` rather than a number: no `systemctl`,
    /// `MainPID=0` (the unit is not running, or is a type systemd does not
    /// track a main PID for), an unreadable `/proc` entry, or the PID being
    /// recycled between the two reads. The caller must not be able to tell a
    /// zero-progress process from an unreadable one by the value alone.
    async fn work_counter(&self, unit: &str) -> Option<u64> {
        let out = run(&["show", "-p", "MainPID", "--value", unit], PROBE_TIMEOUT).await?;
        let pid: u32 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
        if pid == 0 {
            return None;
        }
        let raw = tokio::fs::read_to_string(format!("/proc/{pid}/io"))
            .await
            .ok()?;
        let mut total: Option<u64> = None;
        for line in raw.lines() {
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            if matches!(key, "rchar" | "wchar") {
                if let Ok(n) = value.trim().parse::<u64>() {
                    total = Some(total.unwrap_or(0).saturating_add(n));
                }
            }
        }
        total
    }

    /// `systemctl mask <unit>` (idempotent).
    async fn mask(&self, unit: &str) {
        let _ = run(&["mask", unit], PROBE_TIMEOUT).await;
    }

    /// `systemctl unmask <unit>` (idempotent).
    async fn unmask(&self, unit: &str) {
        let _ = run(&["unmask", unit], PROBE_TIMEOUT).await;
    }
}
