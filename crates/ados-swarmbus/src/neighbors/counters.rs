//! The counter set the bus publishes.
//!
//! Six numbers, and the discipline about which one moves when matters more than it
//! looks — these are the whole field diagnosis of a swarm bus:
//!
//! - `beacons_bad_magic` nonzero in steady state means the **kernel filter is not
//!   attached**, so the adapter's entire video stream is being copied to userspace
//!   and discarded.
//! - `beacons_bad_tag` nonzero means a node **in range holds a different fleet key**
//!   — a half-provisioned aircraft, or two fleets that were meant to be separate.
//! - `beacons_rx` climbing at roughly `2 × (N−1)` per second is the bus working;
//!   flat while `neighbors_now` is nonzero means the table is coasting on entries
//!   that are about to age out.
//! - `beacons_tx` flat on a drone means it is not radiating at all, which is exactly
//!   the state the identity gate forces on a misprovisioned slot.
//!
//! Conflating any two of them, or counting a malformed capture as either fault,
//! destroys the signal. That is why they are separate fields rather than one
//! `errors` total.

/// The counters the swarm bus reports beside its neighbour table.
///
/// Plain `u64`s rather than atomics: the table and its counters live behind one lock,
/// so there is exactly one home for the numbers and no way for a counter to disagree
/// with the table it describes. `neighbors_now` is deliberately absent — it is derived
/// from the live table at publish time, so it can never drift from the array beside it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SwarmCounters {
    /// Beacons this node transmitted. Zero forever on a ground station, which is not
    /// an aircraft and never emits one.
    pub beacons_tx: u64,
    /// Frames accepted into the table.
    pub beacons_rx: u64,
    /// Frames rejected because the magic was not ours.
    pub beacons_bad_magic: u64,
    /// Frames whose Poly1305 tag did not verify: a wrong fleet key, corruption, or a
    /// forgery. Indistinguishable by design, and all three want the same response.
    pub beacons_bad_tag: u64,
    /// Neighbours dropped after [`super::NEIGHBOR_STALE`].
    pub beacons_stale_dropped: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh bus has reported nothing and must say so, rather than defaulting any
    /// field to a plausible-looking number.
    #[test]
    fn a_fresh_counter_set_is_all_zero() {
        let c = SwarmCounters::default();
        assert_eq!(c.beacons_tx, 0);
        assert_eq!(c.beacons_rx, 0);
        assert_eq!(c.beacons_bad_magic, 0);
        assert_eq!(c.beacons_bad_tag, 0);
        assert_eq!(c.beacons_stale_dropped, 0);
        assert_eq!(c, SwarmCounters::default());
    }

    /// The two fault counters must be independently addressable: a diagnosis reads
    /// them as different conclusions, so a change that made one alias the other would
    /// silently merge "the filter is broken" with "someone has the wrong key".
    #[test]
    fn the_two_fault_counters_are_independent_fields() {
        let magic = SwarmCounters {
            beacons_bad_magic: 1,
            ..SwarmCounters::default()
        };
        let tag = SwarmCounters {
            beacons_bad_tag: 1,
            ..SwarmCounters::default()
        };
        assert_ne!(magic, tag);
        assert_eq!(magic.beacons_bad_tag, 0);
        assert_eq!(tag.beacons_bad_magic, 0);
    }
}
