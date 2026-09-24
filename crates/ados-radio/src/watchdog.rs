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
}

/// Shared handle to the watchdog counters (mirrors the `LinkStats` share).
pub type CounterHandle = Arc<Mutex<WatchdogCounters>>;

/// Construct a fresh, zeroed counter handle.
pub fn new_counters() -> CounterHandle {
    Arc::new(Mutex::new(WatchdogCounters::default()))
}

/// Folds a transmit plane's cumulative stats totals into the tested
/// [`TxLivenessWindow`]: each poll's delta becomes one observation, taken only
/// when the plane printed at least one new stats line (a flat line count is the
/// silent-loop case the window detects on its own). Pure over `(now, totals)`.
#[derive(Debug)]
pub struct TxPlaneTracker {
    prev: TxPlaneTotals,
    window: TxLivenessWindow,
}

impl TxPlaneTracker {
    pub fn new(now: tokio::time::Instant, totals: TxPlaneTotals, silence: Duration) -> Self {
        Self {
            prev: totals,
            window: TxLivenessWindow::new(now, silence),
        }
    }

    /// Fold one poll and judge the window (`None` while it is still filling).
    pub fn observe(
        &mut self,
        now: tokio::time::Instant,
        totals: TxPlaneTotals,
    ) -> Option<TxVerdict> {
        if totals.lines > self.prev.lines {
            self.window.observe(
                TxPkt {
                    bytes_in: totals.bytes_in.saturating_sub(self.prev.bytes_in),
                    bytes_injected: totals
                        .bytes_injected
                        .saturating_sub(self.prev.bytes_injected),
                    packets_dropped: 0,
                },
                now,
            );
        }
        self.prev = totals;
        self.window.evaluate(now)
    }
}

/// Watch the data-plane `wfb_tx`'s own stats counters. Returns when it should
/// be killed ([`WatchdogFired::TxStalled`]), when its PHY is muted
/// ([`WatchdogFired::PhyMuted`], a recovery rather than a kill), or on
/// `cancel`. `stats` is the data plane's process-lifetime counter handle, so a
/// retune respawn keeps feeding the same window.
pub async fn tx_health_watchdog(
    iface: &str,
    stats: Arc<TxPlaneCounters>,
    counters: CounterHandle,
    cancel: Shutdown,
) -> WatchdogFired {
    let mut tracker = TxPlaneTracker::new(
        tokio::time::Instant::now(),
        stats.totals(),
        TX_SILENCE_THRESHOLD,
    );
    let mut last_upstream_silent_log = Instant::now() - UPSTREAM_SILENT_LOG_INTERVAL;

    loop {
        tokio::select! {
            _ = tokio::time::sleep(POLL_INTERVAL) => {}
            _ = cancel.wait() => return WatchdogFired::Cancelled,
        }
        match tracker.observe(tokio::time::Instant::now(), stats.totals()) {
            Some(TxVerdict::StalledNotInjecting) => {
                // Ingress arrives but nothing reaches the radio. If the PHY is
                // pinned at the muted not-permitted floor (the RTL8812EU `set
                // type monitor` mute), killing wfb_tx can never un-mute it: the
                // fault is in the driver/PHY, so route to a PHY recovery.
                let muted = crate::adapter::read_tx_power(iface)
                    .await
                    .map(|dbm| dbm <= crate::adapter::MUTED_TX_POWER_DBM)
                    .unwrap_or(false);
                if muted {
                    tracing::warn!(
                        iface,
                        "wfb_tx_stalled_phy_muted: routing to PHY-recovery, not a kill"
                    );
                    return WatchdogFired::PhyMuted;
                }
                tracing::warn!(iface, "wfb_tx_stalled_kill: ingress with no injection");
                counters.lock().await.tx_zombie_kills += 1;
                return WatchdogFired::TxStalled;
            }
            Some(TxVerdict::StalledSilent) => {
                tracing::warn!(
                    iface,
                    "wfb_tx_stalled_kill: no stats line for the whole window"
                );
                counters.lock().await.tx_zombie_kills += 1;
                return WatchdogFired::TxStalled;
            }
            Some(TxVerdict::Idle) => {
                // Upstream (the video encoder) offered nothing — not a fault.
                if last_upstream_silent_log.elapsed() >= UPSTREAM_SILENT_LOG_INTERVAL {
                    tracing::info!(iface, "wfb_tx_upstream_silent");
                    last_upstream_silent_log = Instant::now();
                }
            }
            Some(TxVerdict::Healthy) | None => {}
        }
    }
}

