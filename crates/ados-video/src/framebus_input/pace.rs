//! Spacing frame-bus frames back out to their capture cadence.
//!
//! The encoder stamps a frame-bus frame with the time it reads it off the
//! pipe, so the time the feed writes a frame IS its timestamp on the wire. A
//! source that stalls and then delivers several frames at once (a CPU quota
//! period, a renderer catching up) would reach the viewer as a bunch of frames
//! with near-equal stamps followed by a gap. The pacer holds each frame until
//! its capture time plus a small offset, measured against the fastest recent
//! transit (arrival minus capture), so a burst leaves at the spacing it was
//! captured with.
//!
//! The offset adapts: it rises at once to the lateness of a late frame (that
//! frame is written immediately, and the ones behind it are spaced out after
//! it) and decays by 1/32 of a frame period per frame while frames arrive on
//! time. A source with steady delivery is therefore held for close to zero, and
//! the hold never exceeds [`MAX_HOLD_FRAMES`] frame periods.

/// How many frames may wait for the pipe: a whole catch-up burst from the
/// source (up to four frames at once) fits, so a burst is spaced out rather
/// than losing its oldest frame. The oldest is dropped when it is full.
pub(crate) const QUEUE_DEPTH: usize = 4;

/// The longest a frame is held, in frame periods. One less than the queue, so
/// a held frame is normally still in its ring slot when its turn comes.
const MAX_HOLD_FRAMES: u64 = 3;

/// The window the fastest transit is tracked over. Two windows are kept, so
/// the floor follows a slow clock drift within two to four seconds.
const FLOOR_WINDOW_NS: u64 = 2_000_000_000;

/// A frame later than this behind the floor means the capture clock jumped
/// (a restarted source, a new clock domain): start the floor over from it.
const RESYNC_NS: u64 = 1_000_000_000;

/// The minimum transit over the last one to two windows.
#[derive(Debug, Default)]
struct TransitFloor {
    start_ns: u64,
    current: Option<i128>,
    previous: Option<i128>,
}

impl TransitFloor {
    fn update(&mut self, transit: i128, now_ns: u64) -> i128 {
        match self.current {
            Some(c) if now_ns.saturating_sub(self.start_ns) < FLOOR_WINDOW_NS => {
                self.current = Some(c.min(transit));
            }
            Some(c) => {
                self.previous = Some(c);
                self.current = Some(transit);
                self.start_ns = now_ns;
            }
            None => self.reset(transit, now_ns),
        }
        let current = self.current.unwrap_or(transit);
        self.previous.map_or(current, |p| p.min(current))
    }

    fn reset(&mut self, transit: i128, now_ns: u64) {
        self.previous = None;
        self.current = Some(transit);
        self.start_ns = now_ns;
    }
}

/// The de-jitter clock of one feed.
#[derive(Debug)]
pub(crate) struct Pacer {
    max_hold_ns: u64,
    decay_ns: u64,
    floor: TransitFloor,
    hold_ns: u64,
}

impl Pacer {
    /// A pacer for a source of nominally `fps` frames per second.
    pub(crate) fn new(fps: u32) -> Self {
        let period_ns = 1_000_000_000 / u64::from(fps.max(1));
        Self {
            max_hold_ns: period_ns * MAX_HOLD_FRAMES,
            decay_ns: (period_ns / 32).max(1),
            floor: TransitFloor::default(),
            hold_ns: 0,
        }
    }

    /// How long to hold a frame captured at `capture_ns` (any clock that
    /// advances in nanoseconds) that arrived at `now_ns` (the local monotonic
    /// clock) before writing it.
    pub(crate) fn hold_for(&mut self, capture_ns: u64, now_ns: u64) -> u64 {
        let transit = i128::from(now_ns) - i128::from(capture_ns);
        let floor = self.floor.update(transit, now_ns);
        let late = u64::try_from(transit - floor).unwrap_or(u64::MAX);
        if late > RESYNC_NS {
            self.floor.reset(transit, now_ns);
            self.hold_ns = 0;
            return 0;
        }
        self.hold_ns = self
            .hold_ns
            .saturating_sub(self.decay_ns)
            .max(late.min(self.max_hold_ns));
        self.hold_ns.saturating_sub(late)
    }

    /// The current offset over the fastest transit, in nanoseconds.
    pub(crate) fn hold_ns(&self) -> u64 {
        self.hold_ns
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000_000;
    const PERIOD: u64 = 33_333_333;

    /// Feed `frames` of (capture, arrival) and return each frame's write time.
    fn writes(p: &mut Pacer, frames: &[(u64, u64)]) -> Vec<u64> {
        frames.iter().map(|&(c, a)| a + p.hold_for(c, a)).collect()
    }

    #[test]
    fn a_steady_source_is_not_held() {
        let mut p = Pacer::new(30);
        let frames: Vec<_> = (0..90u64)
            .map(|i| (i * PERIOD, 5 * MS + i * PERIOD))
            .collect();
        for (w, (_, a)) in writes(&mut p, &frames).iter().zip(&frames) {
            assert_eq!(w, a);
        }
        assert_eq!(p.hold_ns(), 0);
    }

    #[test]
    fn a_burst_after_a_stall_leaves_at_its_capture_spacing() {
        let mut p = Pacer::new(30);
        // Ten on-time frames, then four captured on cadence that all arrive
        // together 110 ms after the first of them was captured.
        let mut frames: Vec<_> = (0..10u64).map(|i| (i * PERIOD, i * PERIOD)).collect();
        let arrive = 10 * PERIOD + 110 * MS;
        frames.extend((10..14u64).map(|i| (i * PERIOD, arrive)));
        let w = writes(&mut p, &frames);
        let burst = &w[10..];
        assert_eq!(burst[0], arrive, "the late frame is not held");
        for pair in burst.windows(2) {
            let gap = pair[1] - pair[0];
            assert!(
                (20 * MS..=PERIOD + MS).contains(&gap),
                "a burst must be spaced out, not collapsed: {burst:?}"
            );
        }
        // The next on-time frame keeps the spacing too.
        let next = 14 * PERIOD;
        let at = next + p.hold_for(next, next);
        assert!(at > burst[3] + 25 * MS, "{at} after {:?}", burst[3]);
    }

    #[test]
    fn the_hold_never_exceeds_the_queue_and_decays_back() {
        let mut p = Pacer::new(30);
        let _ = p.hold_for(0, 0);
        // A frame 400 ms late: written at once, the hold capped at the queue.
        assert_eq!(p.hold_for(PERIOD, PERIOD + 400 * MS), 0);
        assert_eq!(p.hold_ns(), PERIOD * MAX_HOLD_FRAMES);
        // On-time frames afterwards bring it back down to nothing.
        let mut i = 2;
        while p.hold_ns() > 0 && i < 200 {
            let _ = p.hold_for(i * PERIOD, i * PERIOD);
            i += 1;
        }
        assert_eq!(p.hold_ns(), 0, "decayed within {i} frames");
    }

    #[test]
    fn a_capture_clock_that_jumps_back_does_not_stall_the_feed() {
        let mut p = Pacer::new(30);
        for i in 0..10u64 {
            let _ = p.hold_for(1_000_000 * MS + i * PERIOD, i * PERIOD);
        }
        // The source restarted with its clock near zero.
        assert_eq!(p.hold_for(0, 10 * PERIOD), 0);
        assert_eq!(p.hold_for(PERIOD, 11 * PERIOD), 0);
    }
}
