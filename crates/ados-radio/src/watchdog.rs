//! Rule-37 TX liveness watchdogs for `wfb_tx`.
//!
//! Two independent watchers mirror `manager.py:1141-1424`:
//!
//! 1. **TX health watchdog**: polls `/sys/class/net/<iface>/statistics/tx_bytes`
//!    every 5s. If the counter is flat for 30s while ingress IS feeding
//!    (confirmed via `/proc/<pid>/io rchar` or `/proc/net/udp` rx_queue),
//!    `wfb_tx` has silently stalled — kill it so the manager respawns it.
//!    If ingress is also flat, the video encoder is idle; log once per 5min
//!    but do not kill.
//!
//! 2. **Video receive-queue watchdog**: reads the UDP 5600 kernel rx_queue
//!    from `/proc/net/udp` every 5s. If the queue exceeds 256 KiB continuously
//!    for 15s, `wfb_tx` is wedged reading from the socket — kill it.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

use crate::process::RadioProcesses;

const POLL_INTERVAL: Duration = Duration::from_secs(5);
const TX_SILENCE_THRESHOLD: Duration = Duration::from_secs(30);
const RECVQ_BACKLOG_THRESHOLD_BYTES: u64 = 256 * 1024;
const RECVQ_SUSTAINED_THRESHOLD: Duration = Duration::from_secs(15);
/// Log "upstream silent" at most once per this interval.
const UPSTREAM_SILENT_LOG_INTERVAL: Duration = Duration::from_secs(300);

/// Snapshot used to detect counter progress.
#[derive(Debug, Default, Clone)]
struct TxSnapshot {
    tx_bytes: u64,
    rchar: u64,
    udp_rx_queue: u64,
}

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
    pub tx_video_recvq_bytes: u64,
    /// Live PHY-mute flag (the heartbeat sets it each tick): the TX PHY reads
    /// back at the muted not-permitted floor, so wfb_tx injects but radiates
    /// nothing. Surfaced on the radio sidecar/heartbeat so Mission Control shows
    /// a "PHY muted" badge instead of a silent dead link.
    pub phy_muted: bool,
}

/// Shared handle to the watchdog counters (mirrors the `LinkStats` share).
pub type CounterHandle = Arc<Mutex<WatchdogCounters>>;

/// Construct a fresh, zeroed counter handle.
pub fn new_counters() -> CounterHandle {
    Arc::new(Mutex::new(WatchdogCounters::default()))
}

/// Resolves the **currently-running** data-plane `wfb_tx` PID.
///
/// The data-tx process is killed and respawned (with a NEW PID) whenever an
/// FEC/MCS/manual-tier change or the adaptive controller retunes the radio. If
/// the watchdog kept reading `/proc/<old_pid>/io` it would either read `None`
/// on a dead PID (freezing the `rchar` ingress signal) or, worse, read garbage
/// from an unrelated process that the OS recycled the old PID onto. Resolving
/// the live PID each poll keeps the ingress signal pinned to the live process.
pub trait LivePid: Send + Sync {
    /// The live data-tx PID, or `None` when it cannot be determined (the process
    /// has exited and not yet respawned). The watchdog treats `None`/`0` as
    /// "skip the `rchar` read this tick" rather than freezing the previous value.
    fn data_tx_pid(&self) -> impl std::future::Future<Output = Option<u32>> + Send;
}

impl LivePid for Arc<Mutex<RadioProcesses>> {
    async fn data_tx_pid(&self) -> Option<u32> {
        self.lock().await.data_tx_pid()
    }
}

