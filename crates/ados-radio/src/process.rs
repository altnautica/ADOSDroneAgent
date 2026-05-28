//! wfb subprocess manager with process-group isolation.
//!
//! Owns the three radio C subprocesses the WFB TX service forks:
//!   - **data plane** `wfb_tx -p 0 -u 5600 …` (video) (`manager.py:479-504`)
//!   - **tx control** `wfb_tx -p 1 -u 5803 -k 1 -n 2 …` — carries HopAnnounce /
//!     PresenceBeacon OVER THE AIR (`manager.py:547-602`)
//!   - **rx control** `wfb_rx -p 1 -c 127.0.0.1 -u 5810 -l 1000 …` — receives
//!     HopAck off the air onto UDP 5810 (`manager.py:604-646`)
//!
//! The Python predecessor used `asyncio.create_subprocess_exec()` with no
//! `setsid`/`killpg`, so a `terminate()` that raised or timed out left the C
//! binary an orphan in the agent's cgroup — the v0.46.4 bug. This module fixes
//! that structurally:
//! - `setsid()` in the child's pre-exec hook makes the child its own process
//!   group leader (PGID == PID).
//! - `kill()` calls `killpg(pgid, SIGKILL)` — the whole group dies atomically.
//! - `Drop` also calls `killpg` so a process never outlives its Rust owner.

use std::path::Path;

use crate::config::WfbConfig;

const TX_CONTROL_LOG: &str = "/run/ados/wfb-drone-tx-control.log";
const RX_CONTROL_LOG: &str = "/run/ados/wfb-drone-rx-control.log";

fn key_str(key_path: &Path) -> String {
    key_path
        .to_str()
        .unwrap_or("/etc/ados/wfb/tx.key")
        .to_string()
}

/// Data-plane `wfb_tx` args (radio_id 0, UDP 5600, video FEC k/n from config).
pub fn data_tx_args(iface: &str, cfg: &WfbConfig, key_path: &Path) -> Vec<String> {
    vec![
        "-p".into(),
        "0".into(),
        "-u".into(),
        "5600".into(),
        "-K".into(),
        key_str(key_path),
        "-k".into(),
        cfg.fec_k.to_string(),
        "-n".into(),
        cfg.fec_n.to_string(),
        "-B".into(),
        "20".into(),
        "-M".into(),
        cfg.mcs_index.to_string(),
        iface.into(),
    ]
}

/// Control-plane `wfb_tx` args (radio_id 1, UDP 5803, light FEC k=1/n=2).
pub fn tx_control_args(iface: &str, cfg: &WfbConfig, key_path: &Path) -> Vec<String> {
    vec![
        "-p".into(),
        "1".into(),
        "-u".into(),
        "5803".into(),
        "-K".into(),
        key_str(key_path),
        "-k".into(),
        "1".into(),
        "-n".into(),
        "2".into(),
        "-B".into(),
        "20".into(),
        "-M".into(),
        cfg.mcs_index.to_string(),
        iface.into(),
    ]
}

/// Control-plane `wfb_rx` args (radio_id 1, re-emit HopAck on 127.0.0.1:5810).
pub fn rx_control_args(iface: &str, key_path: &Path) -> Vec<String> {
    vec![
        "-p".into(),
        "1".into(),
        "-c".into(),
        "127.0.0.1".into(),
        "-u".into(),
        "5810".into(),
        "-K".into(),
        key_str(key_path),
        "-l".into(),
        "1000".into(),
        iface.into(),
    ]
}

/// Data-plane stats `wfb_rx` args (radio_id 0). `-l 1000` emits the per-second
/// PKT/RX_ANT stats lines the link-quality monitor parses. The decoded payload
/// goes to **127.0.0.1:5601** — deliberately NOT 5600 (the data-plane TX's
/// video ingress) so the stats receiver can never inject into the video path.
/// Uses the **rx** key (decrypts the GS uplink).
pub fn stats_rx_args(iface: &str, rx_key_path: &Path) -> Vec<String> {
    vec![
        "-p".into(),
        "0".into(),
        "-c".into(),
        "127.0.0.1".into(),
        "-u".into(),
        "5601".into(),
        "-K".into(),
        key_str(rx_key_path),
        "-l".into(),
        "1000".into(),
        iface.into(),
    ]
}

