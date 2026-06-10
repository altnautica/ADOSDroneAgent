//! Ground-side `WfbRxManager`: the receive run loop.
//!
//! Spawns the three radio C subprocesses the receive side needs and drives the
//! liveness machinery from chunk 2:
//!   - **data RX** `wfb_rx -p 0 -c 127.0.0.1 -u 5599 -K <rx.key> -l 1000 <iface>`
//!     decodes the FEC video stream to the internal fan-out port; stdout carries
//!     the per-second `PKT`/`RX_ANT` stats lines the link monitor parses.
//!   - **rx control** `wfb_rx -p 1 -c 127.0.0.1 -u 5803 -K <rx.key> -l 1000
//!     <iface>` decodes inbound HopAnnounce/Presence frames onto the listener's
//!     port. NOTE: the GS uses **5803** here, not the drone side's 5810. The
//!     ports are mirrored between rigs, so the drone-side `ados_radio::process`
//!     arg builders are NOT reused verbatim.
//!   - **tx control** `wfb_tx -p 1 -u 5810 -K <rx.key> -k 1 -n 2 -B 20 -M <mcs>
//!     <iface>` transmits HopAck/Presence back over the air from the loopback
//!     ingress 5810.
//!
//! Process-group isolation (`setsid`/`killpg`) follows the same discipline as
//! `ados_radio::process::WfbProcess` so the orphan-child bug class is
//! structurally impossible here too; the GS-specific spawn lives in
//! `process_spawn::GsWfbProcess` because the drone crate exposes no generic
//! spawn entry point and the GS arg sets differ. The run loop wires the stats
//! stream into the valid-packet watchdog (its counter/presence/acquirer seams)
//! and the acquirer, writes the ground `wfb-stats.json` sidecar, and runs the
//! stdout-silence zombie watchdog.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, AtomicU32, Ordering};
use std::sync::Arc;

use tokio::sync::Mutex;

use ados_radio::config::WfbConfig;
use ados_radio::link_quality::{LinkQualityMonitor, LinkStats};

/// The safe default regulatory domain applied before monitor-mode bring-up when
/// the config carries none. Matches the air side's `WfbConfig` default and the
/// Python `DEFAULT_REG_DOMAIN`: U-NII-3 / channel 149 is permitted at usable TX
/// power, so the home rendezvous channel is not capped to the kernel's startup
/// domain (the -100 dBm "not permitted" sentinel).
pub const DEFAULT_REG_DOMAIN: &str = "US";

use crate::acquire::{ChannelAcquirer, ChannelSetter, ValidPacketCounter};
use crate::presence::GsPresenceCache;
use crate::process_spawn::{GsWfbProcess, Stdout};
use crate::watchdog::{
    Clock, FileLockedChannelHint, LockedChannelHint, RxProcess, ValidPacketWatchdog,
    RX_HEALTH_SILENCE_THRESHOLD_S,
};
// Re-exported so the run loop can build the shared receive-health seam through
// the same module that owns the stats reader.
pub use crate::watchdog::SharedRxHealth;

/// The regulatory picture the receive sidecar surfaces, symmetric with the
/// drone side. `domain` is the LIVE global country (`None` when unreadable);
/// `verified` is true only when it matched the wanted domain; `enabled_channels`
/// is the domain's permitted set (empty = could not determine). Resolved by
/// `prepare_interface` and stored on the manager so every sidecar write carries
/// the same regulatory truth instead of nothing.
#[derive(Debug, Clone, Default)]
pub struct GsRegSnapshot {
    pub domain: Option<String>,
    pub verified: bool,
    pub enabled_channels: Vec<u8>,
}

/// Internal data-RX egress port (the fan-out reads here). Differs from the
/// drone side's 5601 stats port.
pub const DATA_RX_PORT: u16 = 5599;
/// GS rx-control egress (decoded HopAnnounce/Presence → the listener's port).
pub const RX_CONTROL_PORT: u16 = 5803;
/// GS tx-control loopback ingress (HopAck/Presence out over the air).
pub const TX_CONTROL_PORT: u16 = 5810;
/// wfb stats poll interval: the zombie watchdog cadence.
pub const RX_HEALTH_POLL_INTERVAL_S: f64 = 5.0;

/// Data-plane RX `wfb_rx` args for the ground profile. `-l 1000` enables the
/// per-second stats lines on stdout (without it the monitor stays empty and the
/// link reports disabled). Egress to the internal fan-out port 5599.
pub fn data_rx_args(iface: &str, rx_key: &Path, channel_port: u16) -> Vec<String> {
    vec![
        "-p".into(),
        "0".into(),
        "-c".into(),
        "127.0.0.1".into(),
        "-u".into(),
        channel_port.to_string(),
        "-K".into(),
        rx_key.to_string_lossy().into_owned(),
        "-l".into(),
        "1000".into(),
        iface.into(),
    ]
}

/// GS rx-control `wfb_rx` args: radio_id 1, decode to the listener's port 5803.
pub fn gs_rx_control_args(iface: &str, rx_key: &Path) -> Vec<String> {
    vec![
        "-p".into(),
        "1".into(),
        "-c".into(),
        "127.0.0.1".into(),
        "-u".into(),
        RX_CONTROL_PORT.to_string(),
        "-K".into(),
        rx_key.to_string_lossy().into_owned(),
        "-l".into(),
        "1000".into(),
        iface.into(),
    ]
}

