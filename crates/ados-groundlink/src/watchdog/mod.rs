//! Receive-side valid-packet watchdog: reacquire then restart when the video
//! plane goes silent, but only when the peer is also gone.
//!
//! The valid-decode counter is the trustworthy receive signal: interface
//! `rx_packets` is inflated by ambient RF the receiver cannot decode.
//! `last_valid_rx_change_at` is refreshed on every interval that decoded a
//! valid video packet; a stale timestamp means no video has arrived recently.
//!
//! Silent video alone is NOT a fault. A paired link with the drone simply not
//! transmitting video (idle-but-paired) decodes zero video packets every
//! interval, which is normal. Sweeping or killing on that would knock a healthy
//! link off the air. So the sweep/kill is gated on PEER PRESENCE: when a
//! presence beacon was decoded recently the peer is alive and we stay put,
//! logging "paired, no video". Only when the video plane is silent AND no
//! recent peer presence exists do we act, and even then, if the peer most
//! recently announced a specific channel we do a beacon-guided lock to that
//! channel before a blind band sweep. Reacquisition failure terminates the
//! receive subprocess so the run loop respawns it.
//!
//! INVARIANT: the watchdog NEVER writes the operator's immutable rendezvous
//! home channel (`video.wfb.channel`). A locked channel is recorded ONLY as a
//! tmpfs runtime hint at `/run/ados/wfb-locked-channel` so a restart can try it
//! first; the home channel where both sides deterministically meet is never
//! auto-overwritten.
//!
//! Module layout:
//! - `seams`: the timing constants + the injected `Clock` / `RxProcess` /
//!   `PresenceCache` / `LockedChannelHint` seams.
//! - `health`: the live receive-health publish seam (`SharedRxHealth`).
//! - `hint`: the default Contract-E hint sink (`FileLockedChannelHint`).
//! - this module root: the `ValidPacketWatchdog` FSM + its golden scenarios.

use std::sync::Arc;
use std::time::Duration;

use crate::acquire::ChannelAcquirer;

pub mod health;
pub mod hint;
pub mod seams;

pub use health::SharedRxHealth;
pub use hint::FileLockedChannelHint;
pub use seams::{
    Clock, LockedChannelHint, PresenceCache, RxProcess, COLD_START_HOME_HOLD_S,
    PEER_PRESENCE_FRESH_S, PEER_PRESENCE_LOST_S, RX_HEALTH_SILENCE_THRESHOLD_S,
    VALID_RX_POLL_INTERVAL_S, VALID_RX_SILENCE_THRESHOLD_S,
};

/// Mutable watchdog state + injected seams. Owning the state in one struct keeps
/// the FSM transcription a 1:1 mirror of the Python method body.
pub struct ValidPacketWatchdog {
    pub interface: String,
    /// Current operating channel (the receiver tunes this; never the home).
    pub channel: u8,
    /// The configured immutable rendezvous home channel (`_config.channel`).
    pub home_channel: u8,
    pub running: bool,
    pub reacquire_kills: u32,
    pub ever_linked: bool,
    pub cold_sweep_done: bool,
    pub cold_start_at: f64,
    pub last_valid_rx_change_at: f64,
    /// The cumulative valid-decode count observed at the previous poll. The
    /// per-poll delta against the live counter drives `update_rx_rates`, so a
    /// healthy video stream refreshes `last_valid_rx_change_at` even when no peer
    /// beacon is heard. Seeded at run start; never written by another task.
    last_valid_count: i64,

    clock: Arc<dyn Clock>,
    rx: Arc<dyn RxProcess>,
    presence: Arc<dyn PresenceCache>,
    hint: Arc<dyn LockedChannelHint>,
    acquirer: ChannelAcquirer,
    /// Live receive-health publish seam. The watchdog mirrors its
    /// `reacquire_kills` + valid-decode silence here each poll so the stats
    /// reader can carry the real values on the sidecar. `None` in unit tests.
    health: Option<SharedRxHealth>,

    // Overridable thresholds so tests can drive a branch on the first poll
    // (the Python tests `patch` the module constants for the same purpose).
    poll_interval_s: f64,
    silence_threshold_s: f64,
    cold_home_hold_s: f64,
}

