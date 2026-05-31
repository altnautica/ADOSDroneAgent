//! The single shell-out primitive.
//!
//! Every external command the installer runs (apt/dpkg, modprobe/dkms,
//! systemctl, curl, python/pip, git, ip, hostnamectl, install) goes through
//! [`run`] so command execution is logged + classified in one place. The
//! installer is a sequential orchestrator, so this is deliberately synchronous
//! `std::process` — there is no concurrency to gain from async here, and a sync
//! primitive keeps the [`crate::graph::Step`] `run` bodies plain.
//!
//! Hang protection: commands that can block indefinitely on the network (curl)
//! pass their own bounding flags (`--max-time`, `--connect-timeout`); apt/dkms
//! are bounded in practice exactly as the predecessor bash installer left them.

use std::process::Command;

/// Outcome of one external command. `code` is `None` when the process could not
/// be spawned at all (binary missing on PATH); `spawned` distinguishes that
/// from a non-zero exit.
#[derive(Debug, Clone)]
pub struct CmdResult {
    /// Process exit code, or `None` if the spawn itself failed.
    pub code: Option<i32>,
    /// Captured stdout (lossy UTF-8).
    pub stdout: String,
    /// Captured stderr (lossy UTF-8).
    pub stderr: String,
    /// True once the process was spawned (regardless of its exit code).
    pub spawned: bool,
}

impl CmdResult {
    /// True iff the process spawned and exited 0.
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }
}

/// Run `program args...`, capturing stdout/stderr. Never panics: a spawn
/// failure (missing binary) yields a `CmdResult { code: None, spawned: false }`
/// rather than an error, so callers decide whether a missing tool is fatal.
pub fn run(program: &str, args: &[&str]) -> CmdResult {
    tracing::debug!(program, ?args, "exec");
    match Command::new(program).args(args).output() {
        Ok(out) => {
            let res = CmdResult {
                code: out.status.code(),
                stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
                spawned: true,
            };
            tracing::debug!(program, code = ?res.code, "exec done");
            res
        }
        Err(e) => {
            tracing::warn!(program, error = %e, "exec spawn failed");
            CmdResult {
                code: None,
                stdout: String::new(),
                stderr: e.to_string(),
                spawned: false,
            }
        }
    }
}

/// Convenience: run and report only whether it exited 0. Use for best-effort
/// commands where the output is not needed.
pub fn run_ok(program: &str, args: &[&str]) -> bool {
    run(program, args).success()
}

/// Run a command that MUST succeed; returns the captured result or an error
/// carrying the trimmed stderr. Use inside Required steps.
pub fn run_checked(program: &str, args: &[&str]) -> anyhow::Result<CmdResult> {
    let res = run(program, args);
    if res.success() {
        Ok(res)
    } else if !res.spawned {
        anyhow::bail!("`{program}` could not be spawned: {}", res.stderr.trim());
    } else {
        anyhow::bail!("`{program}` exited {:?}: {}", res.code, res.stderr.trim());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_captures_stdout_of_true_echo() {
        // `echo` is on PATH on every CI runner + dev host.
        let r = run("echo", &["hello"]);
        assert!(r.success());
        assert_eq!(r.stdout.trim(), "hello");
    }

    #[test]
    fn run_reports_spawn_failure_without_panicking() {
        let r = run("definitely-not-a-real-binary-xyz", &["--nope"]);
        assert!(!r.spawned);
        assert_eq!(r.code, None);
        assert!(!r.success());
    }

    #[test]
    fn run_checked_errors_on_nonzero() {
        // `false` exits 1.
        let err = run_checked("false", &[]).unwrap_err();
        assert!(err.to_string().contains("exited"));
    }
}
