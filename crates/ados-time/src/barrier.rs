//! The agent's lockstep barrier: one `ACK` to the simulator per grant, once
//! every registered participant is done.
//!
//! The simulator sends a `TICK` at each grant boundary and steps past it only
//! when the agent acknowledges the grant (`ACK.done_ns >= grant_ns`).
//! Participants (the pilot core, the router's control lanes, opted-in
//! plugins) register by connecting to the participants socket and report
//! `DONE`: every piece of their work due before `done_ns` is finished and
//! its commands written. A tick is acknowledged when every registered
//! participant reported `done_ns` past the tick's `sim_ns`; with none
//! registered it is acknowledged at once. A participant that connects while
//! a tick waits is waited on too; one that disconnects stops counting.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Instant;

use ados_protocol::simclock::wire::{Ack, Tick, FLAG_LOCKSTEP, FLAG_PAUSED};

/// At most this many participants register; one past it is refused.
pub const MAX_PARTICIPANTS: usize = 64;

/// What the barrier sends: an `ACK` and the address of the tick it answers.
pub type Release = (Ack, SocketAddr);

struct Pending {
    tick: Tick,
    from: SocketAddr,
    at: Instant,
}

/// The barrier's state. Pure: the relay feeds it ticks and reports and sends
/// what it releases.
#[derive(Default)]
pub struct Barrier {
    /// Each participant's latest report; `None` until its first.
    parts: BTreeMap<u64, Option<u64>>,
    next_id: u64,
    pending: Option<Pending>,
    last: Option<Release>,
}

impl Barrier {
    /// Registered participants.
    pub fn parts(&self) -> usize {
        self.parts.len()
    }

    /// Register a participant; `None` past [`MAX_PARTICIPANTS`].
    pub fn join(&mut self) -> Option<u64> {
        if self.parts.len() >= MAX_PARTICIPANTS {
            return None;
        }
        let id = self.next_id;
        self.next_id += 1;
        self.parts.insert(id, None);
        Some(id)
    }

    /// A participant left (its connection closed).
    pub fn leave(&mut self, id: u64, now: Instant) -> Option<Release> {
        self.parts.remove(&id);
        self.release(now)
    }

    /// A participant's report. A lower one than it reported before is kept
    /// at the higher.
    pub fn done(&mut self, id: u64, done_ns: u64, now: Instant) -> Option<Release> {
        if let Some(d) = self.parts.get_mut(&id) {
            *d = Some(d.map_or(done_ns, |p| p.max(done_ns)));
        }
        self.release(now)
    }

    /// A tick from `from`. Only a running lockstep tick waits (a paused or
    /// realtime one is the clock held or paced, never a grant); a repeat of
    /// the tick last acknowledged gets the same `ACK` again (the simulator
    /// resends while it waits); a newer tick replaces one still waiting.
    pub fn tick(&mut self, tick: &Tick, from: SocketAddr, now: Instant) -> Option<Release> {
        if tick.flags & FLAG_LOCKSTEP == 0 || tick.flags & FLAG_PAUSED != 0 {
            return None;
        }
        if let Some(last) = self.last.filter(|(a, to)| a.seq == tick.seq && *to == from) {
            return Some(last);
        }
        if self
            .pending
            .as_ref()
            .is_some_and(|p| p.tick.seq == tick.seq)
        {
            return None;
        }
        self.pending = Some(Pending {
            tick: *tick,
            from,
            at: now,
        });
        self.release(now)
    }

    fn release(&mut self, now: Instant) -> Option<Release> {
        let p = self.pending.as_ref()?;
        let ready = self
            .parts
            .values()
            .all(|d| d.is_some_and(|d| d > p.tick.sim_ns));
        if !ready {
            return None;
        }
        let ack = Ack {
            parts: self.parts.len() as u16,
            seq: p.tick.seq,
            done_ns: p.tick.grant_ns,
            lag_wall_us: now
                .saturating_duration_since(p.at)
                .as_micros()
                .min(u128::from(u32::MAX)) as u32,
        };
        let out = (ack, p.from);
        self.pending = None;
        self.last = Some(out);
        Some(out)
    }
}

#[cfg(test)]
#[path = "barrier_tests.rs"]
mod tests;
