//! The decentralized swarm state bus.
//!
//! Every drone in a fleet broadcasts a 20-byte position/velocity beacon twice a
//! second, and every node — drone and ground station alike — hears every other
//! node. That is the whole contract, and its important property is what it does
//! *not* need: no ground station in the path, no leader, no session, no
//! negotiation. Power the ground station off and each drone's neighbour table is
//! unchanged.
//!
//! ```text
//! state.sock ──► vehicle::beacon_from_state ──► SwarmBus::broadcast ──► 802.11
//!                                                                        │
//!                                                     (every other node) │
//!                                                                        ▼
//! swarm.sock ◄── publish::neighbors_payload ◄── NeighborTable ◄── ingest::ingest_frame
//! ```
//!
//! Module map, roughly in wire order:
//!
//! - [`beacon`] — the 20-byte message and its status bits.
//! - [`precedence`] — the mode-precedence level packed into three of those bits.
//! - [`frame`] — the radiotap + 802.11 envelope, and the kernel filter that picks
//!   it out of the video stream sharing the adapter.
//! - [`crypto`] — the fleet-keyed ChaCha20-Poly1305 seal.
//! - [`radio`] — the only platform-specific code: `AF_PACKET` on Linux, a refusing
//!   stub elsewhere.
//! - [`bus`] — transmit and receive over one interface.
//! - [`ingest`] — captured bytes to table effect, as one pure function.
//! - [`neighbors`] — the table, the counters, the dead reckoning.
//! - [`schedule`] — 2 Hz with per-transmission jitter.
//! - [`vehicle`] — filling this node's own beacon from the flight controller.
//! - [`publish`] — the JSON contract on `swarm.sock`.
//! - [`service`] — the daemon loop that wires those together.
//!
//! ## Airtime
//!
//! This is the number the design lives or dies by, so it is stated with its
//! arithmetic rather than asserted.
//!
//! One beacon on the air, at [`frame::BEACON_MCS_INDEX`] (MCS 0, BPSK 1/2, 20 MHz,
//! long guard interval, 6.5 Mbps):
//!
//! ```text
//!   802.11 MPDU      = 24 B header + 50 B payload + 4 B FCS   = 78 B
//!                      (the 13 B radiotap header is injection
//!                       metadata and is stripped before transmission)
//!   coded bits       = 16 service + 8 x 78 + 6 tail            = 646
//!   OFDM symbols     = ceil(646 / 26 bits per symbol)          = 25
//!   data time        = 25 x 4 us                               = 100 us
//!   HT-mixed preamble (L-STF/L-LTF/L-SIG/HT-SIG/HT-STF/HT-LTF) =  40 us
//!   DIFS (no ACK, so no SIFS+ACK follows)                      =  34 us
//!   ------------------------------------------------------------------
//!   per beacon                                                 = 174 us
//! ```
//!
//! At N=24 drones and [`BEACON_HZ`] = 2, that is 48 frames per second:
//!
//! ```text
//!   48 x 174 us = 8.35 ms/s = 0.84% airtime
//! ```
//!
//! At MCS 1 the same frame costs 126 µs and the bus is 0.60%; MCS 0 buys about 3 dB
//! of link margin for a quarter of a percent of airtime, and the beacon is the
//! input to collision avoidance — the one message that must still decode when the
//! video link is already failing. Even at N=50 the bus is 1.7%. Against a video
//! plane measured at 48% for a single stream, the swarm bus is free.

pub mod beacon;
pub mod bus;
pub mod config;
pub mod crypto;
pub mod fleet_join;
pub mod frame;
pub mod ingest;
pub mod neighbors;
pub mod precedence;
pub mod publish;
pub mod radio;
pub mod schedule;
pub mod service;
pub mod vehicle;

