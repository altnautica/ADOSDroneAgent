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
//!
//! Every `ACK` also says whether the run may start ([`start_flags`]): the
//! participants that gate the start (`DONE_GATE`, the router's FC link) are
//! all attached (`ACK_ATTACHED`) or all ready (`ACK_READY`), with at least one
//! gate registered. A repeat of the last `ACK` (the simulator resends a tick
//! it is still holding) carries the readiness as it is now.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Instant;

use ados_protocol::simclock::wire::{
    Ack, Tick, ACK_ATTACHED, ACK_READY, DONE_ATTACHED, DONE_GATE, DONE_READY, FLAG_LOCKSTEP,
    FLAG_PAUSED,
};

/// At most this many participants register; one past it is refused.
pub const MAX_PARTICIPANTS: usize = 64;

/// What the barrier sends: an `ACK` and the address of the tick it answers.
pub type Release = (Ack, SocketAddr);

struct Pending {
    tick: Tick,
    from: SocketAddr,
    at: Instant,
}

/// One participant: its latest report (`None` until its first) and flags.
#[derive(Clone, Copy, Default)]
struct Part {
    done: Option<u64>,
    flags: u8,
}

/// The barrier's state. Pure: the relay feeds it ticks and reports and sends
/// what it releases.
#[derive(Default)]
pub struct Barrier {
    parts: BTreeMap<u64, Part>,
    next_id: u64,
    pending: Option<Pending>,
    last: Option<Release>,
}

/// The `ACK` flags for the gates among `flags` (each participant's `DONE`
/// flags): [`ACK_ATTACHED`] when at least one gate is registered and every
/// gate is attached or ready, [`ACK_READY`] when every gate is ready.
pub fn start_flags(flags: impl IntoIterator<Item = u8>) -> u8 {
    let gates: Vec<u8> = flags.into_iter().filter(|f| f & DONE_GATE != 0).collect();
    if gates.is_empty() {
        return 0;
    }
    let ready = gates.iter().all(|f| f & DONE_READY != 0);
    let attached = gates.iter().all(|f| f & (DONE_ATTACHED | DONE_READY) != 0);
    (if ready { ACK_READY } else { 0 }) | (if attached { ACK_ATTACHED } else { 0 })
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
        self.parts.insert(id, Part::default());
        Some(id)
    }

    /// A participant left (its connection closed).
    pub fn leave(&mut self, id: u64, now: Instant) -> Option<Release> {
        self.parts.remove(&id);
        self.release(now)
    }

    /// A participant's report with its `DONE` flags. A lower `done_ns` than
    /// it reported before is kept at the higher.
    pub fn done(&mut self, id: u64, done_ns: u64, flags: u8, now: Instant) -> Option<Release> {
        if let Some(p) = self.parts.get_mut(&id) {
            p.done = Some(p.done.map_or(done_ns, |d| d.max(done_ns)));
            p.flags = flags;
        }
        self.release(now)
    }

    /// The `ACK` flags as the gates stand now.
    pub fn flags(&self) -> u8 {
        start_flags(self.parts.values().map(|p| p.flags))
    }

    /// A tick from `from`. Only a running lockstep tick waits (a paused or
    /// realtime one is the clock held or paced, never a grant); a repeat of
    /// the tick last acknowledged gets the same `ACK` again, with the
    /// readiness flags as they are now (the simulator resends while it
    /// waits); a newer tick replaces one still waiting.
    pub fn tick(&mut self, tick: &Tick, from: SocketAddr, now: Instant) -> Option<Release> {
        if tick.flags & FLAG_LOCKSTEP == 0 || tick.flags & FLAG_PAUSED != 0 {
            return None;
        }
        let flags = self.flags();
        if let Some(last) = self.last.as_mut().filter(|(a, to)| a.seq == tick.seq && *to == from) {
            last.0.flags = flags;
            return Some(*last);
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
            .all(|d| d.done.is_some_and(|d| d > p.tick.sim_ns));
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
            flags: self.flags(),
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
