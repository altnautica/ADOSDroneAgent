//! Plain rate-limited stderr drain for the encoder / cloud-push / SEI-tap
//! subprocesses.
//!
//! These are the children that are NOT the wfb tap: they have no `-progress`
//! token to count (the encoder is the source; its liveness is asserted through
//! the mediamtx inbound-byte counter, not its own stderr). Attaching the
//! wfb-tee `ProgressTracker` to the encoder would conflate two independent
//! liveness signals — so these get this plain drain instead.
//!
//! The drain still matters for two reasons: an undrained stderr pipe fills at
//! 64 KB and blocks the child's next write (freezing it while it still looks
//! alive), and a child hammering a dead device (ffmpeg against a `/dev/video`
//! node that no longer opens) can emit tens of lines a second. We drain every
//! line so the pipe never deadlocks, but cap the logged output to a few lines
//! per window with a single suppressed-count summary — the same shape as the
//! Python `_drain_stderr`.

use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::ChildStderr;

/// At most this many real-diagnostic lines per [`DRAIN_WINDOW`] reach the log.
const DRAIN_MAX_LINES_PER_WINDOW: u32 = 5;
/// Rolling window for the rate limit.
const DRAIN_WINDOW: Duration = Duration::from_secs(10);

/// Drain `stderr` to completion, logging real diagnostics at `warn` up to the
/// per-window rate limit and summarising the rest. `label` identifies the child
/// in the log lines. Runs until the stream closes (the child exited or the
/// handle was dropped).
/// What one drain reported, so a caller (or a test) can assert on it without
/// having to capture a tracing subscriber.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DrainStats {
    /// Lines forwarded to the log.
    pub logged: u32,
    /// Lines dropped by the rate limit and reported only as a count.
    pub suppressed: u32,
}

pub async fn drain_plain(stderr: ChildStderr, label: &'static str) -> DrainStats {
    let mut lines = BufReader::new(stderr).lines();
    let mut window_start = Instant::now();
    let mut logged: u32 = 0;
    let mut total_logged: u32 = 0;
    let mut suppressed: u32 = 0;
    let mut last_suppressed_line = String::new();

    while let Ok(Some(raw)) = lines.next_line().await {
        let text = raw.trim_end();
        if text.is_empty() {
            continue;
        }
        let now = Instant::now();
        if now.duration_since(window_start) >= DRAIN_WINDOW {
            if suppressed > 0 {
                tracing::warn!(
                    label,
                    suppressed,
                    window_s = now.duration_since(window_start).as_secs_f64(),
                    last_line = %last_suppressed_line,
                    "subprocess_stderr_suppressed"
                );
            }
            window_start = now;
            logged = 0;
            suppressed = 0;
            last_suppressed_line.clear();
        }
        if logged < DRAIN_MAX_LINES_PER_WINDOW {
            tracing::warn!(label, line = %text, "subprocess_stderr");
            logged += 1;
            total_logged += 1;
        } else {
            suppressed += 1;
            last_suppressed_line = text.to_string();
        }
    }

    // Flush on stream close, not only when the next window opens.
    //
    // The summary used to be emitted exclusively at a window boundary, so a
    // child that died INSIDE its first window took its real error with it: the
    // stream closed, the loop ended, and everything past the rate limit was
    // simply never reported. That is the worst case to lose, because a
    // subprocess that fails immediately is failing at startup — the encoder
    // fault that needed manual reproduction to find had logged nothing but a
    // banner for exactly this reason.
    if suppressed > 0 {
        tracing::warn!(
            label,
            suppressed,
            window_s = window_start.elapsed().as_secs_f64(),
            last_line = %last_suppressed_line,
            "subprocess_stderr_suppressed"
        );
    }
    DrainStats {
        logged: total_logged,
        suppressed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::ManagedProcess;

    /// A child that dies inside its FIRST rate-limit window must still report
    /// what it suppressed.
    ///
    /// The summary used to be emitted only when the NEXT window opened, so a
    /// fast-failing subprocess took its real error with it: the stream closed,
    /// the loop ended, and everything past the rate limit was never reported.
    /// That is the worst case to lose — a child that fails immediately is
    /// failing at startup, which is exactly when its output matters most. An
    /// encoder fault that needed manual reproduction to find had logged nothing
    /// but a banner for this reason.
    #[tokio::test]
    async fn a_child_dying_in_its_first_window_still_reports_what_it_suppressed() {
        let over = DRAIN_MAX_LINES_PER_WINDOW + 12;
        let mut child = ManagedProcess::spawn(
            "drain-test",
            "sh",
            &[
                "-c".to_string(),
                format!(
                    "i=0; while [ $i -lt {over} ]; do echo boom$i 1>&2; i=$((i+1)); done; exit 1"
                ),
            ],
        )
        .expect("spawn the fast-failing child");
        let stderr = child.take_stderr().expect("stderr is piped");

        let stats = drain_plain(stderr, "test").await;

        assert_eq!(
            stats.logged, DRAIN_MAX_LINES_PER_WINDOW,
            "the rate limit still holds"
        );
        assert!(
            stats.suppressed > 0,
            "the child outran the rate limit and died in the same window, so the \
             overflow must be accounted for rather than silently lost"
        );
        assert_eq!(stats.logged + stats.suppressed, over);
    }

    #[test]
    fn rate_limit_constants() {
        assert_eq!(DRAIN_MAX_LINES_PER_WINDOW, 5);
        assert_eq!(DRAIN_WINDOW, Duration::from_secs(10));
    }

    #[tokio::test]
    async fn drains_a_flood_to_completion() {
        // A flood of diagnostics must be drained without panicking; the rate
        // limit is internal, the contract is that the drain consumes the whole
        // stream (so the pipe never deadlocks).
        let mut script = String::new();
        for i in 0..30 {
            script.push_str(&format!("echo 'diag line {i}' >&2\n"));
        }
        let mut p = ManagedProcess::spawn("test-drain", "bash", &["-c".into(), script]).unwrap();
        let stderr = p.take_stderr().unwrap();
        drain_plain(stderr, "test").await;
        p.terminate(Duration::from_millis(200)).await;
    }
}