/// Everything the receive-queue watchdog observes about the world outside its
/// own state machine: the kernel queue depth and the data plane's ingress total
/// it cross-checks, the monotonic clock its sustained-window arithmetic runs on,
/// and the wait between polls. Behind this seam a scenario is a list of values;
/// [`ProcSignals`] is the production implementation.
pub trait RecvqSignals: Send + Sync {
    /// Cumulative bytes the data plane has read off its UDP ingress — the
    /// evidence that it is actually emptying the socket rather than merely
    /// still being alive.
    fn ingress_bytes(&self) -> u64;

    /// Kernel receive-queue depth in bytes for `port`.
    fn udp_recvq(&self, port: u16) -> impl Future<Output = Option<u64>> + Send;

    /// Wait one poll interval. Raced against the cancel notification by the
    /// caller, so an implementation that never completes simply parks the
    /// watchdog until it is cancelled.
    fn wait(&self, interval: Duration) -> impl Future<Output = ()> + Send;

    /// Read the monotonic clock. Called once per poll; every window in the loop
    /// is measured against that single reading.
    fn now(&self) -> Instant;
}

/// The production [`RecvqSignals`]: the data plane's own stats totals, the real
/// `/proc/net/udp` depth, the real tokio timer, the real monotonic clock.
#[derive(Debug, Clone)]
pub struct ProcSignals {
    stats: Arc<TxPlaneCounters>,
}

impl RecvqSignals for ProcSignals {
    fn ingress_bytes(&self) -> u64 {
        self.stats.totals().bytes_in
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
/// A deep queue on its own is not evidence of a wedge. Two very different
/// conditions produce the same queue depth:
///
/// - **Wedged**: `wfb_tx` has stopped reading the socket; its ingress total is
///   flat. Killing it is the correct and only recovery.
/// - **Backpressured**: `wfb_tx` is reading as fast as the air allows, but the
///   encoder is offering more than the current MCS and FEC can carry; its
///   ingress total advances. Killing it drops the link for a full radio-group
///   respawn, resets the adaptive bitrate controller, and leaves the encoder
///   still over-feeding, so the queue refills and the kill repeats on a fixed
///   period (observed on a bench rig as 26 consecutive kills at 23 s
///   intervals). Backpressure is logged periodically instead.
pub async fn video_recvq_watchdog(
    stats: Arc<TxPlaneCounters>,
    counters: CounterHandle,
    cancel: Shutdown,
) -> WatchdogFired {
    video_recvq_watchdog_with(ProcSignals { stats }, counters, cancel).await
}

/// [`video_recvq_watchdog`] with its view of the outside world supplied. The
/// public entry point above is this function with [`ProcSignals`]; tests drive
/// it with a scripted one.
pub async fn video_recvq_watchdog_with<S: RecvqSignals>(
    signals: S,
    counters: CounterHandle,
    cancel: Shutdown,
) -> WatchdogFired {
    let mut high_since: Option<Instant> = None;
    let mut prev_ingress: u64 = 0;
    let mut last_backpressure_log = signals.now() - BACKPRESSURE_LOG_INTERVAL;

    loop {
        tokio::select! {
            _ = signals.wait(POLL_INTERVAL) => {}
            _ = cancel.wait() => return WatchdogFired::Cancelled,
        }
        // One clock reading per poll, so every window below is measured against
        // the same instant and a slow tick cannot make two of them disagree.
        let now = signals.now();
        let q = signals.udp_recvq(5600).await.unwrap_or(0);
        let ingress = signals.ingress_bytes();
        let draining = ingress > prev_ingress;
        prev_ingress = ingress;

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

/// Classify one poll. `draining` is whether the data plane's ingress total advanced
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
    /// A control-plane process (the HopAnnounce/beacon transmitter or the
    /// HopAck/link-stats receiver) is alive but its own stats counter stayed
    /// flat for the whole silence window. The caller respawns the group.
    ControlStalled,
    Cancelled,
}

/// Flat-counter window before a control plane counts as silently stalled. Same
/// 30 s window as the data-plane TX watchdog.
const CONTROL_SILENCE_THRESHOLD: Duration = Duration::from_secs(30);

/// Which control plane stopped doing work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlPlane {
    /// The tx-control `wfb_tx`: the ingress bytes its own stats report stopped
    /// advancing even though the presence-beacon emitter feeds it every 10 s.
    TxControl,
    /// The rx-control `wfb_rx`: it stopped printing its per-second stats line.
    RxControl,
}

impl ControlPlane {
    pub fn as_str(self) -> &'static str {
        match self {
            ControlPlane::TxControl => "tx_control",
            ControlPlane::RxControl => "rx_control",
        }
    }
}

/// Delta-counter judgement over the control planes' cumulative stats counters
/// `(tx-control ingress bytes, rx-control stats lines)`. Each counter must advance inside a rolling
/// [`CONTROL_SILENCE_THRESHOLD`] window; a live process whose counter stays flat
/// is doing no work. Pure over `(now, counters)` so the rule is testable without
/// processes or a clock.
#[derive(Debug)]
pub struct ControlStallTracker {
    last: (u64, u64),
    tx_progress_at: Instant,
    rx_progress_at: Instant,
}

impl ControlStallTracker {
    /// Start a window at `now` from the counters' current values.
    pub fn new(now: Instant, counters: (u64, u64)) -> Self {
        Self {
            last: counters,
            tx_progress_at: now,
            rx_progress_at: now,
        }
    }

