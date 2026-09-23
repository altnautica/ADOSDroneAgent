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
//! no filtering, no extrapolation. Position smoothing and dead reckoning belong to
//! whoever consumes the published table (the onboard control loop reads it off
//! `swarm.sock` and projects it into its own frame).

pub mod counters;
pub mod replay;

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use ados_radio::config::SLOT_GROUND;

use crate::beacon::SwarmBeacon;
use crate::crypto::{SenderNonce, NONCE_PREFIX_LEN};

pub use counters::SwarmCounters;
use replay::{SenderMarks, SenderVerdict};

/// How long a neighbour survives without a beacon: six missed transmissions at
/// [`crate::BEACON_HZ`].
///
/// Six rather than two because the evidence says outages of about a second are
/// normal in real formation flight, and dropping a neighbour that is still there
/// is worse than carrying a slightly old one — the separation layer would stop
/// avoiding an aircraft that has not gone anywhere.
pub const NEIGHBOR_STALE: Duration = Duration::from_secs(3);

/// Hard cap on table size. A legal fleet cannot reach it
/// ([`ados_radio::config::FLEET_MAX_SLOTS`] is 24); it bounds the table against a garbage or hostile
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
    /// The nonce prefix the beacon was sealed under: the sender's identity on the
    /// bus for its current run. Distinct for two senders that share a slot, which
    /// is what lets separation tell a misprovisioned same-slot pair apart.
    pub sender: [u8; NONCE_PREFIX_LEN],
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

/// What [`NeighborTable::record`] did with one authenticated beacon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recorded {
    /// Recorded for its slot.
    Accepted,
    /// Recorded, but it carries this node's own slot: another node is provisioned
    /// with our slot. Kept in the table so separation sees the aircraft, and
    /// counted as a slot conflict.
    OwnSlotConflict,
    /// Not recorded, not a fault: a beacon claiming the ground slot, or a table
    /// already at its cap.
    Ignored,
    /// Not recorded: a frame already seen, or from a sender run the slot has moved
    /// on from.
    Replayed,
    /// Not recorded: a second live sender on a peer's slot. The first one heard
    /// keeps the slot.
    SecondSender,
}

/// Every peer this node currently hears, keyed by fleet slot.
#[derive(Debug)]
pub struct NeighborTable {
    /// `BTreeMap` rather than a hash map so iteration is slot-ordered: the
    /// operator's table and the JSON payload are stable frame to frame, which
    /// matters more than lookup speed at N=24.
    by_slot: BTreeMap<u8, Neighbor>,
    own_slot: u8,
    /// This node's own nonce prefix, once the radio half has built its cipher.
    own_sender: Option<[u8; NONCE_PREFIX_LEN]>,
    /// The monitor interface the bus is listening on, `None` while the radio is
    /// not open. Published so a bus that cannot hear is not read as empty sky.
    radio_iface: Option<String>,
    counters: SwarmCounters,
    senders: SenderMarks,
}

impl NeighborTable {
    /// A table for the node in `own_slot`.
    ///
    /// This node's own looped-back transmissions never reach the table: the
    /// receive path recognises them by the cipher's nonce prefix, which identifies
    /// this process on the bus. The own slot is kept to tell a peer that CLAIMS our
    /// slot apart from the rest (see [`Recorded::OwnSlotConflict`]).
    pub fn new(own_slot: u8) -> Self {
        Self {
            by_slot: BTreeMap::new(),
            own_slot,
            own_sender: None,
            radio_iface: None,
            counters: SwarmCounters::default(),
            senders: SenderMarks::default(),
        }
    }

    /// This node's own fleet slot.
    pub fn own_slot(&self) -> u8 {
        self.own_slot
    }

    /// This node's own nonce prefix, or `None` before the radio half has opened.
    pub fn own_sender(&self) -> Option<[u8; NONCE_PREFIX_LEN]> {
        self.own_sender
    }

