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

/// Watch `wfb_tx` TX liveness. Returns when `wfb_tx` should be killed (the
/// caller then kills it via `WfbTxProcess::kill()` and respawns).
/// Also returns when `cancel` is notified.
pub async fn tx_health_watchdog(
    iface: &str,
    pid: u32,
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

        let tx_bytes = read_tx_bytes(iface).await.unwrap_or(prev.tx_bytes);
        let rchar = read_rchar(pid).await.unwrap_or(prev.rchar);
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
}
