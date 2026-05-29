//! Ground-side channel acquisition: sweep candidates until valid decode.
//!
//! When the receiver comes up it has no way to know which channel the
//! transmitter is actually on. The configured channel can be wrong: the
//! transmitter may have hopped to a quieter frequency, the operator may have
//! changed the band, or a half-finished bind left the two sides on different
//! channels. Sitting on a single channel the transmitter is not using yields a
//! permanently dry link with no error in the log.
//!
//! This module sweeps the configured band's candidate channels, dwelling
//! briefly on each, and locks onto the first one where valid packets are
//! decoded (the valid-decode counter increments). That counter is the only
//! trustworthy signal: the interface `rx_packets` counter is inflated by
//! ambient RF the receiver cannot decode, so it never reaches zero outdoors and
//! cannot tell "we hear our peer" from "we hear noise".
//!
//! The transmitter shortcuts the scan by advertising its current operating
//! channel in the control-plane presence beacon; when the receiver hears that
//! beacon it sets the announced channel directly and verifies a valid decode
//! rather than sweeping the whole band.

use std::collections::BTreeSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use ados_radio::channel::STANDARD_CHANNELS;

/// Per-channel dwell while sweeping. Long enough for the transmitter's next FEC
/// block + the receiver's decode to land (the radio retune blackout is
/// ~100-300 ms; a healthy stream emits a valid packet within a few hundred ms
/// once tuned), short enough that a full nine-channel sweep finishes in well
/// under ten seconds.
pub const DWELL_SECONDS: f64 = 0.8;

/// Number of valid-packet samples to read inside each dwell. The receiver stats
/// file is refreshed ~1 Hz; we poll it a few times per dwell so a decode that
/// lands mid-dwell is caught before we move on.
pub const DWELL_POLLS: u32 = 4;

/// Silence window: valid-packet delta flat for this long while unlocked (or
/// after a lock that went dry) triggers a fresh sweep. Matches the
/// receive-liveness silence window so the two watchdogs agree.
pub const VALID_PACKET_SILENCE_SECONDS: f64 = 12.0;

/// Periodic re-attempt cadence while unlocked and no peer beacon has pointed us
/// at a channel.
pub const PERIODIC_RETRY_SECONDS: f64 = 20.0;

/// Bound on consecutive full sweeps before reporting no-peer and pausing until
/// the next external trigger. Keeps a receiver with no peer in range from
/// burning the radio on an endless scan.
pub const MAX_SWEEP_ROUNDS: u32 = 3;

/// Acquisition lifecycle state, surfaced on the receiver status. The wire
/// strings match the Python `AcquireState` `StrEnum` values exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcquireState {
    Idle,
    Searching,
    Locked,
    NoPeer,
}

impl AcquireState {
    /// The status-surface wire string ("idle" / "searching" / "locked" /
    /// "no-peer").
    pub fn as_str(self) -> &'static str {
        match self {
            AcquireState::Idle => "idle",
            AcquireState::Searching => "searching",
            AcquireState::Locked => "locked",
            AcquireState::NoPeer => "no-peer",
        }
    }
}

/// Reads the current cumulative valid-decode packet counter. Injected so the
/// acquirer stays decoupled from the receiver manager's internals and is
/// trivially unit-testable with a synthetic counter series.
pub trait ValidPacketCounter: Send + Sync {
    fn valid_packets(&self) -> i64;
}

impl<F> ValidPacketCounter for F
where
    F: Fn() -> i64 + Send + Sync,
{
    fn valid_packets(&self) -> i64 {
        self()
    }
}

/// Retunes the interface to a channel. Injected so a sweep can be exercised
/// against a synthetic radio in tests. Returns `true` when the retune
/// succeeded.
pub trait ChannelSetter: Send + Sync {
    fn set_channel<'a>(
        &'a self,
        interface: &'a str,
        channel: u8,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>>;
}

