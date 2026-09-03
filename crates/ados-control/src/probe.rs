//! The single bounded, non-blocking seam for shelling out from a route.
//!
//! Every route in this crate follows the same shape: a thin `async fn` handler
//! delegating to a sync helper that holds the I/O. That is a good shape, and it
//! is exactly why a dozen `std::process::Command::…output()` calls ended up one
//! or two sync frames below an axum handler, on the reactor: nothing in the
//! type system objects, because the helper is sync and the blocking is inside
//! it.
//!
//! Two things are wrong with a blocking spawn there, and only one of them is
//! the obvious one:
//!
//! 1. **It occupies a reactor worker.** `nmcli`, `bluetoothctl` and `iw` can
//!    take seconds; `systemctl` can block on a busy bus. While one does, every
//!    other request scheduled on that worker waits — including requests to
//!    unrelated routes.
//! 2. **It is unbounded.** This is the part `spawn_blocking` does not fix.
//!    Moving a hang to the blocking pool moves *where* the thread is stuck, not
//!    *whether* it comes back: `bluetoothctl` waiting on a wedged
//!    `bluetoothd` never returns, and the pool thread is gone for the life of
//!    the process. The default pool is ~512 threads, so this degrades slowly
//!    and invisibly rather than failing.
//!
//! So the seam here is `tokio::process` plus `tokio::time::timeout` plus
//! `kill_on_drop`, which bounds the run and reaps the child — the same idiom
//! the Bluetooth route (`routes::gs_bluetooth::btctl`) and `ados-net`'s
//! `CmdRunner` already use. Every probe returns a degraded value rather than an
//! error, because every call site it replaces already swallowed spawn failures
//! into a default: an absent binary and a hung one now produce the same "we do
//! not know" the route was already designed to render.
//!
//! The probes are `async fn`, which is the load-bearing part. A future
//! contributor cannot call one from a sync helper without making that helper
//! async too, so the class of defect this module fixes cannot be reintroduced
//! by accident.

use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

/// Default bound for a local, read-only probe.
///
/// Sized against what these commands do when healthy — `systemctl is-active`
/// and `iw station dump` answer in single-digit milliseconds, `nmcli` in tens —
/// and against what a route can afford when they are not. A probe that has not
/// answered in three seconds is not going to answer usefully.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// A longer bound for the two commands that are slow even when working.
///
/// `bluetoothctl` talks to `bluetoothd` over D-Bus and routinely takes over a
/// second on a loaded ground station; `nmcli` re-reads every connection profile.
pub const SLOW_PROBE_TIMEOUT: Duration = Duration::from_secs(8);

/// What a probe learned. `Unavailable` folds together every reason we have no
/// answer — binary absent, spawn refused, non-zero exit, timeout — because no
/// call site in this crate distinguishes them: each one already degraded an
/// absent binary and a failing command to the same default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeOutput {
    /// The command ran and exited zero. Carries lossy-decoded stdout.
    Ok(String),
    /// No usable answer.
    Unavailable,
}

impl ProbeOutput {
    /// The stdout of a successful run, or `""`.
    pub fn text(&self) -> &str {
        match self {
            ProbeOutput::Ok(s) => s,
            ProbeOutput::Unavailable => "",
        }
    }

    /// True when the command ran and exited zero.
    pub fn is_ok(&self) -> bool {
        matches!(self, ProbeOutput::Ok(_))
    }
}

/// Run `program args…` with no stdin, bounded by `timeout`, and return its
/// stdout on a zero exit.
///
/// The child is killed and reaped on timeout *and* on cancellation
/// (`kill_on_drop`), so a client that disconnects mid-request never leaves a
/// stray `nmcli` behind — which the blocking version did on every abandoned
/// request.
pub async fn capture(program: &str, args: &[&str], timeout: Duration) -> ProbeOutput {
    let child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn();
    let child = match child {
        Ok(c) => c,
        Err(_) => return ProbeOutput::Unavailable,
    };
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(out)) if out.status.success() => {
            ProbeOutput::Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        }
        _ => ProbeOutput::Unavailable,
    }
}

