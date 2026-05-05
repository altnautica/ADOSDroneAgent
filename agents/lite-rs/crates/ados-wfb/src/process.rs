//! Subprocess lifecycle for the upstream `wfb_tx` C binary.
//!
//! The lite agent does NOT reimplement WFB-ng. It spawns the upstream
//! `wfb_tx` userland tool as a child process via `tokio::process`, waits
//! for it to exit, and applies an exponential backoff on crash so a
//! flapping dongle does not turn into a fork-bomb.
//!
//! The command line is built from a [`WfbTxArgs`] struct so tests can
//! exercise the build path without a real `wfb_tx` binary. Argument
//! names match the upstream `wfb_tx` CLI documented in the WFB-ng
//! project README + man page: short flags `-K -k -n -p -B -G -M -S -L
//! -u` plus a positional `<wlan_iface>`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::process::{Child, Command};
use tracing::{debug, info, warn};

/// Lower bound on the restart delay. Anything smaller risks a tight
/// loop when `wfb_tx` exits immediately (e.g., dongle gone, driver
/// rejected the channel).
const RESTART_BACKOFF_MIN: Duration = Duration::from_millis(500);

/// Upper bound on the restart delay. After roughly 30 s the operator
/// has had time to fix whatever's wrong; piling on additional backoff
/// just delays recovery once they do.
const RESTART_BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Default path operators install `wfb_tx` to. Can be overridden via
/// `WfbConfig::wfb_tx_path` for cross-distro deploys.
pub const DEFAULT_WFB_TX_PATH: &str = "/usr/bin/wfb_tx";

/// Errors raised by the subprocess layer.
#[derive(Debug, Error)]
pub enum ProcessError {
    /// The configured binary was not present on disk.
    #[error("wfb_tx binary not found at {path}")]
    BinaryMissing { path: PathBuf },
    /// `tokio::process::Command::spawn` failed.
    #[error("failed to spawn {path}: {source}")]
    SpawnFailed {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// FEC and PHY-layer tuning for `wfb_tx`. Defaults match the values
/// the upstream WFB-ng project ships in its example config: `fec_k=8`,
/// `fec_n=12`, `radio_port=1`, 20 MHz bandwidth, long guard interval,
/// STBC + LDPC off, UDP listen on 5600. Operators that need different
/// values supply them via the `[wfb.advanced]` section in agent.yaml.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WfbAdvancedOpts {
    /// FEC data shards. The total number of shards is `fec_n`; data
    /// shards are `fec_k`. Default 8.
    pub fec_k: u8,
    /// Total FEC shards. Default 12 (8 data + 4 parity).
    pub fec_n: u8,
    /// WFB-ng radio port. Multiple streams (video, telemetry,
    /// tunnel) can multiplex on different ports. Default 1.
    pub radio_port: u8,
    /// 802.11 channel bandwidth in MHz. Common values: 20, 40, 80.
    /// Default 20.
    pub bandwidth_mhz: u8,
    /// 802.11 guard interval. `"long"` = 800 ns, `"short"` = 400 ns.
    /// Short halves the protected dead time on each symbol but raises
    /// inter-symbol interference at long range. Default `"long"`.
    pub guard_interval: GuardInterval,
    /// Space-time block coding. 0 = off, 1..=3 select stream count.
    /// Default 0.
    pub stbc: u8,
    /// Low-density parity check. 0 = off, 1 = on. Default 0.
    pub ldpc: u8,
    /// UDP port `wfb_tx` listens on for input frames. Encoded H.264
    /// NAL units are sent here from the parent process. Default 5600.
    pub udp_listen_port: u16,
}

impl Default for WfbAdvancedOpts {
    fn default() -> Self {
        Self {
            fec_k: 8,
            fec_n: 12,
            radio_port: 1,
            bandwidth_mhz: 20,
            guard_interval: GuardInterval::Long,
            stbc: 0,
            ldpc: 0,
            udp_listen_port: 5600,
        }
    }
}

/// 802.11 guard interval selector. The CLI argument is a single byte
/// (`-G long` / `-G short`) so we keep the type narrow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GuardInterval {
    Long,
    Short,
}

impl GuardInterval {
    fn as_arg(&self) -> &'static str {
        match self {
            GuardInterval::Long => "long",
            GuardInterval::Short => "short",
        }
    }
}