/// A live wfb child (data or control plane) in its own process group.
pub struct WfbProcess {
    #[cfg(target_os = "linux")]
    pgid: nix::unistd::Pid,
    inner: tokio::process::Child,
}

impl WfbProcess {
    /// Spawn the **data-plane** `wfb_tx`. stderr is piped (drained by the
    /// caller); the Rule-37 watchdog reads `/proc/<pid>/io` + iface stats.
    pub async fn spawn_data_tx(
        iface: &str,
        cfg: &WfbConfig,
        key_path: &Path,
    ) -> std::io::Result<Self> {
        Self::spawn_in_group("wfb_tx", &data_tx_args(iface, cfg, key_path), None).await
    }

    /// Spawn the **tx-control** `wfb_tx` (over-the-air HopAnnounce/PresenceBeacon
    /// transport). stderr → truncated log file (avoids the PIPE deadlock).
    pub async fn spawn_tx_control(
        iface: &str,
        cfg: &WfbConfig,
        key_path: &Path,
    ) -> std::io::Result<Self> {
        Self::spawn_in_group(
            "wfb_tx",
            &tx_control_args(iface, cfg, key_path),
            Some(TX_CONTROL_LOG),
        )
        .await
    }

    /// Spawn the **rx-control** `wfb_rx` (receives HopAck off the air → 5810).
    pub async fn spawn_rx_control(iface: &str, key_path: &Path) -> std::io::Result<Self> {
        Self::spawn_in_group(
            "wfb_rx",
            &rx_control_args(iface, key_path),
            Some(RX_CONTROL_LOG),
        )
        .await
    }

    /// Spawn the **stats** `wfb_rx` (data plane, port 5601) with stdout PIPED so
    /// the caller can read the per-second PKT/RX_ANT stats lines.
    pub async fn spawn_stats_rx(iface: &str, rx_key_path: &Path) -> std::io::Result<Self> {
        // stderr → null (we only want stdout's stats stream); stdout piped.
        Self::spawn_in_group_piped_stdout("wfb_rx", &stats_rx_args(iface, rx_key_path)).await
    }

    /// Take the child's stdout handle (for the stats reader). Returns `None` if
    /// stdout was not piped or already taken.
    pub fn take_stdout(&mut self) -> Option<tokio::process::ChildStdout> {
        self.inner.stdout.take()
    }

    /// Spawn `program` with `args` as a process-group leader (setsid). When
    /// `stderr_log` is `Some`, stderr is redirected to that file (truncated);
    /// otherwise stderr is piped for the caller to drain. stdout is always
    /// discarded (PKT-stats would fill the pipe).
    async fn spawn_in_group(
        program: &str,
        args: &[String],
        stderr_log: Option<&str>,
    ) -> std::io::Result<Self> {
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(args).stdout(std::process::Stdio::null());

        match stderr_log {
            Some(path) => {
                let file = std::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(path)?;
                cmd.stderr(std::process::Stdio::from(file));
            }
            None => {
                cmd.stderr(std::process::Stdio::piped());
            }
        }

        Self::finish_spawn(cmd)
    }

    /// Like [`spawn_in_group`] but pipes stdout (for the stats reader) and
    /// discards stderr. setsid + killpg discipline is identical.
    async fn spawn_in_group_piped_stdout(program: &str, args: &[String]) -> std::io::Result<Self> {
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        Self::finish_spawn(cmd)
    }

