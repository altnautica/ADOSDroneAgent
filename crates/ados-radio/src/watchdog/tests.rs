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