/// Channel numbers to sweep for `band`, current-config-first order.
///
/// The configured band's channels come first so the common case (the peer is on
/// a channel inside the operator's chosen band) locks fast. Falls back to all
/// standard channels when the band key is unknown. Mirrors the Python
/// `candidate_channels`: the band lookup is on the RAW key, so only "u-nii-1",
/// "u-nii-3", and "all" match; anything else falls through to "all".
///
/// `enabled` filters the list to channels this adapter actually permits.
/// Channels the regulatory domain disables fail `iw set channel` with -22 and
/// waste a dwell, so they are dropped. None / empty means "could not determine"
/// and leaves the list unfiltered; a filter that would remove every channel is
/// ignored rather than refusing to sweep.
pub fn candidate_channels(band: &str, enabled: Option<&BTreeSet<u8>>) -> Vec<u8> {
    let band_numbers: Vec<u8> = match band {
        "u-nii-1" => vec![36, 40, 44, 48],
        "u-nii-3" => vec![149, 153, 157, 161, 165],
        // "all" and every unknown key use the full standard set.
        _ => STANDARD_CHANNELS.iter().map(|&(c, _)| c).collect(),
    };
    let mut ordered = band_numbers;
    // Append any remaining standard channels so a peer that hopped out of band
    // is still found, just later in the sweep.
    for &(ch, _) in STANDARD_CHANNELS {
        if !ordered.contains(&ch) {
            ordered.push(ch);
        }
    }
    if let Some(enabled) = enabled {
        if !enabled.is_empty() {
            let filtered: Vec<u8> = ordered
                .iter()
                .copied()
                .filter(|c| enabled.contains(c))
                .collect();
            // Only apply the filter when it leaves something to sweep.
            if !filtered.is_empty() {
                return filtered;
            }
        }
    }
    ordered
}

/// Sweep candidate channels until a valid decode locks the link.
///
/// Reads the current valid-packet counter through the injected counter and
/// retunes through the injected setter, so it is decoupled from the receiver
/// manager and trivially unit-testable.
pub struct ChannelAcquirer {
    interface: String,
    band: String,
    counter: Arc<dyn ValidPacketCounter>,
    setter: Arc<dyn ChannelSetter>,
    dwell_seconds: f64,
    max_sweep_rounds: u32,
    enabled_channels: Option<BTreeSet<u8>>,
    state: AcquireState,
    locked_channel: Option<u8>,
    // Retune serialization note: the Python predecessor used an `asyncio.Lock`
    // so the watchdog's full-band `acquire()` and the per-beacon
    // `acquire_target()` could not drive the channel setter on the same
    // interface concurrently and corrupt each other's dwell measurement. In this
    // Rust port every entry point (`acquire` / `acquire_target` / `try_channel`)
    // takes `&mut self`, so the borrow checker statically guarantees the same
    // exclusivity: a sweep can never interleave with a beacon verify. No runtime
    // lock is needed.
}

impl ChannelAcquirer {
    /// Build an acquirer. `setter` retunes the radio; `counter` reads the
    /// valid-decode counter.
    pub fn new(
        interface: impl Into<String>,
        band: impl Into<String>,
        counter: Arc<dyn ValidPacketCounter>,
        setter: Arc<dyn ChannelSetter>,
        dwell_seconds: f64,
        max_sweep_rounds: u32,
        enabled_channels: Option<BTreeSet<u8>>,
    ) -> Self {
        let enabled_channels = enabled_channels.filter(|s| !s.is_empty());
        Self {
            interface: interface.into(),
            band: band.into(),
            counter,
            setter,
            dwell_seconds,
            max_sweep_rounds,
            enabled_channels,
            state: AcquireState::Idle,
            locked_channel: None,
        }
    }

    pub fn state(&self) -> AcquireState {
        self.state
    }

    pub fn locked_channel(&self) -> Option<u8> {
        self.locked_channel
    }

    pub fn channel_locked(&self) -> bool {
        self.state == AcquireState::Locked
    }

    /// Drop the lock so the next trigger sweeps again. Called by the
    /// receive-liveness watchdog when a previously locked link goes silent (the
    /// transmitter may have hopped away).
    pub fn mark_unlocked(&mut self) {
        if self.state == AcquireState::Locked {
            tracing::info!(channel = ?self.locked_channel, "acquire_lock_dropped");
        }
        self.state = AcquireState::Searching;
        self.locked_channel = None;
    }

    /// Record that valid video is decoding on `channel`.
    ///
    /// A sweep is only ONE way the link becomes locked. A rig that boots already
    /// tuned to the persisted channel (the common case) never runs a sweep, yet
    /// it is plainly locked the moment valid decodes flow. The receive path
    /// calls this when packets are arriving so the lock state reflects reality.
    /// Idempotent.
    pub fn mark_locked(&mut self, channel: u8) {
        if self.state != AcquireState::Locked || self.locked_channel != Some(channel) {
            tracing::info!(channel, "acquire_locked_on_decode");
        }
        self.state = AcquireState::Locked;
        self.locked_channel = Some(channel);
    }

