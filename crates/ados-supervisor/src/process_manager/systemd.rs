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

    /// `systemctl mask <unit>` (idempotent).
    async fn mask(&self, unit: &str) {
        let _ = run(&["mask", unit], PROBE_TIMEOUT).await;
    }

    /// `systemctl unmask <unit>` (idempotent).
    async fn unmask(&self, unit: &str) {
        let _ = run(&["unmask", unit], PROBE_TIMEOUT).await;
    }
}
