//! The `simclock.v1` datagrams, little-endian.
//!
//! Simulator to agent, `TICK` (32 bytes):
//! `u8 v=1, u8 kind=1, u8 flags, u8 rsv, u32 seq, u64 sim_ns, u64 grant_ns, f64 speed`.
//! Agent to simulator, `ACK` (24 bytes):
//! `u8 v=1, u8 kind=2, u16 parts, u32 seq, u64 done_ns, u32 lag_wall_us, u32 rsv`.
//! The agent's local relay passes each `TICK` on to its followers unchanged,
//! as the 32 bytes on a Unix stream socket.

/// The contract version both datagrams carry in their first byte.
pub const SIMCLOCK_V1_VERSION: u8 = 1;
pub const KIND_TICK: u8 = 1;
pub const KIND_ACK: u8 = 2;
pub const TICK_LEN: usize = 32;
pub const ACK_LEN: usize = 24;

/// `TICK` flag bits.
pub const FLAG_PAUSED: u8 = 1 << 0;
pub const FLAG_REALTIME: u8 = 1 << 1;
pub const FLAG_FINAL: u8 = 1 << 2;
pub const FLAG_LOCKSTEP: u8 = 1 << 3;

/// One simulator time broadcast.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tick {
    pub flags: u8,
    pub seq: u32,
    /// Sim time now.
    pub sim_ns: u64,
    /// Sim time the simulator may advance to before the next tick.
    pub grant_ns: u64,
    /// Sim seconds per wall second the simulator is pacing at.
    pub speed: f64,
}

impl Tick {
    pub fn encode(&self) -> [u8; TICK_LEN] {
        let mut b = [0u8; TICK_LEN];
        b[0] = SIMCLOCK_V1_VERSION;
        b[1] = KIND_TICK;
        b[2] = self.flags;
        b[4..8].copy_from_slice(&self.seq.to_le_bytes());
        b[8..16].copy_from_slice(&self.sim_ns.to_le_bytes());
        b[16..24].copy_from_slice(&self.grant_ns.to_le_bytes());
        b[24..32].copy_from_slice(&self.speed.to_le_bytes());
        b
    }

    /// `None` for anything but a well-formed v1 `TICK`.
    pub fn decode(b: &[u8]) -> Option<Tick> {
        if b.len() != TICK_LEN || b[0] != SIMCLOCK_V1_VERSION || b[1] != KIND_TICK {
            return None;
        }
        let speed = f64::from_le_bytes(b[24..32].try_into().ok()?);
        if !speed.is_finite() || speed < 0.0 {
            return None;
        }
        Some(Tick {
            flags: b[2],
            seq: u32::from_le_bytes(b[4..8].try_into().ok()?),
            sim_ns: u64::from_le_bytes(b[8..16].try_into().ok()?),
            grant_ns: u64::from_le_bytes(b[16..24].try_into().ok()?),
            speed,
        })
    }

    pub fn paused(&self) -> bool {
        self.flags & FLAG_PAUSED != 0
    }
}

/// One agent acknowledgement of a grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ack {
    /// How many participants reported.
    pub parts: u16,
    pub seq: u32,
    pub done_ns: u64,
    pub lag_wall_us: u32,
}

impl Ack {
    pub fn encode(&self) -> [u8; ACK_LEN] {
        let mut b = [0u8; ACK_LEN];
        b[0] = SIMCLOCK_V1_VERSION;
        b[1] = KIND_ACK;
        b[2..4].copy_from_slice(&self.parts.to_le_bytes());
        b[4..8].copy_from_slice(&self.seq.to_le_bytes());
        b[8..16].copy_from_slice(&self.done_ns.to_le_bytes());
        b[16..20].copy_from_slice(&self.lag_wall_us.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8]) -> Option<Ack> {
        if b.len() != ACK_LEN || b[0] != SIMCLOCK_V1_VERSION || b[1] != KIND_ACK {
            return None;
        }
        Some(Ack {
            parts: u16::from_le_bytes(b[2..4].try_into().ok()?),
            seq: u32::from_le_bytes(b[4..8].try_into().ok()?),
            done_ns: u64::from_le_bytes(b[8..16].try_into().ok()?),
            lag_wall_us: u32::from_le_bytes(b[16..20].try_into().ok()?),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The same bytes the simulator's encoder test produces.
    pub(crate) const GOLDEN_TICK: [u8; TICK_LEN] = [
        0x01, 0x01, 0x09, 0x00, 0x07, 0x00, 0x00, 0x00, 0x00, 0x2f, 0x68, 0x59, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x5c, 0x99, 0x5a, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x08, 0x40,
    ];
    const GOLDEN_ACK: [u8; ACK_LEN] = [
        0x01, 0x02, 0x03, 0x00, 0x07, 0x00, 0x00, 0x00, 0x00, 0x5c, 0x99, 0x5a, 0x00, 0x00, 0x00,
        0x00, 0xfa, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];

    #[test]
    fn tick_matches_the_golden_bytes() {
        let t = Tick::decode(&GOLDEN_TICK).unwrap();
        assert_eq!(
            t,
            Tick {
                flags: FLAG_PAUSED | FLAG_LOCKSTEP,
                seq: 7,
                sim_ns: 1_500_000_000,
                grant_ns: 1_520_000_000,
                speed: 3.0
            }
        );
        assert_eq!(t.encode(), GOLDEN_TICK);
    }

    #[test]
    fn ack_matches_the_golden_bytes() {
        let a = Ack {
            parts: 3,
            seq: 7,
            done_ns: 1_520_000_000,
            lag_wall_us: 250,
        };
        assert_eq!(a.encode(), GOLDEN_ACK);
        assert_eq!(Ack::decode(&GOLDEN_ACK), Some(a));
    }

    #[test]
    fn malformed_datagrams_are_refused() {
        assert!(Tick::decode(&GOLDEN_TICK[..31]).is_none());
        let mut b = GOLDEN_TICK;
        b[0] = 2;
        assert!(Tick::decode(&b).is_none());
        assert!(Tick::decode(&GOLDEN_ACK).is_none());
    }
}
