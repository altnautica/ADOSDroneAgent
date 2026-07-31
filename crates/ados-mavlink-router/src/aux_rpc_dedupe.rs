//! Idempotency cache for relay-proxy requests arriving over the aux uplink.
//!
//! The ground station retransmits a Request datagram it has no answer for: the
//! drone's radio is half-duplex and spends most of its airtime injecting video,
//! so an uplink datagram that lands inside a TX burst dies at the PHY and only
//! a resend gets through. That resend is what makes the lane usable, and it is
//! also what makes a relayed `PUT /api/config` dangerous — without this module
//! a retried write executes twice.
//!
//! ## The cache holds encoded fragments, not the HTTP result
//!
//! A duplicate that arrives after the original finished is answered by
//! re-sending the exact aux payloads the original produced. That is a pure
//! re-send: the HTTP call never runs again, so a write cannot double-execute,
//! and it covers the other loss direction for free — a response whose fragments
//! died on the way down is re-sent verbatim rather than recomputed, which also
//! means the ground reassembles bytes identical to the ones it half-received.
//!
//! ## Bounds
//!
//! Every entry is dropped once it is older than [`DEDUPE_TTL`], and the cache
//! is capped at [`DEDUPE_MAX_ENTRIES`] entries and [`DEDUPE_MAX_BYTES`] of
//! cached payload, oldest evicted first. A caller that vanishes mid-call
//! therefore costs a bounded amount of memory for a bounded time.
//!
//! ## Locking
//!
//! The cache is a plain [`std::sync::Mutex`]: every operation is a short
//! synchronous `Vec` manipulation with no `.await` inside, so an async mutex
//! would buy nothing and cost a scheduling point on the response path.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Most requests remembered at once.
///
/// Sized against the ground's own concurrency bound, which is 1024 pending
/// calls. A drone only ever dedupes the traffic addressed to it plus the
/// broadcasts, so it sees a fraction of that, but the cap has to leave room for
/// every request that could still be retransmitted rather than be derived from
/// a hoped-for call rate: overflowing it evicts in-flight markers, and losing
/// one of those lets a resend re-execute a write.
///
/// Entries are tiny — an id, an instant and a discriminant — and the payloads
/// they may hold are separately bounded by [`DEDUPE_MAX_BYTES`], so raising the
/// count does not raise the memory ceiling.
const DEDUPE_MAX_ENTRIES: usize = 256;

/// Most cached response payload held at once. A maximum-size response is 64
/// fragments of ~1.2 KB (~77 KB), so this holds at least three of the largest
/// answers the lane can carry, and far more of the ordinary ones.
const DEDUPE_MAX_BYTES: usize = 256 * 1024;

/// Comfortably exceeds the ground's 10 s call bound plus its resend schedule.
const DEDUPE_TTL: Duration = Duration::from_secs(60);

/// What the caller should do with a request id it has just been handed.
#[derive(Debug)]
pub enum Admit {
    /// Not seen: caller runs the HTTP call and then calls
    /// [`RequestDedupe::complete`].
    Fresh,
    /// Seen and still running: drop silently.
    Duplicate,
    /// Seen and finished: re-send these payloads, do NOT re-execute.
    Replay(Vec<Vec<u8>>),
}

enum Entry {
    /// The handler is running; a duplicate is dropped and the original answers.
    InFlight,
    /// Completed: the exact aux payloads to re-send on a duplicate.
    Done {
        fragments: Vec<Vec<u8>>,
        bytes: usize,
        at: Instant,
    },
}

impl Entry {
    /// Cached payload this entry is holding. An in-flight marker holds none,
    /// so it never counts against the byte cap.
    fn bytes(&self) -> usize {
        match self {
            Self::InFlight => 0,
            Self::Done { bytes, .. } => *bytes,
        }
    }
}

struct Record {
    id: u32,
    /// When the id was first admitted, which is the age an in-flight marker is
    /// swept on. A handler that dies without completing or abandoning would
    /// otherwise wedge its id shut for the life of the process.
    admitted: Instant,
    entry: Entry,
}

impl Record {
    /// The instant this record's TTL runs from: completion for a finished
    /// answer (the answer is what callers replay), admission for one still
    /// running.
    fn touched(&self) -> Instant {
        match self.entry {
            Entry::Done { at, .. } => at,
            Entry::InFlight => self.admitted,
        }
    }
}

#[derive(Default)]
struct Inner {
    /// Oldest admission first, so eviction and the TTL sweep both work from the
    /// front. A linear scan over at most [`DEDUPE_MAX_ENTRIES`] `u32`s is
    /// cheaper than a hash plus a second structure to keep in step with it.
    records: Vec<Record>,
}