impl ValidPacketWatchdog {
    /// Build a watchdog with the production thresholds.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        interface: impl Into<String>,
        channel: u8,
        home_channel: u8,
        clock: Arc<dyn Clock>,
        rx: Arc<dyn RxProcess>,
        presence: Arc<dyn PresenceCache>,
        hint: Arc<dyn LockedChannelHint>,
        acquirer: ChannelAcquirer,
    ) -> Self {
        let now = clock.monotonic();
        Self {
            interface: interface.into(),
            channel,
            home_channel,
            running: true,
            reacquire_kills: 0,
            ever_linked: false,
            cold_sweep_done: false,
            cold_start_at: now,
            last_valid_rx_change_at: 0.0,
            last_valid_count: 0,
            clock,
            rx,
            presence,
            hint,
            acquirer,
            health: None,
            poll_interval_s: VALID_RX_POLL_INTERVAL_S,
            silence_threshold_s: VALID_RX_SILENCE_THRESHOLD_S,
            cold_home_hold_s: COLD_START_HOME_HOLD_S,
        }
    }

    /// Attach the live receive-health publish seam so the stats reader can carry
    /// the real `reacquire_kills` + valid-decode silence on the sidecar.
    pub fn with_health(mut self, health: SharedRxHealth) -> Self {
        self.health = Some(health);
        self
    }

    /// Override the poll interval (test seam; the production loop uses the
    /// constant).
    pub fn set_poll_interval_s(&mut self, v: f64) {
        self.poll_interval_s = v;
    }

    /// Override the silence threshold (test seam, mirrors `patch(...)`).
    pub fn set_silence_threshold_s(&mut self, v: f64) {
        self.silence_threshold_s = v;
    }

    /// Override the cold-start hold budget (test seam, mirrors `patch(...)`).
    pub fn set_cold_home_hold_s(&mut self, v: f64) {
        self.cold_home_hold_s = v;
    }

    /// Track the valid-decode packet rate. `packets > 0` stamps the change
    /// time, marks `ever_linked`, clears the cold-sweep bookkeeping, and marks
    /// the acquirer locked on the current channel (valid decodes on the current
    /// channel ARE a lock; a sweep is only one way to reach that state).
    pub fn update_rx_rates(&mut self, packets_received: i64) {
        if packets_received > 0 {
            self.last_valid_rx_change_at = self.clock.monotonic();
            self.ever_linked = true;
            self.cold_sweep_done = false;
            self.acquirer.mark_locked(self.channel);
        }
    }

    /// The receive-side valid-packet watchdog loop. Reacquire then restart when
    /// the video plane goes silent, but only when the peer is also gone. Returns
    /// when the loop exits (process gone, terminated for restart, or
    /// `running` cleared by the driver).
    ///
    /// This is a byte-for-byte transcription of the Python
    /// `_valid_packet_watchdog`: branch order, the `continue`/`return` control
    /// flow, and the state writes match the source exactly.
    pub async fn run(&mut self) {
        // Guard: no process or acquirer means nothing to watch. (The acquirer is
        // always present in this Rust port, so only the process is checked.)
        if !self.rx.is_running() {
            return;
        }
        // Seed the change stamp so the first poll window is full rather than
        // carrying over silence accumulated while the receive process spawned.
        self.last_valid_rx_change_at = self.clock.monotonic();
        // Baseline the valid-decode counter so the first poll's delta measures
        // only decodes that arrive after the receive process is up.
        self.last_valid_count = self.acquirer.valid_packets();
        // Restart the cold-start hold budget for this receive generation. If the
        // link was never established the receiver gets a fresh bounded hold on
        // the home channel before the one self-heal sweep.
        if !self.ever_linked {
            self.cold_start_at = self.clock.monotonic();
            self.cold_sweep_done = false;
        }
        while self.running && self.rx.is_running() {
            tokio::time::sleep(Duration::from_secs_f64(self.poll_interval_s)).await;

            // Observe live video off the SAME shared valid-decode counter the
            // acquirer reads: a positive delta since the last poll means the
            // video plane decoded valid packets this interval, which refreshes
            // the silence timer (and marks the link locked) exactly as a peer
            // beacon would. Without this, a healthy stream the operator can see
            // (but whose peer-presence beacon is being dropped) would trip the
            // genuine-loss teardown. Read-only against the counter; the stats
            // reader remains its sole writer.
            let current = self.acquirer.valid_packets();
            let delta = current - self.last_valid_count;
            self.last_valid_count = current;
            if delta > 0 {
                self.update_rx_rates(delta);
            }

            let silent_for = self.clock.monotonic() - self.last_valid_rx_change_at;
            // Publish the live valid-decode silence for the sidecar before any
            // continue/return path so the GS heartbeat reports a real number.
            if let Some(h) = &self.health {
                h.set_silent_seconds(silent_for).await;
            }
            if silent_for < self.silence_threshold_s {
                continue;
            }

            // Video plane silent for the window. Decide whether this is a
            // genuine loss-of-link or an idle-but-paired link.
            if self.presence.peer_present() {
                // The peer is alive (recent presence beacon) but not sending
                // video. This is normal; do not touch the radio. Seeing the peer
                // counts as having been linked, so a later silence is a real loss
                // the sweep may act on.
                self.ever_linked = true;
                self.cold_sweep_done = false;
                tracing::info!(
                    interface = %self.interface,
                    channel = self.channel,
                    peer_presence_age_s = self.presence.presence_age_s().unwrap_or(0.0),
                    "ground_wfb_paired_no_video"
                );
                continue;
            }

            // Marginal-link grace: the peer was seen recently (within the loss
            // window) but not within the strict fresh window. Hold the home
            // channel, do not sweep, do not terminate. Escalate only once
            // presence has been gone past the loss window.
            let age = self.presence.presence_age_s();
            if self.ever_linked && age.is_some_and(|a| a <= PEER_PRESENCE_LOST_S) {
                tracing::info!(
                    interface = %self.interface,
                    channel = self.channel,
                    peer_presence_age_s = age.unwrap_or(0.0),
                    "ground_wfb_presence_gap_hold"
                );
                continue;
            }

            // Rendezvous-first cold start: hold the home channel rather than
            // sweep immediately. Until the link has been established once the
            // transmitter is broadcasting on the fixed home channel, so a
            // correctly-configured receiver simply stays there and links. But
            // holding forever would deadlock a pair whose home channels are
            // mismatched, so after a bounded hold run exactly ONE acquire sweep
            // to self-heal, then fall back to holding home if it finds nothing.
            if !self.ever_linked {
                let home = self.home_channel;
                let cold_for = self.clock.monotonic() - self.cold_start_at;
                if cold_for < self.cold_home_hold_s || self.cold_sweep_done {
                    tracing::info!(
                        interface = %self.interface,
                        channel = self.channel,
                        cold_seconds = cold_for,
                        "ground_wfb_unlinked_hold_home"
                    );
                    continue;
                }

                // Budget elapsed unlinked: one self-heal sweep, then home.
                self.cold_sweep_done = true;
                self.acquirer.mark_unlocked();
                tracing::warn!(
                    interface = %self.interface,
                    channel = self.channel,
                    cold_seconds = cold_for,
                    "ground_wfb_cold_self_heal_sweep"
                );
                let mut cold_locked: Option<u8> = None;
                let cold_announced = self.presence.announced_channel();
                if let Some(ann) = cold_announced {
                    if ann != self.channel && self.acquirer.acquire_target(ann).await {
                        cold_locked = Some(ann);
                    }
                }
                if cold_locked.is_none() {
                    cold_locked = self.acquirer.acquire().await;
                }
                if let Some(locked) = cold_locked {
                    self.channel = locked;
                    self.hint.persist(locked);
                    self.last_valid_rx_change_at = self.clock.monotonic();
                    continue;
                }
                // Sweep found nothing: return to the home channel so the next
                // rendezvous attempt happens where the drone homes, and resume
                // holding (the one-shot flag prevents another sweep until a link
                // is established or the manager restarts).
                //
                // Always retune home, unconditionally. The sweep just drove the
                // radio across every candidate and left it tuned to the LAST one;
                // `self.channel` was never updated to track that, so it still
                // reads the home value and cannot gate the retune. The old
                // `if home != self.channel` guard was therefore dead whenever the
                // sweep started from home (the common cold-start case) and left
                // the netdev stranded on the last swept channel while the heartbeat
                // reported home.
                self.acquirer.try_channel(home).await;
                self.channel = home;
                self.last_valid_rx_change_at = self.clock.monotonic();
                tracing::info!(
                    interface = %self.interface,
                    channel = self.channel,
                    "ground_wfb_cold_self_heal_returned_home"
                );
                continue;
            }

            // No peer presence and no video. The link is genuinely down.
            // Reacquire the channel before resorting to a process restart.
            self.acquirer.mark_unlocked();
            tracing::warn!(
                interface = %self.interface,
                silent_seconds = silent_for,
                channel = self.channel,
                "ground_wfb_valid_rx_silent"
            );
            let mut locked: Option<u8> = None;
            let announced = self.presence.announced_channel();
            if let Some(ann) = announced {
                // Beacon-guided lock: try the peer's last announced channel with
                // a single dwell before the blind sweep.
                if ann != self.channel && self.acquirer.acquire_target(ann).await {
                    locked = Some(ann);
                }
            }
            if locked.is_none() {
                locked = self.acquirer.acquire().await;
            }
            if let Some(locked) = locked {
                self.channel = locked;
                self.hint.persist(locked);
                self.last_valid_rx_change_at = self.clock.monotonic();
                continue;
            }
            // Reacquisition failed across the whole band. The sweep left the
            // radio tuned to the last swept candidate; return it to the
            // rendezvous home before respawning so the new receiver starts where
            // the transmitter homes, not on a stray swept channel.
            self.acquirer.try_channel(self.home_channel).await;
            self.channel = self.home_channel;
            // Terminate so the run loop respawns the receive process (the
            // subprocess itself may be wedged, not just the channel).
            self.reacquire_kills += 1;
            if let Some(h) = &self.health {
                h.set_reacquire_kills(self.reacquire_kills);
            }
            tracing::warn!(
                interface = %self.interface,
                reacquire_kills_total = self.reacquire_kills,
                "ground_wfb_reacquire_failed"
            );
            self.rx.terminate();
            self.last_valid_rx_change_at = self.clock.monotonic();
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acquire::{ChannelSetter, ValidPacketCounter};
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Mutex;

    // ---- fakes -------------------------------------------------------------

    /// Clock that returns a fixed "now". Silence is forced by patching the
    /// silence threshold to 0.0 (mirrors the Python tests), so the clock just
    /// needs to be monotone and stable.
    struct FakeClock {
        now: Mutex<f64>,
    }
    impl FakeClock {
        fn at(now: f64) -> Arc<Self> {
            Arc::new(Self {
                now: Mutex::new(now),
            })
        }
    }
    impl Clock for FakeClock {
        fn monotonic(&self) -> f64 {
            *self.now.lock().unwrap()
        }
    }

    /// Receive process fake. The seed-guard `is_running` check at the top of
    /// `run()` plus the first `while`-condition check must both see "alive"; the
    /// loop then runs exactly ONE body iteration and the next `while`-condition
    /// check sees "dead", ending the loop. This mirrors the Python one-shot
    /// latch (`_peer_present` flips `_running` off after the first call) without
    /// coupling the latch to a branch that some scenarios never reach.
    struct FakeRx {
        terminated: AtomicU32,
        live_checks: AtomicU32,
        /// Number of `is_running` calls that report alive before reporting dead.
        alive_for: u32,
    }
    impl FakeRx {
        fn new() -> Arc<Self> {
            // Guard check (1) + first while-condition (2) alive; the
            // post-iteration while-condition (3) reports dead → one iteration.
            Arc::new(Self {
                terminated: AtomicU32::new(0),
                live_checks: AtomicU32::new(0),
                alive_for: 2,
            })
        }
    }
    impl RxProcess for FakeRx {
        fn is_running(&self) -> bool {
            let n = self.live_checks.fetch_add(1, Ordering::SeqCst);
            n < self.alive_for
        }
        fn terminate(&self) {
            self.terminated.fetch_add(1, Ordering::SeqCst);
        }
        fn terminate_count(&self) -> u32 {
            self.terminated.load(Ordering::SeqCst)
        }
    }

    /// Presence fake: fixed age + announced channel.
    struct FakePresence {
        age: Option<f64>,
        announced: Option<u8>,
    }
    impl FakePresence {
        fn new(age: Option<f64>, announced: Option<u8>) -> Arc<Self> {
            Arc::new(Self { age, announced })
        }
    }
    impl PresenceCache for FakePresence {
        fn presence_age_s(&self) -> Option<f64> {
            self.age
        }
        fn announced_channel(&self) -> Option<u8> {
            self.announced
        }
    }

    /// Recording hint sink.
    struct RecordingHint {
        last: Mutex<Option<u8>>,
        calls: AtomicU32,
    }
    impl RecordingHint {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                last: Mutex::new(None),
                calls: AtomicU32::new(0),
            })
        }
    }
    impl LockedChannelHint for RecordingHint {
        fn persist(&self, channel: u8) {
            *self.last.lock().unwrap() = Some(channel);
            self.calls.fetch_add(1, Ordering::SeqCst);
        }
    }

    // ---- scriptable acquirer over the acquire.rs seams ---------------------

    /// A counter whose increments are scripted per-call: a queue of returns is
    /// not needed because the acquirer's `try_channel_locked` reads baseline
    /// then polls; we make the per-channel decode programmable through the
    /// setter recording the channel and the counter answering for it.
    struct ScriptCounter {
        good: Mutex<std::collections::BTreeSet<u8>>,
        current: Mutex<Option<u8>>,
        value: AtomicU32,
    }
    impl ScriptCounter {
        fn new(good: &[u8]) -> Arc<Self> {
            Arc::new(Self {
                good: Mutex::new(good.iter().copied().collect()),
                current: Mutex::new(None),
                value: AtomicU32::new(0),
            })
        }
    }
    impl ValidPacketCounter for ScriptCounter {
        fn valid_packets(&self) -> i64 {
            let cur = *self.current.lock().unwrap();
            if let Some(ch) = cur {
                if self.good.lock().unwrap().contains(&ch) {
                    return self.value.fetch_add(1, Ordering::SeqCst) as i64 + 1;
                }
            }
            self.value.load(Ordering::SeqCst) as i64
        }
    }
    struct ScriptSetter {
        counter: Arc<ScriptCounter>,
    }
    impl ChannelSetter for ScriptSetter {
        fn set_channel<'a>(
            &'a self,
            _iface: &'a str,
            channel: u8,
        ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
            Box::pin(async move {
                *self.counter.current.lock().unwrap() = Some(channel);
                true
            })
        }
    }

    /// Build an acquirer whose sweeps/targets lock iff the tuned channel is in
    /// `good`. With `good` empty, every sweep/target fails (models acquire→None,
    /// acquire_target→false).
    fn acquirer(good: &[u8], band: &str) -> ChannelAcquirer {
        let counter = ScriptCounter::new(good);
        let setter = Arc::new(ScriptSetter {
            counter: counter.clone(),
        });
        ChannelAcquirer::new("wlan0", band, counter, setter, 0.0, 3, None)
    }

    /// Assemble a watchdog wired like the Python `_make_manager`: silence
    /// threshold + poll interval patched to 0.0 so one poll runs the chosen
    /// branch, process alive, `ever_linked` defaulting true (the linked cases).
    /// The driver clears `running` after the first sleep so the loop runs
    /// exactly one iteration even on the no-action path.
    #[allow(clippy::too_many_arguments)]
    fn make(
        good: &[u8],
        band: &str,
        rx: Arc<FakeRx>,
        presence: Arc<FakePresence>,
        hint: Arc<RecordingHint>,
        channel: u8,
        home: u8,
        ever_linked: bool,
    ) -> (ValidPacketWatchdog, Arc<FakeClock>) {
        let clock = FakeClock::at(1000.0);
        let mut wd = ValidPacketWatchdog::new(
            "wlan0",
            channel,
            home,
            clock.clone(),
            rx,
            presence,
            hint,
            acquirer(good, band),
        );
        wd.set_poll_interval_s(0.0);
        wd.set_silence_threshold_s(0.0);
        wd.ever_linked = ever_linked;
        (wd, clock)
    }

    /// Run a single poll iteration. The `FakeRx` reports alive for the guard
    /// check and the first `while`-condition, then dead, so the loop body runs
    /// exactly once before the loop exits, the same one-shot bound the Python
    /// test gets from its `_peer_present` latch + `_run_watchdog` wrapper. A
    /// belt-and-suspenders timeout guards against any future-hang regression.
    async fn run_one(wd: &mut ValidPacketWatchdog) {
        let _ = tokio::time::timeout(Duration::from_secs(2), wd.run()).await;
    }

    // ---- the 10 golden scenarios ------------------------------------------

    // 1. Video flowing (timestamp fresh) → watchdog does nothing.
    #[tokio::test]
    async fn test_video_flowing_no_action() {
        let rx = FakeRx::new();
        let presence = FakePresence::new(None, None);
        let hint = RecordingHint::new();
        // Fresh timestamp == not silent. We model "fresh" by keeping the silence
        // threshold high so the silence branch never trips.
        let (mut wd, _clk) = make(
            &[149],
            "u-nii-3",
            rx.clone(),
            presence,
            hint.clone(),
            149,
            149,
            true,
        );
        wd.set_silence_threshold_s(9999.0);
        run_one(&mut wd).await;

        assert_eq!(rx.terminate_count(), 0);
        assert_eq!(wd.reacquire_kills, 0);
        assert_eq!(hint.calls.load(Ordering::SeqCst), 0);
    }

    // 2. Video silent BUT peer present → "paired, no video", no sweep, no kill.
    #[tokio::test]
    async fn test_silent_but_peer_present_does_not_sweep_or_kill() {
        let rx = FakeRx::new();
        let presence = FakePresence::new(Some(5.0), None); // fresh
        let hint = RecordingHint::new();
        let (mut wd, _clk) = make(
            &[157],
            "u-nii-3",
            rx.clone(),
            presence,
            hint.clone(),
            149,
            149,
            true,
        );
        run_one(&mut wd).await;

        assert_eq!(rx.terminate_count(), 0);
        assert_eq!(wd.reacquire_kills, 0);
        assert_eq!(hint.calls.load(Ordering::SeqCst), 0);
        // Peer present means it counts as ever-linked and clears the cold flag.
        assert!(wd.ever_linked);
        assert!(!wd.cold_sweep_done);
    }

    // 3. Video silent AND no peer, reacquire succeeds → channel relocked.
    #[tokio::test]
    async fn test_silent_no_peer_reacquire_succeeds_no_terminate() {
        let rx = FakeRx::new();
        let presence = FakePresence::new(None, None); // no peer ever
        let hint = RecordingHint::new();
        // The blind sweep locks 157.
        let (mut wd, _clk) = make(
            &[157],
            "u-nii-3",
            rx.clone(),
            presence,
            hint.clone(),
            149,
            149,
            true,
        );
        run_one(&mut wd).await;

        assert_eq!(wd.channel, 157);
        assert_eq!(*hint.last.lock().unwrap(), Some(157));
        assert_eq!(rx.terminate_count(), 0);
        assert_eq!(wd.reacquire_kills, 0);
    }

    // 4. Video silent AND no peer, reacquire fails → terminate for restart.
    #[tokio::test]
    async fn test_silent_no_peer_reacquire_fails_terminates() {
        let rx = FakeRx::new();
        let presence = FakePresence::new(None, None);
        let hint = RecordingHint::new();
        // Nothing decodes → acquire() returns None.
        let (mut wd, _clk) = make(&[], "u-nii-3", rx.clone(), presence, hint, 149, 149, true);
        run_one(&mut wd).await;

        assert_eq!(rx.terminate_count(), 1);
        assert_eq!(wd.reacquire_kills, 1);
    }

    // 5. Video silent, no peer, peer announced a channel → beacon-guided lock
    //    tried before the blind sweep.
    #[tokio::test]
    async fn test_silent_no_peer_beacon_guided_lock_tried_first() {
        let rx = FakeRx::new();
        // No fresh presence (age None), but an announced channel 44.
        let presence = FakePresence::new(None, Some(44));
        let hint = RecordingHint::new();
        // 44 decodes via acquire_target; the blind sweep is never needed.
        let (mut wd, _clk) = make(
            &[44],
            "u-nii-3",
            rx.clone(),
            presence,
            hint.clone(),
            149,
            149,
            true,
        );
        run_one(&mut wd).await;

        assert_eq!(wd.channel, 44);
        assert_eq!(*hint.last.lock().unwrap(), Some(44));
        assert_eq!(rx.terminate_count(), 0);
    }

    // 6. Cold start (never linked) + silent + no peer → hold home, no sweep.
    #[tokio::test]
    async fn test_cold_start_never_linked_holds_home_no_sweep() {
        let rx = FakeRx::new();
        let presence = FakePresence::new(None, None);
        let hint = RecordingHint::new();
        // ever_linked=false, default cold hold budget (75s) not elapsed → hold.
        let (mut wd, _clk) = make(
            &[157],
            "u-nii-3",
            rx.clone(),
            presence,
            hint.clone(),
            149,
            149,
            false,
        );
        run_one(&mut wd).await;

        assert_eq!(rx.terminate_count(), 0);
        assert_eq!(wd.reacquire_kills, 0);
        assert_eq!(wd.channel, 149); // stays on home
        assert_eq!(hint.calls.load(Ordering::SeqCst), 0);
    }

    // 7. Cold start past hold budget → one self-heal sweep, locks + persists.
    #[tokio::test]
    async fn test_cold_start_budget_elapsed_runs_one_self_heal_sweep() {
        let rx = FakeRx::new();
        let presence = FakePresence::new(None, None);
        let hint = RecordingHint::new();
        let (mut wd, _clk) = make(
            &[157],
            "u-nii-3",
            rx.clone(),
            presence,
            hint.clone(),
            149,
            149,
            false,
        );
        // Zero cold hold budget so the freshly-seeded cold timer is already past.
        wd.set_cold_home_hold_s(0.0);
        run_one(&mut wd).await;

        assert_eq!(wd.channel, 157);
        assert_eq!(*hint.last.lock().unwrap(), Some(157));
        assert!(wd.cold_sweep_done);
        assert_eq!(rx.terminate_count(), 0);
    }

    // 8. Cold self-heal sweep finds nothing → return to home channel, no kill.
    #[tokio::test]
    async fn test_cold_start_budget_elapsed_sweep_fails_returns_home() {
        let rx = FakeRx::new();
        let presence = FakePresence::new(None, None);
        let hint = RecordingHint::new();
        // Drifted off home (161) from an earlier attempt; home is 149. Nothing
        // decodes so the sweep fails and we try_channel back to home.
        let (mut wd, _clk) = make(
            &[],
            "u-nii-3",
            rx.clone(),
            presence,
            hint.clone(),
            161,
            149,
            false,
        );
        wd.set_cold_home_hold_s(0.0);
        run_one(&mut wd).await;

        assert_eq!(wd.channel, 149); // returned to home
        assert_eq!(rx.terminate_count(), 0);
        assert_eq!(wd.reacquire_kills, 0);
        assert!(wd.cold_sweep_done);
    }

    // 9. Linked + silent + presence gap inside the loss window → hold, no sweep.
    #[tokio::test]
    async fn test_silent_marginal_presence_gap_holds_no_sweep() {
        let rx = FakeRx::new();
        // 60 s: past the 30 s fresh window but inside the 120 s loss window.
        let presence = FakePresence::new(Some(60.0), None);
        let hint = RecordingHint::new();
        let (mut wd, _clk) = make(
            &[157],
            "u-nii-3",
            rx.clone(),
            presence,
            hint.clone(),
            149,
            149,
            true,
        );
        run_one(&mut wd).await;

        assert_eq!(rx.terminate_count(), 0);
        assert_eq!(wd.reacquire_kills, 0);
        assert_eq!(wd.channel, 149); // held home
        assert_eq!(hint.calls.load(Ordering::SeqCst), 0);
    }

    // 10. Linked + silent + presence gone past the loss window → genuine loss,
    //     sweep runs.
    #[tokio::test]
    async fn test_silent_presence_lost_beyond_window_sweeps() {
        let rx = FakeRx::new();
        // 200 s: beyond the 120 s loss window.
        let presence = FakePresence::new(Some(200.0), None);
        let hint = RecordingHint::new();
        let (mut wd, _clk) = make(
            &[157],
            "u-nii-3",
            rx.clone(),
            presence,
            hint,
            149,
            149,
            true,
        );
        run_one(&mut wd).await;

        assert_eq!(wd.channel, 157);
    }

    // ---- update_rx_rates ---------------------------------------------------

    #[test]
    fn update_rx_rates_marks_linked_on_positive_packets() {
        let rx = FakeRx::new();
        let presence = FakePresence::new(None, None);
        let hint = RecordingHint::new();
        let (mut wd, _clk) = make(&[], "u-nii-3", rx, presence, hint, 149, 149, false);
        wd.cold_sweep_done = true;
        wd.update_rx_rates(5);
        assert!(wd.ever_linked);
        assert!(!wd.cold_sweep_done);
        assert!(wd.last_valid_rx_change_at > 0.0);

        // Zero packets is a no-op.
        let before = wd.last_valid_rx_change_at;
        wd.ever_linked = false;
        wd.update_rx_rates(0);
        assert!(!wd.ever_linked);
        assert_eq!(wd.last_valid_rx_change_at, before);
    }

    // ---- F1: live-video observation off the shared counter -----------------

    /// A counter that advances on every read, modelling a healthy video stream
    /// decoding valid packets regardless of the channel. The watchdog reads this
    /// through its acquirer once at seed time and once per poll, so a positive
    /// per-poll delta drives `update_rx_rates` and keeps the silence timer fresh.
    struct FlowingCounter {
        value: AtomicU32,
    }
    impl FlowingCounter {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                value: AtomicU32::new(0),
            })
        }
    }
    impl ValidPacketCounter for FlowingCounter {
        fn valid_packets(&self) -> i64 {
            self.value.fetch_add(1, Ordering::SeqCst) as i64 + 1
        }
    }

    /// Build a watchdog over an injected counter (here a flowing stream) with a
    /// no-op setter, so the test exercises the run-loop's live-video observation
    /// rather than the sweep machinery.
    fn make_with_counter(
        counter: Arc<dyn ValidPacketCounter>,
        rx: Arc<FakeRx>,
        presence: Arc<FakePresence>,
        hint: Arc<RecordingHint>,
        channel: u8,
        home: u8,
    ) -> ValidPacketWatchdog {
        // A setter that always succeeds but the flowing counter ignores: the
        // sweep is never expected to run on the live-video path.
        let dummy = ScriptCounter::new(&[]);
        let setter = Arc::new(ScriptSetter { counter: dummy });
        let acquirer = ChannelAcquirer::new("wlan0", "u-nii-3", counter, setter, 0.0, 3, None);
        let clock = FakeClock::at(1000.0);
        let mut wd =
            ValidPacketWatchdog::new("wlan0", channel, home, clock, rx, presence, hint, acquirer);
        wd.set_poll_interval_s(0.0);
        // A real silence threshold so a fresh stamp keeps silent_for below it.
        wd.set_silence_threshold_s(12.0);
        wd
    }

    // Healthy video, no peer beacon, fixed clock: the per-poll counter delta
    // refreshes the silence timer via update_rx_rates, so the watchdog neither
    // sweeps nor terminates. This is the GS self-heal teardown regression the
    // wiring fixes: without the live-counter observation a healthy stream whose
    // presence beacon is being dropped would trip the genuine-loss kill.
    #[tokio::test]
    async fn test_healthy_video_no_beacon_does_not_tear_down() {
        let counter = FlowingCounter::new();
        let rx = FakeRx::new();
        let presence = FakePresence::new(None, None); // no peer beacon at all
        let hint = RecordingHint::new();
        let mut wd = make_with_counter(counter, rx.clone(), presence, hint.clone(), 149, 149);
        run_one(&mut wd).await;

        // No teardown, no sweep, no channel change: the link is plainly healthy.
        assert_eq!(rx.terminate_count(), 0);
        assert_eq!(wd.reacquire_kills, 0);
        assert_eq!(hint.calls.load(Ordering::SeqCst), 0);
        assert_eq!(wd.channel, 149);
        // Live decodes mark the link as established.
        assert!(wd.ever_linked);
    }

    // ---- F4: the live receive-health publish seam --------------------------

    #[tokio::test]
    async fn health_seam_mirrors_reacquire_kills_on_genuine_loss() {
        let health = SharedRxHealth::new();
        // No peer, nothing decodes → the genuine-loss path runs and the band
        // sweep fails, so the watchdog terminates and bumps reacquire_kills. The
        // health seam must carry that real count for the sidecar.
        let rx = FakeRx::new();
        let presence = FakePresence::new(None, None);
        let hint = RecordingHint::new();
        let (mut wd, _clk) = make(&[], "u-nii-3", rx.clone(), presence, hint, 149, 149, true);
        wd = wd.with_health(health.clone());
        run_one(&mut wd).await;

        assert_eq!(rx.terminate_count(), 1);
        assert_eq!(wd.reacquire_kills, 1);
        assert_eq!(health.reacquire_kills(), 1);
    }

    #[tokio::test]
    async fn health_seam_publishes_silence_each_poll() {
        // Even on a no-action poll (silent below threshold) the seam records the
        // current valid-decode silence so the sidecar reports a real number.
        let counter = FlowingCounter::new();
        let rx = FakeRx::new();
        let presence = FakePresence::new(None, None);
        let hint = RecordingHint::new();
        let health = SharedRxHealth::new();
        let mut wd = make_with_counter(counter, rx.clone(), presence, hint, 149, 149)
            .with_health(health.clone());
        run_one(&mut wd).await;

        // The seam was written at least once with a concrete (non-None) value.
        assert!(health.silent_seconds().await.is_some());
    }
}
