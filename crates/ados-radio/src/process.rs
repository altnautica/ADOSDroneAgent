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
const AUX_TX_LOG: &str = "/run/ados/wfb-drone-aux-tx.log";
const AUX_RX_LOG: &str = "/run/ados/wfb-drone-aux-rx.log";

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
/// `link_id` is this drone's own (`link_id(fleet_id, fleet_slot)`): the video
/// downlink is transmitted under the drone's identity so each drone in a fleet
/// occupies its own `channel_id` on the shared channel.
///
/// `-C` binds the wfb-ng 24.08 management socket ([`crate::tx_cmd`]) on
/// `8000 + radio_port`, so the adaptive controller can retune FEC and MCS on the
/// running transmitter instead of killing and respawning it (a 1-2 s video gap).
pub fn data_tx_args(iface: &str, cfg: &WfbConfig, key_path: &Path, link_id: u32) -> Vec<String> {
    vec![
        "-p".into(),
        "0".into(),
        "-i".into(),
        link_id.to_string(),
        "-C".into(),
        crate::tx_cmd::control_port(0).to_string(),
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
/// `link_id` is this drone's own: HopAck / PresenceBeacon travel downlink, so
/// they are keyed to the drone that emitted them. `-C` binds this plane's own
/// management socket (see [`data_tx_args`]).
pub fn tx_control_args(iface: &str, cfg: &WfbConfig, key_path: &Path, link_id: u32) -> Vec<String> {
    vec![
        "-p".into(),
        "1".into(),
        "-i".into(),
        link_id.to_string(),
        "-C".into(),
        crate::tx_cmd::control_port(1).to_string(),
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
/// `link_id` is the GROUND STATION's (`link_id(fleet_id, SLOT_GROUND)`), not
/// this drone's: the uplink is a single shared transmitter every drone in the
/// fleet listens to, so a fleet-wide command is one transmission, not N.
pub fn rx_control_args(iface: &str, key_path: &Path, link_id: u32) -> Vec<String> {
    vec![
        "-p".into(),
        "1".into(),
        "-i".into(),
        link_id.to_string(),
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
///
/// `link_id` is this drone's OWN: the stats receiver listens to the drone's own
/// downlink `channel_id` to measure what it is transmitting, so keying it to
/// the ground slot would report the uplink's health as the video link's.
pub fn stats_rx_args(iface: &str, rx_key_path: &Path, link_id: u32) -> Vec<String> {
    vec![
        "-p".into(),
        "0".into(),
        "-i".into(),
        link_id.to_string(),
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

/// Auxiliary-stream `wfb_tx` args (radio_id 2). A separate radio-port from the
/// data plane (0) and control plane (1) carrying an application stream on its own
/// UDP ingress port, with its own (light) Reed-Solomon ratio and MCS. The aux
/// pair shares the injection adapter but never the radio_id, so it can never
/// collide with the data or control planes on the air.
///
/// `link_id` is this drone's own — the aux downlink is the drone talking. `-C`
/// binds this plane's own management socket (see [`data_tx_args`]).
pub fn aux_tx_args(iface: &str, cfg: &WfbConfig, key_path: &Path, link_id: u32) -> Vec<String> {
    vec![
        "-p".into(),
        "2".into(),
        "-i".into(),
        link_id.to_string(),
        "-C".into(),
        crate::tx_cmd::control_port(2).to_string(),
        "-u".into(),
        cfg.aux_tx_port.to_string(),
        "-K".into(),
        key_str(key_path),
        "-k".into(),
        cfg.aux_fec_k.to_string(),
        "-n".into(),
        cfg.aux_fec_n.to_string(),
        "-B".into(),
        "20".into(),
        "-M".into(),
        cfg.aux_mcs().to_string(),
        iface.into(),
    ]
}

/// Auxiliary-stream `wfb_rx` args (radio_id 3, re-emit decoded application frames
/// on 127.0.0.1:`aux_rx_port`). The re-emit port is deliberately distinct from
/// the aux tx ingress (`aux_tx_port`) so the receive side can never feed back into
/// the transmit ingress. Uses the same key path as the aux tx.
///
/// `link_id` is the GROUND STATION's: like the control uplink, the aux uplink is
/// one shared transmitter the whole fleet receives.
pub fn aux_rx_args(iface: &str, cfg: &WfbConfig, key_path: &Path, link_id: u32) -> Vec<String> {
    vec![
        "-p".into(),
        "3".into(),
        "-i".into(),
        link_id.to_string(),
        "-c".into(),
        "127.0.0.1".into(),
        "-u".into(),
        cfg.aux_rx_port.to_string(),
        "-K".into(),
        key_str(key_path),
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
        link_id: u32,
    ) -> std::io::Result<Self> {
        Self::spawn_in_group("wfb_tx", &data_tx_args(iface, cfg, key_path, link_id), None).await
    }

    /// Spawn the **tx-control** `wfb_tx` (over-the-air HopAnnounce/PresenceBeacon
    /// transport). stderr → truncated log file (avoids the PIPE deadlock).
    pub async fn spawn_tx_control(
        iface: &str,
        cfg: &WfbConfig,
        key_path: &Path,
        link_id: u32,
    ) -> std::io::Result<Self> {
        Self::spawn_in_group(
            "wfb_tx",
            &tx_control_args(iface, cfg, key_path, link_id),
            Some(TX_CONTROL_LOG),
        )
        .await
    }

    /// Spawn the **rx-control** `wfb_rx` (receives HopAck off the air → 5810).
    /// `link_id` is the ground station's — the shared uplink.
    pub async fn spawn_rx_control(
        iface: &str,
        key_path: &Path,
        link_id: u32,
    ) -> std::io::Result<Self> {
        Self::spawn_in_group(
            "wfb_rx",
            &rx_control_args(iface, key_path, link_id),
            Some(RX_CONTROL_LOG),
        )
        .await
    }

    /// Spawn the **stats** `wfb_rx` (data plane, port 5601) with stdout PIPED so
    /// the caller can read the per-second PKT/RX_ANT stats lines.
    pub async fn spawn_stats_rx(
        iface: &str,
        rx_key_path: &Path,
        link_id: u32,
    ) -> std::io::Result<Self> {
        // stderr → null (we only want stdout's stats stream); stdout piped.
        Self::spawn_in_group_piped_stdout("wfb_rx", &stats_rx_args(iface, rx_key_path, link_id))
            .await
    }

    /// Spawn the **auxiliary tx** `wfb_tx` (radio_id 2, application ingress).
    /// stderr → truncated log file (avoids the PIPE deadlock), same as the
    /// control planes.
    pub async fn spawn_aux_tx(
        iface: &str,
        cfg: &WfbConfig,
        key_path: &Path,
        link_id: u32,
    ) -> std::io::Result<Self> {
        Self::spawn_in_group(
            "wfb_tx",
            &aux_tx_args(iface, cfg, key_path, link_id),
            Some(AUX_TX_LOG),
        )
        .await
    }

    /// Spawn the **auxiliary rx** `wfb_rx` (radio_id 3, re-emit decoded frames on
    /// 127.0.0.1:`aux_rx_port`). stderr → truncated log file. `link_id` is the
    /// ground station's — the shared aux uplink.
    pub async fn spawn_aux_rx(
        iface: &str,
        cfg: &WfbConfig,
        key_path: &Path,
        link_id: u32,
    ) -> std::io::Result<Self> {
        Self::spawn_in_group(
            "wfb_rx",
            &aux_rx_args(iface, cfg, key_path, link_id),
            Some(AUX_RX_LOG),
        )
        .await
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

/// The wfb-ng radio port (`-p`) the video data plane occupies. Also selects its
/// management-socket port via [`crate::tx_cmd::control_port`] — MUST stay equal
/// to the `-p` value [`data_tx_args`] pushes, or a live retune would be sent to
/// the wrong transmitter.
const DATA_RADIO_PORT: u8 = 0;

/// Tally of how the live data-plane retunes were applied.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ApplyCounters {
    /// Applied over the `wfb_tx` management socket — no video interruption.
    pub tx_cmd: u64,
    /// Applied by killing and respawning the data plane — a 1-2 s video gap.
    pub respawn: u64,
    /// Management-socket attempts that failed and forced the respawn fallback.
    pub tx_cmd_failed: u64,
}

/// Which half of the retained data-plane trio a retune actually changed. Only
/// the changed half is pushed: re-sending an unchanged FEC ratio would restart
/// the receiver's FEC session for nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Retune {
    Fec,
    Modulation,
    Both,
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
    /// Auxiliary application-stream transmit (radio_id 2). `None` whenever the
    /// aux stream is closed, which is the boot state (safe-by-default): no aux
    /// process is ever spawned until something explicitly opens the stream.
    aux_tx: Option<WfbProcess>,
    /// Auxiliary application-stream receive (radio_id 3). Paired with `aux_tx`:
    /// both are `Some` while the stream is open, both `None` while it is closed.
    aux_rx: Option<WfbProcess>,
    /// The aux pair's retained tunables (ports + FEC + MCS), captured at open so
    /// a whole-group respawn (a channel hop) can re-open the aux pair on the new
    /// channel with the same settings. `None` while the aux stream is closed.
    aux_settings: Option<AuxSettings>,
    /// The interface + key + current data-plane FEC/MCS, retained so a single
    /// data-tx process can be respawned with new tunables without touching the
    /// control planes (an adaptive FEC/MCS change restarts only the data plane).
    iface: String,
    tx_key_path: PathBuf,
    /// This node's own wfb-ng `link_id` (`link_id(fleet_id, fleet_slot)`), the
    /// key for every process the drone TRANSMITS on plus the stats RX that
    /// listens to its own downlink. Retained rather than recomputed so a channel
    /// hop or an FEC/MCS respawn re-keys IDENTICALLY: `respawn_data_tx` rebuilds
    /// its cfg view from `WfbConfig::default()`, whose fleet fields are the
    /// unprovisioned defaults, and re-deriving from that would silently move a
    /// live transmitter onto the ground station's `channel_id`.
    own_link_id: u32,
    /// The ground station's `link_id` (`link_id(fleet_id, SLOT_GROUND)`), the key
    /// for every process the drone RECEIVES the shared uplink on. One uplink
    /// transmitter serves the whole fleet, so this is the same value on every
    /// drone in a fleet.
    uplink_link_id: u32,
    data_fec_k: u8,
    data_fec_n: u8,
    data_mcs_index: u8,
    /// Which path the live data-plane retunes took (management socket vs
    /// respawn). Reported through the heartbeat: a rig whose `respawn` counter
    /// grows is a rig where `-C` never reached the transmitter, and every tier
    /// change there costs a 1-2 s video gap.
    applies: ApplyCounters,
}

/// The auxiliary stream's retained tunables, captured at open so the pair can be
/// re-spawned (after a channel hop) byte-identically without re-reading config.
#[derive(Debug, Clone, Copy)]
struct AuxSettings {
    tx_port: u16,
    rx_port: u16,
    fec_k: u8,
    fec_n: u8,
    mcs_index: u8,
}

impl AuxSettings {
    /// Read the aux trio from a [`WfbConfig`] (the `open` request resolves the
    /// effective settings through the config defaults / overrides).
    fn from_cfg(cfg: &WfbConfig) -> Self {
        Self {
            tx_port: cfg.aux_tx_port,
            rx_port: cfg.aux_rx_port,
            fec_k: cfg.aux_fec_k,
            fec_n: cfg.aux_fec_n,
            mcs_index: cfg.aux_mcs(),
        }
    }

    /// Rebuild a [`WfbConfig`]-shaped view carrying these aux settings, so the
    /// `aux_*_args` builders (which read from a `WfbConfig`) produce the retained
    /// pair on a respawn. Only the aux fields matter to those builders.
    fn to_cfg(self) -> WfbConfig {
        WfbConfig {
            aux_tx_port: self.tx_port,
            aux_rx_port: self.rx_port,
            aux_fec_k: self.fec_k,
            aux_fec_n: self.fec_n,
            aux_mcs_index: Some(self.mcs_index),
            ..WfbConfig::default()
        }
    }
}

impl RadioProcesses {
    /// Spawn the data plane + both control planes, and (when `/etc/ados/wfb/rx.key`
    /// exists) the stats RX with a reader task that updates `link` from the
    /// `wfb_rx` stats stream.
    ///
    /// The two fleet link ids are derived once from `cfg` here and retained: the
    /// transmit side and the stats receiver key to this drone's own slot, the
    /// two uplink receivers key to the ground station's shared slot 0.
    pub async fn spawn(
        iface: &str,
        cfg: &WfbConfig,
        key_path: &Path,
        link: std::sync::Arc<tokio::sync::Mutex<crate::link_quality::LinkStats>>,
    ) -> std::io::Result<Self> {
        let own_link_id = crate::config::link_id(cfg.fleet_id, cfg.fleet_slot);
        let uplink_link_id = crate::config::link_id(cfg.fleet_id, crate::config::SLOT_GROUND);
        let data_tx = WfbProcess::spawn_data_tx(iface, cfg, key_path, own_link_id).await?;
        let tx_control = WfbProcess::spawn_tx_control(iface, cfg, key_path, own_link_id).await?;
        let rx_control = WfbProcess::spawn_rx_control(iface, key_path, uplink_link_id).await?;

        // Stats RX is best-effort + gated on the rx key (the GS-uplink decryptor).
        // Without it the link block stays at default sentinels — same as Python.
        let (stats_rx, stats_reader) = if Path::new(crate::paths::WFB_RX_KEY).exists() {
            match WfbProcess::spawn_stats_rx(
                iface,
                Path::new(crate::paths::WFB_RX_KEY),
                own_link_id,
            )
            .await
            {
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

        tracing::info!(
            fleet_id = cfg.fleet_id,
            fleet_slot = cfg.fleet_slot,
            own_link_id,
            uplink_link_id,
            "wfb_fleet_identity"
        );

        Ok(Self {
            data_tx,
            tx_control,
            rx_control,
            stats_rx,
            stats_reader,
            // Safe-by-default: the auxiliary stream never starts at boot. It is
            // brought up only by an explicit open_aux_stream call.
            aux_tx: None,
            aux_rx: None,
            aux_settings: None,
            iface: iface.to_string(),
            tx_key_path: key_path.to_path_buf(),
            own_link_id,
            uplink_link_id,
            data_fec_k: cfg.fec_k,
            data_fec_n: cfg.fec_n,
            data_mcs_index: cfg.mcs_index,
            applies: ApplyCounters::default(),
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

    /// How the live data-plane retunes have been applied since this group came
    /// up. Surfaced through the bitrate snapshot onto `wfb-stats.json`.
    pub fn apply_counters(&self) -> ApplyCounters {
        self.applies
    }

    /// Push the retained data-plane trio onto the RUNNING `wfb_tx` over its
    /// wfb-ng 24.08 management socket. No process restart, so no video gap.
    ///
    /// Only ever sends the MCS index and the FEC pair. `set_radio` replaces the
    /// whole injected radiotap header, so [`crate::tx_cmd::RadioSettings::with_mcs`]
    /// re-states the pinned 20 MHz width and the spawn-time GI / STBC / LDPC /
    /// VHT values — a rate change can never silently retune the channel width.
    async fn tx_cmd_apply(&self, what: Retune) -> Result<(), crate::tx_cmd::TxCmdError> {
        let client = crate::tx_cmd::TxCmdClient::for_radio_port(DATA_RADIO_PORT);
        if matches!(what, Retune::Fec | Retune::Both) {
            client.set_fec(self.data_fec_k, self.data_fec_n).await?;
        }
        if matches!(what, Retune::Modulation | Retune::Both) {
            client
                .set_radio(&crate::tx_cmd::RadioSettings::with_mcs(self.data_mcs_index))
                .await?;
        }
        Ok(())
    }

    /// Apply the already-updated retained trio to the live data plane, preferring
    /// the management socket and falling back to a respawn.
    ///
    /// The fallback is not optional: a `wfb_tx` too old for `-C`, a control socket
    /// that failed to bind, or a transmitter that has died all land here, and the
    /// operator's or the ladder's intent must still take effect. The respawn is
    /// the historical path and carries its historical cost (a 1-2 s video gap),
    /// which is why the two paths are counted separately.
    async fn apply_retained(&mut self, what: Retune) -> bool {
        match self.tx_cmd_apply(what).await {
            Ok(()) => {
                self.applies.tx_cmd += 1;
                true
            }
            Err(e) => {
                self.applies.tx_cmd_failed += 1;
                tracing::warn!(
                    error = %e,
                    port = crate::tx_cmd::control_port(DATA_RADIO_PORT),
                    "data_plane_tx_cmd_failed_falling_back_to_respawn"
                );
                if self.respawn_data_tx().await {
                    self.applies.respawn += 1;
                    true
                } else {
                    false
                }
            }
        }
    }

    /// The retained `link_id` every transmitter in this group is keyed to (this
    /// drone's fleet slot).
    pub fn own_link_id(&self) -> u32 {
        self.own_link_id
    }

    /// The retained `link_id` the two uplink receivers are keyed to (the ground
    /// station's slot 0).
    pub fn uplink_link_id(&self) -> u32 {
        self.uplink_link_id
    }

    /// Apply a new Reed-Solomon `(k, n)` ratio to the live data plane.
    ///
    /// Applied on the RUNNING transmitter through the `wfb_tx` management socket
    /// (`set_fec`, which restarts only the FEC session — no video gap), falling
    /// back to kill-and-respawn when that socket does not answer. The two control
    /// planes and the stats RX carry their own fixed FEC and are never touched, so
    /// an FEC change does not interrupt HopAnnounce/HopAck or the link-quality
    /// stream. Returns `false` on an invalid ratio, or when both the live path and
    /// the respawn fallback failed; in that case the previous data tunables are
    /// restored in the retained state so a later respawn does not silently keep
    /// the rejected values, and the data plane is left dead for the supervisor to
    /// restart the whole group (the same fail-safe path the watchdog kills take).
    /// A no-op when the ratio already matches the running data plane.
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
        if self.apply_retained(Retune::Fec).await {
            tracing::info!(k = fec_k, n = fec_n, "set_fec_applied");
            true
        } else {
            self.data_fec_k = old_k;
            self.data_fec_n = old_n;
            tracing::warn!(k = fec_k, n = fec_n, "set_fec_apply_failed");
            false
        }
    }

    /// Apply a new MCS index to the live data plane (same live-then-respawn path
    /// as [`set_fec`](Self::set_fec), via `set_radio`). The accepted range is
    /// 0..=7 (the RTL8812EU range); `wfb_tx` rejects anything wider. A no-op when
    /// the index already matches the running data plane.
    ///
    /// This is the only radio knob varied at runtime. The channel width rides
    /// along pinned at 20 MHz and the TX power is left to the adapter's own
    /// ramp-until-accepted path — see [`crate::tx_cmd`].
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
        if self.apply_retained(Retune::Modulation).await {
            tracing::info!(mcs, "set_mcs_applied");
            true
        } else {
            self.data_mcs_index = old_mcs;
            tracing::warn!(mcs, "set_mcs_apply_failed");
            false
        }
    }

    /// Pin a full manual link tier — the `(mcs_index, fec_k, fec_n)` trio — onto
    /// the live data plane in one operation.
    ///
    /// This is the manual half of the auto/manual tier control: the operator
    /// fixes the radio rate + redundancy and the adaptive controller is held off
    /// (the caller disables it). Only the halves that actually changed are pushed,
    /// so pinning a tier that differs in MCS alone does not needlessly restart the
    /// receiver's FEC session. Validates the FEC ratio and the MCS range up front;
    /// on an invalid input it changes nothing and returns `false`. A no-op
    /// (returns `true` without touching the radio) when the trio already matches
    /// the running data plane. When both the live path and the respawn fallback
    /// fail, the previous trio is restored in the retained state and the data
    /// plane is left dead for the supervisor to restart the whole group.
    pub async fn set_manual_tier(&mut self, mcs: u8, fec_k: u8, fec_n: u8) -> bool {
        if !mcs_index_valid(mcs) {
            tracing::warn!(mcs, "set_manual_tier_mcs_out_of_range");
            return false;
        }
        if !fec_ratio_valid(fec_k, fec_n) {
            tracing::warn!(k = fec_k, n = fec_n, "set_manual_tier_fec_invalid");
            return false;
        }
        let fec_changed = fec_k != self.data_fec_k || fec_n != self.data_fec_n;
        let mcs_changed = mcs != self.data_mcs_index;
        if !fec_changed && !mcs_changed {
            return true;
        }
        // At least one half changed, so this covers every reachable case.
        let what = match (fec_changed, mcs_changed) {
            (true, true) => Retune::Both,
            (true, false) => Retune::Fec,
            _ => Retune::Modulation,
        };
        let (old_mcs, old_k, old_n) = (self.data_mcs_index, self.data_fec_k, self.data_fec_n);
        self.data_mcs_index = mcs;
        self.data_fec_k = fec_k;
        self.data_fec_n = fec_n;
        if self.apply_retained(what).await {
            tracing::info!(mcs, k = fec_k, n = fec_n, "set_manual_tier_applied");
            true
        } else {
            self.data_mcs_index = old_mcs;
            self.data_fec_k = old_k;
            self.data_fec_n = old_n;
            tracing::warn!(mcs, k = fec_k, n = fec_n, "set_manual_tier_apply_failed");
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

    /// True while the auxiliary application stream is open (both halves running).
    pub fn aux_active(&self) -> bool {
        self.aux_tx.is_some() && self.aux_rx.is_some()
    }

    /// The auxiliary tx PID, for the delta-counter TX-liveness watchdog.
    /// `None` while the aux stream is closed.
    pub fn aux_tx_pid(&self) -> Option<u32> {
        self.aux_tx.as_ref().and_then(|p| p.pid())
    }

    /// The auxiliary rx PID, for the aux receive-side watchdog. `None` while the
    /// aux stream is closed.
    pub fn aux_rx_pid(&self) -> Option<u32> {
        self.aux_rx.as_ref().and_then(|p| p.pid())
    }

    /// Open the auxiliary application stream: spawn the aux tx (radio_id 2) and
    /// aux rx (radio_id 3) pair on the SAME injection interface as the data and
    /// control planes, using the aux settings from `cfg`. ADDITIVE — it never
    /// stops or restarts the data plane or the control planes, so opening the aux
    /// stream cannot interrupt video or the FHSS HopAnnounce/HopAck transport.
    ///
    /// Idempotent: a second open while the stream is already up is a no-op
    /// (returns `true`) rather than spawning a duplicate pair. On a tx-spawn or
    /// rx-spawn failure the partially-spawned half is torn down and the stream is
    /// left closed (returns `false`), so a failed open never leaves a half-open
    /// stream. The retained aux settings are captured on success so a later
    /// whole-group respawn (a channel hop) re-opens the pair on the new channel.
    ///
    /// SAFE: nothing here ever runs at boot — this is reached only from an
    /// explicit open request.
    ///
    /// The operator dead-switch is honoured structurally: when `cfg.aux_enable`
    /// is false the stream is REFUSED (returns `false`) and no process is
    /// spawned, so the config-level disable is a real kill-switch rather than a
    /// false affordance. Safe-by-default is preserved: the flag is off unless an
    /// operator opts in.
    pub async fn open_aux_stream(&mut self, cfg: &WfbConfig) -> bool {
        if !cfg.aux_enable {
            tracing::warn!("aux_stream_open_refused: aux_enable is false");
            return false;
        }
        if self.aux_active() {
            return true;
        }
        let key_path = self.tx_key_path.clone();
        let aux_tx =
            match WfbProcess::spawn_aux_tx(&self.iface, cfg, &key_path, self.own_link_id).await {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(error = %e, "aux_tx_spawn_failed");
                    return false;
                }
            };
        let aux_rx = match WfbProcess::spawn_aux_rx(
            &self.iface,
            cfg,
            &key_path,
            self.uplink_link_id,
        )
        .await
        {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "aux_rx_spawn_failed");
                // Tear down the tx half so a failed open never leaves a half-open
                // stream. `aux_tx` drops here, whose Drop killpg's the group.
                drop(aux_tx);
                return false;
            }
        };
        self.aux_tx = Some(aux_tx);
        self.aux_rx = Some(aux_rx);
        self.aux_settings = Some(AuxSettings::from_cfg(cfg));
        tracing::info!(
            tx_port = cfg.aux_tx_port,
            rx_port = cfg.aux_rx_port,
            "aux_stream_opened"
        );
        true
    }

    /// Close the auxiliary application stream: kill the aux tx + aux rx pair and
    /// drop the retained settings so a later respawn does NOT re-open it. ADDITIVE
    /// — it never touches the data plane or the control planes. Idempotent: a
    /// close while the stream is already down is a quiet no-op.
    pub async fn close_aux_stream(&mut self) {
        let was_open = self.aux_active();
        if let Some(mut tx) = self.aux_tx.take() {
            tx.kill().await;
        }
        if let Some(mut rx) = self.aux_rx.take() {
            rx.kill().await;
        }
        self.aux_settings = None;
        if was_open {
            tracing::info!("aux_stream_closed");
        }
    }

    /// Restart the auxiliary pair in place, reusing the retained aux settings.
    ///
    /// This is the liveness-watchdog recovery path: when the aux tx counter is
    /// flat (a silently-stalled transmitter) the watchdog asks for a restart, NOT
    /// a close — the plugin still wants the stream, so the retained settings are
    /// kept and the pair is re-spawned on the same ports/FEC/MCS. ADDITIVE: it
    /// only touches the aux pair, never the data plane or the control planes.
    ///
    /// A no-op (`false`) when the stream is not open (nothing to restart). On a
    /// re-spawn failure the aux pair is left closed but the retained settings are
    /// KEPT, so a later whole-group respawn (a channel hop) still re-opens it — the
    /// primary link is unaffected regardless of the additive aux pair's health.
    pub async fn restart_aux_stream(&mut self) -> bool {
        let Some(settings) = self.aux_settings else {
            return false;
        };
        // Kill the current pair without clearing the settings.
        if let Some(mut tx) = self.aux_tx.take() {
            tx.kill().await;
        }
        if let Some(mut rx) = self.aux_rx.take() {
            rx.kill().await;
        }
        let aux_cfg = settings.to_cfg();
        let key_path = self.tx_key_path.clone();
        let aux_tx =
            WfbProcess::spawn_aux_tx(&self.iface, &aux_cfg, &key_path, self.own_link_id).await;
        let aux_rx =
            WfbProcess::spawn_aux_rx(&self.iface, &aux_cfg, &key_path, self.uplink_link_id).await;
        match (aux_tx, aux_rx) {
            (Ok(tx), Ok(rx)) => {
                self.aux_tx = Some(tx);
                self.aux_rx = Some(rx);
                tracing::info!("aux_stream_restarted");
                true
            }
            (tx, rx) => {
                if let Err(e) = &tx {
                    tracing::warn!(error = %e, "restart_aux_tx_failed");
                }
                if let Err(e) = &rx {
                    tracing::warn!(error = %e, "restart_aux_rx_failed");
                }
                // Drop any half that spawned so a partial restart never leaves one
                // process running; keep the settings so a channel hop can retry.
                drop(tx);
                drop(rx);
                self.aux_tx = None;
                self.aux_rx = None;
                false
            }
        }
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
        let data_tx =
            match WfbProcess::spawn_data_tx(&self.iface, &data_cfg, &key_path, self.own_link_id)
                .await
            {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(error = %e, "respawn_group_data_tx_failed");
                    return false;
                }
            };
        let tx_control =
            match WfbProcess::spawn_tx_control(&self.iface, cfg, &key_path, self.own_link_id).await
            {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(error = %e, "respawn_group_tx_control_failed");
                    return false;
                }
            };
        let rx_control =
            match WfbProcess::spawn_rx_control(&self.iface, &key_path, self.uplink_link_id).await {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(error = %e, "respawn_group_rx_control_failed");
                    return false;
                }
            };
        let (stats_rx, stats_reader) = if Path::new(crate::paths::WFB_RX_KEY).exists() {
            match WfbProcess::spawn_stats_rx(
                &self.iface,
                Path::new(crate::paths::WFB_RX_KEY),
                self.own_link_id,
            )
            .await
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
        // kill_all cleared the aux processes but kept aux_settings; re-open the
        // aux pair on the NEW channel iff it was open before the hop. A re-open
        // failure leaves the aux stream closed (settings dropped) but does NOT
        // fail the whole group — the data + control planes are already up, so the
        // primary link is healthy regardless of the additive aux pair.
        if let Some(settings) = self.aux_settings {
            let aux_cfg = settings.to_cfg();
            let aux_tx =
                WfbProcess::spawn_aux_tx(&self.iface, &aux_cfg, &key_path, self.own_link_id).await;
            let aux_rx =
                WfbProcess::spawn_aux_rx(&self.iface, &aux_cfg, &key_path, self.uplink_link_id)
                    .await;
            match (aux_tx, aux_rx) {
                (Ok(tx), Ok(rx)) => {
                    self.aux_tx = Some(tx);
                    self.aux_rx = Some(rx);
                }
                (tx, rx) => {
                    if let Err(e) = &tx {
                        tracing::warn!(error = %e, "respawn_group_aux_tx_failed");
                    }
                    if let Err(e) = &rx {
                        tracing::warn!(error = %e, "respawn_group_aux_rx_failed");
                    }
                    // Drop any half that spawned so a partial re-open does not
                    // leave one process running, and forget the settings.
                    drop(tx);
                    drop(rx);
                    self.aux_tx = None;
                    self.aux_rx = None;
                    self.aux_settings = None;
                }
            }
        }
        true
    }

    /// Kill ONLY the data-tx process and respawn it from the retained iface/key
    /// and current data tunables. Leaves the control planes + stats RX running.
    /// Returns `false` if the respawn fails (the data plane is then dead).
    ///
    /// The cfg view is a `WfbConfig::default()` overlay carrying only the data
    /// tunables, so the fleet identity CANNOT be re-derived from it — the
    /// retained `own_link_id` is passed explicitly. Deriving it from the overlay
    /// would key the respawned transmitter to the unprovisioned default
    /// `link_id(1, 0)`, i.e. onto the ground station's uplink `channel_id`.
    async fn respawn_data_tx(&mut self) -> bool {
        let cfg = WfbConfig {
            fec_k: self.data_fec_k,
            fec_n: self.data_fec_n,
            mcs_index: self.data_mcs_index,
            ..WfbConfig::default()
        };
        self.data_tx.kill().await;
        match WfbProcess::spawn_data_tx(&self.iface, &cfg, &self.tx_key_path, self.own_link_id)
            .await
        {
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

    /// Kill every process group + stop the stats reader. This INCLUDES the
    /// auxiliary pair when it is open, so a respawn or shutdown never orphans the
    /// aux processes. `aux_settings` is deliberately left intact so a whole-group
    /// respawn (a channel hop) can re-open the aux pair on the new channel; a
    /// `close` is what clears the settings.
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
        if let Some(mut tx) = self.aux_tx.take() {
            tx.kill().await;
        }
        if let Some(mut rx) = self.aux_rx.take() {
            rx.kill().await;
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
    use crate::config::{link_id, FLEET_MAX_SLOTS, SLOT_GROUND};

    /// Read the value following `flag` in an arg vector.
    fn arg_after(args: &[String], flag: &str) -> String {
        args[args.iter().position(|x| x == flag).unwrap() + 1].clone()
    }

    /// The wfb-ng `channel_id` an arg vector resolves to:
    /// `(link_id << 8) + radio_port`. This is the value `rx.cpp` compiles into
    /// its per-instance BPF, so two arg vectors sharing one is two transmitters
    /// thrashing each other's FEC decoder.
    fn channel_id_of(args: &[String]) -> u64 {
        let link: u64 = arg_after(args, "-i").parse().unwrap();
        let port: u64 = arg_after(args, "-p").parse().unwrap();
        (link << 8) + port
    }

    /// Every `wfb_tx` plane binds a management socket, and each one's `-C` port
    /// MUST be `control_port` of that plane's own `-p` radio port. A drift here
    /// would send a data-plane retune to the control plane's transmitter — the
    /// video would keep its old rate while HopAck traffic silently changed rung.
    /// The three `wfb_rx` builders are deliberately excluded: `-C` is a `wfb_tx`
    /// flag and `wfb_rx` would reject it.
    #[test]
    fn every_tx_plane_binds_its_own_management_port() {
        let cfg = WfbConfig::default();
        let key = Path::new("/etc/ados/wfb/tx.key");
        let planes = [
            data_tx_args("wlan1", &cfg, key, link_id(1, 3)),
            tx_control_args("wlan1", &cfg, key, link_id(1, 3)),
            aux_tx_args("wlan1", &cfg, key, link_id(1, 3)),
        ];
        let mut seen = Vec::new();
        for args in &planes {
            let radio_port: u8 = arg_after(args, "-p").parse().unwrap();
            let ctl: u16 = arg_after(args, "-C").parse().unwrap();
            assert_eq!(
                ctl,
                crate::tx_cmd::control_port(radio_port),
                "radio port {radio_port} bound the wrong management port"
            );
            seen.push(ctl);
        }
        // And no two planes share one, or a retune would hit both.
        seen.sort_unstable();
        let uniq = seen.len();
        seen.dedup();
        assert_eq!(seen.len(), uniq, "two tx planes share a management port");
        // The data plane is the one the adaptive ladder drives.
        assert_eq!(
            crate::tx_cmd::control_port(DATA_RADIO_PORT),
            arg_after(&planes[0], "-C").parse::<u16>().unwrap()
        );
    }

    /// The three `wfb_rx` builders must NOT carry `-C`.
    #[test]
    fn receive_planes_carry_no_management_port() {
        let cfg = WfbConfig::default();
        let key = Path::new("/etc/ados/wfb/rx.key");
        for args in [
            rx_control_args("wlan1", key, link_id(1, 0)),
            stats_rx_args("wlan1", key, link_id(1, 3)),
            aux_rx_args("wlan1", &cfg, key, link_id(1, 0)),
        ] {
            assert!(
                !args.iter().any(|a| a == "-C"),
                "wfb_rx has no -C option: {args:?}"
            );
        }
    }

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
        let args = data_tx_args(
            "wlan1",
            &data_cfg,
            Path::new("/etc/ados/wfb/tx.key"),
            link_id(1, 3),
        );
        assert_eq!(arg_after(&args, "-k"), "4");
        assert_eq!(arg_after(&args, "-n"), "8");
        assert_eq!(arg_after(&args, "-M"), "5");
    }

    #[test]
    fn data_tx_args_match_python() {
        let cfg = WfbConfig::default();
        let a = data_tx_args(
            "wlan1",
            &cfg,
            Path::new("/etc/ados/wfb/tx.key"),
            link_id(1, 3),
        );
        assert_eq!(
            a,
            vec![
                "-p",
                "0",
                "-i",
                "259", // link_id(1, 3) = (1 << 8) | 3
                "-C",
                "8000", // TX_CMD_PORT_BASE + radio_port 0
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
        // wfb_tx -p 1 -i <link> -C 8001 -u 5803 -K <key> -k 1 -n 2 -B 20 -M <mcs> <iface>
        let cfg = WfbConfig::default();
        let a = tx_control_args(
            "wlan1",
            &cfg,
            Path::new("/etc/ados/wfb/tx.key"),
            link_id(1, 3),
        );
        assert_eq!(
            a,
            vec![
                "-p",
                "1",
                "-i",
                "259",
                "-C",
                "8001", // TX_CMD_PORT_BASE + radio_port 1
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
        // wfb_rx -p 1 -i <ground link> -c 127.0.0.1 -u 5810 -K <key> -l 1000 <iface>
        let a = rx_control_args(
            "wlan1",
            Path::new("/etc/ados/wfb/tx.key"),
            link_id(1, SLOT_GROUND),
        );
        assert_eq!(
            a,
            vec![
                "-p",
                "1",
                "-i",
                "256", // link_id(1, 0) — the shared uplink, not this drone's slot
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
        let a = data_tx_args(
            "wlan1",
            &cfg,
            Path::new("/etc/ados/wfb/tx.key"),
            link_id(1, 1),
        );
        assert_eq!(arg_after(&a, "-k"), "8");
        assert_eq!(arg_after(&a, "-n"), "10");
        assert_eq!(arg_after(&a, "-M"), "5");
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
        let link = link_id(1, 1);
        let data = data_tx_args("wlan1", &cfg, Path::new("/k"), link);
        let ctrl = tx_control_args("wlan1", &cfg, Path::new("/k"), link);
        // data plane: k=8 n=12; control plane: k=1 n=2.
        assert_eq!(arg_after(&data, "-k"), "8");
        assert_eq!(arg_after(&ctrl, "-k"), "1");
    }

    #[test]
    fn aux_tx_args_use_radio_id_2_and_aux_port_and_fec() {
        // The aux tx must sit on radio_id 2 (separate from data 0 / control 1)
        // with the aux ingress port and the light aux FEC, riding the data-plane
        // MCS by default.
        let cfg = WfbConfig::default();
        let a = aux_tx_args(
            "wlan1",
            &cfg,
            Path::new("/etc/ados/wfb/tx.key"),
            link_id(1, 1),
        );
        assert_eq!(arg_after(&a, "-p"), "2");
        assert_eq!(arg_after(&a, "-u"), cfg.aux_tx_port.to_string());
        assert_eq!(arg_after(&a, "-k"), cfg.aux_fec_k.to_string());
        assert_eq!(arg_after(&a, "-n"), cfg.aux_fec_n.to_string());
        // No aux MCS override → the aux pair rides the data-plane rate.
        assert_eq!(arg_after(&a, "-M"), cfg.mcs_index.to_string());
        assert_eq!(a.last().unwrap(), "wlan1");
    }

    #[test]
    fn aux_rx_args_use_radio_id_3_and_loopback_reemit() {
        // The aux rx must sit on radio_id 3 and re-emit on loopback at the aux rx
        // port — distinct from the aux tx ingress so receive can't feed transmit.
        let cfg = WfbConfig::default();
        let a = aux_rx_args(
            "wlan1",
            &cfg,
            Path::new("/etc/ados/wfb/tx.key"),
            link_id(1, SLOT_GROUND),
        );
        assert_eq!(arg_after(&a, "-p"), "3");
        assert_eq!(arg_after(&a, "-c"), "127.0.0.1");
        assert_eq!(arg_after(&a, "-u"), cfg.aux_rx_port.to_string());
        // The re-emit port differs from the tx ingress.
        assert_ne!(cfg.aux_rx_port, cfg.aux_tx_port);
    }

    #[test]
    fn aux_tx_args_honour_an_explicit_aux_mcs_override() {
        let cfg = WfbConfig {
            mcs_index: 1,
            aux_mcs_index: Some(5),
            ..WfbConfig::default()
        };
        let a = aux_tx_args("wlan1", &cfg, Path::new("/k"), link_id(1, 1));
        // The aux pair uses the override, not the data-plane MCS.
        assert_eq!(arg_after(&a, "-M"), "5");
    }

    #[test]
    fn aux_radio_ids_never_collide_with_data_or_control_planes() {
        // The four wfb planes claim four distinct radio_ids on the shared adapter:
        // data 0, control 1, aux tx 2, aux rx 3. A collision would corrupt one
        // plane's frames, so this is a hard invariant.
        let cfg = WfbConfig::default();
        let key = Path::new("/k");
        let own = link_id(1, 1);
        let up = link_id(1, SLOT_GROUND);
        let ids = [
            arg_after(&data_tx_args("wlan1", &cfg, key, own), "-p"),
            arg_after(&tx_control_args("wlan1", &cfg, key, own), "-p"),
            arg_after(&aux_tx_args("wlan1", &cfg, key, own), "-p"),
            arg_after(&aux_rx_args("wlan1", &cfg, key, up), "-p"),
        ];
        for i in 0..ids.len() {
            for j in (i + 1)..ids.len() {
                assert_ne!(ids[i], ids[j], "radio_id collision between planes");
            }
        }
    }

    #[test]
    fn aux_settings_round_trip_through_a_cfg_view() {
        // The retained aux settings rebuild a cfg view that reproduces the same
        // aux args on a respawn — the channel-hop preservation path.
        let s = AuxSettings {
            tx_port: 5620,
            rx_port: 5621,
            fec_k: 2,
            fec_n: 4,
            mcs_index: 3,
        };
        let cfg = s.to_cfg();
        let a = aux_tx_args("wlan1", &cfg, Path::new("/k"), link_id(1, 1));
        assert_eq!(arg_after(&a, "-u"), "5620");
        assert_eq!(arg_after(&a, "-k"), "2");
        assert_eq!(arg_after(&a, "-n"), "4");
        assert_eq!(arg_after(&a, "-M"), "3");
        let r = aux_rx_args("wlan1", &cfg, Path::new("/k"), link_id(1, SLOT_GROUND));
        assert_eq!(arg_after(&r, "-u"), "5621");
    }

    #[test]
    fn every_builder_emits_the_link_id_immediately_after_the_radio_port() {
        // `-i` must follow the `-p` pair in every builder. Position is not
        // cosmetic here: an arg vector that carries `-p` but drops `-i` silently
        // falls back to wfb-ng's default link_id 0, which every un-keyed process
        // in radio range also uses — the exact FEC-thrash collision the fleet
        // addressing exists to prevent. A positional assertion catches a builder
        // that was extended without threading the id.
        let cfg = WfbConfig::default();
        let key = Path::new("/k");
        let own = link_id(7, 9);
        let up = link_id(7, SLOT_GROUND);
        let cases: [(&str, Vec<String>, u32); 6] = [
            ("data_tx", data_tx_args("wlan1", &cfg, key, own), own),
            ("tx_control", tx_control_args("wlan1", &cfg, key, own), own),
            ("rx_control", rx_control_args("wlan1", key, up), up),
            ("stats_rx", stats_rx_args("wlan1", key, own), own),
            ("aux_tx", aux_tx_args("wlan1", &cfg, key, own), own),
            ("aux_rx", aux_rx_args("wlan1", &cfg, key, up), up),
        ];
        for (name, args, want) in cases {
            let p = args
                .iter()
                .position(|x| x == "-p")
                .unwrap_or_else(|| panic!("{name}: no -p"));
            assert_eq!(args[p + 2], "-i", "{name}: -i must follow the -p pair");
            assert_eq!(args[p + 3], want.to_string(), "{name}: wrong link_id");
        }
    }

    #[test]
    fn drone_transmit_and_uplink_receive_carry_different_link_ids() {
        // The asymmetry the whole fleet design rests on: a drone TRANSMITS under
        // its own slot but RECEIVES the ground station's single shared uplink.
        // If the receive side were keyed to the drone's own slot it would listen
        // to a channel_id nobody transmits on and the uplink would go silent;
        // if the transmit side were keyed to slot 0 every drone in the fleet
        // would collide on one channel_id.
        let cfg = WfbConfig::default();
        let key = Path::new("/k");
        let own = link_id(1, 5);
        let up = link_id(1, SLOT_GROUND);
        assert_ne!(own, up);

        let tx = arg_after(&tx_control_args("wlan1", &cfg, key, own), "-i");
        let rx = arg_after(&rx_control_args("wlan1", key, up), "-i");
        assert_eq!(tx, own.to_string());
        assert_eq!(rx, up.to_string());
        assert_ne!(tx, rx, "control TX and control RX must not share a link_id");

        let aux_tx = arg_after(&aux_tx_args("wlan1", &cfg, key, own), "-i");
        let aux_rx = arg_after(&aux_rx_args("wlan1", &cfg, key, up), "-i");
        assert_ne!(aux_tx, aux_rx, "aux TX and aux RX must not share a link_id");

        // The stats receiver is the exception among receivers: it listens to
        // this drone's OWN downlink to measure what it is transmitting.
        assert_eq!(arg_after(&stats_rx_args("wlan1", key, own), "-i"), tx);
    }

    #[test]
    fn no_two_slots_in_a_fleet_share_a_transmit_channel_id() {
        // Every drone's four planes must land on their own channel_id across the
        // whole 24-slot fleet plus the ground station's uplink pair. Asserted at
        // the argv level (what the process actually receives), not just on the
        // `link_id` helper.
        let cfg = WfbConfig::default();
        let key = Path::new("/k");
        let fleet = 1u16;
        let up = link_id(fleet, SLOT_GROUND);
        let mut seen = std::collections::BTreeSet::new();

        // The ground station's two uplink transmitters (control p1, aux p3).
        assert!(seen.insert(channel_id_of(&tx_control_args("wlan1", &cfg, key, up))));
        assert!(seen.insert(channel_id_of(&aux_tx_args("wlan1", &cfg, key, up))));

        for slot in 1..=FLEET_MAX_SLOTS {
            let own = link_id(fleet, slot);
            for args in [
                data_tx_args("wlan1", &cfg, key, own),
                tx_control_args("wlan1", &cfg, key, own),
                aux_tx_args("wlan1", &cfg, key, own),
            ] {
                let ch = channel_id_of(&args);
                assert!(seen.insert(ch), "duplicate channel_id {ch} at slot {slot}");
            }
            // The stats RX is the one receiver that MUST share a channel_id: it
            // listens to this drone's own data plane to measure it. Sharing here
            // is the invariant, not a collision — pin it so a future edit cannot
            // point the stats receiver at some other slot's video.
            assert_eq!(
                channel_id_of(&stats_rx_args("wlan1", key, own)),
                channel_id_of(&data_tx_args("wlan1", &cfg, key, own)),
                "the stats RX must listen on this drone's own data channel_id"
            );
        }
        // 2 ground uplink transmitters + 24 slots x 3 distinct transmit planes.
        assert_eq!(seen.len(), 2 + FLEET_MAX_SLOTS as usize * 3);
    }
}
