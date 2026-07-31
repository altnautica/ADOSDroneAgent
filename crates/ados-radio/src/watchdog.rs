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
//!    for 15s AND `wfb_tx` is making no read progress (`/proc/<pid>/io rchar`
//!    flat), it is wedged reading from the socket — kill it. A deep queue that
//!    IS being drained is backpressure, not a wedge: the encoder is offering
//!    more than the link can carry, and a kill neither drains it nor slows the
//!    encoder, so that case is logged and left alone.
//!
//! Both watchdogs therefore hold the same contract: one counter alone never
//! justifies a kill. Flat TX needs confirmed ingress; a deep queue needs
//! confirmed non-drain.

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
/// Log a backed-up-but-draining video queue at most once per this interval.
/// Shorter than the upstream-silent interval because backpressure is actionable
/// (lower the encoder ceiling, or raise the modulation rate) rather than merely
/// informational, but still slow enough not to flood the log store.
const BACKPRESSURE_LOG_INTERVAL: Duration = Duration::from_secs(60);

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

/// Everything the receive-queue watchdog observes about the world outside its
/// own state machine: the two kernel counters it cross-checks, the monotonic
/// clock its sustained-window arithmetic runs on, and the wait between polls.
///
/// The counters live at fixed `/proc` paths, and the clock and the wait came
/// straight from the ambient runtime. That left the loop exercisable only on a
/// board with a real radio attached — nothing could put a deep queue in front of
/// it, hold the read counter flat, and assert on what it concluded, which is
/// precisely the judgement the watchdog exists to make. Behind this seam a
/// scenario is a list of values. [`ProcSignals`] is the production
/// implementation: the same paths, the same cadence, the same clock.
pub trait RecvqSignals: Send + Sync {
    /// Cumulative bytes read by `pid` — the evidence that the data plane is
    /// actually emptying the socket rather than merely still being alive.
    /// `None` when the read fails, which must never be mistaken for progress.
    fn rchar(&self, pid: u32) -> impl std::future::Future<Output = Option<u64>> + Send;

    /// Kernel receive-queue depth in bytes for `port`.
    fn udp_recvq(&self, port: u16) -> impl std::future::Future<Output = Option<u64>> + Send;

    /// Wait one poll interval. Raced against the cancel notification by the
    /// caller, so an implementation that never completes simply parks the
    /// watchdog until it is cancelled.
    fn wait(&self, interval: Duration) -> impl std::future::Future<Output = ()> + Send;

    /// Read the monotonic clock. Called once per poll; every window in the loop
    /// is measured against that single reading.
    fn now(&self) -> Instant;
}

/// The production [`RecvqSignals`]: the real `/proc` counters, the real tokio
/// timer, the real monotonic clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcSignals;

impl RecvqSignals for ProcSignals {
    async fn rchar(&self, pid: u32) -> Option<u64> {
        read_rchar(pid).await
    }

    async fn udp_recvq(&self, port: u16) -> Option<u64> {
        read_udp_recvq(port).await
    }

