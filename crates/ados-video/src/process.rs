//! Process-group-owned media subprocess.
//!
//! The video encoder is spawned as `bash -c "<encoder> | <bridge ffmpeg →
//! rtsp /main>"`, so the encoder and its publish-bridge child share one
//! process group. The Python predecessor reaped only the bash leader's PID on
//! a bare `.kill()`, orphaning the bridge ffmpeg onto the mediamtx `/main`
//! publisher slot → two publishers fought → black video (the v0.46.4 bug).
//!
//! [`ManagedProcess`] makes the fix structural:
//! - `setsid()` in the pre-exec hook makes the child its own process-group
//!   leader (PGID == PID);
//! - [`terminate`](ManagedProcess::terminate) does the graceful
//!   `killpg(SIGTERM)` → wait(grace) → `killpg(SIGKILL)` of the Python
//!   `_terminate_process_group`;
//! - `kill_on_drop` + a `Drop` `killpg` guarantee nothing outlives its owner
//!   even if a future is dropped mid-flight;
//! - [`kill_orphans`] is the pre-spawn `pgrep` sweep for a straggler left by a
//!   previously crashed run (which would otherwise hold the publisher slot).

use std::process::Stdio;
use std::time::Duration;

use tokio::process::{ChildStderr, ChildStdout, Command};

/// A media subprocess (encoder bash pipeline, wfb_tee ffmpeg, cloud-push
/// ffmpeg, mediamtx) running as its own process-group leader.
pub struct ManagedProcess {
    label: String,
    #[cfg(target_os = "linux")]
    pgid: nix::unistd::Pid,
    inner: tokio::process::Child,
}

impl ManagedProcess {
    /// Spawn `program args...` as a process-group leader. stdout is discarded;
    /// stderr is piped for the caller to drain (rate-limited). `label` is for
    /// log lines only.
    pub fn spawn(label: &str, program: &str, args: &[String]) -> std::io::Result<Self> {
        Self::spawn_with(label, program, args, Stdio::null())
    }

    /// Like [`spawn`](Self::spawn) but pipes stdout so the caller can read it —
    /// the vision-tap shim reads ffmpeg's rawvideo off stdout. Take the handle
    /// with [`take_stdout`](Self::take_stdout).
    pub fn spawn_capturing_stdout(
        label: &str,
        program: &str,
        args: &[String],
    ) -> std::io::Result<Self> {
        Self::spawn_with(label, program, args, Stdio::piped())
    }

    fn spawn_with(
        label: &str,
        program: &str,
        args: &[String],
        stdout: Stdio,
    ) -> std::io::Result<Self> {
        let mut cmd = Command::new(program);
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(Stdio::piped())
            // Cross-platform single-PID backstop; the Linux killpg in Drop is
            // the real group backstop.
            .kill_on_drop(true);

        #[cfg(target_os = "linux")]
        // Safety: setsid() is async-signal-safe and is the only call in the hook.
        unsafe {
            cmd.pre_exec(|| {
                nix::unistd::setsid().map_err(|e| std::io::Error::from_raw_os_error(e as i32))?;
                Ok(())
            });
        }

        let child = cmd.spawn()?;

        #[cfg(target_os = "linux")]
        let pgid = {
            let raw = child
                .id()
                .ok_or_else(|| std::io::Error::other("media child has no PID yet"))?;
            // After setsid the child leads its own group: PGID == PID.
            nix::unistd::Pid::from_raw(raw as i32)
        };

        Ok(Self {
            label: label.to_string(),
            #[cfg(target_os = "linux")]
            pgid,
            inner: child,
        })
    }

    /// Take the piped stdout handle (only present after
    /// [`spawn_capturing_stdout`](Self::spawn_capturing_stdout)).
    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.inner.stdout.take()
    }

    /// The OS PID (for `/proc/<pid>/...` reads + liveness checks).
    pub fn pid(&self) -> Option<u32> {
        self.inner.id()
    }

    /// The display label (for orchestrator log lines).
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Take the piped stderr handle (for the rate-limited drain task).
    pub fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.inner.stderr.take()
    }

    /// True while the process has not yet exited.
    pub fn is_running(&mut self) -> bool {
        matches!(self.inner.try_wait(), Ok(None))
    }

    /// Graceful teardown of the whole process group: `SIGTERM`, wait up to `grace`,
    /// then `SIGKILL`.
    #[cfg(target_os = "linux")]
    pub async fn terminate(&mut self, grace: Duration) {
        use nix::sys::signal::{killpg, Signal};
        let _ = killpg(self.pgid, Signal::SIGTERM);
        if tokio::time::timeout(grace, self.inner.wait())
            .await
            .is_err()
        {
            tracing::warn!(label = %self.label, "process group did not exit on SIGTERM; SIGKILL");
            let _ = killpg(self.pgid, Signal::SIGKILL);
            let _ = self.inner.wait().await;
        }
    }

    /// Non-Linux fallback: single-PID kill (no process groups in the test host).
    #[cfg(not(target_os = "linux"))]
    pub async fn terminate(&mut self, _grace: Duration) {
        let _ = self.inner.start_kill();
        let _ = self.inner.wait().await;
    }

    #[cfg(target_os = "linux")]
    fn killpg_now(&self) {
        use nix::sys::signal::{killpg, Signal};
        let _ = killpg(self.pgid, Signal::SIGKILL);
    }

    #[cfg(not(target_os = "linux"))]
    fn killpg_now(&self) {}
}