pub use beacon::{
    SwarmBeacon, BEACON_WIRE_LEN, STATUS_ARMED, STATUS_EMERGENCY, STATUS_GPS_OK, STATUS_GUIDED,
    STATUS_HERO, STATUS_PRECEDENCE_MASK,
};
pub use bus::SwarmBus;
pub use config::{SwarmBusConfig, CONFIG_YAML};
pub use crypto::{derive_fleet_key, resolve_fleet_key, SwarmCipher};
pub use frame::{SwarmFrame, SwarmFrameKind};
pub use ingest::{ingest_frame, Ingest, IngestReject};
pub use neighbors::{Neighbor, NeighborTable, SwarmCounters, MAX_NEIGHBORS, NEIGHBOR_STALE};
pub use precedence::ModePrecedence;
pub use schedule::{BEACON_HZ, BEACON_JITTER_MS, BEACON_PERIOD};
pub use service::run;

/// The airtime figure derived in the module documentation above, as a fraction of
/// one 20 MHz channel at the full fleet size.
///
/// Exported so it can be asserted rather than only written down: a change to the
/// beacon size, the rate or the modulation that pushes the bus past a percent of
/// the channel should fail a test, not be discovered on a flight line.
pub const AIRTIME_BUDGET: AirtimeBudget = AirtimeBudget {
    frame_bytes_on_air: 78,
    per_frame_us: 174,
    frames_per_second_at_full_fleet: 48,
};

/// The airtime arithmetic, as data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AirtimeBudget {
    /// The 802.11 MPDU including the FCS, excluding the stripped radiotap header.
    pub frame_bytes_on_air: usize,
    /// Occupancy of one beacon in microseconds, preamble and DIFS included.
    pub per_frame_us: u64,
    /// `FLEET_MAX_SLOTS` drones at [`BEACON_HZ`].
    pub frames_per_second_at_full_fleet: u64,
}

impl AirtimeBudget {
    /// Fraction of one channel the bus occupies at the full fleet size.
    pub fn occupancy(&self) -> f64 {
        (self.frames_per_second_at_full_fleet * self.per_frame_us) as f64 / 1e6
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_radio::config::FLEET_MAX_SLOTS;

    /// The frame size the airtime arithmetic assumes must be the frame size the
    /// codec actually produces. If the beacon or the seal grows, this fails and the
    /// budget above has to be recomputed rather than silently becoming fiction.
    #[test]
    fn the_airtime_arithmetic_matches_the_real_frame_size() {
        let mpdu = frame::IEEE80211_HDR_LEN + BEACON_WIRE_LEN + crypto::PAYLOAD_OVERHEAD + 4;
        assert_eq!(
            AIRTIME_BUDGET.frame_bytes_on_air, mpdu,
            "the on-air frame is {mpdu} B; the documented budget assumes {}",
            AIRTIME_BUDGET.frame_bytes_on_air
        );

        // And the per-frame time follows from that size at MCS 0.
        let coded_bits = 16 + 8 * mpdu + 6;
        let symbols = coded_bits.div_ceil(26);
        let us = symbols as u64 * 4 + 40 + 34;
        assert_eq!(AIRTIME_BUDGET.per_frame_us, us, "{symbols} symbols");
    }

    #[test]
    fn the_frame_rate_is_the_full_fleet_at_the_beacon_rate() {
        assert_eq!(
            AIRTIME_BUDGET.frames_per_second_at_full_fleet,
            FLEET_MAX_SLOTS as u64 * BEACON_HZ as u64
        );
        assert_eq!(AIRTIME_BUDGET.frames_per_second_at_full_fleet, 48);
    }

    /// The claim the design rests on: the bus is under one percent of the channel at
    /// the full fleet size, so it never competes with video for airtime.
    #[test]
    fn the_bus_stays_under_one_percent_of_the_channel() {
        let occupancy = AIRTIME_BUDGET.occupancy();
        assert!(
            occupancy < 0.01,
            "the swarm bus occupies {:.2}% of the channel",
            occupancy * 100.0
        );
        assert!((occupancy - 0.00835).abs() < 1e-5, "{occupancy}");
    }

    /// Even at twice the committed fleet size the bus is negligible, which is the
    /// headroom claim in the plan.
    #[test]
    fn the_bus_is_still_negligible_at_fifty_nodes() {
        let fifty = AirtimeBudget {
            frames_per_second_at_full_fleet: 100,
            ..AIRTIME_BUDGET
        };
        assert!(fifty.occupancy() < 0.02, "{}", fifty.occupancy());
    }
}
