//! Rule-37 liveness watchdogs for the drone's `wfb_tx` planes.
//!
//! Every judgement here reads a transmitter's OWN per-second stats line (the
//! [`TxPlaneCounters`] its `WfbProcess` folds its stdout into). The kernel does
//! not see this work: `wfb_tx` reads its UDP ingress with `recvmsg` and injects
//! with `sendmsg`, neither on the vfs path `/proc/<pid>/io` accounts, and the RTL
//! monitor netdev's byte counters are not a reliable primary signal.
//!
//! 1. **TX health watchdog**: the data plane must keep printing stats lines, and
//!    bytes offered on its ingress must reach the radio, inside a rolling 30 s
//!    window. A silent loop, or ingress with no injection, is a stall; if the
//!    PHY reads back muted the caller runs a PHY recovery instead of a kill. No
//!    ingress at all is an idle encoder — logged, never killed.
//!
//! 2. **Video receive-queue watchdog**: reads the UDP 5600 kernel rx_queue
//!    depth from `/proc/net/udp` every 5s. If the queue exceeds 256 KiB
//!    continuously for 15s AND `wfb_tx` is reading nothing (its ingress byte
//!    total flat), it is wedged reading from the socket — kill it. A deep queue
//!    that IS being drained is backpressure, not a wedge: the encoder is
//!    offering more than the link can carry, and a kill neither drains it nor
//!    slows the encoder, so that case is logged and left alone.
//!
//! Both hold the same contract: one counter alone never justifies a kill.

use ados_protocol::shutdown::Shutdown;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

use crate::process::{RadioProcesses, TxPlaneCounters, TxPlaneTotals};
use crate::tx_liveness::{TxLivenessWindow, TxPkt, TxVerdict};

mod aux_liveness;
mod control;
mod recvq;
mod tx_health;

pub use aux_liveness::*;
pub use control::*;
pub use recvq::*;
pub use tx_health::*;

const POLL_INTERVAL: Duration = Duration::from_secs(5);
const TX_SILENCE_THRESHOLD: Duration = Duration::from_secs(30);
const RECVQ_BACKLOG_THRESHOLD_BYTES: u64 = 256 * 1024;
const RECVQ_SUSTAINED_THRESHOLD: Duration = Duration::from_secs(15);
/// Log "upstream silent" at most once per this interval.
const UPSTREAM_SILENT_LOG_INTERVAL: Duration = Duration::from_secs(300);
/// Log a backed-up-but-draining video queue at most once per this interval.
/// Shorter than the upstream-silent interval because backpressure is actionable
/// (lower the encoder ceiling, or raise the modulation rate) rather than merely
/// informational, but still slow enough not to flood the log store.
const BACKPRESSURE_LOG_INTERVAL: Duration = Duration::from_secs(60);

/// Watchdog kill/stall counters surfaced on `wfb-stats.json`. The heartbeat
/// reads a shared handle to these on its 2 s cadence, so the GCS panel sees the
/// same churn numbers the Python `get_status` reports. Names map directly:
/// `tx_zombie_kills` ← the TX-health stall kills, `tx_video_stall_kills` ← the
/// video receive-queue backlog kills, `tx_video_stalled` ← the live "the video
/// queue is currently backed up" flag, `tx_video_recvq_bytes` ← the last
/// observed UDP 5600 receive-queue depth.
#[derive(Debug, Default, Clone, Copy)]
pub struct WatchdogCounters {
    pub tx_zombie_kills: u64,
    pub tx_video_stall_kills: u64,
    pub tx_video_stalled: bool,
    /// Live "the video queue is deep but `wfb_tx` is still draining it" flag:
    /// the encoder is offering more than the link carries. Distinct from
    /// `tx_video_stalled`, which means nothing is draining at all. The adaptive
    /// bitrate ladder reads this as its congestion signal, which is the only
    /// closed-loop feedback available on a drone — it transmits its own downlink
    /// and cannot hear it, so it has no loss or RSSI sample to work from.
    pub tx_video_backpressured: bool,
    pub tx_video_recvq_bytes: u64,
    /// Live PHY-mute flag (the heartbeat sets it each tick): the TX PHY reads
    /// back at the muted not-permitted floor, so wfb_tx injects but radiates
    /// nothing. Surfaced on the radio sidecar/heartbeat so Mission Control shows
    /// a "PHY muted" badge instead of a silent dead link.
    pub phy_muted: bool,
    /// Live TX-power readback in dBm from the heartbeat's `iw dev <if> info`
    /// read, or `None` when it was not read (no bring-up yet, unreadable).
    pub tx_power_readback_dbm: Option<f32>,
}

/// Shared handle to the watchdog counters (mirrors the `LinkStats` share).
pub type CounterHandle = Arc<Mutex<WatchdogCounters>>;

/// Construct a fresh, zeroed counter handle.
pub fn new_counters() -> CounterHandle {
    Arc::new(Mutex::new(WatchdogCounters::default()))
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchdogFired {
    TxStalled,
    RecvqBacklog,
    /// TX is flat while ingress feeds AND the PHY reads back muted (txpower at
    /// the not-permitted floor). The caller must run a PHY-recovery, not kill
    /// wfb_tx — respawning the process cannot un-mute a driver/PHY-level mute.
    PhyMuted,
    /// A control-plane process (the HopAnnounce/beacon transmitter or the
    /// HopAck/link-stats receiver) is alive but its own stats counter stayed
    /// flat for the whole silence window. The caller respawns the group.
    ControlStalled,
    Cancelled,
}

#[cfg(test)]
mod tests;