impl Drop for ManagedProcess {
    fn drop(&mut self) {
        self.killpg_now();
    }
}

/// Sweep orphaned subprocesses left by a previously crashed run: `pgrep -f --
/// <pattern>` and `SIGKILL` each match except our own PID. The `--` matters —
/// a pattern that starts with `-` (e.g. an ffmpeg `-i /dev/video0`) would
/// otherwise be parsed as a pgrep flag. Best-effort + idempotent; called
/// before every (re)spawn so a stale publisher can't fight the fresh one for
/// the mediamtx `/main` slot or the UDP 5600 wfb ingress.
pub async fn kill_orphans(pattern: &str) {
    let output = match Command::new("pgrep")
        .args(["-f", "--", pattern])
        .stderr(Stdio::null())
        .output()
        .await
    {
        Ok(o) => o,
        Err(_) => return, // pgrep missing → nothing to do
    };
    let me = std::process::id();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Ok(pid) = line.trim().parse::<i32>() else {
            continue;
        };
        if pid as u32 == me {
            continue;
        }
        kill_pid(pid);
    }
}

/// Sweep only the orphaned *publishers* to `uri`, leaving every process that
/// merely reads that stream alone.
///
/// Why this exists rather than `kill_orphans(uri)`: the encoder publishes the
/// local `main` RTSP stream and the radio tap, the cloud push, the vision tap
/// and the SEI tap all *consume* it, so all five carry the identical URI on
/// their command line. A `pgrep -f <uri>` sweep before an encoder respawn
/// therefore SIGKILLed the four consumers as well — which turned a routine
/// adaptive-bitrate step or attention switch into a radio, cloud, vision and
/// latency-probe outage, against the documented contract that a respawn costs
/// a sub-second encoder gap and nothing else.
///
/// Classification is [`cmdline_publishes_to`] over the candidate's real
/// `/proc/<pid>/cmdline`, and it fails CLOSED: a command line that cannot be
/// read or cannot be positively identified as a publisher is never signalled.
pub async fn kill_publisher_orphans(uri: &str) {
    let output = match Command::new("pgrep")
        .args(["-f", "--", uri])
        .stderr(Stdio::null())
        .output()
        .await
    {
        Ok(o) => o,
        Err(_) => return, // pgrep missing → nothing to do
    };
    let me = std::process::id();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Ok(pid) = line.trim().parse::<i32>() else {
            continue;
        };
        if pid as u32 == me {
            continue;
        }
        let Some(cmdline) = read_cmdline(pid).await else {
            continue;
        };
        if !cmdline_publishes_to(&cmdline, uri) {
            tracing::debug!(pid, "orphan_sweep_spared_reader");
            continue;
        }
        tracing::info!(pid, "orphan_sweep_killed_stale_publisher");
        kill_pid(pid);
    }
}