/// [`capture`] with the environment forced to plain, uncoloured, unpaged `C`
/// output.
///
/// `systemctl` prefixes a failed unit's row with a status glyph and will page or
/// colour its output depending on the caller's environment. Every `systemctl`
/// probe in this crate parses columns, so every one of them needs this — and
/// each was setting the three variables itself.
pub async fn capture_systemctl(args: &[&str], timeout: Duration) -> ProbeOutput {
    let child = Command::new("systemctl")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env("SYSTEMD_COLORS", "0")
        .env("SYSTEMD_PAGER", "")
        .env("LANG", "C")
        .kill_on_drop(true)
        .spawn();
    let child = match child {
        Ok(c) => c,
        Err(_) => return ProbeOutput::Unavailable,
    };
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(out)) if out.status.success() => {
            ProbeOutput::Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        }
        _ => ProbeOutput::Unavailable,
    }
}

/// Run `program args…` for its exit status only, discarding output.
///
/// For the signal-style commands (`systemctl kill -s HUP`) whose output nobody
/// reads.
pub async fn status_only(program: &str, args: &[&str], timeout: Duration) -> bool {
    let child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(_) => return false,
    };
    matches!(
        tokio::time::timeout(timeout, child.wait()).await,
        Ok(Ok(status)) if status.success()
    )
}

/// Move a blocking closure off the reactor, folding a panic or a cancelled join
/// into `None`.
///
/// For blocking work that is *not* a process spawn and so cannot be bounded by
/// killing a child — a large `/proc` read, a directory walk. Prefer
/// [`capture`] for anything that shells out: `spawn_blocking` around a process
/// wait relocates an unbounded hang, it does not bound it.
pub async fn offload<T, F>(f: F) -> Option<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f).await.ok()
}

/// True when `unit` reports `active`.
///
/// One definition, replacing the two byte-identical `hostapd_running()` copies
/// that had drifted into `routes::gs_network` and `routes::gs_status`. A
/// duplicated liveness probe is how two surfaces come to give an operator two
/// different answers about the same unit.
pub async fn unit_is_active(unit: &str) -> bool {
    capture_systemctl(&["is-active", unit], PROBE_TIMEOUT)
        .await
        .text()
        .trim()
        == "active"
}

/// True when `program` resolves on `PATH`.
pub async fn on_path(program: &str) -> bool {
    capture("which", &[program], PROBE_TIMEOUT).await.is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_missing_binary_is_unavailable_not_a_hang() {
        let out = capture("ados-no-such-binary-xyz", &[], PROBE_TIMEOUT).await;
        assert_eq!(out, ProbeOutput::Unavailable);
        assert_eq!(out.text(), "");
        assert!(!out.is_ok());
    }

    #[tokio::test]
    async fn stdout_is_captured_on_a_zero_exit() {
        let out = capture("echo", &["hello"], PROBE_TIMEOUT).await;
        assert_eq!(out.text().trim(), "hello");
        assert!(out.is_ok());
    }

    #[tokio::test]
    async fn a_nonzero_exit_is_unavailable() {
        // `false` exits 1 with no output. Every call site this replaces
        // degraded a non-zero exit to its default, so the probe must too.
        assert_eq!(
            capture("false", &[], PROBE_TIMEOUT).await,
            ProbeOutput::Unavailable
        );
    }

    #[tokio::test]
    async fn a_hung_command_is_bounded_and_the_child_is_reaped() {
        // This is the property `spawn_blocking` does not give: a command that
        // never exits must not hold anything for the life of the process.
        let started = std::time::Instant::now();
        let out = capture("sleep", &["30"], Duration::from_millis(150)).await;
        assert_eq!(out, ProbeOutput::Unavailable);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "a hung probe was not bounded: {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn status_only_reports_the_exit_status() {
        assert!(status_only("true", &[], PROBE_TIMEOUT).await);
        assert!(!status_only("false", &[], PROBE_TIMEOUT).await);
        assert!(!status_only("ados-no-such-binary-xyz", &[], PROBE_TIMEOUT).await);
    }

    #[tokio::test]
    async fn status_only_is_bounded_too() {
        let started = std::time::Instant::now();
        assert!(!status_only("sleep", &["30"], Duration::from_millis(150)).await);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn offload_folds_a_panic_into_none() {
        let out: Option<u8> = offload(|| panic!("boom")).await;
        assert_eq!(out, None);
    }

    #[tokio::test]
    async fn offload_returns_the_value() {
        assert_eq!(offload(|| 7u8).await, Some(7));
    }

    #[tokio::test]
    async fn an_absent_unit_is_not_active() {
        assert!(!unit_is_active("ados-no-such-unit-xyz.service").await);
    }
}
