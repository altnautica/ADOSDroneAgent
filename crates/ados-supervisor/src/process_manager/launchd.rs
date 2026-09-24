//! launchd backend: drives service jobs via the `launchctl` binary.
//!
//! Maps the supervisor's lifecycle verbs onto the modern `launchctl` subcommand
//! surface in the GUI domain of the running user (`gui/<uid>/<label>`). A unit
//! name like `ados-control.service` maps to the reverse-DNS launchd label
//! `co.ados.control`. launchd has no direct `reset-failed` analogue, so that
//! verb is a documented best-effort no-op. A missing `launchctl` or a timeout is
//! a soft failure, matching the systemd backend.

use std::time::Duration;

use ados_protocol::launchd::{current_uid, unit_to_label};
use async_trait::async_trait;
use tokio::process::Command;
use tokio::time::timeout;

use super::ProcessManager;

const ACT_TIMEOUT: Duration = Duration::from_secs(30);
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Drives service jobs via the `launchctl` binary on macOS.
pub struct LaunchdManager;

/// The `gui/<uid>/<label>` service target the modern `launchctl` verbs address.
fn service_target(unit: &str) -> String {
    format!("gui/{}/{}", current_uid(), unit_to_label(unit))
}

async fn run(args: &[&str], dur: Duration) -> Option<std::process::Output> {
    let child = Command::new("launchctl")
        .args(args)
        .kill_on_drop(true)
        .output();
    match timeout(dur, child).await {
        Ok(Ok(out)) => Some(out),
        Ok(Err(_)) => None, // spawn error (launchctl missing)
        Err(_) => None,     // timed out
    }
}

fn ok(out: &Option<std::process::Output>) -> bool {
    out.as_ref().map(|o| o.status.success()).unwrap_or(false)
}

#[async_trait]
impl ProcessManager for LaunchdManager {
    /// `launchctl kickstart -k gui/<uid>/<label>` — start (and replace any
    /// running instance of) the job.
    async fn start(&self, unit: &str) -> bool {
        let target = service_target(unit);
        ok(&run(&["kickstart", "-k", &target], ACT_TIMEOUT).await)
    }

    /// `launchctl bootout gui/<uid>/<label>` — remove the job from the domain.
    async fn stop(&self, unit: &str) -> bool {
        let target = service_target(unit);
        ok(&run(&["bootout", &target], ACT_TIMEOUT).await)
    }

    /// `launchctl kickstart -k gui/<uid>/<label>` — `-k` forces a fresh spawn
    /// cycle by killing any running instance first, the restart equivalent.
    async fn restart(&self, unit: &str) -> bool {
        let target = service_target(unit);
        ok(&run(&["kickstart", "-k", &target], ACT_TIMEOUT).await)
    }

    /// Restart only a job that is running: `kickstart -k` on a job that is
    /// not would start it, which is the caller's decision to make.
    async fn try_restart(&self, unit: &str) -> bool {
        match self.is_active(unit).await {
            Some(true) => self.restart(unit).await,
            Some(false) => true,
            None => false,
        }
    }

    /// No-op: launchd has no `reset-failed` analogue. `kickstart -k` already
    /// forces a restart regardless of the prior exit state, so there is no
    /// failed-burst counter to clear before a start.
    async fn reset_failed(&self, _unit: &str) {}

    /// `launchctl print gui/<uid>/<label>`: a job that prints `state = running`
    /// is active; a job that prints something else, or is not loaded (non-zero
    /// exit), is not; a timed-out or unspawnable probe is no verdict.
    async fn is_active(&self, unit: &str) -> Option<bool> {
        let target = service_target(unit);
        let out = run(&["print", &target], PROBE_TIMEOUT).await?;
        Some(
            out.status.success()
                && String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .any(|l| l.trim() == "state = running"),
        )
    }

    /// `launchctl disable gui/<uid>/<label>` (idempotent).
    async fn mask(&self, unit: &str) {
        let target = service_target(unit);
        let _ = run(&["disable", &target], PROBE_TIMEOUT).await;
    }

    /// `launchctl enable gui/<uid>/<label>` (idempotent).
    async fn unmask(&self, unit: &str) {
        let target = service_target(unit);
        let _ = run(&["enable", &target], PROBE_TIMEOUT).await;
    }
}
