//! wfb subprocess manager with process-group isolation.
//!
//! Owns the three radio C subprocesses the WFB TX service forks:
//!   - **data plane** `wfb_tx -p 0 -u 5600 …` (video)
//!   - **tx control** `wfb_tx -p 1 -u 5803 -k 1 -n 2 …` — carries HopAnnounce /
//!     PresenceBeacon OVER THE AIR
//!   - **rx control** `wfb_rx -p 1 -c 127.0.0.1 -u 5810 -l 1000 …` — receives
//!     HopAck off the air onto UDP 5810
//!
//! Every plane runs as an `ados_proc::GroupChild`: its own process group,
//! killed as a whole on `kill()` and on drop, so a wfb binary never outlives
//! its Rust owner (the orphan-in-cgroup bug class is structurally impossible).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use ados_proc::logcap::cap_log;
use ados_proc::{GroupChild, Spec, StderrMode, StdoutMode};

use crate::config::WfbConfig;

mod args;
mod aux_plane;
mod plane;
mod respawn;
mod spawn;
mod tune;

pub use args::*;
use respawn::*;

// Every plane's stderr goes to a log file, emptied at spawn and capped by
// `cap_plane_logs`. A piped stderr that nobody reads fills its 64 KiB kernel
// buffer and `wfb_tx` then blocks inside `fprintf(stderr)`: it prints a
// dropped-packets line every stats interval while the link is congested, so an
// undrained pipe wedges the transmitter within the hour.
const DATA_TX_LOG: &str = "/run/ados/wfb-drone-data-tx.log";
const TX_CONTROL_LOG: &str = "/run/ados/wfb-drone-tx-control.log";
const RX_CONTROL_LOG: &str = "/run/ados/wfb-drone-rx-control.log";
const AUX_TX_LOG: &str = "/run/ados/wfb-drone-aux-tx.log";
const AUX_RX_LOG: &str = "/run/ados/wfb-drone-aux-rx.log";

/// Cumulative totals from one `wfb_tx` plane's own per-second `PKT` stats line
/// (`vendor/wfb-ng/src/tx.cpp:796-797`). The counts on the wire are per
/// interval; they are summed here. One handle per plane, shared by every process
/// that plane ever runs, so a respawn keeps counting on the same totals.
///
/// These are the watchdogs' delta counters. They are the transmitter's own
/// account of its work: `/proc/<pid>/io` never sees it (`wfb_tx` reads its UDP
/// ingress with `recvmsg` and injects with `sendmsg`, neither on the vfs path
/// the kernel's `rchar`/`wchar` accounting follows), and the RTL monitor
/// netdev's byte counters are not a reliable primary signal.
#[derive(Debug, Default)]
pub struct TxPlaneCounters {
    lines: AtomicU64,
    bytes_in: AtomicU64,
    bytes_injected: AtomicU64,
}

/// A point-in-time read of a [`TxPlaneCounters`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TxPlaneTotals {
    /// Stats lines printed. `wfb_tx` prints one every second from the top of its
    /// poll loop whether or not traffic moved, so this proves the loop turns.
    pub lines: u64,
    /// Bytes read off the plane's UDP ingress.
    pub bytes_in: u64,
    /// Bytes successfully injected onto the radio.
    pub bytes_injected: u64,
}

/// A live wfb child (data or control plane) in its own process group.
pub struct WfbProcess {
    child: GroupChild,
    /// The task draining a transmitter's stdout stats stream into its plane's
    /// [`TxPlaneCounters`]. Owned by the process so every spawn path — first
    /// bring-up, hop respawn, data-plane retune respawn, aux open and restart —
    /// is measured, and aborted with it.
    stats_task: Option<tokio::task::JoinHandle<()>>,
}

/// Size at which a plane's stderr log is emptied. The files live on the RAM
/// backed `/run`, and a congested `wfb_tx` prints a dropped-packets line every
/// stats interval for as long as the process runs.
pub const PLANE_LOG_CAP_BYTES: u64 = 1024 * 1024;

/// Empty every plane stderr log that has grown past [`PLANE_LOG_CAP_BYTES`].
/// Called on the heartbeat cadence; a missing file is skipped.
pub async fn cap_plane_logs() {
    for path in [
        DATA_TX_LOG,
        TX_CONTROL_LOG,
        RX_CONTROL_LOG,
        AUX_TX_LOG,
        AUX_RX_LOG,
    ] {
        if let Err(e) = cap_log(Path::new(path), PLANE_LOG_CAP_BYTES).await {
            tracing::debug!(path, error = %e, "plane_log_cap_failed");
        }
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
    /// The task reading the rx-control receiver's stdout stats stream into the
    /// uplink link block. `None` only when the stdout handle could not be
    /// taken, which would leave the control plane unmeasured.
    stats_reader: Option<tokio::task::JoinHandle<()>>,
    /// The uplink link block both uplink receivers (rx-control and aux rx) feed.
    /// Replaced on a whole-group respawn: a hop moves to a new channel, so a
    /// measurement held from the old one no longer describes the link.
    uplink: Arc<UplinkFeed>,
    /// Each transmit plane's cumulative stats totals, fed by the reader every
    /// `WfbProcess` of that plane owns. Process-lifetime handles: a hop respawn,
    /// a retune respawn or an aux restart keeps counting on the same totals, so
    /// the watchdogs judge deltas without ever re-resolving a PID.
    data_stats: Arc<TxPlaneCounters>,
    control_stats: Arc<TxPlaneCounters>,
    aux_stats: Arc<TxPlaneCounters>,
    /// Stats lines the rx-control `wfb_rx` printed. It dumps a `PKT` line every
    /// `-l 1000` interval unconditionally (even with no RF arriving), so a live
    /// receiver advances it once a second.
    rx_stats_lines: Arc<AtomicU64>,
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
    /// key for every process the drone TRANSMITS on. Retained rather than
    /// recomputed so a channel
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

/// The drone's uplink link block, fed by every uplink receiver's stats reader.
/// The aggregator lock is held while the block is published, so two readers can
/// never publish out of order.
pub struct UplinkFeed {
    agg: tokio::sync::Mutex<crate::link_quality::UplinkAggregator>,
    link: Arc<tokio::sync::Mutex<crate::link_quality::LinkStats>>,
}

#[cfg(test)]
mod tests;
