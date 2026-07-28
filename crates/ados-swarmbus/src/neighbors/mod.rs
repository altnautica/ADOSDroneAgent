//! The neighbour table: what every node knows about every other node in its
//! fleet, and the counters that make the bus diagnosable.
//!
//! This is the decentralized layer's whole data structure. On a drone it is the
//! input to separation, flocking and formation control; on a ground station it is
//! what the operator's fleet view renders. Both read the same table built from the
//! same broadcasts, so the ground station is a *listener*, not a hub — powering it
//! off does not change what any drone knows.
//!
//! The table is deliberately small and total: one entry per fleet slot, no history,
//! no filtering. Position smoothing belongs to whoever consumes it;
//! [`NeighborTable::predicted`] offers the one extrapolation the 10 Hz control loop
//! needs from a 2 Hz beacon.

pub mod counters;
pub mod geo;

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use ados_radio::config::{FLEET_MAX_SLOTS, SLOT_GROUND};

use crate::beacon::SwarmBeacon;

pub use counters::SwarmCounters;
pub use geo::{dead_reckon, distance_m, R_EARTH};

/// How long a neighbour survives without a beacon: six missed transmissions at
/// [`crate::BEACON_HZ`].
///
/// Six rather than two because the evidence says outages of about a second are
/// normal in real formation flight, and dropping a neighbour that is still there
/// is worse than carrying a slightly old one — the separation layer would stop
/// avoiding an aircraft that has not gone anywhere.
pub const NEIGHBOR_STALE: Duration = Duration::from_secs(3);

/// Hard cap on table size. A legal fleet cannot reach it
/// ([`FLEET_MAX_SLOTS`] is 24); it bounds the table against a garbage or hostile
/// slot flood, since `slot` is a `u8` and 255 distinct values are expressible.
pub const MAX_NEIGHBORS: usize = 64;

/// One tracked peer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Neighbor {
    pub beacon: SwarmBeacon,
    /// Local monotonic receipt time. Deliberately an [`Instant`], not a wall
    /// clock: staleness must survive an NTP step or a clock that never set.
    pub received_at: Instant,
    /// Radiotap antenna signal in dBm, or `None` when the capture carried none.
    pub rssi_dbm: Option<i8>,
}

impl Neighbor {
    /// How long ago this beacon arrived.
    pub fn age(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.received_at)
    }

    /// Whether this entry is past [`NEIGHBOR_STALE`].
    pub fn is_stale(&self, now: Instant) -> bool {
        self.age(now) >= NEIGHBOR_STALE
    }
}

/// Every peer this node currently hears, keyed by fleet slot.
#[derive(Debug)]
pub struct NeighborTable {
    /// `BTreeMap` rather than a hash map so iteration is slot-ordered: the
    /// operator's table and the JSON payload are stable frame to frame, which
    /// matters more than lookup speed at N=24.
    by_slot: BTreeMap<u8, Neighbor>,
    own_slot: u8,
    counters: SwarmCounters,
}

impl NeighborTable {
    /// A table for the node in `own_slot`.
    ///
    /// The own slot is needed because a monitor interface loops locally injected
    /// frames back to the capture path, so a node hears its own beacons. Recording
    /// them would make every drone its own nearest neighbour at zero distance —
    /// which the separation layer would treat as an imminent collision with itself.
    /// This is the same self-pair guard `ados_groundlink::presence` applies by
    /// device id.
    pub fn new(own_slot: u8) -> Self {
        Self {
            by_slot: BTreeMap::new(),
            own_slot,
            counters: SwarmCounters::default(),
        }
    }

    /// This node's own fleet slot.
    pub fn own_slot(&self) -> u8 {
        self.own_slot
    }

    /// The published counters, with the live table size folded in.
    pub fn counters(&self) -> SwarmCounters {
        self.counters
    }

    /// How many neighbours are currently tracked.
    pub fn len(&self) -> usize {
        self.by_slot.len()
    }

    /// Whether no neighbour is currently tracked. The onboard autonomy layer
    /// treats this as "fly nothing": with no neighbours there is no formation and
    /// no separation solution, so it emits no setpoints and lets the FC hold.
    pub fn is_empty(&self) -> bool {
        self.by_slot.is_empty()
    }