/// GS tx-control `wfb_tx` args: radio_id 1, loopback ingress 5810, light FEC.
pub fn gs_tx_control_args(iface: &str, rx_key: &Path, mcs_index: u8) -> Vec<String> {
    vec![
        "-p".into(),
        "1".into(),
        "-u".into(),
        TX_CONTROL_PORT.to_string(),
        "-K".into(),
        rx_key.to_string_lossy().into_owned(),
        "-k".into(),
        "1".into(),
        "-n".into(),
        "2".into(),
        "-B".into(),
        "20".into(),
        "-M".into(),
        mcs_index.to_string(),
        iface.into(),
    ]
}

/// The cumulative valid-decode packet counter the stats reader updates and the
/// watchdog/acquirer read. Implements both the watchdog's and the acquirer's
/// counter seams.
#[derive(Debug, Default, Clone)]
pub struct SharedValidCounter {
    inner: Arc<AtomicI64>,
}

impl SharedValidCounter {
    pub fn new() -> Self {
        Self::default()
    }
    /// Add this interval's valid-decode count (the per-interval `packets_received`).
    pub fn add(&self, n: i64) {
        if n > 0 {
            self.inner.fetch_add(n, Ordering::SeqCst);
        }
    }
    pub fn get(&self) -> i64 {
        self.inner.load(Ordering::SeqCst)
    }
}

impl ValidPacketCounter for SharedValidCounter {
    fn valid_packets(&self) -> i64 {
        self.get()
    }
}

/// Real channel setter: `iw <iface> set channel <n>` over the monitor interface
/// (the GS-side async sibling of the hop listener's channel set). Returns true
/// when `iw` reports success.
#[derive(Debug, Default)]
pub struct IwChannelSetter;

impl ChannelSetter for IwChannelSetter {
    fn set_channel<'a>(
        &'a self,
        interface: &'a str,
        channel: u8,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
        let iface = interface.to_string();
        Box::pin(async move {
            let out = tokio::process::Command::new("iw")
                .args([&iface, "set", "channel", &channel.to_string()])
                .output()
                .await;
            match out {
                Ok(o) if o.status.success() => true,
                Ok(o) => {
                    tracing::warn!(
                        channel,
                        stderr = %String::from_utf8_lossy(&o.stderr).trim(),
                        "acquire_set_channel_failed"
                    );
                    false
                }
                Err(e) => {
                    tracing::warn!(channel, error = %e, "acquire_set_channel_error");
                    false
                }
            }
        })
    }
}