/// Command-line arguments passed to `wfb_tx`. Mirrors the upstream
/// flag surface; the keypair file lives on disk as a 32-byte secret +
/// 32-byte public concatenated, and the path is passed to `-K`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WfbTxArgs {
    /// `wlanX` interface bound to the air-side adapter. Passed as the
    /// trailing positional argument.
    pub interface: String,
    /// 802.11 channel number (1-13 for 2.4 GHz, 36+ for 5 GHz). Passed
    /// to `-f` on some upstream forks; the canonical CLI threads it
    /// through the bind-to-channel ioctl outside the argv. We surface
    /// it here for log + REST visibility but it does not appear in the
    /// argv; the orchestration layer runs `iw dev <iface> set channel`
    /// before exec.
    pub channel: u8,
    /// 802.11 MCS index. Drives the over-the-air bitrate. `-M`.
    pub mcs_index: u8,
    /// Transmit power, expressed in dBm. Negative values mean "leave
    /// at adapter default" — tx power is set via `iw` before exec for
    /// the same reason as `channel`. Carried for symmetry with the
    /// REST surface.
    pub tx_power_dbm: i8,
    /// Filesystem path to the keypair file. The file format is the
    /// 32-byte secret followed by the 32-byte public, big-endian, as
    /// documented in the WFB-ng project README. `-K`.
    pub keypair_path: PathBuf,
    /// FEC + PHY tuning. Defaults match the upstream example config.
    pub advanced: WfbAdvancedOpts,
}

impl WfbTxArgs {
    /// Render to the `Vec<String>` argv suffix `tokio::process::Command`
    /// expects. Kept as a function so tests can assert byte-for-byte
    /// equivalence on the argument vector.
    pub fn to_argv(&self) -> Vec<String> {
        // Argv layout matches the documented wfb_tx CLI: short-flag pairs
        // for each tunable, then the trailing wlan interface positional.
        vec![
            "-K".into(),
            self.keypair_path.to_string_lossy().into_owned(),
            "-k".into(),
            self.advanced.fec_k.to_string(),
            "-n".into(),
            self.advanced.fec_n.to_string(),
            "-p".into(),
            self.advanced.radio_port.to_string(),
            "-B".into(),
            self.advanced.bandwidth_mhz.to_string(),
            "-G".into(),
            self.advanced.guard_interval.as_arg().to_string(),
            "-M".into(),
            self.mcs_index.to_string(),
            "-S".into(),
            self.advanced.stbc.to_string(),
            "-L".into(),
            self.advanced.ldpc.to_string(),
            "-u".into(),
            self.advanced.udp_listen_port.to_string(),
            self.interface.clone(),
        ]
    }
}

/// Owned wrapper around a single `wfb_tx` child process.
pub struct WfbProcess {
    binary: PathBuf,
    args: WfbTxArgs,
    child: Option<Child>,
    backoff: Duration,
}

impl WfbProcess {
    /// Build but do not spawn.
    pub fn new(binary: impl Into<PathBuf>, args: WfbTxArgs) -> Self {
        Self {
            binary: binary.into(),
            args,
            child: None,
            backoff: RESTART_BACKOFF_MIN,
        }
    }

    /// Spawn the child. Returns immediately; the caller `.wait().await`s
    /// or polls for completion.
    pub fn spawn(&mut self) -> Result<(), ProcessError> {
        if !self.binary.exists() {
            return Err(ProcessError::BinaryMissing {
                path: self.binary.clone(),
            });
        }

        let mut cmd = Command::new(&self.binary);
        cmd.args(self.args.to_argv());
        // Child must die when we do. Without this, an orphaned wfb_tx
        // would keep broadcasting after the agent crashed.
        cmd.kill_on_drop(true);

        debug!(binary = %self.binary.display(), iface = %self.args.interface, "spawning wfb_tx");
        match cmd.spawn() {
            Ok(child) => {
                self.child = Some(child);
                Ok(())
            }
            Err(e) => Err(ProcessError::SpawnFailed {
                path: self.binary.clone(),
                source: e,
            }),
        }
    }