    async fn wait(&self, interval: Duration) {
        tokio::time::sleep(interval).await;
    }

    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// Watch the UDP 5600 kernel receive queue. Returns when the queue has been
/// sustained over 256 KiB for 15s **while `wfb_tx` is not draining it**.
/// Updates the shared counters with the live `tx_video_stalled` flag, the last
/// observed queue depth, and the stall-kill count on fire.
///
/// A deep queue on its own is not evidence of a wedge, and treating it as such
/// is the Rule-37 error in reverse: process liveness is not proof of work, but
/// neither is a backlog proof of death. Two very different conditions produce
/// the same queue depth:
///
/// - **Wedged**: `wfb_tx` has stopped reading the socket. `rchar` is flat.
///   Killing it is the correct and only recovery.
/// - **Backpressured**: `wfb_tx` is reading as fast as the air allows, but the
///   encoder is offering more than the current MCS and FEC can carry. `rchar`
///   advances. The process is healthy and killing it fixes nothing — it drops
///   the link for the duration of a full radio-group respawn, resets the
///   adaptive bitrate controller to its configured starting rung, and leaves
///   the encoder still over-feeding, so the queue refills and the kill repeats
///   on a fixed period. Observed on a bench rig as 26 consecutive kills at 23s
///   intervals, each costing about 3s of video and taking the auxiliary lane
///   down with it.
///
/// So the ingress signal (`/proc/<pid>/io` `rchar`) is the required second
/// check, exactly as [`tx_health_watchdog`] uses it. Backpressure is logged
/// periodically instead, because the answer to it is to lower the encoder
/// ceiling or raise the modulation rate, not to restart anything.
pub async fn video_recvq_watchdog<P: LivePid>(
    pid_source: P,
    counters: CounterHandle,
    cancel: std::sync::Arc<tokio::sync::Notify>,
) -> WatchdogFired {
    video_recvq_watchdog_with(pid_source, ProcSignals, counters, cancel).await
}

/// [`video_recvq_watchdog`] with its view of the outside world supplied rather
/// than read from `/proc` and the ambient clock. The public entry point above is
/// this function with [`ProcSignals`]; tests drive it with a scripted one.
pub async fn video_recvq_watchdog_with<P: LivePid, S: RecvqSignals>(
    pid_source: P,
    signals: S,
    counters: CounterHandle,
    cancel: std::sync::Arc<tokio::sync::Notify>,
) -> WatchdogFired {
    let mut high_since: Option<Instant> = None;
    let mut prev_rchar: u64 = 0;
    let mut last_backpressure_log = signals.now() - BACKPRESSURE_LOG_INTERVAL;

    loop {
        tokio::select! {
            _ = signals.wait(POLL_INTERVAL) => {}
            _ = cancel.notified() => return WatchdogFired::Cancelled,
        }
        // One clock reading per poll, so every window below is measured against
        // the same instant and a slow tick cannot make two of them disagree.
        let now = signals.now();
        let q = signals.udp_recvq(5600).await.unwrap_or(0);

        // Resolve the live data-tx PID per tick for the same reason
        // `tx_health_watchdog` does: a respawn hands the data plane a new PID,
        // and reading a dead or OS-recycled one would poison the signal. A
        // missing PID carries the previous value forward, which reads as "not
        // draining" — correct, since a data plane that is not running is
        // certainly not emptying the socket.
        let pid = pid_source.data_tx_pid().await.unwrap_or(0);
        let live_rchar = if pid == 0 {
            None
        } else {
            signals.rchar(pid).await
        };
        let rchar = select_rchar(live_rchar, prev_rchar);
        let draining = rchar > prev_rchar;
        prev_rchar = rchar;

        let tick = recvq_tick_decision(q, draining);
        {
            let mut c = counters.lock().await;
            c.tx_video_recvq_bytes = q;
            // Report a stall only for a genuine wedge. A backpressured link is
            // busy, not stalled, and a surface that calls it stalled trains the
            // operator to ignore the flag that also means a real wedge.
            c.tx_video_stalled = tick == RecvqTick::Wedged;
            c.tx_video_backpressured = tick == RecvqTick::Backpressured;
        }

        match tick {
            RecvqTick::Wedged => {
                let since = *high_since.get_or_insert(now);
                if now.saturating_duration_since(since) >= RECVQ_SUSTAINED_THRESHOLD {
                    tracing::warn!(
                        queue_bytes = q,
                        pid,
                        "wfb_tx_video_recvq_kill: queue sustained with no drain progress"
                    );
                    counters.lock().await.tx_video_stall_kills += 1;
                    return WatchdogFired::RecvqBacklog;
                }
            }
            RecvqTick::Backpressured => {
                // The process is working through the backlog, so a kill would
                // interrupt real progress. Reset the wedge timer: a kill must
                // require a fresh uninterrupted window of genuinely-stuck ticks.
                high_since = None;
                if now.saturating_duration_since(last_backpressure_log) >= BACKPRESSURE_LOG_INTERVAL
                {
                    tracing::warn!(
                        queue_bytes = q,
                        pid,
                        "wfb_tx_video_backpressured: draining, offered rate exceeds link capacity"
                    );
                    last_backpressure_log = now;
                }
            }
            RecvqTick::Clear => high_since = None,
        }
    }
}

/// What one receive-queue poll concludes. Split out as a pure decision so the
/// wedge-versus-backpressure rule is testable without a live `/proc`, matching
/// [`aux_tick_decision`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecvqTick {
    /// Below the backlog threshold. Nothing to do.
    Clear,
    /// Deep, but `wfb_tx` is reading it. Healthy and saturated: report, never
    /// restart.
    Backpressured,
    /// Deep with no read progress. Sustained, this is a wedge.
    Wedged,
}