    /// Slot-ordered iteration over every tracked neighbour.
    pub fn iter(&self) -> impl Iterator<Item = (&u8, &Neighbor)> {
        self.by_slot.iter()
    }

    /// One neighbour by slot.
    pub fn get(&self, slot: u8) -> Option<&Neighbor> {
        self.by_slot.get(&slot)
    }

    /// Count a transmitted beacon.
    pub fn record_tx(&mut self) {
        self.counters.beacons_tx += 1;
    }

    /// Count a frame rejected for a foreign magic.
    pub fn record_bad_magic(&mut self) {
        self.counters.beacons_bad_magic += 1;
    }

    /// Count a frame whose authentication tag did not verify.
    pub fn record_bad_tag(&mut self) {
        self.counters.beacons_bad_tag += 1;
    }

    /// Record a beacon, replacing any previous entry for its slot.
    ///
    /// Returns whether it was accepted. Two rejections, neither counted as an
    /// error:
    ///
    /// - **Our own slot, or slot 0.** A loopback of our own transmission, or a
    ///   ground station emitting a beacon it has no business emitting.
    /// - **A full table.** Only reachable with illegal slots present.
    ///
    /// A slot above [`FLEET_MAX_SLOTS`] is deliberately **accepted**. It is a
    /// misprovisioned fleet member, and the honest response is to make it visible
    /// on the operator's screen — a silent drop would hide the exact
    /// misconfiguration that causes the FEC thrash the slot registry exists to
    /// prevent.
    pub fn record(&mut self, beacon: SwarmBeacon, rssi_dbm: Option<i8>, now: Instant) -> bool {
        if beacon.slot == SLOT_GROUND || beacon.slot == self.own_slot {
            return false;
        }
        if self.by_slot.len() >= MAX_NEIGHBORS && !self.by_slot.contains_key(&beacon.slot) {
            return false;
        }
        self.by_slot.insert(
            beacon.slot,
            Neighbor {
                beacon,
                received_at: now,
                rssi_dbm,
            },
        );
        self.counters.beacons_rx += 1;
        true
    }

    /// Drop every neighbour whose last beacon is older than [`NEIGHBOR_STALE`],
    /// counting each. Returns how many were dropped.
    pub fn prune(&mut self, now: Instant) -> usize {
        let before = self.by_slot.len();
        self.by_slot.retain(|_, n| !n.is_stale(now));
        let dropped = before - self.by_slot.len();
        self.counters.beacons_stale_dropped += dropped as u64;
        dropped
    }

    /// A neighbour's position dead-reckoned forward from its last beacon at
    /// constant velocity, as `(lat_deg, lon_deg, alt_m)`.
    ///
    /// This predict step is what lets a 10 Hz control loop run against a 2 Hz
    /// beacon: between beacons the neighbour's position is extrapolated rather than
    /// held, so the separation layer sees a closing aircraft move continuously
    /// instead of jumping 500 ms at a time.
    ///
    /// Returns `None` for an unknown slot **and for a stale one**. Extrapolating
    /// past [`NEIGHBOR_STALE`] would hand the control layer a confident position
    /// for an aircraft that has been silent for three seconds; refusing is what
    /// makes "never fly on stale data" structural rather than a convention.
    pub fn predicted(&self, slot: u8, now: Instant) -> Option<(f64, f64, f64)> {
        let n = self.by_slot.get(&slot)?;
        if n.is_stale(now) {
            return None;
        }
        Some(geo::dead_reckon(&n.beacon, n.age(now).as_secs_f64()))
    }

    /// The `n` nearest neighbours to `from` (`lat_deg`, `lon_deg`, `alt_m`),
    /// nearest first, by true 3-D distance in metres.
    ///
    /// Ordering is on the last-reported position, not the dead-reckoned one: the
    /// caller that wants prediction folded in already has [`Self::predicted`], and
    /// mixing the two would make the ordering depend on when it was asked.
    pub fn nearest(&self, n: usize, from: (f64, f64, f64)) -> Vec<&Neighbor> {
        let mut scored: Vec<(f64, &Neighbor)> = self
            .by_slot
            .values()
            .map(|nb| (geo::distance_m(from, geo::position_of(&nb.beacon)), nb))
            .collect();
        // Ties broken by slot, which `by_slot` iteration already supplies in order,
        // so a stable sort makes the result deterministic for co-located drones.
        scored.sort_by(|a, b| a.0.total_cmp(&b.0));
        scored.into_iter().take(n).map(|(_, nb)| nb).collect()
    }
}