    /// Retune + dwell on `channel` (the lock is already held by the caller).
    ///
    /// Returns `true` if the valid-packet counter advances within the dwell
    /// window (the peer is on this channel and we are decoding it). On success
    /// the acquirer is left LOCKED on `channel`.
    async fn try_channel_locked(&mut self, channel: u8) -> bool {
        let baseline = self.counter.valid_packets();
        let ok = self.setter.set_channel(&self.interface, channel).await;
        if !ok {
            return false;
        }
        let per_poll = (self.dwell_seconds / DWELL_POLLS as f64).max(0.0);
        for _ in 0..DWELL_POLLS {
            tokio::time::sleep(Duration::from_secs_f64(per_poll)).await;
            if self.counter.valid_packets() > baseline {
                self.state = AcquireState::Locked;
                self.locked_channel = Some(channel);
                tracing::info!(interface = %self.interface, channel, "acquire_locked");
                return true;
            }
        }
        false
    }

    /// Tune to `channel` and watch for a valid-packet increment. Public entry
    /// point for standalone callers. `&mut self` serializes it against a sweep
    /// or beacon verify.
    pub async fn try_channel(&mut self, channel: u8) -> bool {
        self.try_channel_locked(channel).await
    }

    /// Sweep the band until a channel decodes valid packets. Returns the locked
    /// channel number, or `None` when no candidate produced a valid decode
    /// within the sweep bound (status left at `no-peer`). `&mut self` serializes
    /// the whole sweep against a concurrent beacon verify.
    pub async fn acquire(&mut self) -> Option<u8> {
        self.state = AcquireState::Searching;
        let channels = candidate_channels(&self.band, self.enabled_channels.as_ref());
        tracing::info!(
            interface = %self.interface,
            band = %self.band,
            candidates = ?channels,
            "acquire_sweep_start"
        );
        for _round in 0..self.max_sweep_rounds {
            for channel in &channels {
                if self.try_channel_locked(*channel).await {
                    return Some(*channel);
                }
            }
        }
        self.state = AcquireState::NoPeer;
        tracing::warn!(
            interface = %self.interface,
            band = %self.band,
            rounds = self.max_sweep_rounds,
            "acquire_no_peer"
        );
        None
    }

    /// Verify a beacon-announced channel with a single dwell. The transmitter
    /// advertises its operating channel in the presence beacon; rather than
    /// sweep the whole band the receiver tunes to that channel and confirms a
    /// valid decode. `&mut self` serializes it so it cannot race a concurrent
    /// sweep. Falls back to the caller's normal sweep trigger when the announced
    /// channel does not decode (the beacon may be stale or the peer just hopped
    /// again).
    pub async fn acquire_target(&mut self, channel: u8) -> bool {
        self.state = AcquireState::Searching;
        tracing::info!(interface = %self.interface, channel, "acquire_beacon_target");
        self.try_channel_locked(channel).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicI64, Ordering};

    /// Synthetic counter that increments only when "armed" for a target channel.
    struct SyntheticCounter {
        count: AtomicI64,
        /// Channels that will produce a valid decode once tuned to them.
        good_channels: BTreeSet<u8>,
        current: std::sync::Mutex<Option<u8>>,
    }

    impl SyntheticCounter {
        fn new(good: &[u8]) -> Arc<Self> {
            Arc::new(Self {
                count: AtomicI64::new(0),
                good_channels: good.iter().copied().collect(),
                current: std::sync::Mutex::new(None),
            })
        }
    }

    impl ValidPacketCounter for SyntheticCounter {
        fn valid_packets(&self) -> i64 {
            // A read on a "good" current channel advances the counter, so the
            // dwell sees an increment over its baseline.
            let cur = *self.current.lock().unwrap();
            if let Some(ch) = cur {
                if self.good_channels.contains(&ch) {
                    return self.count.fetch_add(1, Ordering::SeqCst) + 1;
                }
            }
            self.count.load(Ordering::SeqCst)
        }
    }

    /// Setter that records the channel the synthetic counter should consider
    /// current.
    struct SyntheticSetter {
        counter: Arc<SyntheticCounter>,
    }