/// Per-call ceiling on the live-channel `iw info` read so a hung `iw` (driver
/// wedged) cannot stall the stats loop.
const LIVE_CHANNEL_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Read the interface's LIVE channel from `iw <iface> info`, or `None` when `iw`
/// cannot be run or its output carries no channel. The acquirer sweep can land
/// the netdev on a different channel than the configured/operating one, so the
/// sidecar reads the live value rather than reporting the configured channel.
async fn live_channel(iface: &str) -> Option<u8> {
    let out = tokio::time::timeout(
        LIVE_CHANNEL_READ_TIMEOUT,
        tokio::process::Command::new("iw")
            .args([iface, "info"])
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    parse_iface_channel(&String::from_utf8_lossy(&out.stdout))
}

/// Parse the `channel <N>` token out of an `iw <iface> info` body. The line
/// reads e.g. `\tchannel 149 (5745 MHz), width: 20 MHz, …`; the first integer
/// after the `channel` keyword is the channel number. Pure helper, symmetric
/// with the drone-side parser.
fn parse_iface_channel(info: &str) -> Option<u8> {
    for line in info.lines() {
        let mut toks = line.split_whitespace();
        while let Some(tok) = toks.next() {
            if tok == "channel" {
                if let Some(n) = toks.next() {
                    if let Ok(ch) = n.parse::<u8>() {
                        return Some(ch);
                    }
                }
            }
        }
    }
    None
}

/// Monotonic system clock (the production `Clock` seam).
#[derive(Debug, Default)]
pub struct SystemClock {
    epoch: std::sync::OnceLock<std::time::Instant>,
}

impl Clock for SystemClock {
    fn monotonic(&self) -> f64 {
        let start = self.epoch.get_or_init(std::time::Instant::now);
        start.elapsed().as_secs_f64()
    }
}

/// Wraps a live `WfbProcess` so the watchdog can poll liveness + terminate it.
/// The data-RX child is shared (the stats reader takes its stdout; the watchdog
/// holds this handle to assert liveness and request a restart).
pub struct DataRxHandle {
    proc: Mutex<Option<GsWfbProcess>>,
    terminated: AtomicU32,
}

impl DataRxHandle {
    pub fn new(proc: GsWfbProcess) -> Arc<Self> {
        Arc::new(Self {
            proc: Mutex::new(Some(proc)),
            terminated: AtomicU32::new(0),
        })
    }
}

impl RxProcess for DataRxHandle {
    fn is_running(&self) -> bool {
        // try_lock so a liveness poll never blocks behind a kill; treat a
        // contended lock as "alive" (the killer holds it only momentarily).
        match self.proc.try_lock() {
            Ok(mut guard) => guard.as_mut().map(|p| p.is_running()).unwrap_or(false),
            Err(_) => true,
        }
    }
    fn terminate(&self) {
        self.terminated.fetch_add(1, Ordering::SeqCst);
        // Best-effort: take the process out and drop it so its `Drop` fires the
        // synchronous killpg without this fn having to await a wait. Dropping
        // the handle is the structural kill (the whole process group dies); the
        // run loop respawns it on the next generation. A contended lock means a
        // kill is already in flight, so skip.
        if let Ok(mut guard) = self.proc.try_lock() {
            guard.take();
        }
    }
    fn terminate_count(&self) -> u32 {
        self.terminated.load(Ordering::SeqCst)
    }
}

/// The receive plane's top-level lifecycle string for the sidecar `state`
/// field. The drone side writes a sibling top-level `state`; the GS heartbeat
/// reads the sidecar raw, so without this key the GS link block reports a null
/// state. "active" once the data RX is up; "searching" while it is not.
pub const STATE_ACTIVE: &str = "active";
pub const STATE_SEARCHING: &str = "searching";
/// The receive plane refuses to bring up monitor mode / spawn the receive chain
/// until the wanted regulatory domain verifies and the rendezvous channel is
/// permitted. Mirrors the drone-side `reg_blocked` state so the panel shows the
/// regulatory conflict on either rig in one glance.
pub const STATE_REG_BLOCKED: &str = "reg_blocked";

/// Write a minimal `reg_blocked` ground sidecar so the heartbeat + panel show the
/// regulatory conflict while the run loop retries the gate. Carries the reason
/// code and the rendezvous channel under inspection; no receive chain is running,
/// so the link-quality block defaults. Atomic via the Contract E writer.
pub fn write_reg_blocked_sidecar(
    interface: &str,
    chipset: Option<&str>,
    channel: u8,
    cfg: &WfbConfig,
    reg: &GsRegSnapshot,
    reason: &str,
) {
    let snap = LinkStats::default();
    // The chain is not running, so the live channel cannot be read; report the
    // rendezvous home for actual/rendezvous/operating.
    let channels = GsChannelTruth {
        actual: channel,
        rendezvous: channel,
        operating: channel,
    };
    let mut v = build_gs_stats(
        &snap,
        interface,
        chipset,
        false, // no injection while blocked
        channels,
        reg,
        cfg,
        STATE_REG_BLOCKED,
        crate::acquire::AcquireState::Searching.as_str(),
        false, // not channel-locked
        0.0,   // no valid decodes
        0,     // no reacquire kills
        0,     // no zombie kills
        None,  // no silence window (the chain is not running)
        0.0,   // no inbound video
    );
    if let Some(obj) = v.as_object_mut() {
        obj.insert(
            "reg_block_reason".to_string(),
            serde_json::Value::String(reason.to_string()),
        );
    }
    let _ = crate::sidecars::write_json_atomic(
        std::path::Path::new(crate::paths::WFB_STATS_JSON),
        &v,
        0o644,
    );
}

/// The truthful channel picture the receive sidecar surfaces, symmetric with the
/// drone side. `actual` is the LIVE interface channel; `rendezvous` is the
/// operator's home; `operating` is the runtime channel (== rendezvous unless a
/// coordinated move committed). The GS proves the link by its own valid-decode
/// count, so it has no `rf_unverified` of its own (always false here).
#[derive(Debug, Clone, Copy)]
pub struct GsChannelTruth {
    pub actual: u8,
    pub rendezvous: u8,
    pub operating: u8,
}

/// Build the ground `wfb-stats.json` sidecar payload (the GS-extras the
/// cross-process API + heartbeat read). `profile` is always "ground_station".
#[allow(clippy::too_many_arguments)]
pub fn build_gs_stats(
    snap: &LinkStats,
    interface: &str,
    adapter_chipset: Option<&str>,
    adapter_injection_ok: bool,
    channels: GsChannelTruth,
    reg: &GsRegSnapshot,
    cfg: &WfbConfig,
    state: &str,
    acquire_state: &str,
    channel_locked: bool,
    valid_rx_packets_per_s: f64,
    reacquire_kills: u32,
    rx_zombie_kills: u32,
    rx_silent_seconds: Option<f64>,
    video_inbound_bytes_per_s: f64,
) -> serde_json::Value {
    serde_json::json!({
        // Top-level lifecycle string, mirroring the drone-side sidecar so the GS
        // heartbeat reads a real state instead of null.
        "state": state,
        // The state-machine state under its own key, mirroring the drone side.
        "link_state": state,
        "interface": interface,
        "adapter_chipset": adapter_chipset,
        "adapter_injection_ok": adapter_injection_ok,
        // Back-compat alias: `channel` now reflects the LIVE interface channel.
        "channel": channels.actual,
        "actual_channel": channels.actual,
        "rendezvous_channel": channels.rendezvous,
        "operating_channel": channels.operating,
        // Live regulatory picture, symmetric with the drone side.
        "reg_domain": reg.domain,
        "reg_verified": reg.verified,
        "enabled_channels": reg.enabled_channels,
        // The GS proves the link by its own valid-decode count, not by a TX
        // counter, so it is never `rf_unverified` (the transmitting-zero-
        // reception flag is a transmit-side concept). Surfaced for schema
        // symmetry so the panel reads one shape from either rig.
        "rf_unverified": false,
        "tx_power_dbm": cfg.tx_power_dbm,
        // The TX-power ceiling, mirroring the drone-side sidecar key so the panel
        // renders the headroom from either rig's stats.
        "tx_power_max_dbm": cfg.tx_power_max_dbm,
        "topology": cfg.topology,
        "mcs_index": cfg.mcs_index,
        "profile": "ground_station",
        "rx_silent_seconds": rx_silent_seconds,
        "rx_zombie_kills": rx_zombie_kills,
        "acquire_state": acquire_state,
        "channel_locked": channel_locked,
        "valid_rx_packets_per_s": (valid_rx_packets_per_s * 100.0).round() / 100.0,
        "reacquire_kills": reacquire_kills,
        "video_inbound_bytes_per_s": (video_inbound_bytes_per_s * 10.0).round() / 10.0,
        // Link-quality block (parity with the air side).
        "rssi_dbm": snap.rssi_dbm,
        // Noise floor, mirroring the drone-side sidecar key.
        "noise_dbm": snap.noise_dbm,
        "snr_db": snap.snr_db,
        "packets_received": snap.packets_received,
        "packets_lost": snap.packets_lost,
        "fec_recovered": snap.fec_recovered,
        "fec_failed": snap.fec_failed,
        "bitrate_kbps": snap.bitrate_kbps,
        "loss_percent": snap.loss_percent,
        "timestamp": snap.timestamp,
    })
}

/// Resolve the rx key path used by every receive subprocess. The data RX, both
/// control planes, and the stats decode all use the same `rx.key` (wfb-ng key
/// files carry both crypto_box halves so one file authenticates frames in both
/// directions).
fn rx_key_path() -> PathBuf {
    PathBuf::from(ados_radio::paths::WFB_RX_KEY)
}

/// The receive manager. Holds the config + the shared liveness state the run
/// loop wires together.
pub struct WfbRxManager {
    config: WfbConfig,
    interface: String,
    channel: u8,
    selected_chipset: Option<String>,
    adapter_injection_ok: bool,
    /// The regulatory-permitted channel set for the receive interface, resolved
    /// once `prepare_interface` has applied the domain + read the wiphy back.
    /// Empty until then (and on a board where the set cannot be determined); the
    /// acquirer treats an empty set as "do not restrict".
    enabled_channels: BTreeSet<u8>,
    /// The regulatory picture (live domain + verified + permitted set) resolved
    /// by `prepare_interface`, surfaced on every sidecar write so the panel sees
    /// the same regulatory truth as the drone. Default (unknown) until prepared.
    reg_snapshot: GsRegSnapshot,
}

impl WfbRxManager {
    pub fn new(config: WfbConfig) -> Self {
        let channel = config.channel;
        let interface = config.interface.clone();
        Self {
            config,
            interface,
            channel,
            selected_chipset: None,
            adapter_injection_ok: false,
            enabled_channels: BTreeSet::new(),
            reg_snapshot: GsRegSnapshot::default(),
        }
    }

    /// The regulatory picture resolved by `prepare_interface`. Default (unknown)
    /// until that runs.
    pub fn reg_snapshot(&self) -> &GsRegSnapshot {
        &self.reg_snapshot
    }

    /// The rendezvous (meeting) channel for this receive plane — the operator's
    /// home, or the optional rendezvous pin. Both rigs derive it identically.
    pub fn rendezvous_channel(&self) -> u8 {
        self.config.rendezvous_channel()
    }

    pub fn interface(&self) -> &str {
        &self.interface
    }

    pub fn channel(&self) -> u8 {
        self.channel
    }

    /// The regulatory-permitted channel set resolved by `prepare_interface`.
    /// Empty until that runs, or on a board where the wiphy channel list could
    /// not be read.
    pub fn enabled_channels(&self) -> &BTreeSet<u8> {
        &self.enabled_channels
    }

    /// Set the selected-adapter identity (the HAL detect path stays in Python;
    /// the run loop sets these when an adapter is chosen so the sidecar carries
    /// the same stranded-link signal the manager holds).
    pub fn set_adapter(&mut self, chipset: Option<String>, injection_ok: bool) {
        self.selected_chipset = chipset;
        self.adapter_injection_ok = injection_ok;
    }

    /// Adopt the receive interface the run loop resolved (the auto-detected RTL
    /// injection adapter, or the operator's `video.wfb.interface` override). The
    /// receive chain and the sidecar both read `self.interface`, so the run loop
    /// sets it here once an adapter is chosen instead of relying on a config
    /// value that is empty when no external detector populated it.
    pub fn set_interface(&mut self, iface: String) {
        self.interface = iface;
    }

    /// Bring `iface` to the receive-ready state BEFORE the wfb_rx spawn, in the
    /// order the kernel requires:
    ///
    ///  1. **Regulatory gate first** (`iw reg set` + verify). The kernel maps the
    ///     permitted channel set and the per-channel TX-power ceiling when the
    ///     driver initialises, so a domain set after monitor-mode bring-up is too
    ///     late and leaves the home channel (149 / 5745 MHz) capped to the
    ///     startup domain's limits — zero injected frames, the -100 dBm "not
    ///     permitted" sentinel. This is a global per-phy call, so it needs no
    ///     interface; an empty/None config value falls back to the safe default.
    ///     Under the strict (default) gate a verify failure returns `Err` and the
    ///     run loop parks in `reg_blocked` rather than spawning the receive chain
    ///     on a capped radio; the lab escape hatch proceeds best-effort.
    ///  2. **Channel readiness assertion**: once the wiphy is known, assert the
    ///     rendezvous channel is in the enabled set and is non-DFS. A mismatch
    ///     returns `Err` under the strict gate.
    ///  3. **Monitor mode** on the interface the run loop resolved (the
    ///     auto-detected RTL injection adapter or the operator's config
    ///     override). `set_monitor_mode_verified` re-asserts it with the 4×
    ///     verify retry and guards the operator's control path so it can never
    ///     sever the management link.
    ///  4. **TX power** on the monitor interface. Without it the dongle runs at
    ///     the driver default (~17-20 dBm) and risks brownout on a host-VBUS USB
    ///     topology — the same guard the air side applies.
    ///  5. **Channel set** to the rendezvous home. wfb_rx receives on whatever
    ///     channel the netdev is set to; it does not retune itself.
    ///
    /// As a side effect it reads the wiphy's enabled channel set back so the
    /// acquirer can intersect its sweep candidates with what this domain permits.
    /// Returns `Err(RegError)` only when the regulatory gate fails under the
    /// strict gate; the monitor / TX-power / channel sub-steps remain best-effort
    /// (a failed sub-step is logged and the chain still spawns).
    pub async fn prepare_interface(
        &mut self,
        iface: &str,
    ) -> Result<(), ados_radio::adapter::RegError> {
        // 1. Regulatory domain set + verify, before anything touches monitor
        // mode. A global per-phy call; needs no interface.
        let reg_domain = self
            .config
            .reg_domain
            .clone()
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| DEFAULT_REG_DOMAIN.to_string());
        let strict = self.config.reg_gate_strict;
        if let Err(e) = ados_radio::adapter::set_reg_domain(&reg_domain).await {
            if strict {
                tracing::error!(
                    interface = iface,
                    domain = %reg_domain,
                    reason = e.reason_code(),
                    "ground_wfb_reg_gate_blocked"
                );
                return Err(e);
            }
            tracing::warn!(
                interface = iface,
                domain = %reg_domain,
                reason = e.reason_code(),
                "ground_wfb_reg_gate_proceeding_best_effort"
            );
        }

        // Read back the regulatory-permitted channel set + the DFS set so the
        // acquirer can intersect its sweep and the gate can assert the home
        // channel. An empty enabled set means "could not determine".
        self.enabled_channels = ados_radio::adapter::enabled_channels(iface).await;
        let dfs = ados_radio::adapter::dfs_channels(iface).await;

        // 2. Assert the rendezvous channel is enabled + non-DFS now the wiphy is
        // known, before any monitor / channel operation. Both rigs derive the
        // rendezvous channel identically from the operator config.
        let rendezvous_ch = self.config.rendezvous_channel();
        if let Err(e) = ados_radio::adapter::assert_reg_ready(
            rendezvous_ch,
            &self.enabled_channels,
            &dfs,
            self.config.dfs_allowed,
        ) {
            if strict {
                tracing::error!(
                    interface = iface,
                    channel = rendezvous_ch,
                    reason = e.reason_code(),
                    "ground_wfb_reg_gate_channel_blocked"
                );
                return Err(e);
            }
            tracing::warn!(
                interface = iface,
                channel = rendezvous_ch,
                reason = e.reason_code(),
                "ground_wfb_reg_gate_channel_proceeding_best_effort"
            );
        }

        if self.enabled_channels.is_empty() {
            tracing::info!(interface = iface, "ground_wfb_enabled_channels_unknown");
        } else {
            tracing::info!(
                interface = iface,
                channels = ?self.enabled_channels,
                "ground_wfb_enabled_channels"
            );
        }

        // Capture the regulatory picture (live domain + verified + permitted set)
        // so every sidecar write surfaces the same truth as the drone side. The
        // live read is a cheap `iw reg get`; the wanted domain is the resolved
        // `reg_domain` the gate just applied.
        let reg_status = ados_radio::adapter::read_reg_status(&reg_domain).await;
        self.reg_snapshot = GsRegSnapshot {
            domain: reg_status.domain,
            verified: reg_status.verified,
            enabled_channels: self.enabled_channels.iter().copied().collect(),
        };

        // 3. Monitor mode on the config-supplied interface (the Python override
        // path). 4× verify retry + the control-iface guard live in the helper.
        if !ados_radio::adapter::set_monitor_mode_verified(iface, 4).await {
            tracing::warn!(interface = iface, "ground_wfb_monitor_not_verified");
        }

        // 4. TX power, before the wfb_rx spawn.
        if ados_radio::adapter::set_tx_power(iface, self.config.tx_power_dbm)
            .await
            .is_none()
        {
            tracing::warn!(
                interface = iface,
                requested = self.config.tx_power_dbm,
                "ground_wfb_txpower_not_applied"
            );
        }

        // 5. Tune the netdev to the rendezvous home channel.
        if IwChannelSetter.set_channel(iface, self.channel).await {
            tracing::info!(
                interface = iface,
                channel = self.channel,
                "ground_wfb_channel_set"
            );
        } else {
            tracing::warn!(
                interface = iface,
                channel = self.channel,
                "ground_wfb_channel_set_failed"
            );
        }

        Ok(())
    }

    /// Spawn the receive subprocesses for `iface` and return their handles. The
    /// data RX has its stdout piped for the stats reader; both control planes
    /// log stderr to truncated files via `WfbProcess`. Adapter detection and
    /// monitor-mode setup stay in Python and are assumed already applied to
    /// `iface`.
    pub async fn spawn_receive_chain(
        &self,
        iface: &str,
    ) -> std::io::Result<(GsWfbProcess, GsWfbProcess, GsWfbProcess)> {
        let rx_key = rx_key_path();
        // Data RX: stdout piped for the stats reader, in its own process group.
        let data_rx = GsWfbProcess::spawn(
            "wfb_rx",
            &data_rx_args(iface, &rx_key, DATA_RX_PORT),
            Stdout::Piped,
            None,
        )
        .await?;
        let rx_control = GsWfbProcess::spawn(
            "wfb_rx",
            &gs_rx_control_args(iface, &rx_key),
            Stdout::Null,
            Some("/run/ados/wfb-gs-rx-control.log"),
        )
        .await?;
        let tx_control = GsWfbProcess::spawn(
            "wfb_tx",
            &gs_tx_control_args(iface, &rx_key, self.config.mcs_index),
            Stdout::Null,
            Some("/run/ados/wfb-gs-tx-control.log"),
        )
        .await?;
        Ok((data_rx, rx_control, tx_control))
    }

    /// Build the chunk-2 watchdog for this receive generation, wired to the
    /// shared counter + presence cache + a fresh acquirer over the supplied
    /// channel setter. The caller owns the lifecycle (one generation per
    /// `wfb_rx` spawn, matching the Python "fresh acquirer each run").
    pub fn build_watchdog(
        &self,
        counter: SharedValidCounter,
        presence: GsPresenceCache,
        rx: Arc<DataRxHandle>,
        clock: Arc<dyn Clock>,
        setter: Arc<dyn ChannelSetter>,
        hint: Arc<dyn LockedChannelHint>,
    ) -> ValidPacketWatchdog {
        // Feed the regulatory-permitted channel set resolved by
        // `prepare_interface` so the sweep skips channels this domain forbids
        // (those fail `iw set channel` with -22 and waste a dwell). An empty set
        // (board where it could not be read) is passed through as "do not
        // restrict" by the acquirer.
        let enabled = if self.enabled_channels.is_empty() {
            None
        } else {
            Some(self.enabled_channels.clone())
        };
        let acquirer = ChannelAcquirer::new(
            &self.interface,
            &self.config.band,
            Arc::new(counter),
            setter,
            crate::acquire::DWELL_SECONDS,
            crate::acquire::MAX_SWEEP_ROUNDS,
            enabled,
        );
        ValidPacketWatchdog::new(
            &self.interface,
            self.channel,
            self.config.channel, // immutable home
            clock,
            rx,
            Arc::new(presence),
            hint,
            acquirer,
        )
    }
}