    /// Fold one poll. Returns the plane whose counter has been flat for the
    /// whole window, or `None` while both are advancing.
    pub fn observe(&mut self, now: Instant, counters: (u64, u64)) -> Option<ControlPlane> {
        if counters.0 > self.last.0 {
            self.tx_progress_at = now;
        }
        if counters.1 > self.last.1 {
            self.rx_progress_at = now;
        }
        self.last = counters;
        if now.saturating_duration_since(self.rx_progress_at) >= CONTROL_SILENCE_THRESHOLD {
            Some(ControlPlane::RxControl)
        } else if now.saturating_duration_since(self.tx_progress_at) >= CONTROL_SILENCE_THRESHOLD {
            Some(ControlPlane::TxControl)
        } else {
            None
        }
    }
}

/// Watch the two control planes' own stats counters and return
/// [`WatchdogFired::ControlStalled`] when either stays flat for the silence
/// window, so the caller respawns the group. Process liveness is never proof of
/// work: a `wfb_rx` blocked on a full pipe or a `wfb_tx` wedged in the driver is
/// still a live PID. Returns [`WatchdogFired::Cancelled`] on `cancel`.
pub async fn control_plane_watchdog(
    proc: Arc<Mutex<RadioProcesses>>,
    cancel: Shutdown,
) -> WatchdogFired {
    let control_counters = |(tx, rx): (crate::process::TxPlaneTotals, u64)| (tx.bytes_in, rx);
    let start = control_counters(proc.lock().await.control_progress());
    let mut tracker = ControlStallTracker::new(Instant::now(), start);
    loop {
        tokio::select! {
            _ = tokio::time::sleep(POLL_INTERVAL) => {}
            _ = cancel.wait() => return WatchdogFired::Cancelled,
        }
        let counters = control_counters(proc.lock().await.control_progress());
        if let Some(plane) = tracker.observe(Instant::now(), counters) {
            tracing::warn!(
                plane = plane.as_str(),
                window_s = CONTROL_SILENCE_THRESHOLD.as_secs(),
                "wfb_control_plane_stalled_respawning"
            );
            return WatchdogFired::ControlStalled;
        }
    }
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
/// never proof of work — over the aux transmitter's own stats totals: its stats
/// line count (the loop is turning; `wfb_tx` prints one every second whether or
/// not traffic moved) and its ingress bytes (the application frames a plugin
/// feeds it). It differs from the data-plane watchdog in ONE deliberate way: it
/// owns its own recovery. A stalled aux pair must NOT trigger a whole-group
/// respawn (that would interrupt the data + control planes, breaking the
/// additive-aux invariant), so on a stall the watchdog calls
/// [`RadioProcesses::restart_aux_stream`] directly and keeps watching. It returns
/// only when cancelled.
///
/// SAFE while the aux stream is closed: the watchdog idles (resetting its
/// windows) and never restarts anything.
///
/// IDLE IS NOT A STALL. A low-rate aux channel legitimately sends nothing for
/// long stretches, so flat ingress on its own is not evidence of a wedge. Two
/// things fire: the stats lines stopping for the whole window (a loop that is
/// no longer turning), or ingress that advanced at least once and then went
/// flat for the window (a transmitter that WAS carrying frames and died).
pub async fn aux_liveness_watchdog(proc: std::sync::Arc<Mutex<RadioProcesses>>, cancel: Shutdown) {
    let mut last_progress = Instant::now();
    let mut last_line = Instant::now();
    let mut prev: Option<TxPlaneTotals> = None;
    // Whether ingress has advanced at least once since the stream was opened.
    // A never-fed (idle) stream keeps this false, so flat ingress alone never
    // restarts it.
    let mut had_activity = false;

    loop {
        tokio::select! {
            _ = tokio::time::sleep(AUX_POLL_INTERVAL) => {}
            _ = cancel.wait() => return,
        }

        // Resolve this tick's pair state under one lock. An exited half, or a
        // stream a plugin still wants whose pair is down (a failed restart),
        // is restarted now and retried every poll until it comes back; the
        // retry is the fixed poll interval with no cap.
        let (action, totals) = {
            let mut p = proc.lock().await;
            let action = aux_pair_action(p.aux_wanted(), p.aux_tx_pid().is_some(), p.aux_exited());
            (action, p.aux_totals())
        };
        let reset = |last_progress: &mut Instant,
                     last_line: &mut Instant,
                     prev: &mut Option<TxPlaneTotals>,
                     had_activity: &mut bool| {
            *last_progress = Instant::now();
            *last_line = Instant::now();
            *prev = None;
            *had_activity = false;
        };
        match action {
            AuxPairAction::Idle => {
                reset(
                    &mut last_progress,
                    &mut last_line,
                    &mut prev,
                    &mut had_activity,
                );
                continue;
            }
            AuxPairAction::Restart => {
                tracing::warn!("aux_pair_down_restarting");
                let _ = proc.lock().await.restart_aux_stream().await;
                reset(
                    &mut last_progress,
                    &mut last_line,
                    &mut prev,
                    &mut had_activity,
                );
                continue;
            }
            AuxPairAction::Watch => {}
        }

        let Some(before) = prev else {
            // First reading of a freshly-opened stream: seed the baseline.
            prev = Some(totals);
            last_progress = Instant::now();
            last_line = Instant::now();
            continue;
        };
        if totals.lines > before.lines {
            last_line = Instant::now();
        }
        let window_elapsed = last_progress.elapsed() >= AUX_SILENCE_THRESHOLD;
        let silent = last_line.elapsed() >= AUX_SILENCE_THRESHOLD;
        let tick = if silent {
            AuxTick::Restart
        } else {
            aux_tick_decision(
                before.bytes_in,
                totals.bytes_in,
                had_activity,
                window_elapsed,
            )
        };
        match tick {
            AuxTick::Progress => {
                // The plugin fed the pipe: real activity. From here a later
                // sustained-flat window is a genuine stall.
                had_activity = true;
                last_progress = Instant::now();
            }
            AuxTick::Restart => {
                tracing::warn!(
                    silent,
                    elapsed_s = last_progress.elapsed().as_secs(),
                    "aux_tx_stalled_restarting"
                );
                // ADDITIVE recovery: restart ONLY the aux pair, in place.
                let _ = proc.lock().await.restart_aux_stream().await;
                reset(
                    &mut last_progress,
                    &mut last_line,
                    &mut prev,
                    &mut had_activity,
                );
                continue;
            }
            AuxTick::Hold => {}
        }
        prev = Some(totals);
    }
}

/// What the aux watchdog does with the pair this tick, before any counter is
/// read. `wanted` is whether the stream is open (settings retained), `spawned`
/// whether an aux tx process is held, `exited` whether either held half has
/// exited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuxPairAction {
    /// Closed: nothing to watch.
    Idle,
    /// Wanted but a half has exited or the pair is down: restart it.
    Restart,
    /// Running: judge its ingress counter.
    Watch,
}

fn aux_pair_action(wanted: bool, spawned: bool, exited: bool) -> AuxPairAction {
    if !wanted {
        AuxPairAction::Idle
    } else if exited || !spawned {
        AuxPairAction::Restart
    } else {
        AuxPairAction::Watch
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

    /// Each control plane's own stats counter must advance inside the 30 s
    /// window. A counter that keeps moving never fires; one that goes flat for
    /// the whole window names its plane, whichever of the two it is.
    #[test]
    fn a_flat_control_plane_counter_fires_after_the_window() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);

        // Both advancing every 5 s poll: never fires.
        let mut live = ControlStallTracker::new(t0, (0, 0));
        for poll in 1..=20u64 {
            assert_eq!(live.observe(at(poll * 5), (poll, poll * 5)), None);
        }

        // The receiver stops printing stats at t=10: fires at t=40, not before.
        let mut rx = ControlStallTracker::new(t0, (0, 0));
        assert_eq!(rx.observe(at(5), (1, 5)), None);
        assert_eq!(rx.observe(at(10), (2, 10)), None);
        assert_eq!(rx.observe(at(35), (7, 10)), None);
        assert_eq!(rx.observe(at(40), (8, 10)), Some(ControlPlane::RxControl));

        // The transmitter stops reading beacons while its stats keep flowing.
        let mut tx = ControlStallTracker::new(t0, (0, 0));
        assert_eq!(tx.observe(at(10), (1, 10)), None);
        assert_eq!(tx.observe(at(30), (1, 30)), None);
        assert_eq!(tx.observe(at(40), (1, 40)), Some(ControlPlane::TxControl));
    }

