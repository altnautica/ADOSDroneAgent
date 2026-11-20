//! The `simclock.v1` datagrams and frames, little-endian.
//!
//! Simulator to agent, `TICK` (32 bytes):
//! `u8 v=1, u8 kind=1, u8 flags, u8 rsv, u32 seq, u64 sim_ns, u64 grant_ns, f64 speed`.
//! Agent to simulator, `ACK` (24 bytes):
//! `u8 v=1, u8 kind=2, u16 parts, u32 seq, u64 done_ns, u32 lag_wall_us,
//! u8 flags, u8 rsv[3]`.
//! The agent's local relay passes each `TICK` on to its followers unchanged,
//! as the 32 bytes on a Unix stream socket.
//! A lockstep participant to the agent's barrier, `DONE` (16 bytes, on the
//! Unix stream socket beside the relay's, [`super::PARTICIPANTS_SOCKET`]):
//! `u8 v=1, u8 kind=3, u8 flags, u8 rsv, u32 rsv, u64 done_ns`.
//!
//! Lockstep: the simulator sends a `TICK` at each grant boundary and steps no
//! further until the agent's `ACK` carries `done_ns >= grant_ns`. A
//! participant's `DONE` says it finished every piece of work due before sim
//! time `done_ns` (a periodic lane reports its next due time). The agent's
//! barrier acknowledges a tick, with `done_ns` = the tick's `grant_ns`, once
//! every registered participant reported `done_ns` past the tick's `sim_ns`.
//!
//! Start readiness: a participant whose `DONE` sets [`DONE_GATE`] is one the
//! run's start waits on (the router's FC link), reporting [`DONE_ATTACHED`]
//! (its FC transport is open) and [`DONE_READY`] (it has heard the FC's
//! HEARTBEAT). Every `ACK` carries [`ACK_ATTACHED`] / [`ACK_READY`] when at
//! least one gate is registered and every gate is attached / ready; a
//! simulator with `[clock] start_ready` holds its start until they are.
//! Frames without flags (byte 2 of `DONE`, byte 20 of `ACK` zero) are
//! unchanged from before.

/// The contract version every datagram and frame carries in its first byte.
pub const SIMCLOCK_V1_VERSION: u8 = 1;
pub const KIND_TICK: u8 = 1;
pub const KIND_ACK: u8 = 2;
pub const KIND_DONE: u8 = 3;
pub const TICK_LEN: usize = 32;
pub const ACK_LEN: usize = 24;
pub const DONE_LEN: usize = 16;

/// `TICK` flag bits.
pub const FLAG_PAUSED: u8 = 1 << 0;
pub const FLAG_REALTIME: u8 = 1 << 1;
pub const FLAG_FINAL: u8 = 1 << 2;
pub const FLAG_LOCKSTEP: u8 = 1 << 3;

/// `DONE` flag bits.
pub const DONE_GATE: u8 = 1 << 0;
pub const DONE_ATTACHED: u8 = 1 << 1;
pub const DONE_READY: u8 = 1 << 2;

/// `ACK` flag bits.
pub const ACK_READY: u8 = 1 << 0;
pub const ACK_ATTACHED: u8 = 1 << 1;

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
    /// The sim time the agent is ready for the simulator to reach: the
    /// acknowledged tick's `grant_ns`.
    pub done_ns: u64,
    /// Wall time from the tick's arrival to the acknowledgement, µs.
    pub lag_wall_us: u32,
    /// [`ACK_READY`], [`ACK_ATTACHED`].
    pub flags: u8,
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
        b[20] = self.flags;
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
            flags: b[20],
        })
    }
}

/// One participant's progress report to the agent's lockstep barrier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Done {
    /// Every piece of the participant's work due before this sim time is done.
    pub done_ns: u64,
    /// [`DONE_GATE`], [`DONE_ATTACHED`], [`DONE_READY`].
    pub flags: u8,
}

impl Done {
    pub fn encode(&self) -> [u8; DONE_LEN] {
        let mut b = [0u8; DONE_LEN];
        b[0] = SIMCLOCK_V1_VERSION;
        b[1] = KIND_DONE;
        b[2] = self.flags;
        b[8..16].copy_from_slice(&self.done_ns.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8]) -> Option<Done> {
        if b.len() != DONE_LEN || b[0] != SIMCLOCK_V1_VERSION || b[1] != KIND_DONE {
            return None;
        }
        Some(Done {
            done_ns: u64::from_le_bytes(b[8..16].try_into().ok()?),
            flags: b[2],
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
    /// The bytes the simulator's `participant` decode test reads back as
    /// `{parts 3, seq 7, done 1.52 s, lag 250 µs}`.
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
            flags: 0,
        };
        assert_eq!(a.encode(), GOLDEN_ACK);
        assert_eq!(Ack::decode(&GOLDEN_ACK), Some(a));
    }

    /// The bytes the simulator's `participant` decode test reads back with
    /// the FC links attached and heard (`ACK_READY | ACK_ATTACHED`).
    const GOLDEN_ACK_READY: [u8; ACK_LEN] = [
        0x01, 0x02, 0x02, 0x00, 0x09, 0x00, 0x00, 0x00, 0x00, 0xf7, 0xcb, 0x3c, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00,
    ];

    #[test]
    fn a_ready_ack_matches_the_golden_bytes() {
        let a = Ack {
            parts: 2,
            seq: 9,
            done_ns: 1_020_000_000,
            lag_wall_us: 0,
            flags: ACK_READY | ACK_ATTACHED,
        };
        assert_eq!(a.encode(), GOLDEN_ACK_READY);
        assert_eq!(Ack::decode(&GOLDEN_ACK_READY), Some(a));
    }

    /// The bytes the Python SDK's participant writes for `done(1.54)`.
    const GOLDEN_DONE: [u8; DONE_LEN] = [
        0x01, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x89, 0xca, 0x5b, 0x00, 0x00, 0x00,
        0x00,
    ];

    #[test]
    fn done_matches_the_golden_bytes() {
        let d = Done {
            done_ns: 1_540_000_000,
            flags: 0,
        };
        assert_eq!(d.encode(), GOLDEN_DONE);
        assert_eq!(Done::decode(&GOLDEN_DONE), Some(d));
        assert!(Done::decode(&GOLDEN_DONE[..15]).is_none());
        assert!(Done::decode(&GOLDEN_ACK).is_none());
        let gate = Done {
            done_ns: 1_540_000_000,
            flags: DONE_GATE | DONE_ATTACHED,
        };
        let b = gate.encode();
        assert_eq!(b[2], 0b011);
        assert_eq!(Done::decode(&b), Some(gate));
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