/// Watch `wfb_tx` TX liveness. Returns when `wfb_tx` should be killed (the
/// caller then kills it via `WfbTxProcess::kill()` and respawns).
/// Also returns when `cancel` is notified.
///
/// `pid_source` resolves the **live** data-tx PID each poll rather than a
/// captured constant: the data plane is respawned with a new PID on every
/// FEC/MCS/tier change, so a one-shot PID would aim the `rchar` ingress read at
/// a dead (or OS-recycled) process. The dual-check contract is unchanged — an
/// advancing iface `tx_bytes` (the TX side) is necessary but never sufficient;
/// the `rchar`/UDP receive-queue ingress signal stays the independent
/// confirmation that the encoder is actually feeding `wfb_tx`.
pub async fn tx_health_watchdog<P: LivePid>(
    iface: &str,
    pid_source: P,
    counters: CounterHandle,
    cancel: std::sync::Arc<tokio::sync::Notify>,
) -> WatchdogFired {
    let mut last_progress = Instant::now();
    let mut last_upstream_silent_log = Instant::now() - UPSTREAM_SILENT_LOG_INTERVAL;
    let mut prev = TxSnapshot::default();

    loop {
        tokio::select! {
            _ = tokio::time::sleep(POLL_INTERVAL) => {}
            _ = cancel.notified() => return WatchdogFired::Cancelled,
        }

        // Resolve the live data-tx PID for THIS tick. A respawn (FEC/MCS/tier/
        // adaptive) hands the data plane a new PID; reading the old one would
        // freeze `rchar` (dead PID → `None`) or read an unrelated recycled
        // process. A `None`/`0` PID (data plane exited, not yet respawned)
        // means we skip the `rchar` read entirely and carry the previous value
        // forward unchanged, so the PID-recycle window can never inject garbage
        // into the ingress signal.
        let pid = pid_source.data_tx_pid().await.unwrap_or(0);

        let tx_bytes = read_tx_bytes(iface).await.unwrap_or(prev.tx_bytes);
        let live_rchar = if pid == 0 {
            None
        } else {
            read_rchar(pid).await
        };
        let rchar = select_rchar(live_rchar, prev.rchar);
        let udp_rx = read_udp_recvq(5600).await.unwrap_or(prev.udp_rx_queue);

        let tx_advancing = tx_bytes > prev.tx_bytes;
        let ingress_advancing = rchar > prev.rchar || udp_rx > prev.udp_rx_queue;

        if tx_advancing {
            last_progress = Instant::now();
        } else if last_progress.elapsed() >= TX_SILENCE_THRESHOLD {
            if ingress_advancing {
                // A flat TX while ingress feeds is a stall — but if the PHY
                // itself is muted (txpower pinned at the not-permitted floor; the
                // RTL8812EU `set type monitor` mute), killing + respawning wfb_tx
                // can NEVER un-mute it: the fault is in the driver/PHY, not the
                // process, so the kill-respawn loops forever with zero effect.
                // Signal PhyMuted so the caller runs a PHY-recovery (re-cycle
                // monitor + channel + txpower) instead of another pointless kill.
                let muted = crate::adapter::read_tx_power(iface)
                    .await
                    .map(|dbm| dbm <= crate::adapter::MUTED_TX_POWER_DBM)
                    .unwrap_or(false);
                if muted {
                    tracing::warn!(
                        iface,
                        pid,
                        elapsed_s = last_progress.elapsed().as_secs(),
                        "wfb_tx_stalled_phy_muted: routing to PHY-recovery, not a kill"
                    );
                    return WatchdogFired::PhyMuted;
                }
                tracing::warn!(
                    iface,
                    pid,
                    elapsed_s = last_progress.elapsed().as_secs(),
                    "wfb_tx_stalled_kill"
                );
                // A real TX stall while ingress feeds: count it before the
                // caller respawns the radio group.
                counters.lock().await.tx_zombie_kills += 1;
                return WatchdogFired::TxStalled;
            } else {
                // Upstream (video encoder) is silent — don't kill; just log.
                if last_upstream_silent_log.elapsed() >= UPSTREAM_SILENT_LOG_INTERVAL {
                    tracing::info!(iface, "wfb_tx_upstream_silent");
                    last_upstream_silent_log = Instant::now();
                }
            }
        }

        prev = TxSnapshot {
            tx_bytes,
            rchar,
            udp_rx_queue: udp_rx,
        };
    }
}