    /// A wanted aux pair whose process exited, or whose restart failed and
    /// left it down, is restarted; only a closed stream is left alone.
    #[test]
    fn a_wanted_aux_pair_that_is_down_is_restarted() {
        assert_eq!(aux_pair_action(false, false, false), AuxPairAction::Idle);
        assert_eq!(aux_pair_action(true, true, false), AuxPairAction::Watch);
        assert_eq!(aux_pair_action(true, true, true), AuxPairAction::Restart);
        assert_eq!(aux_pair_action(true, false, false), AuxPairAction::Restart);
    }

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
        ingress: std::sync::Mutex<Scripted>,
        clock: std::sync::Mutex<Instant>,
        polls: std::sync::atomic::AtomicUsize,
        budget: usize,
        last_port: std::sync::atomic::AtomicU32,
        spent: std::sync::Arc<tokio::sync::Notify>,
    }

    impl FakeSignals {
        fn new(queue: Vec<Option<u64>>, ingress: Vec<Option<u64>>, budget: usize) -> Arc<Self> {
            Arc::new(Self {
                queue: std::sync::Mutex::new(Scripted::new(queue)),
                ingress: std::sync::Mutex::new(Scripted::new(ingress)),
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

        fn last_port(&self) -> u32 {
            self.last_port.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl RecvqSignals for Arc<FakeSignals> {
        fn ingress_bytes(&self) -> u64 {
            self.ingress.lock().unwrap().sample().unwrap_or(0)
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
    async fn run_until_spent(signals: Arc<FakeSignals>, counters: CounterHandle) -> WatchdogFired {
        let cancel = Shutdown::new();
        let spent = signals.spent.clone();
        let mut handle = tokio::spawn(video_recvq_watchdog_with(signals, counters, cancel.clone()));
        tokio::select! {
            // The watchdog left before the scenario ran out of polls, which for
            // these scenarios means it killed. Hand that verdict back so the
            // caller's assertion fails on the spot, rather than waiting forever
            // for a poll budget that will now never be spent.
            verdict = &mut handle => return verdict.expect("watchdog task panicked"),
            _ = spent.notified() => {}
        }
        cancel.trigger();
        handle.await.expect("watchdog task panicked")
    }

    #[tokio::test]
    async fn a_live_process_reading_nothing_is_killed_after_the_sustained_window() {
        // The failure this watchdog exists for: `wfb_tx` is alive, the socket is
        // filling, and it has stopped reading. Process liveness proves nothing —
        // only the ingress counter does, and it is flat.
        let signals = FakeSignals::new(flat(DEEP_QUEUE), flat(500), 50);
        let counters = new_counters();
        let cancel = Shutdown::new();

        let fired = video_recvq_watchdog_with(signals.clone(), counters.clone(), cancel).await;

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
        // ingress baseline (the ingress baseline starts at 0, so any reading looks like
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
        let fired = run_until_spent(signals.clone(), counters.clone()).await;

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

        let fired = run_until_spent(signals, counters.clone()).await;

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

        let fired = run_until_spent(signals, counters.clone()).await;

        assert_eq!(fired, WatchdogFired::Cancelled);
        assert_eq!(counters.lock().await.tx_video_stall_kills, 0);
    }

    #[tokio::test]
    async fn a_restarted_data_plane_recovers_and_is_not_killed_again() {
        // The whole point of the kill: the caller respawns the radio group and a
        // fresh watchdog runs against the new process. The queue drains, the new
        // PID reads, and the stall flag must clear — while the kill count, which
        // the sidecar and heartbeat surface as churn, keeps accumulating.
        let counters = new_counters();

        let wedged = FakeSignals::new(flat(DEEP_QUEUE), flat(500), 50);
        let fired = video_recvq_watchdog_with(wedged, counters.clone(), Shutdown::new()).await;
        assert_eq!(fired, WatchdogFired::RecvqBacklog);
        assert!(counters.lock().await.tx_video_stalled);

        let recovered = FakeSignals::new(
            vec![Some(DEEP_QUEUE), Some(64 * 1024), Some(0)],
            ramp(0, 40_000, 20),
            10,
        );
        let after = run_until_spent(recovered, counters.clone()).await;

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
        let signals = ProcSignals {
            stats: Arc::new(TxPlaneCounters::default()),
        };
        let before = signals.now();
        signals.wait(Duration::from_millis(5)).await;
        assert!(signals.now().saturating_duration_since(before) >= Duration::from_millis(5));
    }

    fn totals(lines: u64, bytes_in: u64, bytes_injected: u64) -> TxPlaneTotals {
        TxPlaneTotals {
            lines,
            bytes_in,
            bytes_injected,
        }
    }

    /// The data plane is judged from its own stats totals: injection keeps it
    /// healthy, ingress with no injection is a stall, a loop that stops printing
    /// is a stall, and no ingress at all is an idle encoder that is never killed.
    #[test]
    fn the_data_plane_is_judged_from_its_own_stats_totals() {
        let t0 = tokio::time::Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let window = TX_SILENCE_THRESHOLD;

        let mut healthy = TxPlaneTracker::new(t0, totals(0, 0, 0), window);
        for poll in 1..=12u64 {
            let v = healthy.observe(at(poll * 5), totals(poll * 5, poll * 1000, poll * 1100));
            assert!(!v.is_some_and(|v| v.is_stall()), "poll {poll}: {v:?}");
        }

        let mut not_injecting = TxPlaneTracker::new(t0, totals(0, 0, 0), window);
        let mut verdict = None;
        for poll in 1..=6u64 {
            verdict = not_injecting.observe(at(poll * 5), totals(poll * 5, poll * 1000, 0));
        }
        assert_eq!(verdict, Some(TxVerdict::StalledNotInjecting));

        let mut silent = TxPlaneTracker::new(t0, totals(0, 0, 0), window);
        assert_eq!(silent.observe(at(5), totals(5, 500, 550)), None);
        assert_eq!(
            silent.observe(at(35), totals(5, 500, 550)),
            Some(TxVerdict::StalledSilent)
        );

        let mut idle = TxPlaneTracker::new(t0, totals(0, 0, 0), window);
        let mut verdict = None;
        for poll in 1..=6u64 {
            verdict = idle.observe(at(poll * 5), totals(poll * 5, 0, 0));
        }
        assert_eq!(verdict, Some(TxVerdict::Idle));
    }

    #[tokio::test]
    async fn tx_health_watchdog_cancels_promptly() {
        let counters = new_counters();
        let cancel = Shutdown::new();
        cancel.trigger();
        let fired = tx_health_watchdog(
            "ados-test-nonexistent-iface",
            Arc::new(TxPlaneCounters::default()),
            counters,
            cancel,
        )
        .await;
        assert_eq!(fired, WatchdogFired::Cancelled);
    }
}