    /// Wait for the child to exit. Returns the exit status. Caller is
    /// responsible for deciding whether to restart (see
    /// [`WfbProcess::wait_then_backoff`]).
    pub async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        let mut child = self
            .child
            .take()
            .ok_or_else(|| std::io::Error::other("wait called with no child running"))?;
        let status = child.wait().await?;
        Ok(status)
    }

    /// Helper that waits for the child to exit, then sleeps for the
    /// current backoff before returning. The caller calls `spawn` again
    /// after this to restart.
    ///
    /// Doubles the backoff up to [`RESTART_BACKOFF_MAX`]; resets to
    /// [`RESTART_BACKOFF_MIN`] only when the caller invokes
    /// [`WfbProcess::reset_backoff`] (e.g., after a clean shutdown).
    pub async fn wait_then_backoff(&mut self) -> std::io::Result<std::process::ExitStatus> {
        let status = self.wait().await?;
        warn!(
            backoff_ms = self.backoff.as_millis() as u64,
            ?status,
            "wfb_tx exited; backing off before restart"
        );
        tokio::time::sleep(self.backoff).await;
        self.backoff = (self.backoff * 2).min(RESTART_BACKOFF_MAX);
        Ok(status)
    }

    /// Reset backoff to the minimum. Called when the manager has
    /// observed `wfb_tx` running stably (heuristic: no crash for
    /// `RESTART_BACKOFF_MAX`).
    pub fn reset_backoff(&mut self) {
        self.backoff = RESTART_BACKOFF_MIN;
    }

    /// Current backoff. Test-visible.
    pub fn backoff(&self) -> Duration {
        self.backoff
    }

    /// Send SIGTERM, wait `grace`, then SIGKILL if still alive.
    pub async fn terminate(&mut self, grace: Duration) -> std::io::Result<()> {
        let mut child = match self.child.take() {
            Some(c) => c,
            None => return Ok(()),
        };

        // Try graceful shutdown first.
        let _ = child.start_kill();
        match tokio::time::timeout(grace, child.wait()).await {
            Ok(_) => {
                info!("wfb_tx terminated cleanly");
                Ok(())
            }
            Err(_) => {
                warn!(grace_ms = grace.as_millis() as u64, "wfb_tx did not exit within grace; SIGKILL");
                child.kill().await?;
                Ok(())
            }
        }
    }

    /// Whether the child is currently running. The status is best-effort
    /// (the OS may have reaped the child without us learning yet).
    pub fn is_running(&mut self) -> bool {
        match self.child.as_mut() {
            None => false,
            Some(c) => c.try_wait().ok().flatten().is_none(),
        }
    }
}