    /// Apply the setsid pre-exec hook, spawn, and capture the process group.
    fn finish_spawn(mut cmd: tokio::process::Command) -> std::io::Result<Self> {
        // Move the child into its own session so killpg later kills it cleanly.
        #[cfg(target_os = "linux")]
        // Safety: setsid() is async-signal-safe and is the only call in this hook.
        unsafe {
            cmd.pre_exec(|| {
                nix::unistd::setsid().map_err(|e| std::io::Error::from_raw_os_error(e as i32))?;
                Ok(())
            });
        }

        let child = cmd.spawn()?;
        #[cfg(target_os = "linux")]
        let pgid = {
            let raw_pid = child
                .id()
                .ok_or_else(|| std::io::Error::other("wfb child has no PID yet"))?;
            // After setsid the child is its own process group leader: PGID == PID.
            nix::unistd::Pid::from_raw(raw_pid as i32)
        };

        Ok(Self {
            #[cfg(target_os = "linux")]
            pgid,
            inner: child,
        })
    }

    /// True if the process has not yet exited.
    pub fn is_running(&mut self) -> bool {
        matches!(self.inner.try_wait(), Ok(None))
    }

    /// The OS PID, for reading `/proc/<pid>/io`.
    pub fn pid(&self) -> Option<u32> {
        self.inner.id()
    }

    /// Kill the entire process group and wait for the child to exit.
    pub async fn kill(&mut self) {
        self.killpg_now();
        let _ = self.inner.wait().await;
    }

    #[cfg(target_os = "linux")]
    fn killpg_now(&self) {
        use nix::sys::signal::{self, Signal};
        let _ = signal::killpg(self.pgid, Signal::SIGKILL);
    }

    #[cfg(not(target_os = "linux"))]
    fn killpg_now(&self) {
        // No-op on non-Linux.
    }
}

impl Drop for WfbProcess {
    fn drop(&mut self) {
        self.killpg_now();
    }
}

/// The drone's radio subprocesses, spawned and torn down in lock-step. The
/// control plane MUST restart with the data plane on every channel hop so
/// HopAnnounce/HopAck keep flowing on the new channel; the stats RX likewise
/// follows the channel.
pub struct RadioProcesses {
    pub data_tx: WfbProcess,
    pub tx_control: WfbProcess,
    pub rx_control: WfbProcess,
    /// Data-plane stats RX (only when an rx key is present). Drives link stats.
    stats_rx: Option<WfbProcess>,
    /// The task reading the stats RX stdout into the shared `LinkStats`.
    stats_reader: Option<tokio::task::JoinHandle<()>>,
}

impl RadioProcesses {
    /// Spawn the data plane + both control planes, and (when `/etc/ados/wfb/rx.key`
    /// exists) the stats RX with a reader task that updates `link` from the
    /// `wfb_rx` stats stream.
    pub async fn spawn(
        iface: &str,
        cfg: &WfbConfig,
        key_path: &Path,
        link: std::sync::Arc<tokio::sync::Mutex<crate::link_quality::LinkStats>>,
    ) -> std::io::Result<Self> {
        let data_tx = WfbProcess::spawn_data_tx(iface, cfg, key_path).await?;
        let tx_control = WfbProcess::spawn_tx_control(iface, cfg, key_path).await?;
        let rx_control = WfbProcess::spawn_rx_control(iface, key_path).await?;

        // Stats RX is best-effort + gated on the rx key (the GS-uplink decryptor).
        // Without it the link block stays at default sentinels — same as Python.
        let (stats_rx, stats_reader) = if Path::new(crate::paths::WFB_RX_KEY).exists() {
            match WfbProcess::spawn_stats_rx(iface, Path::new(crate::paths::WFB_RX_KEY)).await {
                Ok(mut p) => {
                    let stdout = p.take_stdout();
                    let reader = stdout.map(|out| tokio::spawn(stats_reader_loop(out, link)));
                    (Some(p), reader)
                }
                Err(e) => {
                    tracing::warn!(error = %e, "stats_rx_spawn_failed");
                    (None, None)
                }
            }
        } else {
            (None, None)
        };

        Ok(Self {
            data_tx,
            tx_control,
            rx_control,
            stats_rx,
            stats_reader,
        })
    }