    impl ChannelSetter for SyntheticSetter {
        fn set_channel<'a>(
            &'a self,
            _interface: &'a str,
            channel: u8,
        ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
            Box::pin(async move {
                *self.counter.current.lock().unwrap() = Some(channel);
                true
            })
        }
    }

    fn build(good: &[u8], band: &str) -> (ChannelAcquirer, Arc<SyntheticCounter>) {
        let counter = SyntheticCounter::new(good);
        let setter = Arc::new(SyntheticSetter {
            counter: counter.clone(),
        });
        let acq = ChannelAcquirer::new(
            "wlan0",
            band,
            counter.clone(),
            setter,
            // Zero dwell so the test loop is instant; the counter increments on
            // the first poll read for a good channel.
            0.0,
            MAX_SWEEP_ROUNDS,
            None,
        );
        (acq, counter)
    }

    #[test]
    fn constants_match_python_values() {
        assert_eq!(DWELL_SECONDS, 0.8);
        assert_eq!(DWELL_POLLS, 4);
        assert_eq!(VALID_PACKET_SILENCE_SECONDS, 12.0);
        assert_eq!(PERIODIC_RETRY_SECONDS, 20.0);
        assert_eq!(MAX_SWEEP_ROUNDS, 3);
    }

    #[test]
    fn acquire_state_wire_strings() {
        assert_eq!(AcquireState::Idle.as_str(), "idle");
        assert_eq!(AcquireState::Searching.as_str(), "searching");
        assert_eq!(AcquireState::Locked.as_str(), "locked");
        assert_eq!(AcquireState::NoPeer.as_str(), "no-peer");
    }

    #[test]
    fn candidate_channels_band_first_then_remaining() {
        // u-nii-3 channels first, then the u-nii-1 channels appended.
        let got = candidate_channels("u-nii-3", None);
        assert_eq!(got, vec![149, 153, 157, 161, 165, 36, 40, 44, 48]);

        // u-nii-1 first, then the rest.
        let got = candidate_channels("u-nii-1", None);
        assert_eq!(got, vec![36, 40, 44, 48, 149, 153, 157, 161, 165]);

        // Unknown band → full standard set (the "all" fallthrough).
        let got = candidate_channels("garbage", None);
        assert_eq!(got, vec![36, 40, 44, 48, 149, 153, 157, 161, 165]);
    }

    #[test]
    fn candidate_channels_enabled_filter_dropped_when_it_empties() {
        let enabled: BTreeSet<u8> = [149, 153].into_iter().collect();
        let got = candidate_channels("u-nii-3", Some(&enabled));
        assert_eq!(got, vec![149, 153]);

        // A filter that removes everything is ignored.
        let none_enabled: BTreeSet<u8> = [200].into_iter().collect();
        let got = candidate_channels("u-nii-3", Some(&none_enabled));
        assert_eq!(got.len(), 9);
    }

    #[tokio::test]
    async fn try_channel_locks_on_increment() {
        let (mut acq, _c) = build(&[149], "u-nii-3");
        assert!(acq.try_channel(149).await);
        assert_eq!(acq.state(), AcquireState::Locked);
        assert_eq!(acq.locked_channel(), Some(149));
        assert!(acq.channel_locked());
    }

    #[tokio::test]
    async fn try_channel_no_lock_on_dead_channel() {
        let (mut acq, _c) = build(&[157], "u-nii-3");
        assert!(!acq.try_channel(149).await);
        assert_eq!(acq.state(), AcquireState::Idle);
        assert_eq!(acq.locked_channel(), None);
    }

    #[tokio::test]
    async fn acquire_sweeps_and_locks_on_first_good_channel() {
        // Only 157 decodes; the sweep walks 149,153 (dead) then locks 157.
        let (mut acq, _c) = build(&[157], "u-nii-3");
        let got = acq.acquire().await;
        assert_eq!(got, Some(157));
        assert_eq!(acq.state(), AcquireState::Locked);
        assert_eq!(acq.locked_channel(), Some(157));
    }

    #[tokio::test]
    async fn acquire_no_peer_when_nothing_decodes() {
        let (mut acq, _c) = build(&[], "u-nii-3");
        let got = acq.acquire().await;
        assert_eq!(got, None);
        assert_eq!(acq.state(), AcquireState::NoPeer);
    }

    #[tokio::test]
    async fn acquire_target_single_dwell_verify() {
        let (mut acq, _c) = build(&[44], "u-nii-3");
        assert!(acq.acquire_target(44).await);
        assert_eq!(acq.locked_channel(), Some(44));

        // A target that does not decode fails the single dwell.
        let (mut acq, _c) = build(&[149], "u-nii-3");
        assert!(!acq.acquire_target(44).await);
    }

    #[tokio::test]
    async fn mark_locked_and_unlocked_are_idempotent() {
        let (mut acq, _c) = build(&[], "u-nii-3");
        acq.mark_locked(149);
        assert!(acq.channel_locked());
        assert_eq!(acq.locked_channel(), Some(149));
        acq.mark_locked(149); // idempotent
        assert!(acq.channel_locked());
        acq.mark_unlocked();
        assert_eq!(acq.state(), AcquireState::Searching);
        assert_eq!(acq.locked_channel(), None);
    }
}