    /// Record this node's own nonce prefix. The cipher keeps its prefix across a
    /// re-key, so this is set once per process in practice.
    pub fn set_own_sender(&mut self, prefix: [u8; NONCE_PREFIX_LEN]) {
        self.own_sender = Some(prefix);
    }

    /// The interface the radio is open on, or `None` while it is not open.
    pub fn radio_iface(&self) -> Option<&str> {
        self.radio_iface.as_deref()
    }

    /// Record the radio as open on `iface`, or closed with `None`. The radio
    /// supervisor calls this on every open and every teardown.
    pub fn set_radio_iface(&mut self, iface: Option<String>) {
        self.radio_iface = iface;
    }

    /// Whether a peer is currently beaconing this node's own slot. Always false on
    /// a ground station, whose slot no beacon may carry.
    pub fn slot_conflict(&self) -> bool {
        self.own_slot != SLOT_GROUND && self.by_slot.contains_key(&self.own_slot)
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

    /// Record an authenticated beacon sealed under `sender`'s nonce, replacing any
    /// previous entry for its slot.
    ///
    /// Refused, without an entry change:
    ///
    /// - **Slot 0**: a ground station emitting a beacon it has no business
    ///   emitting. Not a fault.
    /// - **A replay**: see [`replay`]. Counted as `beacons_replayed`.
    /// - **A second live sender on a peer's slot**: counted as
    ///   `beacons_slot_conflict`.
    /// - **A full table**: only reachable with illegal slots present.
    ///
    /// A peer claiming **this node's own slot** is recorded and counted as a slot
    /// conflict. Dropping it as loopback would leave two same-slot drones blind to
    /// each other, which is the one pair separation most needs to see.
    ///
    /// A slot above [`ados_radio::config::FLEET_MAX_SLOTS`] is deliberately **accepted**. It is a
    /// misprovisioned fleet member, and the honest response is to make it visible
    /// on the operator's screen — a silent drop would hide the exact
    /// misconfiguration that causes the FEC thrash the slot registry exists to
    /// prevent.
    pub fn record(
        &mut self,
        beacon: SwarmBeacon,
        sender: SenderNonce,
        rssi_dbm: Option<i8>,
        now: Instant,
    ) -> Recorded {
        if beacon.slot == SLOT_GROUND {
            return Recorded::Ignored;
        }
        let slot_live = self
            .by_slot
            .get(&beacon.slot)
            .is_some_and(|n| !n.is_stale(now));
        match self.senders.verdict(beacon.slot, sender, slot_live) {
            SenderVerdict::Fresh => {}
            SenderVerdict::Replayed => {
                self.counters.beacons_replayed += 1;
                return Recorded::Replayed;
            }
            SenderVerdict::SecondSender => {
                self.counters.beacons_slot_conflict += 1;
                return Recorded::SecondSender;
            }
        }
        if self.by_slot.len() >= MAX_NEIGHBORS && !self.by_slot.contains_key(&beacon.slot) {
            return Recorded::Ignored;
        }
        self.senders.accept(beacon.slot, sender);
        self.by_slot.insert(
            beacon.slot,
            Neighbor {
                beacon,
                received_at: now,
                rssi_dbm,
                sender: sender.prefix,
            },
        );
        self.counters.beacons_rx += 1;
        if beacon.slot == self.own_slot {
            self.counters.beacons_slot_conflict += 1;
            Recorded::OwnSlotConflict
        } else {
            Recorded::Accepted
        }
    }

    /// [`Self::record`] with a fresh nonce from the slot's current run, for tests
    /// that exercise the table rather than the replay window.
    #[cfg(test)]
    pub(crate) fn record_next(
        &mut self,
        beacon: SwarmBeacon,
        rssi_dbm: Option<i8>,
        now: Instant,
    ) -> Recorded {
        let sender = self.senders.next_for(beacon.slot);
        self.record(beacon, sender, rssi_dbm, now)
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
        assert_eq!(
            table.record_next(at(3, LAT, LON, 10.0), Some(-48), t0),
            Recorded::Accepted
        );
        assert_eq!(table.len(), 1);
        assert_eq!(table.counters().beacons_rx, 1);
        assert_eq!(table.get(3).unwrap().rssi_dbm, Some(-48));

        // A second beacon for the same slot updates in place, not alongside.
        assert_eq!(
            table.record_next(at(3, LAT, LON, 20.0), None, t0),
            Recorded::Accepted
        );
        assert_eq!(table.len(), 1);
        assert_eq!(table.counters().beacons_rx, 2);
        assert_eq!(table.get(3).unwrap().beacon.alt_dm, 200);
        assert_eq!(
            table.get(3).unwrap().rssi_dbm,
            None,
            "no stale reading kept"
        );
    }

    /// The ground slot is never recorded: a ground station is not an aircraft.
    #[test]
    fn the_ground_slot_is_never_recorded() {
        let t0 = Instant::now();
        let mut table = NeighborTable::new(5);
        assert_eq!(
            table.record_next(at(SLOT_GROUND, LAT, LON, 0.0), None, t0),
            Recorded::Ignored
        );
        assert!(table.is_empty());
        assert_eq!(table.counters(), SwarmCounters::default(), "not a fault");
        assert_eq!(
            table.record_next(at(6, LAT, LON, 10.0), None, t0),
            Recorded::Accepted
        );
        assert_eq!(table.len(), 1);
    }

    /// A peer provisioned with our slot reaches the table (own loopback is
    /// filtered by nonce prefix before this point), so two same-slot drones see
    /// each other and the conflict is counted and flagged.
    #[test]
    fn a_peer_on_our_own_slot_is_recorded_as_a_slot_conflict() {
        let t0 = Instant::now();
        let mut table = NeighborTable::new(5);
        assert!(!table.slot_conflict());
        assert_eq!(
            table.record_next(at(5, LAT, LON, 10.0), None, t0),
            Recorded::OwnSlotConflict
        );
        assert!(table.get(5).is_some(), "separation must see it");
        assert!(table.slot_conflict());
        assert_eq!(table.counters().beacons_slot_conflict, 1);
        assert_eq!(table.counters().beacons_rx, 1);
        // Once it ages out the flag clears on its own.
        table.prune(t0 + NEIGHBOR_STALE);
        assert!(!table.slot_conflict());
        // A ground station never reports one.
        assert!(!NeighborTable::new(SLOT_GROUND).slot_conflict());
    }

    /// A captured beacon re-injected later must not refresh the entry. Before the
    /// replay window a replay overwrote the slot as just received, so a departed
    /// aircraft stayed on the table on its old track.
    #[test]
    fn a_replayed_beacon_is_refused_and_counted() {
        let t0 = Instant::now();
        let mut table = NeighborTable::new(1);
        let run = |counter| SenderNonce {
            prefix: [0x11; 8],
            counter,
        };
        assert_eq!(
            table.record(at(3, LAT, LON, 10.0), run(7), None, t0),
            Recorded::Accepted
        );
        let later = t0 + Duration::from_secs(1);
        assert_eq!(
            table.record(at(3, LAT, LON, 10.0), run(7), None, later),
            Recorded::Replayed
        );
        assert_eq!(table.get(3).unwrap().received_at, t0, "not refreshed");
        assert_eq!(table.counters().beacons_replayed, 1);
        assert_eq!(table.counters().beacons_rx, 1);

        // The replay keeps failing after the slot went stale and was pruned.
        let gone = t0 + NEIGHBOR_STALE;
        assert_eq!(table.prune(gone), 1);
        assert_eq!(
            table.record(at(3, LAT, LON, 10.0), run(7), None, gone),
            Recorded::Replayed
        );
        assert!(table.is_empty());

        // A restarted sender (new prefix) is accepted on the quiet slot, and its
        // earlier run is refused from then on.
        let restarted = SenderNonce {
            prefix: [0x22; 8],
            counter: 0,
        };
        assert_eq!(
            table.record(at(3, LAT, LON, 10.0), restarted, None, gone),
            Recorded::Accepted
        );
        assert_eq!(
            table.record(at(3, LAT, LON, 10.0), run(8), None, gone),
            Recorded::Replayed
        );
    }

    /// Two peers provisioned with one slot: the first heard keeps it and the
    /// second is counted, rather than the row flipping between two positions.
    #[test]
    fn a_second_live_sender_on_a_peers_slot_is_counted_not_recorded() {
        let t0 = Instant::now();
        let mut table = NeighborTable::new(1);
        let a = SenderNonce {
            prefix: [0xAA; 8],
            counter: 0,
        };
        let b = SenderNonce {
            prefix: [0xBB; 8],
            counter: 0,
        };
        table.record(at(4, LAT, LON, 10.0), a, None, t0);
        assert_eq!(
            table.record(at(4, LAT, LON, 99.0), b, None, t0),
            Recorded::SecondSender
        );
        assert_eq!(table.get(4).unwrap().beacon.alt_dm, 100);
        assert_eq!(table.counters().beacons_slot_conflict, 1);
        assert_eq!(table.counters().beacons_replayed, 0);
    }

    /// A misprovisioned slot is carried, not hidden: the operator must be able to
    /// see the exact misconfiguration that causes the FEC thrash.
    #[test]
    fn an_out_of_range_slot_is_carried_and_flagged_rather_than_dropped() {
        let t0 = Instant::now();
        let mut table = NeighborTable::new(1);
        assert_eq!(
            table.record_next(at(200, LAT, LON, 10.0), None, t0),
            Recorded::Accepted
        );
        assert!(table.get(200).is_some());
    }

    #[test]
    fn the_table_is_capped_but_an_existing_slot_still_updates() {
        let t0 = Instant::now();
        let mut table = NeighborTable::new(0);
        for slot in 1..=MAX_NEIGHBORS as u8 {
            assert_eq!(
                table.record_next(at(slot, LAT, LON, 1.0), None, t0),
                Recorded::Accepted
            );
        }
        assert_eq!(table.len(), MAX_NEIGHBORS);
        // A new slot beyond the cap is refused...
        assert_eq!(
            table.record_next(at(200, LAT, LON, 1.0), None, t0),
            Recorded::Ignored
        );
        assert_eq!(table.len(), MAX_NEIGHBORS);
        // ...but a slot already in the table still updates, so a full table can
        // never freeze the positions it already tracks.
        assert_eq!(
            table.record_next(at(1, LAT, LON, 99.0), None, t0),
            Recorded::Accepted
        );
        assert_eq!(table.get(1).unwrap().beacon.alt_dm, 990);
    }

    /// The staleness boundary is exact: at `NEIGHBOR_STALE` the entry goes, one
    /// millisecond earlier it stays. An off-by-one here either drops a live
    /// aircraft the separation layer must avoid, or keeps a dead one forever.
    #[test]
    fn prune_drops_a_neighbour_at_the_stale_boundary_and_counts_it() {
        let t0 = Instant::now();
        let mut table = NeighborTable::new(1);
        table.record_next(at(2, LAT, LON, 10.0), None, t0);
        table.record_next(at(3, LAT, LON, 10.0), None, t0 + Duration::from_secs(2));

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
            table.record_next(at(slot, LAT, LON, 0.0), None, t0);
        }
        let slots: Vec<u8> = table.iter().map(|(s, _)| *s).collect();
        assert_eq!(slots, vec![2, 5, 9, 24]);
    }

    #[test]
    fn neighbour_age_is_monotonic_and_never_underflows() {
        let t0 = Instant::now();
        let mut table = NeighborTable::new(1);
        table.record_next(at(2, LAT, LON, 0.0), None, t0 + Duration::from_secs(5));
        let n = table.get(2).unwrap();
        // A `now` earlier than the receipt saturates to zero rather than panicking
        // on the Duration subtraction.
        assert_eq!(n.age(t0), Duration::ZERO);
        assert!(!n.is_stale(t0));
        assert_eq!(n.age(t0 + Duration::from_secs(6)), Duration::from_secs(1));
    }
}
