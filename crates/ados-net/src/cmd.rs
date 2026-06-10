//! Async command runner for the uplink managers.
//!
//! Mirrors the Python `_run(cmd, timeout) -> (rc, stdout, stderr)` tuple shape:
//! a timeout yields rc 124 (the `timeout(1)` convention), a spawn error yields
//! rc 1. Execution is abstracted behind [`CmdRunner`] so the ethernet and
//! wifi-client managers are unit-testable against a scripted fake instead of a
//! live `nmcli` / `systemctl`.

use std::time::Duration;

use async_trait::async_trait;
use tokio::io::AsyncWriteExt;
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

    /// Run `argv` with `stdin_data` piped to the child's stdin, with `timeout`.
    /// This keeps a secret (a WiFi passphrase) out of the argv vector so it
    /// never appears in `/proc/<pid>/cmdline`. The default forwards to
    /// [`run`](CmdRunner::run) with the data discarded, so a test fake that only
    /// records argv keeps working; the production runner overrides it to feed
    /// the bytes on stdin.
    async fn run_with_stdin(&self, argv: &[&str], stdin_data: &[u8], timeout: Duration) -> CmdOut {
        let _ = stdin_data;
        self.run(argv, timeout).await
    }
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

    async fn run_with_stdin(&self, argv: &[&str], stdin_data: &[u8], timeout: Duration) -> CmdOut {
        let Some((program, args)) = argv.split_first() else {
            return CmdOut::failed(1, "empty_argv");
        };
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(args);
        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(exc) => {
                warn!(program = program, error = %exc, "uplink.cmd_spawn_failed");
                return CmdOut::failed(1, exc.to_string());
            }
        };

        // Feed the secret on stdin (never in argv → never in /proc/<pid>/cmdline)
        // then drop the handle so the child sees EOF. A broken-pipe write (the
        // child exited before reading) is non-fatal; the wait below captures the
        // real outcome.
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(stdin_data).await;
            let _ = stdin.shutdown().await;
            // `stdin` drops here, closing the pipe so the child sees EOF.
        }

        match tokio::time::timeout(timeout, child.wait_with_output()).await {
            Ok(Ok(out)) => CmdOut {
                rc: out.status.code().unwrap_or(-1),
                stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            },
            Ok(Err(exc)) => {
                warn!(program = program, error = %exc, "uplink.cmd_wait_failed");
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
    /// calls do not panic. Calls made through [`CmdRunner::run_with_stdin`] also
    /// record the stdin bytes they were handed so a test can assert a secret
    /// travelled out of argv.
    #[derive(Default)]
    pub struct ScriptedRunner {
        responses: Mutex<VecDeque<CmdOut>>,
        pub calls: Mutex<Vec<Vec<String>>>,
        /// stdin bytes per call, in call order; an empty `Vec` for an argv-only
        /// `run` call.
        pub stdins: Mutex<Vec<Vec<u8>>>,
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

        /// The recorded stdin payloads, in call order.
        pub fn recorded_stdins(&self) -> Vec<Vec<u8>> {
            self.stdins.lock().unwrap().clone()
        }

        fn record(&self, argv: &[&str], stdin_data: &[u8]) -> CmdOut {
            self.calls
                .lock()
                .unwrap()
                .push(argv.iter().map(|s| s.to_string()).collect());
            self.stdins.lock().unwrap().push(stdin_data.to_vec());
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

    #[async_trait]
    impl CmdRunner for ScriptedRunner {
        async fn run(&self, argv: &[&str], _timeout: Duration) -> CmdOut {
            self.record(argv, &[])
        }

        async fn run_with_stdin(
            &self,
            argv: &[&str],
            stdin_data: &[u8],
            _timeout: Duration,
        ) -> CmdOut {
            self.record(argv, stdin_data)
        }
    }
}