/// The production locked-channel hint sink (atomic single-int tmpfs write).
pub fn default_hint() -> Arc<dyn LockedChannelHint> {
    Arc::new(FileLockedChannelHint)
}

/// Read `wfb_rx` stdout line-by-line, feed the link monitor, update the shared
/// counter + LinkStats + the stdout-liveness stamp, and write the ground
/// `wfb-stats.json` sidecar on every parsed line. Ends on EOF (process death)
/// or task abort.
#[allow(clippy::too_many_arguments)]
pub async fn stats_reader_loop(
    stdout: tokio::process::ChildStdout,
    counter: SharedValidCounter,
    link: Arc<Mutex<LinkStats>>,
    last_stdout_at: Arc<Mutex<f64>>,
    clock: Arc<dyn Clock>,
    interface: String,
    channel: u8,
    rendezvous: u8,
    reg: GsRegSnapshot,
    cfg: WfbConfig,
    chipset: Option<String>,
    injection_ok: bool,
    health: Option<SharedRxHealth>,
    zombie_kills: Arc<AtomicU32>,
) {
    use tokio::io::AsyncBufReadExt;
    let mut lines = tokio::io::BufReader::new(stdout).lines();
    let mut mon = LinkQualityMonitor::new();
    // Last successfully-read live channel; seeded to the operating channel so a
    // momentary `iw info` failure keeps reporting the last-known live value.
    let mut last_live_channel = channel;
    while let Ok(Some(line)) = lines.next_line().await {
        *last_stdout_at.lock().await = clock.monotonic();
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let now_iso = now_iso();
        if let Some(snap) = mon.feed_line(line, &now_iso) {
            // Per-interval valid-decode count feeds the cumulative counter the
            // watchdog/acquirer poll.
            counter.add(snap.packets_received);
            let valid_pps = snap.packets_received as f64; // stats interval = 1 s
            let video_bps = snap.bitrate_kbps as f64 * 1000.0 / 8.0;
            // Lock-state surface: decoding valid video on the current channel ==
            // locked even when no sweep ran. This is the GS received-side proof.
            let (channel_locked, acquire_state) = if snap.packets_received > 0 {
                (true, "locked")
            } else {
                (false, "searching")
            };
            // Top-level lifecycle: the data RX is up and producing stats lines, so
            // the plane is active; the per-channel lock state is the finer-grained
            // `acquire_state` above.
            let state = STATE_ACTIVE;
            // Pull the live receive-health counters the watchdogs produce so the
            // sidecar carries real values rather than the previous hardcoded
            // zeros. Absent in tests, where the kills/silence default to zero.
            let (reacquire_kills, rx_silent_seconds) = match &health {
                Some(h) => (h.reacquire_kills(), h.silent_seconds().await),
                None => (0, None),
            };
            let rx_zombie_kills = zombie_kills.load(Ordering::SeqCst);
            *link.lock().await = snap.clone();
            // Truthful channel: read the LIVE interface channel (the acquirer
            // sweep can land it away from the configured/operating channel), with
            // the last-known value held through a transient read failure.
            if let Some(live) = live_channel(&interface).await {
                last_live_channel = live;
            }
            let channels = GsChannelTruth {
                actual: last_live_channel,
                rendezvous,
                operating: channel,
            };
            let payload = build_gs_stats(
                &snap,
                &interface,
                chipset.as_deref(),
                injection_ok,
                channels,
                &reg,
                &cfg,
                state,
                acquire_state,
                channel_locked,
                valid_pps,
                reacquire_kills,
                rx_zombie_kills,
                rx_silent_seconds,
                video_bps,
            );
            let path = Path::new(crate::paths::WFB_STATS_JSON);
            if let Err(e) = crate::sidecars::write_json_atomic(path, &payload, 0o644) {
                tracing::debug!(error = %e, "ground_wfb_stats_persist_failed");
            }
        }
    }
}