impl Inner {
    fn total_bytes(&self) -> usize {
        self.records.iter().map(|r| r.entry.bytes()).sum()
    }

    fn sweep(&mut self, now: Instant) {
        self.records
            .retain(|r| now.saturating_duration_since(r.touched()) < DEDUPE_TTL);
    }

    /// Drop the oldest record matching `pred` and report the bytes it freed.
    /// `None` when nothing matches.
    fn drop_oldest_completed(&mut self) -> Option<usize> {
        let idx = self
            .records
            .iter()
            .position(|r| !matches!(r.entry, Entry::InFlight))?;
        Some(self.records.remove(idx).entry.bytes())
    }

    fn enforce_bounds(&mut self) {
        // Make room out of completed records first. An in-flight marker is the
        // only thing standing between a retransmit and a SECOND execution of
        // the same request, so evicting one turns the next resend of an
        // unfinished write from a dropped duplicate into a fresh call.
        while self.records.len() > DEDUPE_MAX_ENTRIES {
            if self.drop_oldest_completed().is_none() {
                // Everything is in flight, so the cap is genuinely exceeded by
                // live calls rather than by cached answers. Shed the oldest
                // rather than grow without bound.
                self.records.remove(0);
            }
        }
        let mut bytes = self.total_bytes();
        while bytes > DEDUPE_MAX_BYTES {
            // Only a completed record holds a payload, so only a completed
            // record can free bytes. Evicting in-flight markers here freed
            // nothing and could not end the loop on its own terms, so it walked
            // the cache shedding exactly the entries that guarantee
            // idempotency.
            match self.drop_oldest_completed() {
                Some(freed) => bytes -= freed,
                None => break,
            }
        }
    }
}

/// Recently-seen relay requests, keyed by the ground's request id.
#[derive(Default)]
pub struct RequestDedupe {
    inner: Mutex<Inner>,
}

impl RequestDedupe {
    pub fn new() -> Self {
        Self::default()
    }

    /// Decide what to do with an arriving request id, and mark it in flight if
    /// it is new.
    pub fn admit(&self, id: u32) -> Admit {
        self.admit_at(id, Instant::now())
    }

    /// Record the fragments an answered request produced, so a retransmit of
    /// the same id replays them instead of re-running the HTTP call.
    pub fn complete(&self, id: u32, fragments: &[Vec<u8>]) {
        self.complete_at(id, fragments, Instant::now());
    }

    /// Drops the InFlight marker when the HTTP call failed to produce
    /// fragments, so the ground's next retransmit gets a real attempt rather
    /// than being dropped as a duplicate of a call that answered nothing.
    pub fn abandon(&self, id: u32) {
        let mut inner = self.lock();
        inner
            .records
            .retain(|r| !(r.id == id && matches!(r.entry, Entry::InFlight)));
    }

    /// The critical sections here are pure `Vec` manipulation with no caller
    /// code running under the lock, so a poisoned mutex can only follow an
    /// unrelated abort. Recovering the data keeps the relay lane answering
    /// instead of taking every later request down with it.
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn admit_at(&self, id: u32, now: Instant) -> Admit {
        let mut inner = self.lock();
        inner.sweep(now);
        if let Some(i) = inner.records.iter().position(|r| r.id == id) {
            return match &inner.records[i].entry {
                Entry::InFlight => Admit::Duplicate,
                Entry::Done { fragments, .. } => Admit::Replay(fragments.clone()),
            };
        }
        inner.records.push(Record {
            id,
            admitted: now,
            entry: Entry::InFlight,
        });
        inner.enforce_bounds();
        Admit::Fresh
    }

