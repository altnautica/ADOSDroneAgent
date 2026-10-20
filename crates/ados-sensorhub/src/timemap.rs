//! Hub time → host monotonic time.
//!
//! Every hub packet carries hub-time stamps; its arrival is stamped with the
//! host monotonic clock. Transport delay only ever makes `host - hub` larger,
//! so the lower envelope of those differences tracks the true relation. Per
//! one-second host bucket the pair with the smallest `host - hub` is kept;
//! when the bucket closes that pair feeds a [`ClockFit`] and is published as a
//! `sync` observation, so `ados-clock` fits the same relation.

use ados_sensorbus::fit::ClockFit;
use ados_sensorbus::{SyncObs, SyncSource};

/// Envelope bucket length on the host clock.
const BUCKET_NS: u64 = 1_000_000_000;
/// A hub stamp this far behind the newest one seen means the hub timebase
/// restarted (a reboot); packets are only reordered by milliseconds.
pub const BACKWARD_JUMP_NS: u64 = 1_000_000_000;

#[derive(Debug, Clone, Copy)]
struct Bucket {
    index: u64,
    host_ns: u64,
    hub_ns: u64,
}

impl Bucket {
    fn diff(&self) -> i128 {
        self.host_ns as i128 - self.hub_ns as i128
    }
}

/// What one observation produced.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Observation {
    /// The closed bucket's envelope pair, to publish.
    pub sync: Option<SyncObs>,
    /// The hub timebase jumped backwards and the fit was reset.
    pub reset: bool,
}

/// The hub clock relation, built from arrival observations.
#[derive(Debug, Clone)]
pub struct HubClock {
    fit: ClockFit,
    bucket: Option<Bucket>,
    newest_hub_ns: Option<u64>,
}

impl Default for HubClock {
    fn default() -> Self {
        Self::new()
    }
}

impl HubClock {
    pub fn new() -> Self {
        Self {
            fit: ClockFit::sixty_seconds(),
            bucket: None,
            newest_hub_ns: None,
        }
    }

    /// Forget everything (new link, or the hub restarted).
    pub fn reset(&mut self) {
        self.fit.reset();
        self.bucket = None;
        self.newest_hub_ns = None;
    }

    /// Feed one pair: a packet arrived at `host_ns` and its newest hub stamp
    /// is `hub_ns`.
    pub fn observe(&mut self, host_ns: u64, hub_ns: u64) -> Observation {
        let mut out = Observation::default();
        if self
            .newest_hub_ns
            .is_some_and(|newest| hub_ns.saturating_add(BACKWARD_JUMP_NS) < newest)
        {
            self.reset();
            out.reset = true;
        }
        self.newest_hub_ns = Some(self.newest_hub_ns.map_or(hub_ns, |n| n.max(hub_ns)));

        let cand = Bucket {
            index: host_ns / BUCKET_NS,
            host_ns,
            hub_ns,
        };
        match self.bucket {
            Some(b) if b.index == cand.index => {
                if cand.diff() < b.diff() {
                    self.bucket = Some(cand);
                }
            }
            Some(b) => {
                self.fit.push(b.host_ns, b.hub_ns as i64);
                out.sync = Some(SyncObs {
                    source: SyncSource::Hub,
                    host_ns: b.host_ns,
                    source_ns: b.hub_ns as i64,
                    rtt_ns: 0,
                    sim_time: false,
                });
                self.bucket = Some(cand);
            }
            None => self.bucket = Some(cand),
        }
        out
    }

    /// Whether the fit has the two pairs a mapping needs.
    pub fn ready(&self) -> bool {
        self.fit.len() >= 2
    }

    /// Host time of hub time `hub_ns`, `None` until [`Self::ready`].
    pub fn to_host(&self, hub_ns: u64) -> Option<u64> {
        if !self.ready() {
            return None;
        }
        self.fit.to_host(hub_ns as i64)
    }

    /// Host time of `hub_ns` for a packet that arrived at `arrival_ns` and
    /// whose newest stamp is `ref_hub_ns`. Before the fit is ready the stamp
    /// is placed behind the arrival by its distance from the newest stamp.
    pub fn host_time(&self, hub_ns: u64, arrival_ns: u64, ref_hub_ns: u64) -> u64 {
        self.to_host(hub_ns)
            .unwrap_or_else(|| arrival_ns.saturating_sub(ref_hub_ns.saturating_sub(hub_ns)))
    }

    pub fn sigma_ns(&self) -> i64 {
        self.fit.sigma_ns()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hub = host · (1 + 20 ppm) − 3 s; each packet is delayed 250-900 µs
    /// in transit, except one 200 µs minimum-delay packet per second.
    fn hub_of(host: u64) -> u64 {
        ((host as f64) * (1.0 + 20e-6)) as u64 - 3_000_000_000
    }

    #[test]
    fn the_lower_envelope_fit_maps_hub_time_within_the_minimum_delay() {
        let mut clock = HubClock::new();
        let mut syncs = Vec::new();
        let start = 10_000_000_000u64;
        for k in 0..2_000u64 {
            // 100 Hz packets over 20 s.
            let sent_host = start + k * 10_000_000;
            let delay = if k % 100 == 37 {
                200_000
            } else {
                250_000 + (k * 7919 % 650_000)
            };
            let obs = clock.observe(sent_host + delay, hub_of(sent_host));
            assert!(!obs.reset);
            syncs.extend(obs.sync);
        }
        assert_eq!(syncs.len(), 19);
        assert!(syncs
            .iter()
            .all(|s| s.source == SyncSource::Hub && s.rtt_ns == 0));
        // Each published pair is the minimum-delay one of its second.
        for s in &syncs {
            let sent = s.host_ns - 200_000;
            assert_eq!(s.source_ns as u64, hub_of(sent));
        }
        // Mapping recovers the send time plus the 200 µs floor.
        let t = start + 15_500_000_000;
        let mapped = clock.to_host(hub_of(t)).unwrap();
        assert!(
            (mapped as i64 - (t + 200_000) as i64).abs() < 2_000,
            "{mapped} vs {t}"
        );
    }

    #[test]
    fn before_two_pairs_the_mapping_falls_back_to_arrival() {
        let mut clock = HubClock::new();
        assert_eq!(clock.to_host(5), None);
        clock.observe(1_000_000_000, 50_000_000);
        // IMU sample 7 of 10 at 1 ms spacing, newest at hub 50 ms.
        assert_eq!(
            clock.host_time(47_000_000, 1_000_000_000, 50_000_000),
            997_000_000
        );
        clock.observe(2_000_000_000, 1_050_000_000);
        assert!(!clock.ready());
        clock.observe(3_000_000_000, 2_050_000_000);
        assert!(clock.ready());
        assert_eq!(clock.to_host(2_050_000_000), Some(3_000_000_000));
    }

    #[test]
    fn a_backward_hub_jump_resets_the_fit() {
        let mut clock = HubClock::new();
        for s in 0..5u64 {
            clock.observe(s * BUCKET_NS + 1, 100 * BUCKET_NS + s * BUCKET_NS);
        }
        assert!(clock.ready());
        // Small reordering is not a reboot.
        assert!(!clock.observe(5 * BUCKET_NS, 104 * BUCKET_NS).reset);
        let obs = clock.observe(6 * BUCKET_NS, 1_000);
        assert!(obs.reset);
        assert!(obs.sync.is_none());
        assert!(!clock.ready());
        assert_eq!(clock.to_host(1_000), None);
    }
}
