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

use std::path::{Path, PathBuf};

use crate::config::WfbConfig;

const TX_CONTROL_LOG: &str = "/run/ados/wfb-drone-tx-control.log";
const RX_CONTROL_LOG: &str = "/run/ados/wfb-drone-rx-control.log";

/// True when a Reed-Solomon `(k, n)` ratio is valid for `wfb_tx`: a positive
/// data-shard count and at least one parity shard (`n > k`). Mirrors the Python
/// `set_fec` guard `fec_k <= 0 or fec_n <= fec_k`.
pub fn fec_ratio_valid(fec_k: u8, fec_n: u8) -> bool {
    fec_k != 0 && fec_n > fec_k
}

/// True when an MCS index is in the accepted RTL8812EU VHT80 range (0..=7).
pub fn mcs_index_valid(mcs: u8) -> bool {
    mcs <= 7
}

/// Build the data-plane config for a whole-group respawn: the boot-time `cfg`
/// with the data-plane FEC/MCS trio overlaid from the live retained tunables
/// (`data_fec_k`/`data_fec_n`/`data_mcs_index`). Keeps every other field (iface,
/// band, channel, ports, power) from the boot config so only the data tier is
/// preserved across a hop — the operator's pinned manual tier or the adaptive
/// FEC/MCS is not reverted to the boot defaults. Pure so a respawn can assert the
/// retained trio reaches the data-plane args without spawning a real process.
fn data_cfg_from_retained(cfg: &WfbConfig, fec_k: u8, fec_n: u8, mcs_index: u8) -> WfbConfig {
    WfbConfig {
        fec_k,
        fec_n,
        mcs_index,
        ..cfg.clone()
    }
}

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
    /// The interface + key + current data-plane FEC/MCS, retained so a single
    /// data-tx process can be respawned with new tunables without touching the
    /// control planes (an adaptive FEC/MCS change restarts only the data plane).
    iface: String,
    tx_key_path: PathBuf,
    data_fec_k: u8,
    data_fec_n: u8,
    data_mcs_index: u8,
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
            iface: iface.to_string(),
            tx_key_path: key_path.to_path_buf(),
            data_fec_k: cfg.fec_k,
            data_fec_n: cfg.fec_n,
            data_mcs_index: cfg.mcs_index,
        })
    }

    /// The data-plane PID, for the Rule-37 TX watchdog.
    pub fn data_tx_pid(&self) -> Option<u32> {
        self.data_tx.pid()
    }

    /// True while the data-plane `wfb_tx` has not exited. A cheap `try_wait`
    /// reap (it never blocks), so an exit-watch task can poll it on a short
    /// interval to catch a self-crashed transmitter immediately rather than
    /// waiting out the 30 s counter watchdog.
    pub fn data_tx_running(&mut self) -> bool {
        self.data_tx.is_running()
    }

    /// The data plane's currently-running Reed-Solomon `(k, n)` ratio.
    pub fn data_fec(&self) -> (u8, u8) {
        (self.data_fec_k, self.data_fec_n)
    }

    /// The data plane's currently-running MCS index.
    pub fn data_mcs(&self) -> u8 {
        self.data_mcs_index
    }

    /// Apply a new Reed-Solomon `(k, n)` ratio to the live data plane.
    ///
    /// `wfb_tx` has no runtime FEC knob, so the only correct application is to
    /// kill the data-tx process and respawn it with the new `-k`/`-n` args. The
    /// two control planes and the stats RX carry their own fixed FEC and are
    /// left running, so an FEC change does not interrupt HopAnnounce/HopAck or
    /// the link-quality stream. Returns `false` on an invalid ratio or a respawn
    /// failure; on a respawn failure the previous data tunables are restored in
    /// the retained state so a later respawn does not silently keep the rejected
    /// values, and the data plane is left dead for the supervisor to restart the
    /// whole group (the same fail-safe path the watchdog kills take). A no-op
    /// when the ratio already matches the running data plane.
    pub async fn set_fec(&mut self, fec_k: u8, fec_n: u8) -> bool {
        if !fec_ratio_valid(fec_k, fec_n) {
            tracing::warn!(k = fec_k, n = fec_n, "set_fec_invalid");
            return false;
        }
        if fec_k == self.data_fec_k && fec_n == self.data_fec_n {
            return true;
        }
        let old_k = self.data_fec_k;
        let old_n = self.data_fec_n;
        self.data_fec_k = fec_k;
        self.data_fec_n = fec_n;
        if self.respawn_data_tx().await {
            tracing::info!(k = fec_k, n = fec_n, "set_fec_applied");
            true
        } else {
            self.data_fec_k = old_k;
            self.data_fec_n = old_n;
            tracing::warn!(k = fec_k, n = fec_n, "set_fec_respawn_failed");
            false
        }
    }

    /// Apply a new MCS index to the live data plane (same restart-on-change path
    /// as [`set_fec`](Self::set_fec)). The accepted range is 0..=7 (the
    /// RTL8812EU VHT80 range); `wfb_tx` rejects anything wider. A no-op when the
    /// index already matches the running data plane.
    pub async fn set_mcs(&mut self, mcs: u8) -> bool {
        if !mcs_index_valid(mcs) {
            tracing::warn!(mcs, "set_mcs_out_of_range");
            return false;
        }
        if mcs == self.data_mcs_index {
            return true;
        }
        let old_mcs = self.data_mcs_index;
        self.data_mcs_index = mcs;
        if self.respawn_data_tx().await {
            tracing::info!(mcs, "set_mcs_applied");
            true
        } else {
            self.data_mcs_index = old_mcs;
            tracing::warn!(mcs, "set_mcs_respawn_failed");
            false
        }
    }

    /// Pin a full manual link tier — the `(mcs_index, fec_k, fec_n)` trio — onto
    /// the live data plane in a single respawn.
    ///
    /// This is the manual half of the auto/manual tier control: the operator
    /// fixes the radio rate + redundancy and the adaptive controller is held off
    /// (the caller disables it). Applying the trio together means one data-plane
    /// restart for all three knobs instead of three. Validates the FEC ratio and
    /// the MCS range up front; on an invalid input it changes nothing and returns
    /// `false`. A no-op (returns `true` without a restart) when the trio already
    /// matches the running data plane. On a respawn failure the previous trio is
    /// restored in the retained state and the data plane is left dead for the
    /// supervisor to restart the whole group.
    pub async fn set_manual_tier(&mut self, mcs: u8, fec_k: u8, fec_n: u8) -> bool {
        if !mcs_index_valid(mcs) {
            tracing::warn!(mcs, "set_manual_tier_mcs_out_of_range");
            return false;
        }
        if !fec_ratio_valid(fec_k, fec_n) {
            tracing::warn!(k = fec_k, n = fec_n, "set_manual_tier_fec_invalid");
            return false;
        }
        if mcs == self.data_mcs_index && fec_k == self.data_fec_k && fec_n == self.data_fec_n {
            return true;
        }
        let (old_mcs, old_k, old_n) = (self.data_mcs_index, self.data_fec_k, self.data_fec_n);
        self.data_mcs_index = mcs;
        self.data_fec_k = fec_k;
        self.data_fec_n = fec_n;
        if self.respawn_data_tx().await {
            tracing::info!(mcs, k = fec_k, n = fec_n, "set_manual_tier_applied");
            true
        } else {
            self.data_mcs_index = old_mcs;
            self.data_fec_k = old_k;
            self.data_fec_n = old_n;
            tracing::warn!(mcs, k = fec_k, n = fec_n, "set_manual_tier_respawn_failed");
            false
        }
    }

    /// Apply a TX power (dBm) to the live adapter via the kernel without a
    /// respawn — `iw dev <iface> set txpower` retunes the running radio in place.
    /// Returns the effective dBm the driver accepted (it can ramp UP from a
    /// rejected low request), or `None` when every ramp step was rejected. The
    /// retained iface is the same one the control planes are injecting on, so the
    /// power change reaches the whole radio group at once.
    pub async fn apply_tx_power(&self, dbm: i8) -> Option<i8> {
        crate::adapter::set_tx_power(&self.iface, dbm).await
    }

    /// Kill the whole radio group and respawn it, REUSING the live data-plane
    /// tunables (`data_fec_k`/`data_fec_n`/`data_mcs_index`) rather than the
    /// boot-time `cfg` values. A channel hop / return-home restarts the entire
    /// group (data + both control planes follow the channel), and the naive path
    /// spawned from `cfg` alone — silently reverting any operator-pinned manual
    /// link tier or adaptive FEC/MCS the data plane had applied. This rebuilds the
    /// data plane from the retained trio and keeps the control planes on the
    /// boot-time control rate, so a hop preserves the running data tier.
    ///
    /// Returns `false` if the group respawn fails (the radio group is then dead;
    /// the supervisor's outer loop respawns from scratch).
    pub async fn respawn_group(
        &mut self,
        cfg: &WfbConfig,
        link: std::sync::Arc<tokio::sync::Mutex<crate::link_quality::LinkStats>>,
    ) -> bool {
        self.kill_all().await;
        // The data plane spawns from the retained trio; the control planes keep
        // the boot-time control rate (their own fixed FEC + the management MCS).
        let data_cfg =
            data_cfg_from_retained(cfg, self.data_fec_k, self.data_fec_n, self.data_mcs_index);
        let key_path = self.tx_key_path.clone();
        let data_tx = match WfbProcess::spawn_data_tx(&self.iface, &data_cfg, &key_path).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "respawn_group_data_tx_failed");
                return false;
            }
        };
        let tx_control = match WfbProcess::spawn_tx_control(&self.iface, cfg, &key_path).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "respawn_group_tx_control_failed");
                return false;
            }
        };
        let rx_control = match WfbProcess::spawn_rx_control(&self.iface, &key_path).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "respawn_group_rx_control_failed");
                return false;
            }
        };
        let (stats_rx, stats_reader) = if Path::new(crate::paths::WFB_RX_KEY).exists() {
            match WfbProcess::spawn_stats_rx(&self.iface, Path::new(crate::paths::WFB_RX_KEY)).await
            {
                Ok(mut p) => {
                    let stdout = p.take_stdout();
                    let reader = stdout.map(|out| tokio::spawn(stats_reader_loop(out, link)));
                    (Some(p), reader)
                }
                Err(e) => {
                    tracing::warn!(error = %e, "respawn_group_stats_rx_failed");
                    (None, None)
                }
            }
        } else {
            (None, None)
        };
        self.data_tx = data_tx;
        self.tx_control = tx_control;
        self.rx_control = rx_control;
        self.stats_rx = stats_rx;
        self.stats_reader = stats_reader;
        true
    }

    /// Kill ONLY the data-tx process and respawn it from the retained iface/key
    /// and current data tunables. Leaves the control planes + stats RX running.
    /// Returns `false` if the respawn fails (the data plane is then dead).
    async fn respawn_data_tx(&mut self) -> bool {
        let cfg = WfbConfig {
            fec_k: self.data_fec_k,
            fec_n: self.data_fec_n,
            mcs_index: self.data_mcs_index,
            ..WfbConfig::default()
        };
        self.data_tx.kill().await;
        match WfbProcess::spawn_data_tx(&self.iface, &cfg, &self.tx_key_path).await {
            Ok(p) => {
                self.data_tx = p;
                true
            }
            Err(e) => {
                tracing::warn!(error = %e, "data_tx_respawn_failed");
                false
            }
        }
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
    fn respawn_data_cfg_reuses_retained_tunables_not_boot_config() {
        // Boot config carries the default tier (k=8, n=12, mcs=1). The live data
        // plane has since moved to a manual / adaptive tier (k=4, n=8, mcs=5). A
        // whole-group respawn (a channel hop) must spawn the data plane from the
        // RETAINED tier, not silently revert it to the boot config.
        let boot = WfbConfig {
            fec_k: 8,
            fec_n: 12,
            mcs_index: 1,
            channel: 149,
            ..WfbConfig::default()
        };
        let data_cfg = data_cfg_from_retained(&boot, 4, 8, 5);
        // The data tier is the retained trio.
        assert_eq!(data_cfg.fec_k, 4);
        assert_eq!(data_cfg.fec_n, 8);
        assert_eq!(data_cfg.mcs_index, 5);
        // Everything else still comes from the boot config (channel preserved).
        assert_eq!(data_cfg.channel, 149);
        // The data-plane args carry the retained trio, not the boot defaults.
        let args = data_tx_args("wlan1", &data_cfg, Path::new("/etc/ados/wfb/tx.key"));
        let pos = |flag: &str| {
            args.iter()
                .position(|a| a == flag)
                .map(|i| args[i + 1].clone())
        };
        assert_eq!(pos("-k").as_deref(), Some("4"));
        assert_eq!(pos("-n").as_deref(), Some("8"));
        assert_eq!(pos("-M").as_deref(), Some("5"));
    }

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
    fn fec_ratio_guard_matches_python() {
        // Valid: positive k with at least one parity shard.
        assert!(fec_ratio_valid(8, 12));
        assert!(fec_ratio_valid(8, 10));
        assert!(fec_ratio_valid(1, 2));
        // k = 0 is rejected (mirrors `fec_k <= 0`).
        assert!(!fec_ratio_valid(0, 4));
        // n <= k is rejected (no parity shards).
        assert!(!fec_ratio_valid(8, 8));
        assert!(!fec_ratio_valid(8, 4));
    }

    #[test]
    fn mcs_guard_accepts_0_through_7() {
        for mcs in 0..=7u8 {
            assert!(mcs_index_valid(mcs), "mcs {mcs} should be valid");
        }
        assert!(!mcs_index_valid(8));
        assert!(!mcs_index_valid(255));
    }

    #[test]
    fn data_tx_args_track_set_fec_inputs() {
        // The data-tx args must carry whatever (k, n, mcs) trio is asked for, so
        // a respawn after set_fec/set_mcs injects the new tunables. This proves
        // the arg wiring the respawn relies on, without spawning a process.
        let cfg = WfbConfig {
            fec_k: 8,
            fec_n: 10,
            mcs_index: 5,
            ..WfbConfig::default()
        };
        let a = data_tx_args("wlan1", &cfg, Path::new("/etc/ados/wfb/tx.key"));
        let k = a[a.iter().position(|x| x == "-k").unwrap() + 1].clone();
        let n = a[a.iter().position(|x| x == "-n").unwrap() + 1].clone();
        let m = a[a.iter().position(|x| x == "-M").unwrap() + 1].clone();
        assert_eq!(k, "8");
        assert_eq!(n, "10");
        assert_eq!(m, "5");
    }

    #[test]
    fn manual_tier_validation_rejects_bad_inputs() {
        // The manual-tier setter gates on the SAME guards as set_mcs + set_fec
        // before it touches the data plane: an out-of-range MCS or an invalid FEC
        // ratio must be rejected up front so a respawn never carries bad args.
        // Exercise the two guard predicates the setter calls (no process spawn).
        assert!(mcs_index_valid(5) && fec_ratio_valid(8, 10)); // a valid trio
        assert!(!mcs_index_valid(9)); // MCS out of the 0..=7 range
        assert!(!fec_ratio_valid(8, 8)); // no parity shard
        assert!(!fec_ratio_valid(0, 4)); // zero data shards
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