/// Classify one poll. `draining` is whether the data plane's `rchar` advanced
/// since the previous poll, i.e. whether it read anything at all.
fn recvq_tick_decision(queue_bytes: u64, draining: bool) -> RecvqTick {
    if queue_bytes <= RECVQ_BACKLOG_THRESHOLD_BYTES {
        RecvqTick::Clear
    } else if draining {
        RecvqTick::Backpressured
    } else {
        RecvqTick::Wedged
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

/// How often the auxiliary-stream liveness watchdog polls.
const AUX_POLL_INTERVAL: Duration = Duration::from_secs(5);
/// Flat-counter window before the aux watchdog treats the aux transmitter as
/// silently stalled. Same 30 s window as the data-plane TX watchdog.
const AUX_SILENCE_THRESHOLD: Duration = Duration::from_secs(30);

/// Watch the **auxiliary** application-stream transmitter's liveness and restart
/// the aux pair IN PLACE on a silent stall, never returning to the run loop's
/// respawn select.
///
/// This mirrors the data-plane delta-counter contract — process-liveness alone is
/// never proof of work, so the watchdog asserts that the aux tx process's ingress
/// counter (the `rchar` it reads from its UDP ingress, i.e. the application frames
/// a plugin feeds it) advances. It differs from the data-plane watchdog in ONE
/// deliberate way: it owns its own recovery. A stalled aux pair must NOT trigger a
/// whole-group respawn (that would interrupt the data + control planes, breaking
/// the additive-aux invariant), so on a stall the watchdog calls
/// [`RadioProcesses::restart_aux_stream`] directly and keeps watching. It returns
/// only when cancelled (a whole-group respawn / shutdown aborts it like the other
/// sibling tasks).
///
/// SAFE while the aux stream is closed: the aux tx PID resolves `None`, so the
/// watchdog idles (resetting its progress clock) and never restarts anything. It
/// can run for the entire radio bring-up regardless of whether a plugin has ever
/// opened the stream.
///
/// IDLE IS NOT A STALL. A low-rate aux channel legitimately sends nothing for
/// long stretches, so a flat ingress counter on its own is NOT evidence of a
/// wedged transmitter — restarting an idle-but-healthy stream every 30 s is
/// churn, not recovery. The watchdog therefore fires ONLY on a *post-activity*
/// stall: the counter must have advanced at least once (the plugin really did
/// feed the pipe) and THEN gone flat for the silence window. A stream that has
/// never been fed since it was opened (or has gone quiescent and stayed there)
/// is left running. That keeps the only fire path the same orphaned-`wfb_tx`
/// failure class the data plane guards — a transmitter that WAS carrying frames
/// and silently died — without ever penalising a healthy idle channel.
///
/// The RF-confirmation half of the dual-check (an independent received-side or
/// PHY-speed signal proving the energy reaches a peer) is bench-gated for the aux
/// pair; this guards the process-liveness + ingress-advance half so a wedged aux
/// transmitter is recovered in the field.
pub async fn aux_liveness_watchdog(
    proc: std::sync::Arc<Mutex<RadioProcesses>>,
    cancel: std::sync::Arc<tokio::sync::Notify>,
) {
    let mut last_progress = Instant::now();
    let mut prev_rchar: Option<u64> = None;
    // Whether the counter has advanced at least once since the stream was opened.
    // A never-fed (idle) stream keeps this false, so it is never restarted; only
    // a stream that DID feed and then went silent is a genuine stall.
    let mut had_activity = false;

    loop {
        tokio::select! {
            _ = tokio::time::sleep(AUX_POLL_INTERVAL) => {}
            _ = cancel.notified() => return,
        }

        // Resolve the live aux tx PID for THIS tick. `None` means the stream is
        // closed (or mid-restart) — reset the progress clock, the prev counter,
        // and the activity flag so a fresh open starts from a clean window and the
        // watchdog never fires on a stream that was simply never opened.
        let pid = { proc.lock().await.aux_tx_pid() };
        let Some(pid) = pid else {
            last_progress = Instant::now();
            prev_rchar = None;
            had_activity = false;
            continue;
        };

        let rchar = read_rchar(pid).await;
        match (prev_rchar, rchar) {
            // First reading of a freshly-opened stream: seed the baseline, don't
            // judge progress yet.
            (None, Some(cur)) => {
                prev_rchar = Some(cur);
                last_progress = Instant::now();
            }
            (Some(prev), Some(cur)) => {
                let window_elapsed = last_progress.elapsed() >= AUX_SILENCE_THRESHOLD;
                match aux_tick_decision(prev, cur, had_activity, window_elapsed) {
                    AuxTick::Progress => {
                        // The plugin fed the pipe: real activity. From here a later
                        // sustained-flat window is a genuine stall.
                        had_activity = true;
                        last_progress = Instant::now();
                        prev_rchar = Some(cur);
                    }
                    AuxTick::Restart => {
                        tracing::warn!(
                            pid,
                            elapsed_s = last_progress.elapsed().as_secs(),
                            "aux_tx_stalled_restarting"
                        );
                        // ADDITIVE recovery: restart ONLY the aux pair, in place.
                        // The data + control planes are untouched. Reset the
                        // baseline + the activity flag so the new pair starts from
                        // a clean window.
                        let _ = proc.lock().await.restart_aux_stream().await;
                        last_progress = Instant::now();
                        prev_rchar = None;
                        had_activity = false;
                    }
                    AuxTick::Hold => {
                        // Flat counter on an idle stream (never fed, or within the
                        // window) — leave it running. Carry the baseline forward.
                        prev_rchar = Some(cur);
                    }
                }
            }
            // The `/proc/<pid>/io` read failed (a recycle window): carry the
            // previous baseline forward, never manufacture progress.
            (_, None) => {}
        }
    }
}

/// The aux liveness watchdog's per-tick decision, factored out so the
/// idle-vs-stall distinction is unit-testable without `/proc` or a live radio.
///
/// Given the previous + current ingress counters, whether the counter has ever
/// advanced since the stream opened (`had_activity`), and whether the silence
/// window has elapsed, decide whether to restart the aux pair. The cardinal
/// rule: an idle stream (one that never fed) is NEVER restarted; only a stream
/// that fed and then went sustained-flat is a stall.
#[derive(Debug, PartialEq, Eq)]
enum AuxTick {
    /// Counter advanced: real activity, reset the progress clock.
    Progress,
    /// Counter flat but the stream is idle / within the window: leave running.
    Hold,
    /// Counter flat after prior activity AND past the silence window: restart.
    Restart,
}

fn aux_tick_decision(prev: u64, cur: u64, had_activity: bool, window_elapsed: bool) -> AuxTick {
    if cur > prev {
        AuxTick::Progress
    } else if had_activity && window_elapsed {
        AuxTick::Restart
    } else {
        AuxTick::Hold
    }
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

    #[test]
    fn a_drained_queue_is_backpressure_not_a_wedge() {
        let deep = RECVQ_BACKLOG_THRESHOLD_BYTES + 1;

        // The production case this exists for: ~3 MB queued while wfb_tx reads
        // steadily, because the encoder offers more than the link can carry.
        // Killing here drops the link, resets the bitrate ladder and changes
        // nothing about the offered rate, so it must never be a wedge.
        assert_eq!(
            recvq_tick_decision(3_127_808, true),
            RecvqTick::Backpressured,
            "a deep queue that is being drained must never be called a wedge"
        );
        assert_eq!(recvq_tick_decision(deep, true), RecvqTick::Backpressured);

        // Same depth, no read progress: genuinely stuck, and a kill is the only
        // recovery.
        assert_eq!(recvq_tick_decision(deep, false), RecvqTick::Wedged);
        assert_eq!(recvq_tick_decision(3_127_808, false), RecvqTick::Wedged);

        // Below the threshold nothing fires, draining or not.
        assert_eq!(recvq_tick_decision(0, false), RecvqTick::Clear);
        assert_eq!(recvq_tick_decision(0, true), RecvqTick::Clear);
        // The threshold itself is not "over" it.
        assert_eq!(
            recvq_tick_decision(RECVQ_BACKLOG_THRESHOLD_BYTES, false),
            RecvqTick::Clear,
            "the threshold is exclusive, matching the original > comparison"
        );
    }

    #[test]
    fn only_a_wedge_reports_the_video_queue_as_stalled() {
        // The flag the sidecar, heartbeat and GCS read must mean "stuck", not
        // merely "busy" — otherwise it is permanently true on a saturated link
        // and the operator learns to ignore it.
        let deep = RECVQ_BACKLOG_THRESHOLD_BYTES + 1;
        assert!(recvq_tick_decision(deep, false) == RecvqTick::Wedged);
        assert!(recvq_tick_decision(deep, true) != RecvqTick::Wedged);
    }

    #[test]
    fn aux_idle_stream_is_never_restarted_but_post_activity_stall_is() {
        // A freshly-opened stream that has never fed: the counter is flat and
        // there has been no prior activity. Even after the silence window has
        // elapsed, an idle stream must be HELD (left running), never restarted.
        assert_eq!(
            aux_tick_decision(100, 100, false, true),
            AuxTick::Hold,
            "an idle stream past the window must not be restarted"
        );
        // The same flat counter within the window is also a Hold.
        assert_eq!(aux_tick_decision(100, 100, false, false), AuxTick::Hold);

        // The plugin fed the pipe: the counter advanced → Progress (resets the
        // clock, marks activity).
        assert_eq!(aux_tick_decision(100, 140, false, false), AuxTick::Progress);
        assert_eq!(aux_tick_decision(100, 140, true, true), AuxTick::Progress);

        // After prior activity the counter goes flat: within the window it Holds,
        // but once the silence window elapses it is a genuine stall → Restart.
        assert_eq!(aux_tick_decision(140, 140, true, false), AuxTick::Hold);
        assert_eq!(
            aux_tick_decision(140, 140, true, true),
            AuxTick::Restart,
            "a fed-then-silent stream past the window is a real stall"
        );
    }

    #[test]
    fn aux_thresholds_match_the_data_plane_contract() {
        // The aux liveness watchdog reuses the same 5 s poll / 30 s silence window
        // as the data-plane TX watchdog, so a stalled aux transmitter is recovered
        // on the same cadence the operator already expects.
        assert_eq!(AUX_POLL_INTERVAL.as_secs(), 5);
        assert_eq!(AUX_SILENCE_THRESHOLD.as_secs(), 30);
    }

    /// One scripted kernel counter. It yields the next value each time the
    /// watchdog samples it and holds the final value once the script runs out,
    /// so a scenario only has to spell out the polls that matter.
    struct Scripted {
        values: Vec<Option<u64>>,
        reads: usize,
    }

    impl Scripted {
        fn new(values: Vec<Option<u64>>) -> Self {
            assert!(!values.is_empty(), "a scripted counter needs a first value");
            Self { values, reads: 0 }
        }

        fn sample(&mut self) -> Option<u64> {
            let v = self.values[self.reads.min(self.values.len() - 1)];
            self.reads += 1;
            v
        }
    }

    fn flat(v: u64) -> Vec<Option<u64>> {
        vec![Some(v)]
    }

    fn ramp(start: u64, step: u64, n: usize) -> Vec<Option<u64>> {
        (0..n).map(|i| Some(start + step * i as u64)).collect()
    }

    /// A scripted stand-in for `/proc` and the clock, so a scenario can be put in
    /// front of the receive-queue watchdog and its verdict asserted.
    ///
    /// Virtual time advances by exactly one poll interval per poll, which is what
    /// the real loop sees on an idle board, and the wait returns immediately so a
    /// 15 s sustained window costs no wall time. Once the scenario's poll budget
    /// is spent the wait parks forever and fires `spent`: the watchdog then ends
    /// only by cancellation, which is how a scenario proves that it did *not*
    /// kill.
    struct FakeSignals {
        queue: std::sync::Mutex<Scripted>,
        rchar: std::sync::Mutex<Scripted>,
        clock: std::sync::Mutex<Instant>,
        polls: std::sync::atomic::AtomicUsize,
        budget: usize,
        last_port: std::sync::atomic::AtomicU32,
        spent: std::sync::Arc<tokio::sync::Notify>,
    }

    impl FakeSignals {
        fn new(queue: Vec<Option<u64>>, rchar: Vec<Option<u64>>, budget: usize) -> Arc<Self> {
            Arc::new(Self {
                queue: std::sync::Mutex::new(Scripted::new(queue)),
                rchar: std::sync::Mutex::new(Scripted::new(rchar)),
                // Start the virtual clock an hour in so the loop's initial
                // `now() - BACKPRESSURE_LOG_INTERVAL` cannot underflow the
                // monotonic clock on a freshly booted machine.
                clock: std::sync::Mutex::new(Instant::now() + Duration::from_secs(3600)),
                polls: std::sync::atomic::AtomicUsize::new(0),
                budget,
                last_port: std::sync::atomic::AtomicU32::new(0),
                spent: std::sync::Arc::new(tokio::sync::Notify::new()),
            })
        }

        fn polls(&self) -> usize {
            self.polls.load(std::sync::atomic::Ordering::SeqCst)
        }

        fn rchar_reads(&self) -> usize {
            self.rchar.lock().unwrap().reads
        }

        fn last_port(&self) -> u32 {
            self.last_port.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl RecvqSignals for Arc<FakeSignals> {
        async fn rchar(&self, _pid: u32) -> Option<u64> {
            self.rchar.lock().unwrap().sample()
        }

        async fn udp_recvq(&self, port: u16) -> Option<u64> {
            self.last_port
                .store(port as u32, std::sync::atomic::Ordering::SeqCst);
            self.queue.lock().unwrap().sample()
        }

        async fn wait(&self, interval: Duration) {
            let n = self.polls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n >= self.budget {
                self.spent.notify_one();
                std::future::pending::<()>().await;
            }
            *self.clock.lock().unwrap() += interval;
        }

        fn now(&self) -> Instant {
            *self.clock.lock().unwrap()
        }
    }

    /// A queue depth well past the 256 KiB threshold. The value is the one
    /// recorded in this module's own backpressure note (~3 MB queued while
    /// `wfb_tx` was reading steadily), reused here so both cases are exercised at
    /// the same realistic depth.
    const DEEP_QUEUE: u64 = 3_127_808;

    /// Run a scenario that is expected NOT to kill: drive it until its poll
    /// budget is spent, hand the counters to the caller to assert on, then cancel
    /// and confirm the watchdog left by the cancel arm rather than by a kill.
    async fn run_until_spent(
        signals: Arc<FakeSignals>,
        pid: std::sync::Arc<FakePid>,
        counters: CounterHandle,
    ) -> WatchdogFired {
        let cancel = std::sync::Arc::new(tokio::sync::Notify::new());
        let spent = signals.spent.clone();
        let mut handle = tokio::spawn(video_recvq_watchdog_with(
            pid,
            signals,
            counters,
            cancel.clone(),
        ));
        tokio::select! {
            // The watchdog left before the scenario ran out of polls, which for
            // these scenarios means it killed. Hand that verdict back so the
            // caller's assertion fails on the spot, rather than waiting forever
            // for a poll budget that will now never be spent.
            verdict = &mut handle => return verdict.expect("watchdog task panicked"),
            _ = spent.notified() => {}
        }
        cancel.notify_one();
        handle.await.expect("watchdog task panicked")
    }

    #[tokio::test]
    async fn a_live_process_reading_nothing_is_killed_after_the_sustained_window() {
        // The failure this watchdog exists for: `wfb_tx` is alive, the socket is
        // filling, and it has stopped reading. Process liveness proves nothing —
        // only the ingress counter does, and it is flat.
        let signals = FakeSignals::new(flat(DEEP_QUEUE), flat(500), 50);
        let counters = new_counters();
        let cancel = std::sync::Arc::new(tokio::sync::Notify::new());

        let fired = video_recvq_watchdog_with(
            std::sync::Arc::new(FakePid::new(4242)),
            signals.clone(),
            counters.clone(),
            cancel,
        )
        .await;

        assert_eq!(fired, WatchdogFired::RecvqBacklog);
        let c = *counters.lock().await;
        assert_eq!(c.tx_video_stall_kills, 1, "the kill must be counted once");
        assert!(c.tx_video_stalled, "a wedge must report as stalled");
        assert!(!c.tx_video_backpressured);
        assert_eq!(c.tx_video_recvq_bytes, DEEP_QUEUE);
        assert_eq!(
            signals.last_port(),
            5600,
            "the video ingress port is watched"
        );

        // Five polls, and the count is the point: the first poll only seeds the
        // ingress baseline (`prev_rchar` starts at 0, so any reading looks like
        // progress), the second is the first poll that can be called wedged, and
        // the kill lands three polls later — a full uninterrupted 15 s window at
        // the 5 s cadence. A kill any sooner would mean the window shrank.
        assert_eq!(signals.polls(), 5);
    }

    #[tokio::test]
    async fn a_deep_queue_that_is_being_drained_is_never_killed() {
        // Same depth as the wedge above, but the process is reading. This is a
        // saturated link, not a stuck one: killing it drops the link, resets the
        // bitrate ladder and leaves the encoder still over-feeding, so the queue
        // refills and the kill repeats forever.
        let signals = FakeSignals::new(flat(DEEP_QUEUE), ramp(500, 40_000, 40), 30);
        let counters = new_counters();

        // Thirty polls is 150 s of virtual time, ten times the sustained window.
        let fired = run_until_spent(
            signals.clone(),
            std::sync::Arc::new(FakePid::new(4242)),
            counters.clone(),
        )
        .await;

        assert_eq!(fired, WatchdogFired::Cancelled);
        let c = *counters.lock().await;
        assert_eq!(c.tx_video_stall_kills, 0, "backpressure must never kill");
        assert!(
            !c.tx_video_stalled,
            "a busy link reported as stalled trains the operator to ignore the flag"
        );
        assert!(c.tx_video_backpressured);
        assert_eq!(c.tx_video_recvq_bytes, DEEP_QUEUE);
    }

    #[tokio::test]
    async fn a_shallow_queue_is_healthy_and_raises_no_flag() {
        let shallow = RECVQ_BACKLOG_THRESHOLD_BYTES / 4;
        let signals = FakeSignals::new(flat(shallow), ramp(500, 40_000, 20), 12);
        let counters = new_counters();

        let fired = run_until_spent(
            signals,
            std::sync::Arc::new(FakePid::new(4242)),
            counters.clone(),
        )
        .await;

        assert_eq!(fired, WatchdogFired::Cancelled);
        let c = *counters.lock().await;
        assert_eq!(c.tx_video_stall_kills, 0);
        assert!(!c.tx_video_stalled);
        assert!(!c.tx_video_backpressured);
        assert_eq!(
            c.tx_video_recvq_bytes, shallow,
            "the depth is reported even when nothing is wrong"
        );
    }

    #[tokio::test]
    async fn one_draining_poll_restarts_the_wedge_window() {
        // Six wedged polls spread either side of a single draining one: 30 s of
        // flat ingress in total, but never 15 s uninterrupted. A kill here would
        // mean the watchdog is accumulating stuck polls instead of requiring a
        // continuous window, and a process that reads in bursts would be killed
        // while it was still working.
        let signals = FakeSignals::new(
            flat(DEEP_QUEUE),
            vec![
                Some(500), // seeds the baseline
                Some(500), // wedged, window opens here
                Some(500),
                Some(500), // 10 s into the window
                Some(600), // one real read: window resets
                Some(600), // wedged, a fresh window opens
                Some(600),
                Some(600), // 10 s into the fresh window
            ],
            8,
        );
        let counters = new_counters();

        let fired = run_until_spent(
            signals,
            std::sync::Arc::new(FakePid::new(4242)),
            counters.clone(),
        )
        .await;

        assert_eq!(fired, WatchdogFired::Cancelled);
        assert_eq!(counters.lock().await.tx_video_stall_kills, 0);
    }

    #[tokio::test]
    async fn a_data_plane_that_is_not_running_reads_as_not_draining() {
        // No live PID means no ingress read at all, and the previous value is
        // carried forward rather than a missing read being taken as progress. A
        // data plane that is not running is certainly not emptying the socket.
        let signals = FakeSignals::new(flat(DEEP_QUEUE), ramp(500, 40_000, 40), 50);
        let counters = new_counters();
        let cancel = std::sync::Arc::new(tokio::sync::Notify::new());

        let fired = video_recvq_watchdog_with(
            std::sync::Arc::new(FakePid::new(0)),
            signals.clone(),
            counters.clone(),
            cancel,
        )
        .await;

        assert_eq!(fired, WatchdogFired::RecvqBacklog);
        assert_eq!(counters.lock().await.tx_video_stall_kills, 1);
        assert_eq!(
            signals.rchar_reads(),
            0,
            "an ingress counter belonging to no live process must never be read"
        );
        // Four polls rather than the five of the live-process case: there is no
        // baseline-seeding poll, because a carried-forward zero never looks like
        // progress, so the window opens on the very first poll.
        assert_eq!(signals.polls(), 4);
    }

    #[tokio::test]
    async fn a_failed_ingress_read_is_not_progress() {
        // `/proc/<pid>/io` can fail to read in the window where the process is
        // being recycled. Treating that as progress would keep a genuinely wedged
        // transmitter alive indefinitely.
        let signals = FakeSignals::new(flat(DEEP_QUEUE), vec![Some(500), None], 50);
        let counters = new_counters();
        let cancel = std::sync::Arc::new(tokio::sync::Notify::new());

        let fired = video_recvq_watchdog_with(
            std::sync::Arc::new(FakePid::new(4242)),
            signals,
            counters.clone(),
            cancel,
        )
        .await;

        assert_eq!(fired, WatchdogFired::RecvqBacklog);
        assert_eq!(counters.lock().await.tx_video_stall_kills, 1);
    }

    #[tokio::test]
    async fn a_restarted_data_plane_recovers_and_is_not_killed_again() {
        // The whole point of the kill: the caller respawns the radio group and a
        // fresh watchdog runs against the new process. The queue drains, the new
        // PID reads, and the stall flag must clear — while the kill count, which
        // the sidecar and heartbeat surface as churn, keeps accumulating.
        let counters = new_counters();

        let wedged = FakeSignals::new(flat(DEEP_QUEUE), flat(500), 50);
        let fired = video_recvq_watchdog_with(
            std::sync::Arc::new(FakePid::new(4242)),
            wedged,
            counters.clone(),
            std::sync::Arc::new(tokio::sync::Notify::new()),
        )
        .await;
        assert_eq!(fired, WatchdogFired::RecvqBacklog);
        assert!(counters.lock().await.tx_video_stalled);

        let recovered = FakeSignals::new(
            vec![Some(DEEP_QUEUE), Some(64 * 1024), Some(0)],
            ramp(0, 40_000, 20),
            10,
        );
        let after = run_until_spent(
            recovered,
            // A respawn hands the data plane a new PID.
            std::sync::Arc::new(FakePid::new(9001)),
            counters.clone(),
        )
        .await;

        assert_eq!(after, WatchdogFired::Cancelled);
        let c = *counters.lock().await;
        assert!(
            !c.tx_video_stalled,
            "the recovered link must clear the flag"
        );
        assert!(!c.tx_video_backpressured);
        assert_eq!(c.tx_video_recvq_bytes, 0);
        assert_eq!(
            c.tx_video_stall_kills, 1,
            "the kill count is cumulative churn, not per-watchdog state"
        );
    }

    #[tokio::test]
    async fn the_production_signals_wait_for_real() {
        // The seam must not have turned the production poll into a hot spin.
        let signals = ProcSignals;
        let before = signals.now();
        signals.wait(Duration::from_millis(5)).await;
        assert!(signals.now().saturating_duration_since(before) >= Duration::from_millis(5));
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