/// Stdout-silence zombie watchdog: terminate the data RX when its per-second
/// stats stream stalls for `RX_HEALTH_SILENCE_THRESHOLD_S` while the process is
/// alive (process-liveness alone is never proof of work). Returns when it kills
/// once or the process exits.
pub async fn zombie_watchdog(
    rx: Arc<DataRxHandle>,
    last_stdout_at: Arc<Mutex<f64>>,
    clock: Arc<dyn Clock>,
    kills: Arc<AtomicU32>,
) {
    // Reset the stamp so we don't carry over silence accumulated while the
    // process spawned; give it a full window to start producing stats.
    *last_stdout_at.lock().await = clock.monotonic();
    while rx.is_running() {
        tokio::time::sleep(std::time::Duration::from_secs_f64(
            RX_HEALTH_POLL_INTERVAL_S,
        ))
        .await;
        let silent_for = clock.monotonic() - *last_stdout_at.lock().await;
        if silent_for >= RX_HEALTH_SILENCE_THRESHOLD_S {
            kills.fetch_add(1, Ordering::SeqCst);
            tracing::warn!(
                silent_seconds = silent_for,
                zombie_kills_total = kills.load(Ordering::SeqCst),
                "ground_wfb_rx_zombie_detected"
            );
            rx.terminate();
            *last_stdout_at.lock().await = clock.monotonic();
            return;
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
    fn data_rx_args_match_python() {
        // wfb_rx -p 0 -c 127.0.0.1 -u 5599 -K <rx.key> -l 1000 <iface>
        let a = data_rx_args("wlan1", Path::new("/etc/ados/wfb/rx.key"), DATA_RX_PORT);
        assert_eq!(
            a,
            vec![
                "-p",
                "0",
                "-c",
                "127.0.0.1",
                "-u",
                "5599",
                "-K",
                "/etc/ados/wfb/rx.key",
                "-l",
                "1000",
                "wlan1"
            ]
        );
    }

    #[test]
    fn gs_rx_control_uses_5803_not_drone_side_5810() {
        // The GS rx-control egress is 5803 (the listener's port), the mirror of
        // the drone side's 5810. This is the asymmetry the task flags.
        let a = gs_rx_control_args("wlan1", Path::new("/k"));
        let u = a.iter().position(|x| x == "-u").unwrap();
        assert_eq!(a[u + 1], "5803");
        assert_eq!(a[1], "1"); // radio_id 1
    }

    #[test]
    fn gs_tx_control_uses_5810_and_light_fec() {
        let a = gs_tx_control_args("wlan1", Path::new("/k"), 3);
        let u = a.iter().position(|x| x == "-u").unwrap();
        assert_eq!(a[u + 1], "5810");
        let k = a.iter().position(|x| x == "-k").unwrap();
        assert_eq!(a[k + 1], "1"); // light FEC k=1
        let m = a.iter().position(|x| x == "-M").unwrap();
        assert_eq!(a[m + 1], "3"); // mcs passed through
    }

    #[test]
    fn default_reg_domain_matches_air_side() {
        // The GS default regulatory domain must equal the air side's so both
        // rigs enable the same channel set (the home channel 149 is permitted).
        assert_eq!(DEFAULT_REG_DOMAIN, "US");
        assert_eq!(DEFAULT_REG_DOMAIN, WfbConfig::default().reg_domain.unwrap());
    }

    #[test]
    fn manager_enabled_channels_default_empty() {
        // Until prepare_interface runs, the permitted set is empty (the acquirer
        // reads empty as "do not restrict").
        let m = WfbRxManager::new(WfbConfig::default());
        assert!(m.enabled_channels().is_empty());
    }

    #[test]
    fn shared_counter_accumulates_positive_intervals_only() {
        let c = SharedValidCounter::new();
        assert_eq!(c.get(), 0);
        c.add(5);
        c.add(0); // ignored
        c.add(3);
        assert_eq!(c.get(), 8);
        assert_eq!(c.valid_packets(), 8);
    }

    #[test]
    fn system_clock_is_monotone() {
        let clk = SystemClock::default();
        let a = clk.monotonic();
        let b = clk.monotonic();
        assert!(b >= a);
    }

    #[test]
    fn gs_stats_carries_ground_station_profile_and_extras() {
        let cfg = WfbConfig::default();
        let snap = LinkStats::default();
        // A locked link on a live channel that drifted from the configured home
        // (the acquirer swept to 157), under a verified domain.
        let channels = GsChannelTruth {
            actual: 157,
            rendezvous: 149,
            operating: 149,
        };
        let reg = GsRegSnapshot {
            domain: Some("US".to_string()),
            verified: true,
            enabled_channels: vec![149, 153, 157, 161, 165],
        };
        let v = build_gs_stats(
            &snap,
            "wlan1",
            Some("rtl88x2eu"),
            true,
            channels,
            &reg,
            &cfg,
            STATE_ACTIVE,
            "locked",
            true,
            12.5,
            2,
            1,
            Some(0.3),
            508_000.0,
        );
        assert_eq!(v["profile"], "ground_station");
        assert_eq!(v["interface"], "wlan1");
        assert_eq!(v["adapter_chipset"], "rtl88x2eu");
        assert_eq!(v["adapter_injection_ok"], true);
        // `channel` is the back-compat alias for the LIVE actual channel.
        assert_eq!(v["channel"], 157);
        assert_eq!(v["actual_channel"], 157);
        assert_eq!(v["rendezvous_channel"], 149);
        assert_eq!(v["operating_channel"], 149);
        assert_eq!(v["reg_domain"], "US");
        assert_eq!(v["reg_verified"], true);
        assert_eq!(
            v["enabled_channels"],
            serde_json::json!([149, 153, 157, 161, 165])
        );
        // The GS proves the link by valid decodes, so rf_unverified is never set.
        assert_eq!(v["rf_unverified"], false);
        assert_eq!(v["link_state"], "active");
        assert_eq!(v["acquire_state"], "locked");
        assert_eq!(v["channel_locked"], true);
        assert_eq!(v["reacquire_kills"], 2);
        assert_eq!(v["rx_zombie_kills"], 1);
        assert_eq!(v["valid_rx_packets_per_s"], 12.5);
        assert_eq!(v["video_inbound_bytes_per_s"], 508000.0);
        // mcs/topology/tx_power mirrored from config.
        assert_eq!(v["mcs_index"], cfg.mcs_index);
        assert_eq!(v["topology"], cfg.topology);
    }

    #[test]
    fn gs_stats_carries_top_level_state_noise_and_tx_power_ceiling() {
        // The drone-side sidecar writes a top-level `state`, a `noise_dbm`, and a
        // `tx_power_max_dbm`; the GS heartbeat reads the sidecar raw, so these
        // must be present on the ground sidecar too or the link block reports
        // null for them.
        let cfg = WfbConfig {
            tx_power_max_dbm: 30,
            ..WfbConfig::default()
        };
        let snap = LinkStats {
            noise_dbm: -91.0,
            ..LinkStats::default()
        };
        let channels = GsChannelTruth {
            actual: 149,
            rendezvous: 149,
            operating: 149,
        };
        let v = build_gs_stats(
            &snap,
            "wlan1",
            Some("rtl88x2eu"),
            true,
            channels,
            &GsRegSnapshot::default(),
            &cfg,
            STATE_ACTIVE,
            "searching",
            false,
            0.0,
            0,
            0,
            Some(7.5),
            0.0,
        );
        assert_eq!(v["state"], "active");
        assert_eq!(v["noise_dbm"], -91.0);
        assert_eq!(v["tx_power_max_dbm"], 30);
        assert_eq!(v["rx_silent_seconds"], 7.5);
        // The new keys must never be null on the ground sidecar.
        assert!(!v["state"].is_null());
        assert!(!v["noise_dbm"].is_null());
        assert!(!v["tx_power_max_dbm"].is_null());
        // An unknown regulatory snapshot reports a JSON null domain (not absent)
        // + an empty enabled set, never a fabricated value.
        assert!(v["reg_domain"].is_null());
        assert_eq!(v["reg_verified"], false);
        assert_eq!(v["enabled_channels"], serde_json::json!([]));
    }

    #[test]
    fn reg_blocked_state_string_is_bland_and_stable() {
        // The sidecar surfaces this verbatim; keep it stable and tag-free.
        assert_eq!(STATE_REG_BLOCKED, "reg_blocked");
    }

    #[test]
    fn reg_blocked_sidecar_carries_state_reason_and_no_injection() {
        // The reg-blocked sidecar is written from the run loop when the gate
        // fails; it must surface the bland reason + the blocked state and never
        // claim injection while no receive chain is running. Write to a tmp dir
        // to verify the JSON shape without touching /run.
        let cfg = WfbConfig::default();
        let snap = LinkStats::default();
        let channels = GsChannelTruth {
            actual: 149,
            rendezvous: 149,
            operating: 149,
        };
        // The forbidden domain the global set could not displace, surfaced.
        let reg = GsRegSnapshot {
            domain: Some("BO".to_string()),
            verified: false,
            enabled_channels: vec![],
        };
        let mut v = build_gs_stats(
            &snap,
            "wlan1",
            Some("rtl88x2eu"),
            false,
            channels,
            &reg,
            &cfg,
            STATE_REG_BLOCKED,
            crate::acquire::AcquireState::Searching.as_str(),
            false,
            0.0,
            0,
            0,
            None,
            0.0,
        );
        if let Some(obj) = v.as_object_mut() {
            obj.insert(
                "reg_block_reason".to_string(),
                serde_json::Value::String("phy_override".to_string()),
            );
        }
        assert_eq!(v["state"], "reg_blocked");
        assert_eq!(v["reg_block_reason"], "phy_override");
        assert_eq!(v["adapter_injection_ok"], false);
        assert_eq!(v["channel_locked"], false);
        assert_eq!(v["valid_rx_packets_per_s"], 0.0);
        assert_eq!(v["profile"], "ground_station");
        // The live conflict is visible in the reg block sidecar.
        assert_eq!(v["reg_domain"], "BO");
        assert_eq!(v["reg_verified"], false);
    }

    #[test]
    fn parse_iface_channel_reads_channel_token() {
        // The live-channel readback the stats loop uses for `actual_channel`.
        let info = "Interface wlan0\n\tifindex 5\n\ttype monitor\n\
                    \tchannel 149 (5745 MHz), width: 20 MHz, center1: 5745 MHz\n";
        assert_eq!(parse_iface_channel(info), Some(149));
        let other = "Interface wlan0\n\tchannel 44 (5220 MHz), width: 20 MHz\n";
        assert_eq!(parse_iface_channel(other), Some(44));
    }

    #[test]
    fn parse_iface_channel_no_channel_is_none() {
        assert_eq!(
            parse_iface_channel("Interface wlan0\n\ttype managed\n"),
            None
        );
        assert_eq!(parse_iface_channel(""), None);
        assert_eq!(parse_iface_channel("\tchannel\n"), None);
    }
}
