//! Run a short command with a timeout and capture its output.

use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

/// How a [`run`] ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunOutcome {
    /// The command exited (`code` is `None` on a signal death).
    Exited {
        code: Option<i32>,
        stdout: String,
        stderr: String,
    },
    /// The command could not be started.
    SpawnFailed(String),
    /// Waiting on or reading from the command failed.
    WaitFailed(String),
    /// The timeout elapsed; the command was killed.
    TimedOut,
}

impl RunOutcome {
    /// True for an exit with status 0.
    pub fn success(&self) -> bool {
        matches!(self, Self::Exited { code: Some(0), .. })
    }

    /// The captured stdout, or `""` when the command did not exit.
    pub fn stdout(&self) -> &str {
        match self {
            Self::Exited { stdout, .. } => stdout,
            _ => "",
        }
    }
}

/// Run `program args` with `env` added, a null stdin and piped stdout and
/// stderr. The child is killed if `timeout` elapses (or the future is
/// dropped). Output is decoded lossily.
pub async fn run(
    program: &str,
    args: &[&str],
    env: &[(&str, &str)],
    timeout: Duration,
) -> RunOutcome {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .envs(env.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return RunOutcome::SpawnFailed(e.to_string()),
    };
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(out)) => RunOutcome::Exited {
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        },
        Ok(Err(e)) => RunOutcome::WaitFailed(e.to_string()),
        Err(_) => RunOutcome::TimedOut,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: Duration = Duration::from_secs(5);

    #[tokio::test]
    async fn captures_exit_code_and_both_streams() {
        let out = run("sh", &["-c", "printf out; printf err >&2; exit 3"], &[], T).await;
        assert_eq!(
            out,
            RunOutcome::Exited {
                code: Some(3),
                stdout: "out".into(),
                stderr: "err".into()
            }
        );
        assert!(!out.success());
    }

    #[tokio::test]
    async fn passes_env_and_reports_success() {
        let out = run("sh", &["-c", "printf \"$X\""], &[("X", "v")], T).await;
        assert!(out.success());
        assert_eq!(out.stdout(), "v");
    }

    #[tokio::test]
    async fn missing_program_is_a_spawn_failure() {
        let out = run("/nonexistent/ados-proc-test", &[], &[], T).await;
        assert!(matches!(out, RunOutcome::SpawnFailed(_)));
        assert_eq!(out.stdout(), "");
    }

    #[tokio::test]
    async fn a_slow_command_times_out() {
        let out = run("sleep", &["10"], &[], Duration::from_millis(100)).await;
        assert_eq!(out, RunOutcome::TimedOut);
    }
}
