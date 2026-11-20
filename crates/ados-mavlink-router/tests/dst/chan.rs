//! A seeded lossy byte link between two in-process actors: each packet is
//! lost, delayed, duplicated or (by the delay jitter) reordered on the run's
//! [`Rng`], and nothing moves while the link is down.

use crate::rng::Rng;

/// What one direction of a link does to its packets.
#[derive(Debug, Clone, Copy)]
pub struct Faults {
    /// Probability a packet is lost.
    pub loss: f32,
    /// Probability a packet is delivered twice.
    pub dup: f32,
    /// Delivery delay, ms, uniform in `[min, max]`: jitter reorders.
    pub delay_ms: (u64, u64),
}

impl Faults {
    pub const CLEAN: Faults = Faults {
        loss: 0.0,
        dup: 0.0,
        delay_ms: (1, 1),
    };
}

/// One direction of a link.
pub struct Link {
    pub faults: Faults,
    /// While set, every packet offered is lost.
    pub down: bool,
    queue: Vec<(u64, u64, Vec<u8>)>,
    seq: u64,
    pub offered: u64,
    pub lost: u64,
    pub duplicated: u64,
}

impl Link {
    pub fn new(faults: Faults) -> Self {
        Self {
            faults,
            down: false,
            queue: Vec::new(),
            seq: 0,
            offered: 0,
            lost: 0,
            duplicated: 0,
        }
    }

    /// Offer `bytes` at `now`; returns whether it will be delivered.
    pub fn send(&mut self, now: u64, bytes: Vec<u8>, rng: &mut Rng) -> bool {
        self.offered += 1;
        if self.down || rng.chance(self.faults.loss) {
            self.lost += 1;
            return false;
        }
        let (lo, hi) = self.faults.delay_ms;
        if rng.chance(self.faults.dup) {
            self.duplicated += 1;
            let at = now + rng.between(lo, hi);
            self.push(at, bytes.clone());
        }
        let at = now + rng.between(lo, hi);
        self.push(at, bytes);
        true
    }

    fn push(&mut self, at: u64, bytes: Vec<u8>) {
        self.seq += 1;
        self.queue.push((at, self.seq, bytes));
    }

    /// Every packet due by `now`, in delivery order.
    pub fn recv(&mut self, now: u64) -> Vec<Vec<u8>> {
        let mut due: Vec<(u64, u64, Vec<u8>)> = Vec::new();
        let mut keep = Vec::with_capacity(self.queue.len());
        for p in self.queue.drain(..) {
            if p.0 <= now {
                due.push(p);
            } else {
                keep.push(p);
            }
        }
        self.queue = keep;
        due.sort_by_key(|p| (p.0, p.1));
        due.into_iter().map(|p| p.2).collect()
    }

    /// Drop everything in flight (a cable pulled, an actor restarted).
    pub fn flush(&mut self) {
        self.lost += self.queue.len() as u64;
        self.queue.clear();
    }

    /// The longest a delivered packet takes.
    pub fn max_delay(&self) -> u64 {
        self.faults.delay_ms.1
    }
}