/// True when `cmdline` names `uri` as a muxer DESTINATION rather than reading
/// it as an input.
///
/// The URI string is byte-identical on both sides of the stream, so the only
/// discriminator is the syntax around it. Two structural facts do it, and
/// neither is a list of names that can rot:
///
///   * an ffmpeg **output** file is a bare positional — never the value of a
///     flag — while every consumer introduces the stream with one (`-i <uri>`
///     for the three ffmpeg taps, `--rtsp <uri>` for the Python SEI tap). So a
///     token starting with `-` immediately before the URI means reader,
///     whatever that flag is called;
///   * the command line must have declared an output muxer (`-f`) before the
///     URI, which every publish stage in this tree does — plain
///     (`-f rtsp … <uri>`) and `-f tee <branch-spec>` alike.
///
/// GStreamer is the one non-ffmpeg publisher and names its sink as an element
/// property, `rtspclientsink location=<uri>`.
///
/// It fails CLOSED: anything that matches none of these is treated as a reader
/// and survives. That is the safe direction — a surviving stale publisher is
/// refused the mediamtx slot and recovered by the health ladder, whereas a
/// wrongly-killed reader is the operator's radio picture, cloud feed or
/// detection stream going dark.
///
/// Tokenising on whitespace also covers the `bash -c "<encoder> | <publish>"`
/// pipeline arms: procfs joins argv with spaces, and the pipeline string's own
/// spaces split the same way, so the final publish stage is still seen.
pub fn cmdline_publishes_to(cmdline: &str, uri: &str) -> bool {
    let mut prev: Option<&str> = None;
    let mut saw_muxer_flag = false;
    for token in cmdline.split_whitespace() {
        if token.contains(uri) {
            if token.starts_with("location=") {
                return true;
            }
            let is_a_flags_value = prev.is_some_and(|p| p.starts_with('-'));
            if !is_a_flags_value && saw_muxer_flag {
                return true;
            }
        }
        if token == "-f" {
            saw_muxer_flag = true;
        }
        prev = Some(token);
    }
    false
}

/// The candidate's real command line, argv items joined by spaces.
///
/// procfs is the source of truth here rather than `pgrep -a`, whose `-a` flag
/// is procps-only. An unreadable entry (the process exited mid-sweep, or the
/// dev host has no procfs) yields `None`, and the caller treats that as
/// "do not signal".
async fn read_cmdline(pid: i32) -> Option<String> {
    let raw = tokio::fs::read(format!("/proc/{pid}/cmdline")).await.ok()?;
    let joined = String::from_utf8_lossy(&raw).replace('\0', " ");
    let trimmed = joined.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

#[cfg(target_os = "linux")]
fn kill_pid(pid: i32) {
    use nix::sys::signal::{kill, Signal};
    let _ = kill(nix::unistd::Pid::from_raw(pid), Signal::SIGKILL);
}

#[cfg(not(target_os = "linux"))]
fn kill_pid(_pid: i32) {
    // The orphan sweep only runs on the real (Linux) rig; on the dev host the
    // match is collected but not signalled (keeps tests side-effect-free).
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn spawn_then_terminate_stops_the_process() {
        // `sleep` exists on both Linux and the macOS dev host.
        let mut p = ManagedProcess::spawn("test-sleep", "sleep", &["30".into()]).unwrap();
        assert!(p.pid().is_some());
        assert!(p.is_running());
        p.terminate(Duration::from_secs(1)).await;
        // After terminate the child has been reaped → no longer running.
        assert!(!p.is_running());
    }

    #[tokio::test]
    async fn stderr_is_takeable_once() {
        let mut p = ManagedProcess::spawn("test-sleep", "sleep", &["30".into()]).unwrap();
        assert!(p.take_stderr().is_some());
        assert!(p.take_stderr().is_none());
        p.terminate(Duration::from_millis(500)).await;
    }

    #[tokio::test]
    async fn kill_orphans_no_match_is_noop() {
        // A pattern that matches nothing must not panic or error.
        kill_orphans("ados-no-such-orphan-pattern-xyzzy").await;
    }

    #[tokio::test]
    async fn kill_publisher_orphans_no_match_is_noop() {
        kill_publisher_orphans("rtsp://localhost:59999/ados-no-such-path-xyzzy").await;
    }

    #[test]
    fn an_unrecognised_shape_is_treated_as_a_reader() {
        // Fail-closed direction: a consumer this crate does not spawn today,
        // naming the stream through some other flag, must still survive the
        // sweep. Leaving a stale publisher alive costs a mediamtx slot refusal
        // the health ladder already recovers from; killing a reader costs the
        // operator their video.
        let uri = "rtsp://localhost:8554/main";
        for line in [
            format!("some-viewer --url {uri}"),
            format!("some-viewer --source {uri} --out /tmp/x"),
            format!("ffmpeg -f rtsp -i {uri} -f null -"),
            format!("python3 -m thing --stream {uri}"),
        ] {
            assert!(!cmdline_publishes_to(&line, uri), "must be spared: {line}");
        }
        // And the two publisher shapes are still caught.
        assert!(cmdline_publishes_to(
            &format!("ffmpeg -i /dev/video0 -f rtsp -rtsp_transport tcp {uri}"),
            uri
        ));
        assert!(cmdline_publishes_to(
            &format!("gst-launch-1.0 -e v4l2src ! rtspclientsink location={uri} protocols=tcp"),
            uri
        ));
    }
}
