//! The CBBA bid vector and its wire form (swarm-bus frame `kind = 2`).
//!
//! Exactly `5 * n_tasks + 2 * n_agents` bytes:
//!
//! ```text
//! per task  (5 B): winning bid   (f32 big-endian)
//!                  winning slot  (u8; 0 = unassigned, since slot 0 is the
//!                                 ground station and never a swarm agent)
//! per agent (2 B): information timestamp (u16 big-endian)
//! ```
//!
//! The three blocks are CBBA's `y`, `z` and `s` vectors. The agent block is
//! ordered by the sorted roster of registered slots, which every drone derives
//! from the same fleet registry — so the encoding needs no explicit roster on the
//! wire.
//!
//! Bids are EVENT-DRIVEN ONLY: emitted on a task-set change or a reallocation,
//! never periodically. At 20 tasks and 24 agents that is 148 bytes per
//! reallocation, which is why this can share the beacon's medium without a
//! bandwidth argument.

/// Bytes per task in the wire encoding.
pub const BID_BYTES_PER_TASK: usize = 5;

/// Bytes per agent in the wire encoding.
pub const BID_BYTES_PER_AGENT: usize = 2;

/// One agent's view of the auction.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct BidVector {
    /// Highest known bid per task, indexed by task.
    pub y: Vec<f32>,
    /// Slot believed to hold each task; `None` = unassigned.
    pub z: Vec<Option<u8>>,
    /// Per-agent information timestamp, indexed by the sorted roster position.
    pub s: Vec<u16>,
}

impl BidVector {
    pub fn new(n_tasks: usize, n_agents: usize) -> Self {
        Self {
            y: vec![0.0; n_tasks],
            z: vec![None; n_tasks],
            s: vec![0; n_agents],
        }
    }

    pub fn wire_len(n_tasks: usize, n_agents: usize) -> usize {
        n_tasks * BID_BYTES_PER_TASK + n_agents * BID_BYTES_PER_AGENT
    }

    /// Encode for `SwarmFrameKind::CbbaBid`.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::wire_len(self.y.len(), self.s.len()));
        for (bid, winner) in self.y.iter().zip(&self.z) {
            out.extend_from_slice(&bid.to_be_bytes());
            out.push(winner.unwrap_or(0));
        }
        for t in &self.s {
            out.extend_from_slice(&t.to_be_bytes());
        }
        out
    }

    /// Decode a peer's bid. `None` when the length does not match the roster the
    /// receiver holds — a mismatched task or agent count means the sender is
    /// auctioning a different problem, and silently truncating would splice two
    /// auctions together.
    pub fn decode(bytes: &[u8], n_tasks: usize, n_agents: usize) -> Option<Self> {
        if bytes.len() != Self::wire_len(n_tasks, n_agents) {
            return None;
        }
        let mut out = Self::new(n_tasks, n_agents);
        for j in 0..n_tasks {
            let at = j * BID_BYTES_PER_TASK;
            out.y[j] = f32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
            out.z[j] = match bytes[at + 4] {
                0 => None,
                slot => Some(slot),
            };
        }
        let base = n_tasks * BID_BYTES_PER_TASK;
        for a in 0..n_agents {
            let at = base + a * BID_BYTES_PER_AGENT;
            out.s[a] = u16::from_be_bytes([bytes[at], bytes[at + 1]]);
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_length_is_the_plans_formula() {
        assert_eq!(BidVector::wire_len(20, 24), 5 * 20 + 2 * 24);
        assert_eq!(BidVector::wire_len(20, 24), 148);
        assert_eq!(BidVector::wire_len(0, 0), 0);
        let v = BidVector::new(20, 24);
        assert_eq!(v.encode().len(), 148);
    }

    #[test]
    fn round_trips_bids_winners_and_timestamps() {
        let mut v = BidVector::new(3, 4);
        v.y = vec![12.5, 0.0, -3.25];
        v.z = vec![Some(7), None, Some(24)];
        v.s = vec![0, 1, 65535, 300];
        let back = BidVector::decode(&v.encode(), 3, 4).expect("same shape");
        assert_eq!(back, v);
    }

    #[test]
    fn slot_zero_is_the_unassigned_sentinel() {
        // Slot 0 is the ground station and is never a swarm agent, so it is free
        // to mean "nobody holds this task".
        let mut v = BidVector::new(1, 1);
        v.z = vec![None];
        let bytes = v.encode();
        assert_eq!(bytes[4], 0);
        assert_eq!(BidVector::decode(&bytes, 1, 1).unwrap().z, vec![None]);
    }

    #[test]
    fn a_length_mismatch_is_rejected_not_truncated() {
        let v = BidVector::new(3, 4);
        let bytes = v.encode();
        assert!(BidVector::decode(&bytes, 3, 4).is_some());
        // A peer auctioning a different task set must not be spliced in.
        assert!(BidVector::decode(&bytes, 2, 4).is_none());
        assert!(BidVector::decode(&bytes, 3, 3).is_none());
        assert!(BidVector::decode(&bytes[..bytes.len() - 1], 3, 4).is_none());
        assert!(BidVector::decode(&[], 3, 4).is_none());
        assert!(BidVector::decode(&[], 0, 0).is_some());
    }

    #[test]
    fn bids_survive_the_f32_round_trip_bit_exactly() {
        // The decision table compares bids for equality in places, so a lossy
        // encode would make two agents disagree about who won.
        let mut v = BidVector::new(4, 1);
        v.y = vec![f32::MIN_POSITIVE, 1.0 / 3.0, 1e30, -0.0];
        let back = BidVector::decode(&v.encode(), 4, 1).unwrap();
        for (a, b) in v.y.iter().zip(&back.y) {
            assert_eq!(a.to_bits(), b.to_bits());
        }
    }
}