    fn complete_at(&self, id: u32, fragments: &[Vec<u8>], now: Instant) {
        let done = Entry::Done {
            fragments: fragments.to_vec(),
            bytes: fragments.iter().map(Vec::len).sum(),
            at: now,
        };
        let mut inner = self.lock();
        let existing = inner.records.iter().position(|r| r.id == id);
        match existing {
            Some(i) => inner.records[i].entry = done,
            // The in-flight marker was evicted or swept while the HTTP call
            // ran. Keep the answer anyway: a retransmit already on its way is
            // exactly what this cache exists to absorb.
            None => inner.records.push(Record {
                id,
                admitted: now,
                entry: done,
            }),
        }
        inner.enforce_bounds();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frags(n: usize, len: usize) -> Vec<Vec<u8>> {
        (0..n).map(|i| vec![i as u8; len]).collect()
    }

    #[test]
    fn a_second_arrival_while_the_first_runs_is_a_duplicate() {
        let d = RequestDedupe::new();
        assert!(matches!(d.admit(9), Admit::Fresh));
        assert!(
            matches!(d.admit(9), Admit::Duplicate),
            "the original call is still running and will answer both"
        );
    }

    #[test]
    fn a_completed_request_replays_the_identical_bytes() {
        let d = RequestDedupe::new();
        let answer = frags(3, 64);
        assert!(matches!(d.admit(9), Admit::Fresh));
        d.complete(9, &answer);
        match d.admit(9) {
            Admit::Replay(got) => assert_eq!(
                got, answer,
                "a replay must be byte-identical, or a retried write is a second write"
            ),
            other => panic!("expected a replay, got {other:?}"),
        }
    }

    #[test]
    fn abandon_reopens_an_id_that_produced_nothing_but_spares_an_answer() {
        let d = RequestDedupe::new();
        assert!(matches!(d.admit(3), Admit::Fresh));
        assert!(matches!(d.admit(3), Admit::Duplicate));
        d.abandon(3);
        assert!(
            matches!(d.admit(3), Admit::Fresh),
            "a call that answered nothing must be retryable"
        );

        d.complete(3, &frags(1, 8));
        d.abandon(3);
        assert!(
            matches!(d.admit(3), Admit::Replay(_)),
            "abandon clears an in-flight marker only, never a cached answer"
        );
    }

    #[test]
    fn an_in_flight_marker_is_never_evicted_to_make_room() {
        let d = RequestDedupe::new();
        // One request is still running. Its marker is what makes a retransmit a
        // dropped duplicate instead of a second execution.
        let running = 9_999u32;
        assert!(matches!(d.admit(running), Admit::Fresh));

        // Now flood the cache with completed answers, well past both caps.
        let big = frags(1, 100 * 1024);
        for id in 0..(DEDUPE_MAX_ENTRIES as u32 + 16) {
            d.admit(id);
            d.complete(id, &big);
        }

        // The in-flight marker survived, so a resend of that request is still
        // refused. Before preferring completed records for eviction, the
        // byte-cap loop would shed zero-byte in-flight markers that could never
        // reduce the byte total, and this read Fresh — re-running the write.
        assert!(
            matches!(d.admit(running), Admit::Duplicate),
            "an unfinished request must still be deduplicated after cache pressure"
        );
        assert!(d.lock().total_bytes() <= DEDUPE_MAX_BYTES);
    }

    #[test]
    fn one_past_the_cap_evicts_the_oldest() {
        let d = RequestDedupe::new();
        for id in 0..DEDUPE_MAX_ENTRIES as u32 {
            assert!(matches!(d.admit(id), Admit::Fresh));
            d.complete(id, &frags(1, 8));
        }
        assert!(
            matches!(d.admit(0), Admit::Replay(_)),
            "at exactly the cap nothing has been dropped yet"
        );

        assert!(matches!(d.admit(DEDUPE_MAX_ENTRIES as u32), Admit::Fresh));
        assert!(
            matches!(d.admit(0), Admit::Fresh),
            "the oldest entry is evicted to make room for the 65th"
        );
    }

    #[test]
    fn an_entry_past_the_ttl_is_not_replayed() {
        let d = RequestDedupe::new();
        let t0 = Instant::now();
        assert!(matches!(d.admit_at(7, t0), Admit::Fresh));
        d.complete_at(7, &frags(1, 16), t0);

        assert!(matches!(
            d.admit_at(7, t0 + DEDUPE_TTL - Duration::from_secs(1)),
            Admit::Replay(_)
        ));
        assert!(
            matches!(
                d.admit_at(7, t0 + DEDUPE_TTL + Duration::from_secs(1)),
                Admit::Fresh
            ),
            "a stale answer must never be handed to a new call that reuses the id"
        );
    }

    #[test]
    fn an_in_flight_marker_does_not_outlive_the_ttl() {
        let d = RequestDedupe::new();
        let t0 = Instant::now();
        assert!(matches!(d.admit_at(4, t0), Admit::Fresh));
        assert!(
            matches!(
                d.admit_at(4, t0 + DEDUPE_TTL + Duration::from_secs(1)),
                Admit::Fresh
            ),
            "a handler that died without completing must not wedge its id shut"
        );
    }

    #[test]
    fn the_cache_never_holds_more_than_its_byte_cap() {
        let d = RequestDedupe::new();
        let big = frags(1, 100 * 1024);
        for id in 0..4u32 {
            assert!(matches!(d.admit(id), Admit::Fresh));
            d.complete(id, &big);
            assert!(
                d.lock().total_bytes() <= DEDUPE_MAX_BYTES,
                "byte cap breached after id {id}"
            );
        }
        assert!(
            matches!(d.admit(3), Admit::Replay(_)),
            "the newest answer survives the eviction it caused"
        );
        assert!(
            matches!(d.admit(0), Admit::Fresh),
            "the oldest answers were evicted to stay under the byte cap"
        );
    }
}