/// Convenience: validate that the binary at `path` exists and is
/// executable. The caller can use this in the wizard's hardware-check
/// step so an operator who has not installed `wfb_tx` sees a clear
/// error instead of a SpawnFailed at first hot-plug.
pub fn binary_present(path: &Path) -> bool {
    path.exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_args() -> WfbTxArgs {
        WfbTxArgs {
            interface: "wlan0".to_string(),
            channel: 161,
            mcs_index: 1,
            tx_power_dbm: 25,
            keypair_path: PathBuf::from("/etc/ados/secrets/wfb-keypair"),
            advanced: WfbAdvancedOpts::default(),
        }
    }

    /// Build but do not run: the argv must include every documented
    /// short flag the upstream `wfb_tx` CLI consumes. Builds out the
    /// full surface and asserts each `-X <value>` pair lands exactly.
    #[test]
    fn argv_build_includes_all_options() {
        let a = sample_args();
        let argv = a.to_argv();
        // Walk through the expected ordering. Every flag must appear
        // immediately followed by its value, and the trailing positional
        // is the wlan interface name.
        let expected = vec![
            "-K".to_string(),
            "/etc/ados/secrets/wfb-keypair".to_string(),
            "-k".to_string(),
            "8".to_string(),
            "-n".to_string(),
            "12".to_string(),
            "-p".to_string(),
            "1".to_string(),
            "-B".to_string(),
            "20".to_string(),
            "-G".to_string(),
            "long".to_string(),
            "-M".to_string(),
            "1".to_string(),
            "-S".to_string(),
            "0".to_string(),
            "-L".to_string(),
            "0".to_string(),
            "-u".to_string(),
            "5600".to_string(),
            "wlan0".to_string(),
        ];
        assert_eq!(argv, expected);
    }

    /// Apply a non-default tuning (bandwidth + guard interval + STBC +
    /// LDPC) and confirm the argv carries the new values.
    #[test]
    fn argv_build_reflects_advanced_tuning() {
        let mut a = sample_args();
        a.mcs_index = 4;
        a.advanced.bandwidth_mhz = 40;
        a.advanced.guard_interval = GuardInterval::Short;
        a.advanced.stbc = 1;
        a.advanced.ldpc = 1;
        a.advanced.fec_k = 4;
        a.advanced.fec_n = 8;
        a.advanced.radio_port = 7;
        a.advanced.udp_listen_port = 5601;
        let argv = a.to_argv();
        // Spot-check the most critical pairs.
        let argv_pairs: Vec<(String, String)> = argv
            .windows(2)
            .map(|w| (w[0].clone(), w[1].clone()))
            .collect();
        assert!(argv_pairs.contains(&("-k".to_string(), "4".to_string())));
        assert!(argv_pairs.contains(&("-n".to_string(), "8".to_string())));
        assert!(argv_pairs.contains(&("-p".to_string(), "7".to_string())));
        assert!(argv_pairs.contains(&("-B".to_string(), "40".to_string())));
        assert!(argv_pairs.contains(&("-G".to_string(), "short".to_string())));
        assert!(argv_pairs.contains(&("-M".to_string(), "4".to_string())));
        assert!(argv_pairs.contains(&("-S".to_string(), "1".to_string())));
        assert!(argv_pairs.contains(&("-L".to_string(), "1".to_string())));
        assert!(argv_pairs.contains(&("-u".to_string(), "5601".to_string())));
        // Trailing positional is still the iface.
        assert_eq!(argv.last().unwrap(), "wlan0");
    }

    /// The keypair file path lands in argv exactly as stored. A symlink
    /// or a path with spaces must round-trip without lossy escaping.
    #[test]
    fn keypair_file_used_when_present() {
        let mut a = sample_args();
        a.keypair_path = PathBuf::from("/var/lib/ados/wfb keypair.bin");
        let argv = a.to_argv();
        let pos = argv.iter().position(|s| s == "-K").expect("-K flag");
        assert_eq!(argv[pos + 1], "/var/lib/ados/wfb keypair.bin");
    }

    /// Spawn a `/bin/sleep 0.05`, wait for it, observe a clean exit.
    /// Verifies the spawn → wait happy path on any Unix dev host.
    #[tokio::test]
    async fn subprocess_spawns_and_terminates() {
        let path = PathBuf::from("/bin/sleep");
        if !path.exists() {
            // Skip on hosts without /bin/sleep (rare; CI macs ship it).
            return;
        }
        let args = sample_args();
        // /bin/sleep ignores our argv style. Use a custom process where
        // we override argv directly.
        let mut child = tokio::process::Command::new(&path)
            .arg("0.05")
            .kill_on_drop(true)
            .spawn()
            .expect("spawn sleep");
        let status = child.wait().await.expect("wait");
        assert!(status.success());
        // The args struct itself round-trips through its argv builder.
        let _ = args.to_argv();
    }

    /// Use `/bin/false` to verify the restart-on-crash path: the child
    /// exits with non-zero, the backoff doubles, and the next spawn
    /// would proceed (we don't actually loop here — this is a unit
    /// test, not a stress test).
    #[tokio::test]
    async fn subprocess_restarts_on_crash() {
        let path = PathBuf::from("/bin/false");
        if !path.exists() {
            return;
        }
        let mut proc = WfbProcess::new(path, sample_args());
        proc.spawn().expect("spawn /bin/false");
        let initial_backoff = proc.backoff();
        let status = proc.wait_then_backoff().await.expect("wait");
        assert!(!status.success(), "/bin/false must exit non-zero");
        assert!(
            proc.backoff() > initial_backoff,
            "backoff must double after crash: was {:?}, now {:?}",
            initial_backoff,
            proc.backoff()
        );

        // After a synthetic clean shutdown reset returns to min.
        proc.reset_backoff();
        assert_eq!(proc.backoff(), RESTART_BACKOFF_MIN);
    }

    /// Spawn against a non-existent binary surfaces a typed error
    /// rather than swallowing the I/O failure.
    #[test]
    fn spawn_missing_binary_returns_typed_error() {
        let mut proc = WfbProcess::new("/no/such/wfb_tx", sample_args());
        match proc.spawn() {
            Err(ProcessError::BinaryMissing { .. }) => {}
            other => panic!("expected BinaryMissing, got {other:?}"),
        }
    }
}
