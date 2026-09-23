//! Per-slot sender tracking: the replay window and the second-sender check.
//!
//! An authenticated beacon proves a fleet member sealed it, not that it was sealed
//! just now. Without this, a captured frame re-injected later opens cleanly, lands
//! in the table stamped as just received, and every consumer dead-reckons a
//! position the aircraft left long ago.
//!
//! Every sender seals under a nonce of `prefix || counter`: the prefix is drawn at
//! random once per sender run, and the counter advances on every frame. So a slot's
//! legitimate traffic is one prefix with a strictly rising counter, and a sender
//! restart shows up as a new prefix. That gives three checks:
//!
//! - **Same prefix, counter not above the last accepted one**: a replay.
//! - **A prefix this slot has already moved on from**: a replay of an earlier run.
//!   A restart always draws a fresh prefix, so a retired one never legitimately
//!   comes back.
//! - **A new prefix while the slot's entry is still live**: a second sender on the
//!   slot. A restart that fast is not possible (the entry only goes stale after
//!   [`super::NEIGHBOR_STALE`] of silence), so this is two nodes provisioned with one
//!   slot, and the first one heard keeps it.

use std::collections::BTreeMap;

use crate::crypto::{SenderNonce, NONCE_PREFIX_LEN};

/// How many superseded runs a slot remembers. A slot restarting more than this
/// often inside one bus lifetime is already a fault in its own right.
pub const RETIRED_PREFIXES: usize = 8;

/// What the replay window says about one authenticated beacon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SenderVerdict {
    /// The next frame from the slot's current run, or the first frame of a new run
    /// on a slot that has gone quiet.
    Fresh,
    /// Already seen, or from a run the slot has moved on from.
    Replayed,
    /// A second live sender on a slot that already has one.
    SecondSender,
}

#[derive(Debug, Clone)]
struct SlotMark {
    /// The current run's prefix and the highest counter accepted from it.
    current: SenderNonce,
    /// Prefixes of earlier runs on this slot, oldest first.
    retired: Vec<[u8; NONCE_PREFIX_LEN]>,
}

/// The per-slot sender state. Deliberately not pruned with the neighbour entries:
/// a slot that went quiet must still refuse its old frames when they come back.
/// Bounded by the slot space (`u8`) times [`RETIRED_PREFIXES`].
#[derive(Debug, Default)]
pub struct SenderMarks {
    by_slot: BTreeMap<u8, SlotMark>,
}

impl SenderMarks {
    /// Judge `sender` for `slot`. `slot_live` is whether the slot's table entry is
    /// present and not stale. Pure: nothing changes until [`Self::accept`].
    pub fn verdict(&self, slot: u8, sender: SenderNonce, slot_live: bool) -> SenderVerdict {
        let Some(mark) = self.by_slot.get(&slot) else {
            return SenderVerdict::Fresh;
        };
        if sender.prefix == mark.current.prefix {
            return if sender.counter > mark.current.counter {
                SenderVerdict::Fresh
            } else {
                SenderVerdict::Replayed
            };
        }
        if mark.retired.contains(&sender.prefix) {
            return SenderVerdict::Replayed;
        }
        if slot_live {
            SenderVerdict::SecondSender
        } else {
            SenderVerdict::Fresh
        }
    }

    /// Record `sender` as the slot's latest accepted frame, retiring the previous
    /// run when the prefix changed.
    pub fn accept(&mut self, slot: u8, sender: SenderNonce) {
        match self.by_slot.get_mut(&slot) {
            Some(mark) => {
                if mark.current.prefix != sender.prefix {
                    if mark.retired.len() == RETIRED_PREFIXES {
                        mark.retired.remove(0);
                    }
                    mark.retired.push(mark.current.prefix);
                }
                mark.current = sender;
            }
            None => {
                self.by_slot.insert(
                    slot,
                    SlotMark {
                        current: sender,
                        retired: Vec::new(),
                    },
                );
            }
        }
    }

    /// The next nonce a well-behaved sender on `slot` would use: the current run
    /// with its counter advanced, or a first frame from a per-slot run.
    #[cfg(test)]
    pub(crate) fn next_for(&self, slot: u8) -> SenderNonce {
        match self.by_slot.get(&slot) {
            Some(mark) => SenderNonce {
                prefix: mark.current.prefix,
                counter: mark.current.counter + 1,
            },
            None => SenderNonce {
                prefix: [slot; NONCE_PREFIX_LEN],
                counter: 0,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(p: u8, counter: u32) -> SenderNonce {
        SenderNonce {
            prefix: [p; NONCE_PREFIX_LEN],
            counter,
        }
    }

    #[test]
    fn a_counter_at_or_below_the_last_accepted_one_is_a_replay() {
        let mut marks = SenderMarks::default();
        assert_eq!(marks.verdict(3, run(1, 10), false), SenderVerdict::Fresh);
        marks.accept(3, run(1, 10));
        assert_eq!(marks.verdict(3, run(1, 10), true), SenderVerdict::Replayed);
        assert_eq!(marks.verdict(3, run(1, 9), true), SenderVerdict::Replayed);
        // Counters need not be contiguous: the bid lane shares the sequence.
        assert_eq!(marks.verdict(3, run(1, 14), true), SenderVerdict::Fresh);
        // Other slots are judged on their own history.
        assert_eq!(marks.verdict(4, run(1, 0), true), SenderVerdict::Fresh);
    }

    #[test]
    fn a_new_run_is_accepted_only_once_the_slot_went_quiet_and_never_goes_back() {
        let mut marks = SenderMarks::default();
        marks.accept(3, run(1, 500));
        assert_eq!(
            marks.verdict(3, run(2, 0), true),
            SenderVerdict::SecondSender,
            "a live slot does not change hands"
        );
        assert_eq!(marks.verdict(3, run(2, 0), false), SenderVerdict::Fresh);
        marks.accept(3, run(2, 0));
        // The superseded run is a replay forever, live slot or not.
        assert_eq!(
            marks.verdict(3, run(1, 501), false),
            SenderVerdict::Replayed
        );
        assert_eq!(marks.verdict(3, run(1, 501), true), SenderVerdict::Replayed);
    }

    #[test]
    fn the_retired_history_is_bounded_and_drops_the_oldest_run() {
        let mut marks = SenderMarks::default();
        for p in 0..=RETIRED_PREFIXES as u8 + 1 {
            marks.accept(3, run(p, 0));
        }
        let mark = &marks.by_slot[&3];
        assert_eq!(mark.retired.len(), RETIRED_PREFIXES);
        assert!(
            !mark.retired.contains(&[0; NONCE_PREFIX_LEN]),
            "oldest dropped"
        );
        assert!(mark.retired.contains(&[1; NONCE_PREFIX_LEN]));
    }
}
