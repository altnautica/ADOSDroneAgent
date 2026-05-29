//! Async command runner for the uplink managers.
//!
//! Mirrors the Python `_run(cmd, timeout) -> (rc, stdout, stderr)` tuple shape:
//! a timeout yields rc 124 (the `timeout(1)` convention), a spawn error yields
//! rc 1. Execution is abstracted behind [`CmdRunner`] so the ethernet and
//! wifi-client managers are unit-testable against a scripted fake instead of a
//! live `nmcli` / `systemctl`.

use std::time::Duration;

use async_trait::async_trait;
use tracing::warn;

/// Result of a command: exit code, captured stdout, captured stderr (decoded
/// lossily, never trimmed here so callers can match the Python `.strip()`
/// behavior at the call site).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CmdOut {
    pub rc: i32,
    pub stdout: String,
    pub stderr: String,
}

impl CmdOut {
    /// A spawn/timeout failure shape.
    pub fn failed(rc: i32, stderr: impl Into<String>) -> Self {
        Self {
            rc,
            stdout: String::new(),
            stderr: stderr.into(),
        }
    }

    pub fn ok(&self) -> bool {
        self.rc == 0
    }
}

/// Runs an external command with a timeout.
#[async_trait]
pub trait CmdRunner: Send + Sync {
    /// Run `argv` (program + args) with `timeout`. Never errors; failures are
    /// encoded in the returned [`CmdOut`].
    async fn run(&self, argv: &[&str], timeout: Duration) -> CmdOut;
}

/// Production runner over `tokio::process::Command`.
#[derive(Debug, Default, Clone, Copy)]
pub struct TokioCmdRunner;

#[async_trait]
impl CmdRunner for TokioCmdRunner {
    async fn run(&self, argv: &[&str], timeout: Duration) -> CmdOut {
        let Some((program, args)) = argv.split_first() else {
            return CmdOut::failed(1, "empty_argv");
        };
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(args);
        cmd.stdin(std::process::Stdio::null());
        let fut = cmd.output();
        match tokio::time::timeout(timeout, fut).await {
            Ok(Ok(out)) => CmdOut {
                rc: out.status.code().unwrap_or(-1),
                stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            },
            Ok(Err(exc)) => {
                warn!(program = program, error = %exc, "uplink.cmd_spawn_failed");
                CmdOut::failed(1, exc.to_string())
            }
            Err(_) => CmdOut::failed(124, "timeout"),
        }
    }
}

#[cfg(test)]
pub mod testing {
    //! A scripted command runner for manager unit tests.

    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// Records the argv of every call and replays queued responses in order.
    /// When the queue is empty it returns a zero-exit empty result so unmatched
    /// calls do not panic.
    #[derive(Default)]
    pub struct ScriptedRunner {
        responses: Mutex<VecDeque<CmdOut>>,
        pub calls: Mutex<Vec<Vec<String>>>,
    }

    impl ScriptedRunner {
        pub fn new() -> Self {
            Self::default()
        }

        /// Queue a response (FIFO).
        pub fn push(&self, out: CmdOut) {
            self.responses.lock().unwrap().push_back(out);
        }

        /// The recorded argv lists, in call order.
        pub fn recorded(&self) -> Vec<Vec<String>> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl CmdRunner for ScriptedRunner {
        async fn run(&self, argv: &[&str], _timeout: Duration) -> CmdOut {
            self.calls
                .lock()
                .unwrap()
                .push(argv.iter().map(|s| s.to_string()).collect());
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| CmdOut {
                    rc: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                })
        }
    }
}