/// Whether a slot is a legal drone slot in this fleet. Exposed so a consumer can
/// flag a misprovisioned member the table deliberately still carries.
pub fn is_legal_drone_slot(slot: u8) -> bool {
    slot != SLOT_GROUND && slot <= FLEET_MAX_SLOTS
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::beacon::STATUS_GPS_OK;

    /// Bengaluru, so the longitude scaling is exercised at a real non-zero
    /// latitude rather than on the equator where `cos(lat)` is 1 and a missing
    /// scale factor would pass.
    const LAT: f64 = 12.9716;
    const LON: f64 = 77.5946;

    fn at(slot: u8, lat: f64, lon: f64, alt_m: f64) -> SwarmBeacon {
        SwarmBeacon {
            slot,
            lat: (lat * 1e7) as i32,
            lon: (lon * 1e7) as i32,
            alt_dm: (alt_m * 10.0) as i16,
            status: STATUS_GPS_OK,
            ..SwarmBeacon::default()
        }
    }

    #[test]
    fn a_beacon_is_recorded_and_replaces_the_previous_one_for_its_slot() {
        let t0 = Instant::now();
        let mut table = NeighborTable::new(1);
        assert!(table.is_empty());
        assert!(table.record(at(3, LAT, LON, 10.0), Some(-48), t0));
        assert_eq!(table.len(), 1);
        assert_eq!(table.counters().beacons_rx, 1);
        assert_eq!(table.get(3).unwrap().rssi_dbm, Some(-48));

        // A second beacon for the same slot updates in place, not alongside.
        assert!(table.record(at(3, LAT, LON, 20.0), None, t0));
        assert_eq!(table.len(), 1);
        assert_eq!(table.counters().beacons_rx, 2);
        assert_eq!(table.get(3).unwrap().beacon.alt_dm, 200);
        assert_eq!(
            table.get(3).unwrap().rssi_dbm,
            None,
            "no stale reading kept"
        );
    }

    /// A monitor interface loops our own injected frames back. Recording them
    /// would make every drone its own nearest neighbour at zero distance — an
    /// imminent collision with itself.
    #[test]
    fn our_own_slot_and_the_ground_slot_are_never_recorded() {
        let t0 = Instant::now();
        let mut table = NeighborTable::new(5);
        assert!(
            !table.record(at(5, LAT, LON, 10.0), None, t0),
            "own loopback"
        );
        assert!(
            !table.record(at(SLOT_GROUND, LAT, LON, 0.0), None, t0),
            "GS"
        );
        assert!(table.is_empty());
        assert_eq!(table.counters().beacons_rx, 0);
        // A real peer still lands.
        assert!(table.record(at(6, LAT, LON, 10.0), None, t0));
        assert_eq!(table.len(), 1);
    }

    /// A misprovisioned slot is carried, not hidden: the operator must be able to
    /// see the exact misconfiguration that causes the FEC thrash.
    #[test]
    fn an_out_of_range_slot_is_carried_and_flagged_rather_than_dropped() {
        let t0 = Instant::now();
        let mut table = NeighborTable::new(1);
        assert!(table.record(at(200, LAT, LON, 10.0), None, t0));
        assert!(table.get(200).is_some());
        assert!(!is_legal_drone_slot(200));
        assert!(is_legal_drone_slot(FLEET_MAX_SLOTS));
        assert!(!is_legal_drone_slot(FLEET_MAX_SLOTS + 1));
        assert!(!is_legal_drone_slot(SLOT_GROUND));
    }

    #[test]
    fn the_table_is_capped_but_an_existing_slot_still_updates() {
        let t0 = Instant::now();
        let mut table = NeighborTable::new(0);
        for slot in 1..=MAX_NEIGHBORS as u8 {
            assert!(table.record(at(slot, LAT, LON, 1.0), None, t0));
        }
        assert_eq!(table.len(), MAX_NEIGHBORS);
        // A new slot beyond the cap is refused...
        assert!(!table.record(at(200, LAT, LON, 1.0), None, t0));
        assert_eq!(table.len(), MAX_NEIGHBORS);
        // ...but a slot already in the table still updates, so a full table can
        // never freeze the positions it already tracks.
        assert!(table.record(at(1, LAT, LON, 99.0), None, t0));
        assert_eq!(table.get(1).unwrap().beacon.alt_dm, 990);
    }

    /// The staleness boundary is exact: at `NEIGHBOR_STALE` the entry goes, one
    /// millisecond earlier it stays. An off-by-one here either drops a live
    /// aircraft the separation layer must avoid, or keeps a dead one forever.
    #[test]
    fn prune_drops_a_neighbour_at_the_stale_boundary_and_counts_it() {
        let t0 = Instant::now();
        let mut table = NeighborTable::new(1);
        table.record(at(2, LAT, LON, 10.0), None, t0);
        table.record(at(3, LAT, LON, 10.0), None, t0 + Duration::from_secs(2));

        // Just before the boundary nothing is dropped.
        assert_eq!(
            table.prune(t0 + NEIGHBOR_STALE - Duration::from_millis(1)),
            0
        );
        assert_eq!(table.len(), 2);
        assert_eq!(table.counters().beacons_stale_dropped, 0);

        // Exactly at it, slot 2 goes and slot 3 (2 s newer) stays.
        assert_eq!(table.prune(t0 + NEIGHBOR_STALE), 1);
        assert_eq!(table.len(), 1);
        assert!(table.get(2).is_none());
        assert!(table.get(3).is_some());
        assert_eq!(table.counters().beacons_stale_dropped, 1);

        // The counter accumulates across calls rather than being a snapshot.
        assert_eq!(table.prune(t0 + Duration::from_secs(10)), 1);
        assert_eq!(table.counters().beacons_stale_dropped, 2);
        assert!(table.is_empty());
        // Pruning an empty table is a no-op, not a phantom drop.
        assert_eq!(table.prune(t0 + Duration::from_secs(20)), 0);
        assert_eq!(table.counters().beacons_stale_dropped, 2);
    }

    /// `predicted` delegates the arithmetic to [`geo::dead_reckon`] (tested there
    /// against a known answer); what belongs to the table is that it feeds in the
    /// right elapsed time and the right beacon.
    #[test]
    fn predicted_feeds_the_elapsed_time_of_the_right_neighbour() {
        let t0 = Instant::now();
        let mut table = NeighborTable::new(1);
        let north = SwarmBeacon {
            slot: 4,
            lat: (LAT * 1e7) as i32,
            lon: (LON * 1e7) as i32,
            vx_cms: 1000, // 10 m/s north
            ..SwarmBeacon::default()
        };
        let south = SwarmBeacon {
            slot: 5,
            lat: (LAT * 1e7) as i32,
            lon: (LON * 1e7) as i32,
            vx_cms: -1000,
            ..SwarmBeacon::default()
        };
        table.record(north, None, t0);
        table.record(south, None, t0);

        // Each slot predicts from ITS OWN beacon, not the first or the last recorded.
        let at_2s = t0 + Duration::from_secs(2);
        assert_eq!(
            table.predicted(4, at_2s),
            Some(geo::dead_reckon(&north, 2.0))
        );
        assert_eq!(
            table.predicted(5, at_2s),
            Some(geo::dead_reckon(&south, 2.0))
        );
        assert!(table.predicted(4, at_2s).unwrap().0 > LAT, "4 moved north");
        assert!(table.predicted(5, at_2s).unwrap().0 < LAT, "5 moved south");

        // The elapsed time is measured from receipt, so the same `now` against a
        // later-received beacon predicts a shorter displacement.
        let mut late = NeighborTable::new(1);
        late.record(north, None, t0 + Duration::from_secs(1));
        assert_eq!(
            late.predicted(4, at_2s),
            Some(geo::dead_reckon(&north, 1.0))
        );
    }

    /// The refusal is the safety property: past the stale window the control layer
    /// gets `None`, never a confident extrapolation of a silent aircraft.
    #[test]
    fn predicted_refuses_a_stale_or_unknown_neighbour() {
        let t0 = Instant::now();
        let mut table = NeighborTable::new(1);
        table.record(
            SwarmBeacon {
                slot: 4,
                vx_cms: 1000,
                ..SwarmBeacon::default()
            },
            None,
            t0,
        );
        assert!(table
            .predicted(4, t0 + NEIGHBOR_STALE - Duration::from_millis(1))
            .is_some());
        assert!(table.predicted(4, t0 + NEIGHBOR_STALE).is_none(), "stale");
        assert!(table.predicted(9, t0).is_none(), "unknown slot");
    }

    /// Ordering must be by TRUE 3-D distance. A 2-D-only implementation, or one
    /// that forgets the `cos(lat)` longitude scale, orders these differently.
    #[test]
    fn nearest_orders_by_true_three_dimensional_distance() {
        let t0 = Instant::now();
        let mut table = NeighborTable::new(1);
        let deg_north = |m: f64| (m / R_EARTH).to_degrees();
        let deg_east = |m: f64| (m / (R_EARTH * LAT.to_radians().cos())).to_degrees();

        // 30 m north, 10 m east, and directly overhead at 5 m.
        table.record(at(2, LAT + deg_north(30.0), LON, 0.0), None, t0);
        table.record(at(3, LAT, LON + deg_east(10.0), 0.0), None, t0);
        table.record(at(4, LAT, LON, 5.0), None, t0);
        // 8 m east but 100 m up: nearest in 2-D, farthest in 3-D. This is the
        // entry that catches an altitude-blind distance.
        table.record(at(5, LAT, LON + deg_east(8.0), 100.0), None, t0);

        let from = (LAT, LON, 0.0);
        let order: Vec<u8> = table
            .nearest(4, from)
            .iter()
            .map(|n| n.beacon.slot)
            .collect();
        assert_eq!(order, vec![4, 3, 2, 5], "5 m, 10 m, 30 m, ~100 m");

        // `n` truncates from the near end.
        assert_eq!(table.nearest(1, from)[0].beacon.slot, 4);
        assert_eq!(table.nearest(2, from).len(), 2);
        // Asking for more than exist returns everything, not a panic.
        assert_eq!(table.nearest(99, from).len(), 4);
        assert!(NeighborTable::new(1).nearest(3, from).is_empty());
    }

    #[test]
    fn counters_are_independent_and_only_ever_advance() {
        let mut table = NeighborTable::new(1);
        assert_eq!(table.counters(), SwarmCounters::default());
        table.record_tx();
        table.record_bad_magic();
        table.record_bad_magic();
        table.record_bad_tag();
        let c = table.counters();
        assert_eq!(c.beacons_tx, 1);
        assert_eq!(c.beacons_bad_magic, 2);
        assert_eq!(c.beacons_bad_tag, 1);
        assert_eq!(c.beacons_rx, 0, "a rejected frame is not a receipt");
        assert_eq!(c.beacons_stale_dropped, 0);
    }

    #[test]
    fn iteration_is_slot_ordered_regardless_of_arrival_order() {
        let t0 = Instant::now();
        let mut table = NeighborTable::new(1);
        for slot in [9u8, 2, 24, 5] {
            table.record(at(slot, LAT, LON, 0.0), None, t0);
        }
        let slots: Vec<u8> = table.iter().map(|(s, _)| *s).collect();
        assert_eq!(slots, vec![2, 5, 9, 24]);
    }

    #[test]
    fn neighbour_age_is_monotonic_and_never_underflows() {
        let t0 = Instant::now();
        let mut table = NeighborTable::new(1);
        table.record(at(2, LAT, LON, 0.0), None, t0 + Duration::from_secs(5));
        let n = table.get(2).unwrap();
        // A `now` earlier than the receipt saturates to zero rather than panicking
        // on the Duration subtraction.
        assert_eq!(n.age(t0), Duration::ZERO);
        assert!(!n.is_stale(t0));
        assert_eq!(n.age(t0 + Duration::from_secs(6)), Duration::from_secs(1));
    }
}