    /// The data-plane PID, for the Rule-37 TX watchdog.
    pub fn data_tx_pid(&self) -> Option<u32> {
        self.data_tx.pid()
    }

    /// Kill every process group + stop the stats reader.
    pub async fn kill_all(&mut self) {
        if let Some(r) = self.stats_reader.take() {
            r.abort();
        }
        self.data_tx.kill().await;
        self.tx_control.kill().await;
        self.rx_control.kill().await;
        if let Some(mut s) = self.stats_rx.take() {
            s.kill().await;
        }
    }
}

/// Read `wfb_rx` stdout line-by-line, feed the link-quality monitor, and update
/// the shared `LinkStats` the sidecar + reactive-hop logic read. Ends on EOF
/// (process death) or task abort.
async fn stats_reader_loop(
    stdout: tokio::process::ChildStdout,
    link: std::sync::Arc<tokio::sync::Mutex<crate::link_quality::LinkStats>>,
) {
    use tokio::io::AsyncBufReadExt;
    let mut lines = tokio::io::BufReader::new(stdout).lines();
    let mut mon = crate::link_quality::LinkQualityMonitor::new();
    while let Ok(Some(line)) = lines.next_line().await {
        let now_iso = now_iso();
        if let Some(stats) = mon.feed_line(&line, &now_iso) {
            *link.lock().await = stats;
        }
    }
}

/// Current ISO-8601 UTC timestamp for the link-stats `timestamp` field.
fn now_iso() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_tx_args_match_python() {
        let cfg = WfbConfig::default();
        let a = data_tx_args("wlan1", &cfg, Path::new("/etc/ados/wfb/tx.key"));
        assert_eq!(
            a,
            vec![
                "-p",
                "0",
                "-u",
                "5600",
                "-K",
                "/etc/ados/wfb/tx.key",
                "-k",
                "8",
                "-n",
                "12",
                "-B",
                "20",
                "-M",
                "1",
                "wlan1"
            ]
        );
    }

    #[test]
    fn tx_control_args_match_python() {
        // wfb_tx -p 1 -u 5803 -K <key> -k 1 -n 2 -B 20 -M <mcs> <iface>
        let cfg = WfbConfig::default();
        let a = tx_control_args("wlan1", &cfg, Path::new("/etc/ados/wfb/tx.key"));
        assert_eq!(
            a,
            vec![
                "-p",
                "1",
                "-u",
                "5803",
                "-K",
                "/etc/ados/wfb/tx.key",
                "-k",
                "1",
                "-n",
                "2",
                "-B",
                "20",
                "-M",
                "1",
                "wlan1"
            ]
        );
    }

    #[test]
    fn rx_control_args_match_python() {
        // wfb_rx -p 1 -c 127.0.0.1 -u 5810 -K <key> -l 1000 <iface>
        let a = rx_control_args("wlan1", Path::new("/etc/ados/wfb/tx.key"));
        assert_eq!(
            a,
            vec![
                "-p",
                "1",
                "-c",
                "127.0.0.1",
                "-u",
                "5810",
                "-K",
                "/etc/ados/wfb/tx.key",
                "-l",
                "1000",
                "wlan1"
            ]
        );
    }

    #[test]
    fn control_planes_use_lighter_fec_than_data() {
        let cfg = WfbConfig::default();
        let data = data_tx_args("wlan1", &cfg, Path::new("/k"));
        let ctrl = tx_control_args("wlan1", &cfg, Path::new("/k"));
        // data plane: k=8 n=12; control plane: k=1 n=2.
        let data_k = data[data.iter().position(|x| x == "-k").unwrap() + 1].clone();
        let ctrl_k = ctrl[ctrl.iter().position(|x| x == "-k").unwrap() + 1].clone();
        assert_eq!(data_k, "8");
        assert_eq!(ctrl_k, "1");
    }
}
