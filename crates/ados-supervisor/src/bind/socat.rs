//! socat key-transfer tunnel, process-group isolated.
//!
//! The Python predecessor (`_run_socat_with_kill_on_cancel`) spawned socat with
//! `asyncio.create_subprocess_exec` and relied on a `finally` `proc.kill()` to
//! reap it on cancel — single-PID, so the `EXEC:`-spawned shell child could
//! orphan. This port uses the same `setsid` + `killpg` RAII discipline as the
//! radio service's `WfbProcess`: the child is its own process-group leader, and
//! both an explicit `kill()` and `Drop` `killpg(SIGKILL)` the whole group, so a
//! dropped future (timeout / operator abort) can never leak the socat tree.

use std::process::Stdio;

use tokio::io::AsyncReadExt;
use tokio::process::Command;

use super::{BIND_TCP_PORT, DRONE_BIND_PEER_IP, WFB_BIND_CLIENT_SH, WFB_BIND_SERVER_SH};

/// Drone-side socat: listen on the tunnel rendezvous and hand the connection to
/// the upstream server wrapper. Mirrors `_run_drone_server`'s command exactly.
pub fn drone_server_args() -> Vec<String> {
    vec![
        "-d".into(),
        format!("TCP4-LISTEN:{BIND_TCP_PORT},bind={DRONE_BIND_PEER_IP},reuseaddr,crlf"),
        format!("EXEC:{WFB_BIND_SERVER_SH}"),
    ]
}

/// GS-side socat: connect to the drone's listener, handing off to the upstream
/// client wrapper. The connect retry is bounded to the key-transfer budget
/// (95 ≳ 90 s) rather than the predecessor's 24 h: a client that leaks past its
/// session (a missed process-group kill, an aborted window) must die on its own
/// instead of roaming for a day and phantom-connecting into a LATER drone bind
/// window — a half-dead connection EOFs the drone's listener conversation,
/// which exits 0 and used to mark that unrelated session Paired.
pub fn gs_client_args() -> Vec<String> {
    vec![
        "-d".into(),
        format!("TCP4:{DRONE_BIND_PEER_IP}:{BIND_TCP_PORT},crlf,retry=95,interval=1"),
        format!("EXEC:{WFB_BIND_CLIENT_SH}"),
    ]
}

/// A live socat child in its own process group.
pub struct SocatProcess {
    #[cfg(target_os = "linux")]
    pgid: nix::unistd::Pid,
    inner: tokio::process::Child,
}

impl SocatProcess {
    /// Spawn `socat <args>` as a process-group leader with stdout + stderr
    /// piped (both drained by [`run`](Self::run)).
    pub fn spawn(args: &[String]) -> std::io::Result<Self> {
        let mut cmd = Command::new("socat");
        cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());

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
                .ok_or_else(|| std::io::Error::other("socat child has no PID yet"))?;
            nix::unistd::Pid::from_raw(raw as i32)
        };

        Ok(Self {
            #[cfg(target_os = "linux")]
            pgid,
            inner: child,
        })
    }

    /// Drain stdout + stderr to EOF and wait for exit. Returns
    /// `(returncode, stdout, stderr)`; a signal death yields `-1` (non-zero) so
    /// the caller's `rc != 0` check trips, matching the Python contract. If the
    /// enclosing future is dropped before this resolves, [`Drop`] `killpg`s the
    /// group.
    pub async fn run(&mut self) -> std::io::Result<(i32, Vec<u8>, Vec<u8>)> {
        let mut out = self.inner.stdout.take();
        let mut err = self.inner.stderr.take();
        let out_fut = async {
            let mut buf = Vec::new();
            if let Some(s) = out.as_mut() {
                s.read_to_end(&mut buf).await?;
            }
            Ok::<_, std::io::Error>(buf)
        };
        let err_fut = async {
            let mut buf = Vec::new();
            if let Some(s) = err.as_mut() {
                s.read_to_end(&mut buf).await?;
            }
            Ok::<_, std::io::Error>(buf)
        };
        let (stdout, stderr, status) = tokio::join!(out_fut, err_fut, self.inner.wait());
        let rc = status?.code().unwrap_or(-1);
        Ok((rc, stdout?, stderr?))
    }

    #[cfg(target_os = "linux")]
    fn killpg_now(&self) {
        use nix::sys::signal::{self, Signal};
        let _ = signal::killpg(self.pgid, Signal::SIGKILL);
    }

    #[cfg(not(target_os = "linux"))]
    fn killpg_now(&self) {}
}

impl Drop for SocatProcess {
    fn drop(&mut self) {
        self.killpg_now();
    }
}

/// Sweep stale bind socats left by a previously aborted session (the drone
/// listener / gs client that would otherwise hold port 5555 on 10.5.99.2 and
/// crash the next attempt with `Address already in use`). Mirrors
/// `_kill_stale_bind_socats`: `pkill -9 -f <pattern>` for both shapes, each
/// bounded to 5s. Idempotent + best-effort.
pub async fn kill_stale_bind_socats() {
    let patterns = [
        format!("socat.*TCP4-LISTEN:{BIND_TCP_PORT},bind={DRONE_BIND_PEER_IP}"),
        format!("socat.*TCP4:{DRONE_BIND_PEER_IP}:{BIND_TCP_PORT}"),
    ];
    for pattern in patterns {
        let fut = Command::new("pkill")
            .args(["-9", "-f", &pattern])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), fut).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drone_server_args_match_python() {
        assert_eq!(
            drone_server_args(),
            vec![
                "-d".to_string(),
                "TCP4-LISTEN:5555,bind=10.5.99.2,reuseaddr,crlf".to_string(),
                "EXEC:/usr/bin/wfb_bind_server.sh".to_string(),
            ]
        );
    }

    #[test]
    fn gs_client_retry_is_bounded_to_the_session_budget() {
        assert_eq!(
            gs_client_args(),
            vec![
                "-d".to_string(),
                "TCP4:10.5.99.2:5555,crlf,retry=95,interval=1".to_string(),
                "EXEC:/usr/bin/wfb_bind_client.sh".to_string(),
            ]
        );
    }
}