/// Watch the UDP 5600 kernel receive queue. Returns when the queue has been
/// sustained over 256 KiB for 15s (wfb_tx is not draining its socket). Updates
/// the shared counters with the live `tx_video_stalled` flag, the last observed
/// queue depth, and the stall-kill count on fire.
pub async fn video_recvq_watchdog(
    counters: CounterHandle,
    cancel: std::sync::Arc<tokio::sync::Notify>,
) -> WatchdogFired {
    let mut high_since: Option<Instant> = None;

    loop {
        tokio::select! {
            _ = tokio::time::sleep(POLL_INTERVAL) => {}
            _ = cancel.notified() => return WatchdogFired::Cancelled,
        }
        let q = read_udp_recvq(5600).await.unwrap_or(0);
        {
            let mut c = counters.lock().await;
            c.tx_video_recvq_bytes = q;
            // The live "video queue currently backed up" flag, mirroring the
            // Python `_tx_video_stalled` heartbeat field.
            c.tx_video_stalled = q > RECVQ_BACKLOG_THRESHOLD_BYTES;
        }
        if q > RECVQ_BACKLOG_THRESHOLD_BYTES {
            let since = high_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= RECVQ_SUSTAINED_THRESHOLD {
                tracing::warn!(queue_bytes = q, "wfb_tx_video_recvq_kill");
                counters.lock().await.tx_video_stall_kills += 1;
                return WatchdogFired::RecvqBacklog;
            }
        } else {
            high_since = None;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchdogFired {
    TxStalled,
    RecvqBacklog,
    /// TX is flat while ingress feeds AND the PHY reads back muted (txpower at
    /// the not-permitted floor). The caller must run a PHY-recovery, not kill
    /// wfb_tx — respawning the process cannot un-mute a driver/PHY-level mute.
    PhyMuted,
    Cancelled,
}

/// Read `/sys/class/net/<iface>/statistics/tx_bytes`.
async fn read_tx_bytes(iface: &str) -> Option<u64> {
    let path = format!("/sys/class/net/{}/statistics/tx_bytes", iface);
    let raw = tokio::fs::read_to_string(&path).await.ok()?;
    raw.trim().parse().ok()
}

/// Pick the `rchar` value to carry into this tick's snapshot.
///
/// `live` is `Some` only when the live data-tx PID was known AND its
/// `/proc/<pid>/io` read succeeded. When the PID is unknown/recycling-risk
/// (the data plane just respawned and we resolved `None`/`0`) or the read
/// failed, fall back to the previous value rather than treating a missing read
/// as ingress progress — this keeps the recycle window from injecting garbage
/// and never *manufactures* an advancing ingress signal.
fn select_rchar(live: Option<u64>, prev: u64) -> u64 {
    live.unwrap_or(prev)
}

/// Read the `rchar` field from `/proc/<pid>/io` (cumulative bytes read by the
/// process — the primary signal that the video encoder is feeding `wfb_tx`).
async fn read_rchar(pid: u32) -> Option<u64> {
    let path = format!("/proc/{}/io", pid);
    let raw = tokio::fs::read_to_string(&path).await.ok()?;
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("rchar:") {
            return rest.trim().parse().ok();
        }
    }
    None
}

/// Read the UDP receive-queue depth for a given port from `/proc/net/udp`.
/// Returns the queue depth in bytes (hex `rx_queue` field from the kernel).
async fn read_udp_recvq(port: u16) -> Option<u64> {
    // The port is in hex in /proc/net/udp, big-endian.
    let port_hex = format!("{:04X}", port);
    let raw = tokio::fs::read_to_string("/proc/net/udp").await.ok()?;
    for line in raw.lines().skip(1) {
        // Format: sl  local_address rem_address   st tx_queue:rx_queue ...
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 5 {
            continue;
        }
        // local_address is "addr:port" in hex; we match the port suffix.
        if cols[1].ends_with(&format!(":{}", port_hex)) {
            // tx_queue:rx_queue — we want rx_queue (right side of colon).
            if let Some(q) = cols[4].split(':').nth(1) {
                return u64::from_str_radix(q, 16).ok();
            }
        }
    }
    Some(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recvq_threshold_is_256kib() {
        assert_eq!(RECVQ_BACKLOG_THRESHOLD_BYTES, 262144);
    }

    #[test]
    fn tx_silence_threshold_is_30s() {
        assert_eq!(TX_SILENCE_THRESHOLD.as_secs(), 30);
    }

    #[test]
    fn poll_interval_is_5s() {
        assert_eq!(POLL_INTERVAL.as_secs(), 5);
    }

    #[test]
    fn recvq_sustained_threshold_is_15s() {
        assert_eq!(RECVQ_SUSTAINED_THRESHOLD.as_secs(), 15);
    }

    #[test]
    fn fresh_counters_are_zeroed() {
        let c = WatchdogCounters::default();
        assert_eq!(c.tx_zombie_kills, 0);
        assert_eq!(c.tx_video_stall_kills, 0);
        assert_eq!(c.tx_video_recvq_bytes, 0);
        assert!(!c.tx_video_stalled);
    }

    #[tokio::test]
    async fn counter_handle_is_shareable_and_mutable() {
        let counters = new_counters();
        let clone = counters.clone();
        clone.lock().await.tx_zombie_kills += 1;
        clone.lock().await.tx_video_stalled = true;
        let c = *counters.lock().await;
        assert_eq!(c.tx_zombie_kills, 1);
        assert!(c.tx_video_stalled);
    }

    #[test]
    fn select_rchar_carries_prev_when_pid_unknown() {
        // A respawn (or a recycle-risk) resolves `None` for the live read: the
        // watchdog must carry the previous value forward, NOT treat a missing
        // read as zero or as progress.
        assert_eq!(select_rchar(None, 42), 42);
        assert_eq!(select_rchar(None, 0), 0);
    }

    #[test]
    fn select_rchar_uses_live_read_when_available() {
        // A successful read of the live PID overrides the previous snapshot,
        // including a higher value (real ingress progress).
        assert_eq!(select_rchar(Some(100), 42), 100);
        // A live read lower than prev (a respawn reset the per-process counter)
        // is taken as-is — the advancing check (`rchar > prev.rchar`) then sees
        // no progress this tick, which is correct: the new process has not yet
        // read anything, so ingress is genuinely not advancing on its `rchar`.
        assert_eq!(select_rchar(Some(5), 42), 5);
    }

    /// A `LivePid` whose value can change mid-run, simulating a data-tx respawn
    /// handing the data plane a new PID under the watchdog.
    struct FakePid {
        pid: std::sync::atomic::AtomicU32,
    }

    impl FakePid {
        fn new(pid: u32) -> Self {
            Self {
                pid: std::sync::atomic::AtomicU32::new(pid),
            }
        }
        fn respawn_to(&self, pid: u32) {
            self.pid.store(pid, std::sync::atomic::Ordering::SeqCst);
        }
    }

    impl LivePid for std::sync::Arc<FakePid> {
        async fn data_tx_pid(&self) -> Option<u32> {
            match self.pid.load(std::sync::atomic::Ordering::SeqCst) {
                0 => None,
                p => Some(p),
            }
        }
    }

    #[tokio::test]
    async fn live_pid_reflects_a_respawn() {
        // The watchdog resolves the PID per poll through this trait, so a
        // respawn that changes the underlying PID is picked up on the next tick
        // instead of the watchdog being stuck on the original (now dead) PID.
        let src = std::sync::Arc::new(FakePid::new(1234));
        assert_eq!(LivePid::data_tx_pid(&src).await, Some(1234));
        src.respawn_to(5678);
        assert_eq!(LivePid::data_tx_pid(&src).await, Some(5678));
        // A respawn-in-progress window (no live process yet) resolves None, which
        // the watchdog maps to the rchar-skip path via select_rchar.
        src.respawn_to(0);
        assert_eq!(LivePid::data_tx_pid(&src).await, None);
    }

    #[tokio::test]
    async fn tx_health_watchdog_cancels_promptly_with_live_pid_source() {
        // Drive the real watchdog with a fake live-PID source and an immediate
        // cancel: it must honor the cancel arm and return `Cancelled` without
        // panicking, proving the generic `LivePid` plumbing compiles and runs
        // end-to-end. (The full stall/kill paths read real /proc + /sys and are
        // covered on-rig; this guards the wiring + the cancel contract.)
        let src = std::sync::Arc::new(FakePid::new(1));
        let counters = new_counters();
        let cancel = std::sync::Arc::new(tokio::sync::Notify::new());
        cancel.notify_one();
        let fired = tx_health_watchdog("ados-test-nonexistent-iface", src, counters, cancel).await;
        assert_eq!(fired, WatchdogFired::Cancelled);
    }
}
